//! UI-independent action intents, destructive confirmation, and result policy.
//!
//! This module is the application half of ADR 0008. It turns a real, current
//! scan node into a validated [`FilesystemTarget`], drives the pure
//! [`ConfirmationFlow`] that produces single-use capabilities
//! ([`Confirmed`] and [`StronglyConfirmed`]), and defines the honest result
//! model the platform adapter must report against. Nothing here performs I/O,
//! names a COM or Win32 type, or lets a display string become an action path.
//!
//! Capabilities bind the operation kind, the exact raw native path or Recycle
//! Bin scope, the scan generation, the target kind, and the stable file
//! identity when the scanner captured one. Their constructors are private to
//! this module and reachable only through the flow, they cannot be cloned,
//! and consuming one yields the plain request plus the typed post-action
//! obligation the runtime must honour after every attempt.

use crate::format::format_path_for_display;
use diskpie_core::{
    EntryKind, FileIdentity, GenerationId, NodeId, ReparseKind, TreeSnapshot, VolumeKey,
};
use std::{
    error::Error,
    fmt,
    path::{Component, Path, PathBuf, Prefix},
};

/// Invariant ASCII word a user must type before permanent deletion.
pub const DELETE_WORD: &str = "DELETE";
/// Invariant ASCII word a user must type before emptying a Recycle Bin.
pub const EMPTY_WORD: &str = "EMPTY";

/// Every Shell action DiskPie can request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ActionKind {
    Open,
    Reveal,
    OpenInstalledApps,
    Recycle,
    DeletePermanently,
    EmptyRecycleBin,
}

impl ActionKind {
    /// Whether the action can mutate the filesystem or a Recycle Bin.
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Recycle | Self::DeletePermanently | Self::EmptyRecycleBin)
    }

    /// Returns the invariant action word required for strong confirmation.
    #[must_use]
    pub const fn required_word(self) -> Option<&'static str> {
        match self {
            Self::DeletePermanently => Some(DELETE_WORD),
            Self::EmptyRecycleBin => Some(EMPTY_WORD),
            Self::Open | Self::Reveal | Self::OpenInstalledApps | Self::Recycle => None,
        }
    }
}

/// Domain kind of a validated real target.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TargetKind {
    /// The scan root itself; valid for open/reveal only.
    ScanRoot,
    Directory,
    File,
    /// A reparse entry: the action addresses the link object, not its target.
    ReparsePoint(ReparseKind),
}

impl TargetKind {
    const fn from_entry(kind: EntryKind) -> Option<Self> {
        match kind {
            EntryKind::Root => Some(Self::ScanRoot),
            EntryKind::Directory => Some(Self::Directory),
            EntryKind::File => Some(Self::File),
            EntryKind::ReparsePoint(reparse) => Some(Self::ReparsePoint(reparse)),
            EntryKind::SyntheticGroup => None,
        }
    }

    /// Whether the target is a reparse entry whose link object is addressed.
    #[must_use]
    pub const fn is_reparse(self) -> bool {
        matches!(self, Self::ReparsePoint(_))
    }
}

/// Known and unknown portions of one aggregate size.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SizeSummary {
    pub known_bytes: u128,
    pub unknown_entries: u64,
}

/// What an action is being validated for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionPurpose {
    Open,
    Reveal,
    Destructive,
}

/// A real, current scan node resolved to its exact native path.
///
/// Only [`TargetValidator`] constructs this type, so every instance passed a
/// synthetic, stale, root, hidden, and self-protection check.
#[derive(Clone, Eq, PartialEq)]
pub struct FilesystemTarget {
    generation: GenerationId,
    node: NodeId,
    parent: Option<NodeId>,
    path: PathBuf,
    parent_path: Option<PathBuf>,
    kind: TargetKind,
    identity: Option<FileIdentity>,
    logical: SizeSummary,
    allocated: SizeSummary,
}

impl FilesystemTarget {
    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        self.generation
    }

    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    #[must_use]
    pub const fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// Returns the exact native path reconstructed by the core model.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the exact native path of the containing directory.
    #[must_use]
    pub fn parent_path(&self) -> Option<&Path> {
        self.parent_path.as_deref()
    }

    #[must_use]
    pub const fn kind(&self) -> TargetKind {
        self.kind
    }

    #[must_use]
    pub const fn identity(&self) -> Option<FileIdentity> {
        self.identity
    }

    #[must_use]
    pub const fn logical(&self) -> SizeSummary {
        self.logical
    }

    #[must_use]
    pub const fn allocated(&self) -> SizeSummary {
        self.allocated
    }

    fn presentation(&self) -> TargetPresentation {
        TargetPresentation {
            exact_path: self.path.clone(),
            display: format_path_for_display(&self.path),
            escaped_utf16: escaped_native_path(&self.path),
            kind: self.kind,
            logical: self.logical,
            allocated: self.allocated,
            reparse_note: self.kind.is_reparse(),
        }
    }
}

impl fmt::Debug for FilesystemTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FilesystemTarget")
            .field("generation", &self.generation)
            .field("node", &self.node)
            .field("parent", &self.parent)
            .field("kind", &self.kind)
            .field("identity", &self.identity)
            .field("path_present", &true)
            .finish()
    }
}

/// Why a node cannot become an action target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetRejection {
    StaleGeneration {
        requested: GenerationId,
        current: GenerationId,
    },
    UnknownNode {
        node: NodeId,
    },
    SyntheticNode {
        node: NodeId,
    },
    HiddenNode {
        node: NodeId,
    },
    EmptyPath {
        node: NodeId,
    },
    /// A drive, volume, or share root is never a destructive target.
    FilesystemRoot {
        node: NodeId,
    },
    /// The scan root has no parent to rescan, so it is never a destructive target.
    ScanRoot {
        node: NodeId,
    },
    RunningExecutable {
        node: NodeId,
    },
    ContainsRunningExecutable {
        node: NodeId,
    },
    /// The running executable path was not supplied, so self-protection
    /// cannot be proven for a destructive action.
    ExecutableUnknown {
        node: NodeId,
    },
}

impl fmt::Display for TargetRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { requested, current } => write!(
                formatter,
                "node belongs to generation {} but the snapshot is generation {}",
                requested.get(),
                current.get()
            ),
            Self::UnknownNode { node } => write!(formatter, "node {node} does not exist"),
            Self::SyntheticNode { node } => {
                write!(formatter, "node {node} is a synthetic presentation group")
            }
            Self::HiddenNode { node } => write!(formatter, "node {node} is hidden"),
            Self::EmptyPath { node } => write!(formatter, "node {node} has no native path"),
            Self::FilesystemRoot { node } => {
                write!(formatter, "node {node} is a drive, volume, or share root")
            }
            Self::ScanRoot { node } => write!(formatter, "node {node} is the scan root"),
            Self::RunningExecutable { node } => {
                write!(formatter, "node {node} is the running executable")
            }
            Self::ContainsRunningExecutable { node } => {
                write!(formatter, "node {node} contains the running executable")
            }
            Self::ExecutableUnknown { node } => write!(
                formatter,
                "node {node} cannot be checked against an unknown executable path"
            ),
        }
    }
}

impl Error for TargetRejection {}

/// Resolves scan nodes into validated targets without performing I/O.
#[derive(Clone, Copy)]
pub struct TargetValidator<'a> {
    snapshot: &'a TreeSnapshot,
    executable: Option<&'a Path>,
}

impl<'a> TargetValidator<'a> {
    /// Creates a validator for one immutable snapshot.
    ///
    /// `executable` is the running executable path as resolved by the
    /// composition root; this module never calls `current_exe` itself.
    #[must_use]
    pub const fn new(snapshot: &'a TreeSnapshot, executable: Option<&'a Path>) -> Self {
        Self { snapshot, executable }
    }

    /// Validates `node`, sealed by the caller with `generation`, for `purpose`.
    ///
    /// `hidden` is the caller's statement that the node is currently hidden
    /// from the chart; hidden nodes are rejected for every purpose.
    pub fn validate(
        &self,
        node: NodeId,
        generation: GenerationId,
        hidden: bool,
        purpose: ActionPurpose,
    ) -> Result<FilesystemTarget, TargetRejection> {
        let current = self.snapshot.generation();
        if generation != current {
            return Err(TargetRejection::StaleGeneration { requested: generation, current });
        }
        let record = self.snapshot.node(node).ok_or(TargetRejection::UnknownNode { node })?;
        let kind =
            TargetKind::from_entry(record.kind()).ok_or(TargetRejection::SyntheticNode { node })?;
        if hidden {
            return Err(TargetRejection::HiddenNode { node });
        }
        let path = self.snapshot.path(node).map_err(|_| TargetRejection::UnknownNode { node })?;
        if path.as_os_str().is_empty() {
            return Err(TargetRejection::EmptyPath { node });
        }
        let parent = record.parent().filter(|parent| {
            self.snapshot
                .node(*parent)
                .is_some_and(|record| record.kind() != EntryKind::SyntheticGroup)
        });
        let parent_path = match parent {
            Some(parent) => {
                let path = self
                    .snapshot
                    .path(parent)
                    .map_err(|_| TargetRejection::UnknownNode { node: parent })?;
                (!path.as_os_str().is_empty()).then_some(path)
            }
            None => None,
        };

        if purpose == ActionPurpose::Destructive {
            if is_filesystem_root(&path) {
                return Err(TargetRejection::FilesystemRoot { node });
            }
            if kind == TargetKind::ScanRoot {
                return Err(TargetRejection::ScanRoot { node });
            }
            let executable = self.executable.ok_or(TargetRejection::ExecutableUnknown { node })?;
            match executable_relation(&path, executable) {
                ExecutableRelation::IsExecutable => {
                    return Err(TargetRejection::RunningExecutable { node });
                }
                ExecutableRelation::ContainsExecutable => {
                    return Err(TargetRejection::ContainsRunningExecutable { node });
                }
                ExecutableRelation::Unrelated => {}
            }
        }

        let aggregate = record.aggregate();
        Ok(FilesystemTarget {
            generation,
            node,
            parent,
            path,
            parent_path,
            kind,
            identity: record.file_identity(),
            logical: SizeSummary {
                known_bytes: aggregate.logical().known_bytes(),
                unknown_entries: aggregate.logical().unknown_entries(),
            },
            allocated: SizeSummary {
                known_bytes: aggregate.allocated().known_bytes(),
                unknown_entries: aggregate.allocated().unknown_entries(),
            },
        })
    }
}

/// Whether a path is a drive, volume, or share root with no further component.
#[must_use]
pub fn is_filesystem_root(path: &Path) -> bool {
    let mut has_prefix = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) => has_prefix = true,
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => return false,
        }
    }
    has_prefix
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutableRelation {
    IsExecutable,
    ContainsExecutable,
    Unrelated,
}

/// Compares a target with the executable path conservatively.
///
/// Components are compared after lossy, case-folded normalization and verbatim
/// prefix stripping. Lossy conversion can only widen the set of rejected
/// targets, which is the fail-closed direction for self-protection.
fn executable_relation(target: &Path, executable: &Path) -> ExecutableRelation {
    let target = comparable_components(target);
    let executable = comparable_components(executable);
    if target.is_empty() || executable.is_empty() {
        return ExecutableRelation::Unrelated;
    }
    if target == executable {
        return ExecutableRelation::IsExecutable;
    }
    if executable.len() > target.len() && executable[..target.len()] == target[..] {
        return ExecutableRelation::ContainsExecutable;
    }
    ExecutableRelation::Unrelated
}

fn comparable_components(path: &Path) -> Vec<String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => components.push(comparable_prefix(prefix.kind())),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => components.push("..".to_owned()),
            Component::Normal(name) => components.push(fold_case(&name.to_string_lossy())),
        }
    }
    components
}

fn comparable_prefix(prefix: Prefix<'_>) -> String {
    match prefix {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            format!("{}:", letter.to_ascii_uppercase() as char)
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            format!(
                "\\\\{}\\{}",
                fold_case(&server.to_string_lossy()),
                fold_case(&share.to_string_lossy())
            )
        }
        Prefix::Verbatim(name) => format!("\\\\?\\{}", fold_case(&name.to_string_lossy())),
        Prefix::DeviceNS(name) => format!("\\\\.\\{}", fold_case(&name.to_string_lossy())),
    }
}

fn fold_case(value: &str) -> String {
    value.to_uppercase()
}

/// Escapes raw UTF-16 code units losslessly for confirmation display.
///
/// Well-formed code units render as their characters. An unpaired surrogate
/// renders as `\u{XXXX}`. A literal backslash directly followed by `u{` is
/// rendered as `\u{5C}` so the output stays unambiguous; every other
/// backslash is left alone because Windows paths contain many of them.
#[must_use]
pub fn escape_utf16(units: &[u16]) -> String {
    let mut output = String::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if (0xD800..0xDC00).contains(&unit) {
            if let Some(&low) = units.get(index + 1)
                && (0xDC00..0xE000).contains(&low)
            {
                let scalar =
                    0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                if let Some(character) = char::from_u32(scalar) {
                    output.push(character);
                    index += 2;
                    continue;
                }
            }
            push_escape(&mut output, unit);
            index += 1;
            continue;
        }
        if (0xDC00..0xE000).contains(&unit) {
            push_escape(&mut output, unit);
            index += 1;
            continue;
        }
        if unit == u16::from(b'\\')
            && units.get(index + 1) == Some(&u16::from(b'u'))
            && units.get(index + 2) == Some(&u16::from(b'{'))
        {
            push_escape(&mut output, unit);
            index += 1;
            continue;
        }
        match char::from_u32(u32::from(unit)) {
            Some(character) => output.push(character),
            None => push_escape(&mut output, unit),
        }
        index += 1;
    }
    output
}

fn push_escape(output: &mut String, unit: u16) {
    output.push_str(&format!("\\u{{{unit:04X}}}"));
}

/// Failure while decoding an [`escape_utf16`] representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnescapeError {
    MalformedEscape { at: usize },
}

impl fmt::Display for UnescapeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedEscape { at } => {
                write!(formatter, "malformed UTF-16 escape at character {at}")
            }
        }
    }
}

impl Error for UnescapeError {}

/// Restores the raw UTF-16 code units from an [`escape_utf16`] string.
pub fn unescape_utf16(text: &str) -> Result<Vec<u16>, UnescapeError> {
    let mut units = Vec::with_capacity(text.len());
    let mut characters = text.char_indices().peekable();
    while let Some((at, character)) = characters.next() {
        if character == '\\' && text[at + 1..].starts_with("u{") {
            let hex_start = at + 3;
            let Some(hex_end) = text[hex_start..].find('}') else {
                return Err(UnescapeError::MalformedEscape { at });
            };
            let hex = &text[hex_start..hex_start + hex_end];
            if hex.is_empty() || hex.len() > 4 {
                return Err(UnescapeError::MalformedEscape { at });
            }
            let unit =
                u16::from_str_radix(hex, 16).map_err(|_| UnescapeError::MalformedEscape { at })?;
            units.push(unit);
            let consumed = hex_start + hex_end + 1;
            while characters.peek().is_some_and(|(index, _)| *index < consumed) {
                characters.next();
            }
            continue;
        }
        let mut buffer = [0_u16; 2];
        units.extend_from_slice(character.encode_utf16(&mut buffer));
    }
    Ok(units)
}

/// Returns the escaped raw UTF-16 spelling only when UTF-8 display is lossy.
#[must_use]
pub fn escaped_native_path(path: &Path) -> Option<String> {
    if path.to_str().is_some() {
        return None;
    }
    escaped_native_path_units(path)
}

#[cfg(windows)]
fn escaped_native_path_units(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;

    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    Some(escape_utf16(&units))
}

#[cfg(not(windows))]
fn escaped_native_path_units(_path: &Path) -> Option<String> {
    None
}

/// Exact `X:\` Recycle Bin root that is never empty.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DriveRoot {
    letter: char,
    path: PathBuf,
}

/// Why a path is not an exact drive root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriveRootError {
    Empty,
    NotADriveRoot,
}

impl fmt::Display for DriveRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("an empty string is not a drive root"),
            Self::NotADriveRoot => formatter.write_str("expected an exact `X:\\` drive root"),
        }
    }
}

impl Error for DriveRootError {}

impl DriveRoot {
    /// Accepts exactly one ASCII drive letter, a colon, and a backslash.
    pub fn parse(value: &str) -> Result<Self, DriveRootError> {
        if value.is_empty() {
            return Err(DriveRootError::Empty);
        }
        let bytes = value.as_bytes();
        match bytes {
            [letter, b':', b'\\'] if letter.is_ascii_alphabetic() => {
                let letter = letter.to_ascii_uppercase() as char;
                Ok(Self { letter, path: PathBuf::from(format!("{letter}:\\")) })
            }
            _ => Err(DriveRootError::NotADriveRoot),
        }
    }

    /// Creates a root from a drive letter.
    pub fn from_letter(letter: char) -> Result<Self, DriveRootError> {
        if !letter.is_ascii_alphabetic() {
            return Err(DriveRootError::NotADriveRoot);
        }
        let letter = letter.to_ascii_uppercase();
        Ok(Self { letter, path: PathBuf::from(format!("{letter}:\\")) })
    }

    #[must_use]
    pub const fn letter(&self) -> char {
        self.letter
    }

    /// Returns the exact `X:\` path; it is non-empty by construction.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

/// Which Recycle Bins an empty-bin action addresses.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RecycleBinScope {
    Drive(DriveRoot),
    /// Every Recycle Bin on every drive; only this explicit variant maps to a
    /// null native root.
    AllDrives,
}

/// Advisory, racy Recycle Bin contents estimate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecycleBinEstimate {
    pub items: u64,
    pub bytes: u64,
}

/// Recycle intent produced only by consuming a [`Confirmed`] capability.
#[derive(Debug, Eq, PartialEq)]
pub struct RecycleRequest {
    target: FilesystemTarget,
}

impl RecycleRequest {
    #[must_use]
    pub const fn target(&self) -> &FilesystemTarget {
        &self.target
    }
}

/// Permanent-deletion intent produced only by consuming a [`StronglyConfirmed`] capability.
#[derive(Debug, Eq, PartialEq)]
pub struct DeleteRequest {
    target: FilesystemTarget,
}

impl DeleteRequest {
    #[must_use]
    pub const fn target(&self) -> &FilesystemTarget {
        &self.target
    }
}

/// Empty-bin intent produced only by consuming a [`StronglyConfirmed`] capability.
#[derive(Debug, Eq, PartialEq)]
pub struct EmptyRecycleBinRequest {
    scope: RecycleBinScope,
    generation: GenerationId,
    estimate: Option<RecycleBinEstimate>,
}

impl EmptyRecycleBinRequest {
    #[must_use]
    pub const fn scope(&self) -> &RecycleBinScope {
        &self.scope
    }

    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        self.generation
    }

    #[must_use]
    pub const fn estimate(&self) -> Option<RecycleBinEstimate> {
        self.estimate
    }
}

/// Non-destructive intent to activate a real item with its default verb.
#[derive(Debug, Eq, PartialEq)]
pub struct OpenItem {
    target: FilesystemTarget,
}

impl OpenItem {
    /// Wraps a target validated for [`ActionPurpose::Open`].
    #[must_use]
    pub const fn new(target: FilesystemTarget) -> Self {
        Self { target }
    }

    #[must_use]
    pub const fn target(&self) -> &FilesystemTarget {
        &self.target
    }
}

/// Non-destructive intent to reveal a real item in Explorer.
#[derive(Debug, Eq, PartialEq)]
pub struct RevealItem {
    target: FilesystemTarget,
}

impl RevealItem {
    /// Wraps a target validated for [`ActionPurpose::Reveal`].
    #[must_use]
    pub const fn new(target: FilesystemTarget) -> Self {
        Self { target }
    }

    #[must_use]
    pub const fn target(&self) -> &FilesystemTarget {
        &self.target
    }
}

/// Non-destructive intent to open the modern Installed Apps settings page.
///
/// The request carries the generation it was issued from purely for
/// correlation; it never carries a URI, verb, or path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenInstalledApps {
    generation: Option<GenerationId>,
}

impl OpenInstalledApps {
    #[must_use]
    pub const fn new(generation: Option<GenerationId>) -> Self {
        Self { generation }
    }

    #[must_use]
    pub const fn generation(self) -> Option<GenerationId> {
        self.generation
    }
}

/// Exact values a capability is bound to.
#[derive(Clone, Eq, PartialEq)]
pub enum BoundTarget {
    Filesystem { path: PathBuf, kind: TargetKind, identity: Option<FileIdentity> },
    RecycleBin { scope: RecycleBinScope },
}

impl fmt::Debug for BoundTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filesystem { kind, identity, .. } => formatter
                .debug_struct("Filesystem")
                .field("kind", kind)
                .field("identity", identity)
                .field("path_present", &true)
                .finish(),
            Self::RecycleBin { scope } => {
                formatter.debug_struct("RecycleBin").field("scope", scope).finish()
            }
        }
    }
}

/// Which bound field differs from the value a capability was checked against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingMismatch {
    Kind,
    Generation,
    Path,
    TargetKind,
    Identity,
    Scope,
}

/// Immutable binding recorded when a capability was issued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionBinding {
    kind: ActionKind,
    generation: GenerationId,
    target: BoundTarget,
}

impl ActionBinding {
    #[must_use]
    pub const fn kind(&self) -> ActionKind {
        self.kind
    }

    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        self.generation
    }

    #[must_use]
    pub const fn target(&self) -> &BoundTarget {
        &self.target
    }

    /// Checks that a filesystem target still matches every bound field.
    pub fn verify_target(&self, current: &FilesystemTarget) -> Result<(), BindingMismatch> {
        let BoundTarget::Filesystem { path, kind, identity } = &self.target else {
            return Err(BindingMismatch::Kind);
        };
        if current.generation != self.generation {
            return Err(BindingMismatch::Generation);
        }
        if current.path.as_os_str() != path.as_os_str() {
            return Err(BindingMismatch::Path);
        }
        if current.kind != *kind {
            return Err(BindingMismatch::TargetKind);
        }
        if current.identity != *identity {
            return Err(BindingMismatch::Identity);
        }
        Ok(())
    }

    /// Checks that a Recycle Bin scope still matches every bound field.
    pub fn verify_scope(
        &self,
        scope: &RecycleBinScope,
        generation: GenerationId,
    ) -> Result<(), BindingMismatch> {
        let BoundTarget::RecycleBin { scope: bound } = &self.target else {
            return Err(BindingMismatch::Kind);
        };
        if generation != self.generation {
            return Err(BindingMismatch::Generation);
        }
        if bound != scope {
            return Err(BindingMismatch::Scope);
        }
        Ok(())
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A destructive request that a capability can bind.
pub trait BindableRequest: sealed::Sealed + fmt::Debug {
    /// Operation kind the capability is locked to.
    const KIND: ActionKind;
    #[doc(hidden)]
    fn binding(&self) -> ActionBinding;
    #[doc(hidden)]
    fn obligation(&self) -> PostActionObligation;
}

/// Marker for requests that ordinary confirmation suffices for.
pub trait WeaklyConfirmable: BindableRequest {}
/// Marker for requests that require the typed action word.
pub trait StronglyConfirmable: BindableRequest {}

impl sealed::Sealed for RecycleRequest {}
impl sealed::Sealed for DeleteRequest {}
impl sealed::Sealed for EmptyRecycleBinRequest {}

impl BindableRequest for RecycleRequest {
    const KIND: ActionKind = ActionKind::Recycle;

    fn binding(&self) -> ActionBinding {
        filesystem_binding(Self::KIND, &self.target)
    }

    fn obligation(&self) -> PostActionObligation {
        parent_obligation(&self.target)
    }
}

impl WeaklyConfirmable for RecycleRequest {}

impl BindableRequest for DeleteRequest {
    const KIND: ActionKind = ActionKind::DeletePermanently;

    fn binding(&self) -> ActionBinding {
        filesystem_binding(Self::KIND, &self.target)
    }

    fn obligation(&self) -> PostActionObligation {
        parent_obligation(&self.target)
    }
}

impl StronglyConfirmable for DeleteRequest {}

impl BindableRequest for EmptyRecycleBinRequest {
    const KIND: ActionKind = ActionKind::EmptyRecycleBin;

    fn binding(&self) -> ActionBinding {
        ActionBinding {
            kind: Self::KIND,
            generation: self.generation,
            target: BoundTarget::RecycleBin { scope: self.scope.clone() },
        }
    }

    fn obligation(&self) -> PostActionObligation {
        PostActionObligation::RescanRecycleBin { scope: self.scope.clone() }
    }
}

impl StronglyConfirmable for EmptyRecycleBinRequest {}

fn filesystem_binding(kind: ActionKind, target: &FilesystemTarget) -> ActionBinding {
    ActionBinding {
        kind,
        generation: target.generation,
        target: BoundTarget::Filesystem {
            path: target.path.clone(),
            kind: target.kind,
            identity: target.identity,
        },
    }
}

fn parent_obligation(target: &FilesystemTarget) -> PostActionObligation {
    match (target.parent, target.parent_path.clone()) {
        (Some(parent), Some(parent_path)) => PostActionObligation::RescanParent {
            generation: target.generation,
            parent,
            parent_path,
        },
        _ => PostActionObligation::RescanAll { generation: target.generation },
    }
}

/// Single-use permission to recycle one exact target.
#[derive(Debug)]
pub struct Confirmed<T: WeaklyConfirmable> {
    request: T,
    binding: ActionBinding,
}

impl<T: WeaklyConfirmable> Confirmed<T> {
    fn issue(request: T) -> Self {
        let binding = request.binding();
        Self { request, binding }
    }

    #[must_use]
    pub const fn binding(&self) -> &ActionBinding {
        &self.binding
    }

    #[must_use]
    pub const fn request(&self) -> &T {
        &self.request
    }

    /// Consumes the capability; the request and obligation cannot be reused.
    #[must_use]
    pub fn consume(self) -> (T, PendingObligation) {
        let obligation = PendingObligation { kind: T::KIND, obligation: self.request.obligation() };
        (self.request, obligation)
    }
}

/// Single-use permission for an irreversible action after the typed word.
#[derive(Debug)]
pub struct StronglyConfirmed<T: StronglyConfirmable> {
    request: T,
    binding: ActionBinding,
}

impl<T: StronglyConfirmable> StronglyConfirmed<T> {
    fn issue(request: T) -> Self {
        let binding = request.binding();
        Self { request, binding }
    }

    #[must_use]
    pub const fn binding(&self) -> &ActionBinding {
        &self.binding
    }

    #[must_use]
    pub const fn request(&self) -> &T {
        &self.request
    }

    /// Consumes the capability; the request and obligation cannot be reused.
    #[must_use]
    pub fn consume(self) -> (T, PendingObligation) {
        let obligation = PendingObligation { kind: T::KIND, obligation: self.request.obligation() };
        (self.request, obligation)
    }
}

/// What the runtime must do after a destructive attempt, whatever its outcome.
#[derive(Clone, Eq, PartialEq)]
pub enum PostActionObligation {
    RescanParent {
        generation: GenerationId,
        parent: NodeId,
        parent_path: PathBuf,
    },
    /// The parent is unavailable in the snapshot; every root must be rescanned.
    RescanAll {
        generation: GenerationId,
    },
    RescanRecycleBin {
        scope: RecycleBinScope,
    },
}

impl fmt::Debug for PostActionObligation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RescanParent { generation, parent, .. } => formatter
                .debug_struct("RescanParent")
                .field("generation", generation)
                .field("parent", parent)
                .field("parent_path_present", &true)
                .finish(),
            Self::RescanAll { generation } => {
                formatter.debug_struct("RescanAll").field("generation", generation).finish()
            }
            Self::RescanRecycleBin { scope } => {
                formatter.debug_struct("RescanRecycleBin").field("scope", scope).finish()
            }
        }
    }
}

/// Obligation retained while the platform executes a consumed capability.
#[derive(Debug)]
pub struct PendingObligation {
    kind: ActionKind,
    obligation: PostActionObligation,
}

impl PendingObligation {
    #[must_use]
    pub const fn kind(&self) -> ActionKind {
        self.kind
    }

    #[must_use]
    pub const fn obligation(&self) -> &PostActionObligation {
        &self.obligation
    }

    /// Combines the obligation with the platform outcome into one report.
    #[must_use]
    pub fn settle(self, outcome: ActionOutcome) -> ActionReport {
        ActionReport { kind: self.kind, obligation: self.obligation, outcome }
    }
}

/// Outcome of a consumed destructive capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionOutcome {
    Filesystem(DestructiveOutcome),
    RecycleBin(EmptyBinOutcome),
}

impl From<DestructiveOutcome> for ActionOutcome {
    fn from(outcome: DestructiveOutcome) -> Self {
        Self::Filesystem(outcome)
    }
}

impl From<EmptyBinOutcome> for ActionOutcome {
    fn from(outcome: EmptyBinOutcome) -> Self {
        Self::RecycleBin(outcome)
    }
}

/// Everything the runtime consumes after a destructive attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionReport {
    pub kind: ActionKind,
    pub obligation: PostActionObligation,
    pub outcome: ActionOutcome,
}

impl ActionReport {
    /// Whether DiskPie must stop subsequent destructive work and surface an incident.
    #[must_use]
    pub const fn requires_halt(&self) -> bool {
        matches!(
            self.outcome,
            ActionOutcome::Filesystem(
                DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete { .. }
            )
        )
    }
}

/// Native stage at which a destructive attempt failed or became uncertain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureStage {
    IdentityValidation,
    CreateOperation,
    SetOwnerWindow,
    SetOperationFlags,
    Advise,
    CreateShellItem,
    QueueDelete,
    PerformOperations,
    QueryAborted,
    PostDeleteItem,
    FinishOperations,
    MissingCallbacks,
    Unadvise,
    QueryRecycleBin,
    EmptyRecycleBin,
    WorkerPanicked,
    ServiceShutdown,
}

/// Why the target no longer matches its confirmation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetChangeReason {
    Missing,
    IdentityMismatch,
    KindMismatch,
}

/// How strongly the late validation matched the confirmed target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityAssurance {
    /// Volume and 128-bit file identity matched the confirmation.
    Exact,
    /// No stable identity was captured; only existence and kind were checked.
    KindOnly,
}

/// Evidence that a recycle callback identified a Recycle Bin item.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct RecycleReceipt {
    /// Display name of the new Recycle Bin item, for the UI only.
    pub display_name: Option<String>,
}

impl fmt::Debug for RecycleReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecycleReceipt")
            .field("display_name_present", &self.display_name.is_some())
            .finish()
    }
}

/// Actual effect on one item, taken from the per-item callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ItemEffect {
    Recycled { receipt: RecycleReceipt },
    PermanentlyDeleted,
    Unchanged,
    Unknown,
}

/// One per-item callback result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemResult {
    pub code: i32,
    pub effect: ItemEffect,
}

/// Result of a recycle or permanent-deletion attempt per ADR 0008 items 12-13.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DestructiveOutcome {
    Completed { items: Vec<ItemResult>, assurance: IdentityAssurance },
    CancelledBeforeMutation,
    Partial { committed: usize, failed: usize, aborted: bool, items: Vec<ItemResult> },
    Failed { stage: FailureStage, code: Option<i32> },
    UnknownMayHaveMutated { stage: FailureStage, code: Option<i32> },
    SafetyViolationUnexpectedPermanentDelete { items: Vec<ItemResult> },
    TargetChanged { reason: TargetChangeReason },
}

/// Result of an empty-bin attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EmptyBinOutcome {
    Completed {
        before: Option<RecycleBinEstimate>,
        after: Option<RecycleBinEstimate>,
    },
    CancelledBeforeMutation,
    Failed {
        stage: FailureStage,
        code: Option<i32>,
        before: Option<RecycleBinEstimate>,
    },
    UnknownMayHaveMutated {
        stage: FailureStage,
        code: Option<i32>,
        before: Option<RecycleBinEstimate>,
        after: Option<RecycleBinEstimate>,
    },
}

/// Which deletion semantics were requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteMode {
    Recycle,
    Permanent,
}

/// One `PreDeleteItem` callback observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreDeleteEvidence {
    /// The sink returned the cancellation HRESULT instead of allowing the item.
    pub cancelled: bool,
}

/// One `PostDeleteItem` callback observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostDeleteEvidence {
    pub hresult: i32,
    /// `Some` when a newly created Recycle Bin item was reported.
    pub newly_created: Option<RecycleReceipt>,
}

/// Plain-data record of everything one `IFileOperation` attempt reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteEvidence {
    pub mode: DeleteMode,
    pub target_kind: TargetKind,
    pub assurance: IdentityAssurance,
    pub cancel_requested: bool,
    pub started: bool,
    pub pre_delete: Vec<PreDeleteEvidence>,
    pub post_delete: Vec<PostDeleteEvidence>,
    /// `FinishOperations` HRESULT when the callback arrived.
    pub finish: Option<i32>,
    pub perform: Result<(), i32>,
    pub aborted: Result<bool, i32>,
}

/// Classifies native evidence exactly per ADR 0008 item 13.
///
/// Queuing and `PerformOperations` success are never completion. Success
/// requires consistent per-item callbacks, a successful final callback, no
/// abort, and, for recycling, a newly created Recycle Bin item per item.
#[must_use]
pub fn classify_delete_evidence(evidence: &DeleteEvidence) -> DestructiveOutcome {
    let mut items = Vec::with_capacity(evidence.post_delete.len());
    let mut committed = 0_usize;
    let mut failed = 0_usize;
    let mut safety_violation = false;
    for post in &evidence.post_delete {
        let succeeded = post.hresult >= 0;
        let effect = match (succeeded, evidence.mode, &post.newly_created) {
            (true, DeleteMode::Recycle, Some(receipt)) => {
                ItemEffect::Recycled { receipt: receipt.clone() }
            }
            (true, DeleteMode::Recycle, None) => {
                safety_violation = true;
                ItemEffect::PermanentlyDeleted
            }
            (true, DeleteMode::Permanent, _) => ItemEffect::PermanentlyDeleted,
            (false, _, _) => match evidence.target_kind {
                TargetKind::Directory | TargetKind::ScanRoot => ItemEffect::Unknown,
                TargetKind::File | TargetKind::ReparsePoint(_) => ItemEffect::Unchanged,
            },
        };
        if succeeded {
            committed += 1;
        } else {
            failed += 1;
        }
        items.push(ItemResult { code: post.hresult, effect });
    }

    if safety_violation {
        return DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete { items };
    }

    let cancelled_before_item = evidence.pre_delete.iter().any(|pre| pre.cancelled);
    let mutation_started = evidence.pre_delete.iter().any(|pre| !pre.cancelled);
    let aborted = match evidence.aborted {
        Ok(aborted) => aborted,
        Err(code) => {
            if committed == 0 && !mutation_started {
                return DestructiveOutcome::Failed {
                    stage: FailureStage::QueryAborted,
                    code: Some(code),
                };
            }
            return DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::QueryAborted,
                code: Some(code),
            };
        }
    };

    if evidence.post_delete.is_empty() {
        if (evidence.cancel_requested || cancelled_before_item) && !mutation_started {
            return DestructiveOutcome::CancelledBeforeMutation;
        }
        return match evidence.perform {
            Err(code) if !mutation_started => DestructiveOutcome::Failed {
                stage: FailureStage::PerformOperations,
                code: Some(code),
            },
            Err(code) => DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::PerformOperations,
                code: Some(code),
            },
            Ok(()) => DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::MissingCallbacks,
                code: None,
            },
        };
    }

    if committed == 0 {
        let code = items.first().map(|item| item.code);
        return match evidence.target_kind {
            TargetKind::Directory | TargetKind::ScanRoot => {
                DestructiveOutcome::UnknownMayHaveMutated {
                    stage: FailureStage::PostDeleteItem,
                    code,
                }
            }
            TargetKind::File | TargetKind::ReparsePoint(_) => {
                DestructiveOutcome::Failed { stage: FailureStage::PostDeleteItem, code }
            }
        };
    }

    let perform_failed = evidence.perform.is_err();
    let finish_failed = evidence.finish.is_some_and(|code| code < 0);
    if failed > 0 || aborted || cancelled_before_item || perform_failed || finish_failed {
        if failed == 0 && !aborted && !cancelled_before_item {
            let (stage, code) = match evidence.perform {
                Err(code) => (FailureStage::PerformOperations, Some(code)),
                Ok(()) => (FailureStage::FinishOperations, evidence.finish),
            };
            return DestructiveOutcome::UnknownMayHaveMutated { stage, code };
        }
        return DestructiveOutcome::Partial {
            committed,
            failed,
            aborted: aborted || cancelled_before_item,
            items,
        };
    }

    if !evidence.started || evidence.finish.is_none() {
        return DestructiveOutcome::UnknownMayHaveMutated {
            stage: FailureStage::MissingCallbacks,
            code: None,
        };
    }

    DestructiveOutcome::Completed { items, assurance: evidence.assurance }
}

/// Exact-path presentation for a recycle or delete confirmation.
#[derive(Clone, Eq, PartialEq)]
pub struct TargetPresentation {
    /// The exact native path; never a display conversion.
    pub exact_path: PathBuf,
    /// Friendly lossy display without verbatim prefixes.
    pub display: String,
    /// Lossless escaped raw UTF-16 spelling, present only when `display` is lossy.
    pub escaped_utf16: Option<String>,
    pub kind: TargetKind,
    pub logical: SizeSummary,
    pub allocated: SizeSummary,
    /// The item is a reparse entry: the link object, not its target, is addressed.
    pub reparse_note: bool,
}

impl fmt::Debug for TargetPresentation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TargetPresentation")
            .field("kind", &self.kind)
            .field("logical", &self.logical)
            .field("allocated", &self.allocated)
            .field("reparse_note", &self.reparse_note)
            .field("escaped_present", &self.escaped_utf16.is_some())
            .field("path_present", &true)
            .finish()
    }
}

/// Scope presentation for an empty-bin confirmation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinPresentation {
    pub scope: RecycleBinScope,
    pub estimate: Option<RecycleBinEstimate>,
    /// Always true: nothing can stop `SHEmptyRecycleBinW` after dispatch.
    pub cancellation_unavailable_after_dispatch: bool,
}

/// What a confirmation dialog must show.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmationPresentation {
    Filesystem { kind: ActionKind, target: TargetPresentation },
    RecycleBin(BinPresentation),
}

impl ConfirmationPresentation {
    #[must_use]
    pub const fn kind(&self) -> ActionKind {
        match self {
            Self::Filesystem { kind, .. } => *kind,
            Self::RecycleBin(_) => ActionKind::EmptyRecycleBin,
        }
    }
}

/// Externally observable flow state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowStatus {
    Idle,
    Reviewing { kind: ActionKind },
    AwaitingWord { kind: ActionKind, required_word: &'static str, matches: bool },
    Confirmed { kind: ActionKind },
    StronglyConfirmed { kind: ActionKind },
}

/// Why a flow transition was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowError {
    NotIdle,
    NotReviewing,
    NotAwaitingWord,
    WordMismatch,
    NothingConfirmed,
}

impl fmt::Display for FlowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NotIdle => "a confirmation is already in progress",
            Self::NotReviewing => "no confirmation is being reviewed",
            Self::NotAwaitingWord => "the confirmation is not awaiting the action word",
            Self::WordMismatch => "the typed word does not match the required action word",
            Self::NothingConfirmed => "no confirmed capability is available",
        };
        formatter.write_str(message)
    }
}

impl Error for FlowError {}

enum PendingIntent {
    Recycle(FilesystemTarget),
    Delete(FilesystemTarget),
    EmptyBin(EmptyRecycleBinRequest),
}

impl PendingIntent {
    const fn kind(&self) -> ActionKind {
        match self {
            Self::Recycle(_) => ActionKind::Recycle,
            Self::Delete(_) => ActionKind::DeletePermanently,
            Self::EmptyBin(_) => ActionKind::EmptyRecycleBin,
        }
    }
}

enum FlowState {
    Idle,
    Reviewing { intent: PendingIntent, presentation: ConfirmationPresentation },
    AwaitingWord { intent: PendingIntent, presentation: ConfirmationPresentation, typed: String },
    Confirmed(Confirmed<RecycleRequest>),
    StronglyConfirmedDelete(StronglyConfirmed<DeleteRequest>),
    StronglyConfirmedEmpty(StronglyConfirmed<EmptyRecycleBinRequest>),
}

/// Pure confirmation state machine for ADR 0008.
///
/// `Idle -> Reviewing -> Confirmed` for recycling;
/// `Idle -> Reviewing -> AwaitingWord -> StronglyConfirmed` for permanent
/// deletion and emptying a bin. Cancel is available from every state and is
/// the only way back to `Idle` besides taking the issued capability.
pub struct ConfirmationFlow {
    state: FlowState,
}

impl Default for ConfirmationFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfirmationFlow {
    #[must_use]
    pub const fn new() -> Self {
        Self { state: FlowState::Idle }
    }

    /// Returns the observable state without exposing capabilities.
    #[must_use]
    pub fn status(&self) -> FlowStatus {
        match &self.state {
            FlowState::Idle => FlowStatus::Idle,
            FlowState::Reviewing { intent, .. } => FlowStatus::Reviewing { kind: intent.kind() },
            FlowState::AwaitingWord { intent, typed, .. } => {
                let kind = intent.kind();
                let required_word = kind.required_word().unwrap_or("");
                FlowStatus::AwaitingWord { kind, required_word, matches: word_matches(typed, kind) }
            }
            FlowState::Confirmed(_) => FlowStatus::Confirmed { kind: ActionKind::Recycle },
            FlowState::StronglyConfirmedDelete(_) => {
                FlowStatus::StronglyConfirmed { kind: ActionKind::DeletePermanently }
            }
            FlowState::StronglyConfirmedEmpty(_) => {
                FlowStatus::StronglyConfirmed { kind: ActionKind::EmptyRecycleBin }
            }
        }
    }

    /// Returns what the dialog must show while reviewing or typing.
    #[must_use]
    pub const fn presentation(&self) -> Option<&ConfirmationPresentation> {
        match &self.state {
            FlowState::Reviewing { presentation, .. }
            | FlowState::AwaitingWord { presentation, .. } => Some(presentation),
            FlowState::Idle
            | FlowState::Confirmed(_)
            | FlowState::StronglyConfirmedDelete(_)
            | FlowState::StronglyConfirmedEmpty(_) => None,
        }
    }

    /// Returns the action word typed so far.
    #[must_use]
    pub fn typed_word(&self) -> Option<&str> {
        match &self.state {
            FlowState::AwaitingWord { typed, .. } => Some(typed),
            _ => None,
        }
    }

    /// Starts reviewing a recycle of a target validated for [`ActionPurpose::Destructive`].
    pub fn begin_recycle(&mut self, target: FilesystemTarget) -> Result<(), FlowError> {
        self.begin(PendingIntent::Recycle(target))
    }

    /// Starts reviewing a permanent deletion of a validated target.
    pub fn begin_delete(&mut self, target: FilesystemTarget) -> Result<(), FlowError> {
        self.begin(PendingIntent::Delete(target))
    }

    /// Starts reviewing an empty-bin action for an exact scope.
    pub fn begin_empty_recycle_bin(
        &mut self,
        scope: RecycleBinScope,
        generation: GenerationId,
        estimate: Option<RecycleBinEstimate>,
    ) -> Result<(), FlowError> {
        self.begin(PendingIntent::EmptyBin(EmptyRecycleBinRequest { scope, generation, estimate }))
    }

    fn begin(&mut self, intent: PendingIntent) -> Result<(), FlowError> {
        if !matches!(self.state, FlowState::Idle) {
            return Err(FlowError::NotIdle);
        }
        let presentation = match &intent {
            PendingIntent::Recycle(target) => ConfirmationPresentation::Filesystem {
                kind: ActionKind::Recycle,
                target: target.presentation(),
            },
            PendingIntent::Delete(target) => ConfirmationPresentation::Filesystem {
                kind: ActionKind::DeletePermanently,
                target: target.presentation(),
            },
            PendingIntent::EmptyBin(request) => {
                ConfirmationPresentation::RecycleBin(BinPresentation {
                    scope: request.scope.clone(),
                    estimate: request.estimate,
                    cancellation_unavailable_after_dispatch: true,
                })
            }
        };
        self.state = FlowState::Reviewing { intent, presentation };
        Ok(())
    }

    /// Accepts the review: recycling is confirmed, stronger actions await the word.
    pub fn accept_review(&mut self) -> Result<FlowStatus, FlowError> {
        if !matches!(self.state, FlowState::Reviewing { .. }) {
            return Err(FlowError::NotReviewing);
        }
        let FlowState::Reviewing { intent, presentation } =
            std::mem::replace(&mut self.state, FlowState::Idle)
        else {
            unreachable!("state was checked above");
        };
        self.state = match intent {
            PendingIntent::Recycle(target) => {
                FlowState::Confirmed(Confirmed::issue(RecycleRequest { target }))
            }
            intent @ (PendingIntent::Delete(_) | PendingIntent::EmptyBin(_)) => {
                FlowState::AwaitingWord { intent, presentation, typed: String::new() }
            }
        };
        Ok(self.status())
    }

    /// Replaces the typed action word; no normalization is applied.
    pub fn set_typed_word(&mut self, text: &str) -> Result<FlowStatus, FlowError> {
        let FlowState::AwaitingWord { typed, .. } = &mut self.state else {
            return Err(FlowError::NotAwaitingWord);
        };
        typed.clear();
        typed.push_str(text);
        Ok(self.status())
    }

    /// Issues the strong capability when the typed word matches exactly.
    pub fn confirm_word(&mut self) -> Result<FlowStatus, FlowError> {
        let FlowState::AwaitingWord { intent, typed, .. } = &self.state else {
            return Err(FlowError::NotAwaitingWord);
        };
        if !word_matches(typed, intent.kind()) {
            return Err(FlowError::WordMismatch);
        }
        let FlowState::AwaitingWord { intent, .. } =
            std::mem::replace(&mut self.state, FlowState::Idle)
        else {
            unreachable!("state was checked above");
        };
        self.state = match intent {
            PendingIntent::Delete(target) => {
                FlowState::StronglyConfirmedDelete(StronglyConfirmed::issue(DeleteRequest {
                    target,
                }))
            }
            PendingIntent::EmptyBin(request) => {
                FlowState::StronglyConfirmedEmpty(StronglyConfirmed::issue(request))
            }
            PendingIntent::Recycle(_) => {
                unreachable!("recycling never awaits a word")
            }
        };
        Ok(self.status())
    }

    /// Returns to `Idle` from any state, discarding an unissued or issued capability.
    pub fn cancel(&mut self) {
        self.state = FlowState::Idle;
    }

    /// Takes the recycle capability; the flow returns to `Idle`.
    pub fn take_recycle(&mut self) -> Result<Confirmed<RecycleRequest>, FlowError> {
        match std::mem::replace(&mut self.state, FlowState::Idle) {
            FlowState::Confirmed(capability) => Ok(capability),
            other => {
                self.state = other;
                Err(FlowError::NothingConfirmed)
            }
        }
    }

    /// Takes the permanent-deletion capability; the flow returns to `Idle`.
    pub fn take_delete(&mut self) -> Result<StronglyConfirmed<DeleteRequest>, FlowError> {
        match std::mem::replace(&mut self.state, FlowState::Idle) {
            FlowState::StronglyConfirmedDelete(capability) => Ok(capability),
            other => {
                self.state = other;
                Err(FlowError::NothingConfirmed)
            }
        }
    }

    /// Takes the empty-bin capability; the flow returns to `Idle`.
    pub fn take_empty_recycle_bin(
        &mut self,
    ) -> Result<StronglyConfirmed<EmptyRecycleBinRequest>, FlowError> {
        match std::mem::replace(&mut self.state, FlowState::Idle) {
            FlowState::StronglyConfirmedEmpty(capability) => Ok(capability),
            other => {
                self.state = other;
                Err(FlowError::NothingConfirmed)
            }
        }
    }
}

impl fmt::Debug for ConfirmationFlow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ConfirmationFlow").field("status", &self.status()).finish()
    }
}

/// Exact, case-sensitive, whitespace-intolerant word comparison.
#[must_use]
pub fn word_matches(typed: &str, kind: ActionKind) -> bool {
    kind.required_word().is_some_and(|required| typed == required)
}

/// Constructs a portable file identity from raw volume and file identifiers.
#[must_use]
pub const fn identity_from_raw(volume_serial: u64, file_id: u128) -> FileIdentity {
    FileIdentity::new(VolumeKey::new(volume_serial as u128), file_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder};
    use std::ffi::OsString;

    fn identity(file: u128) -> FileIdentity {
        FileIdentity::new(VolumeKey::new(7), file)
    }

    fn file(name: impl Into<OsString>, file_identity: Option<FileIdentity>) -> NodeSpec {
        let metrics = OwnMetrics::new(
            SizeMetric::known(10, MetricSource::PortableMetadata),
            SizeMetric::known(20, MetricSource::FilesystemAllocation),
        );
        let spec = NodeSpec::file(name, metrics);
        match file_identity {
            Some(file_identity) => spec.with_file_identity(file_identity),
            None => spec,
        }
    }

    struct Fixture {
        snapshot: TreeSnapshot,
        root: NodeId,
        directory: NodeId,
        bin: NodeId,
        exe: NodeId,
        leaf: NodeId,
        link: NodeId,
    }

    fn fixture(generation: u64) -> Fixture {
        let mut builder = TreeBuilder::new(GenerationId::new(generation));
        let root = builder.add_root(NodeSpec::root(r"C:\zqx")).expect("root");
        let directory =
            builder.add_child(root, NodeSpec::directory("directory")).expect("directory");
        let leaf =
            builder.add_child(directory, file("leaf.bin", Some(identity(12)))).expect("leaf");
        let bin = builder.add_child(root, NodeSpec::directory("bin")).expect("bin");
        let exe = builder.add_child(bin, file("diskpie.exe", Some(identity(13)))).expect("exe");
        let link = builder
            .add_child(
                root,
                NodeSpec::new(
                    "link",
                    EntryKind::ReparsePoint(ReparseKind::Directory),
                    OwnMetrics::ZERO_BY_POLICY,
                ),
            )
            .expect("link");
        Fixture {
            snapshot: builder.freeze().expect("snapshot"),
            root,
            directory,
            bin,
            exe,
            leaf,
            link,
        }
    }

    fn generation(value: u64) -> GenerationId {
        GenerationId::new(value)
    }

    fn executable() -> PathBuf {
        PathBuf::from(r"C:\zqx\bin\diskpie.exe")
    }

    fn validator<'a>(fixture: &'a Fixture, executable: Option<&'a Path>) -> TargetValidator<'a> {
        TargetValidator::new(&fixture.snapshot, executable)
    }

    fn destructive_target(fixture: &Fixture, node: NodeId) -> FilesystemTarget {
        let executable = executable();
        validator(fixture, Some(&executable))
            .validate(node, generation(1), false, ActionPurpose::Destructive)
            .expect("valid destructive target")
    }

    fn evidence(mode: DeleteMode) -> DeleteEvidence {
        DeleteEvidence {
            mode,
            target_kind: TargetKind::File,
            assurance: IdentityAssurance::Exact,
            cancel_requested: false,
            started: true,
            pre_delete: vec![PreDeleteEvidence { cancelled: false }],
            post_delete: vec![PostDeleteEvidence {
                hresult: 0,
                newly_created: Some(RecycleReceipt { display_name: Some("leaf.bin".into()) }),
            }],
            finish: Some(0),
            perform: Ok(()),
            aborted: Ok(false),
        }
    }

    #[test]
    fn validates_a_real_current_file_with_sizes_identity_and_parent() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        assert_eq!(target.kind(), TargetKind::File);
        assert_eq!(target.identity(), Some(identity(12)));
        assert_eq!(target.parent(), Some(fixture.directory));
        assert_eq!(target.logical(), SizeSummary { known_bytes: 10, unknown_entries: 0 });
        assert_eq!(target.allocated(), SizeSummary { known_bytes: 20, unknown_entries: 0 });
        assert_eq!(target.path(), Path::new(r"C:\zqx\directory\leaf.bin"));
        assert_eq!(target.parent_path(), Some(Path::new(r"C:\zqx\directory")));
    }

    #[test]
    fn rejects_stale_generation_before_anything_else() {
        let fixture = fixture(3);
        let executable = executable();
        let error = validator(&fixture, Some(&executable))
            .validate(fixture.leaf, generation(2), false, ActionPurpose::Destructive)
            .expect_err("stale generation");
        assert_eq!(
            error,
            TargetRejection::StaleGeneration { requested: generation(2), current: generation(3) }
        );
    }

    #[test]
    fn rejects_unknown_hidden_and_synthetic_nodes() {
        let fixture = fixture(1);
        let executable = executable();
        let validator = validator(&fixture, Some(&executable));
        let unknown = NodeId::from_raw(99);
        assert_eq!(
            validator.validate(unknown, generation(1), false, ActionPurpose::Open),
            Err(TargetRejection::UnknownNode { node: unknown })
        );
        assert_eq!(
            validator.validate(fixture.leaf, generation(1), true, ActionPurpose::Open),
            Err(TargetRejection::HiddenNode { node: fixture.leaf })
        );

        let mut builder = TreeBuilder::new(generation(1));
        let group = builder.add_root(NodeSpec::synthetic_group("summary")).expect("group");
        builder.add_child(group, NodeSpec::root(r"C:\")).expect("root");
        let snapshot = builder.freeze().expect("snapshot");
        assert_eq!(
            TargetValidator::new(&snapshot, Some(&executable)).validate(
                group,
                generation(1),
                false,
                ActionPurpose::Reveal
            ),
            Err(TargetRejection::SyntheticNode { node: group })
        );
    }

    #[test]
    fn rejects_scan_root_and_filesystem_roots_for_destructive_purposes_only() {
        let fixture = fixture(1);
        let executable = executable();
        let validator = validator(&fixture, Some(&executable));
        assert_eq!(
            validator.validate(fixture.root, generation(1), false, ActionPurpose::Destructive),
            Err(TargetRejection::ScanRoot { node: fixture.root })
        );
        let opened = validator
            .validate(fixture.root, generation(1), false, ActionPurpose::Open)
            .expect("the scan root can be opened");
        assert_eq!(opened.kind(), TargetKind::ScanRoot);

        #[cfg(windows)]
        {
            let mut builder = TreeBuilder::new(generation(1));
            let drive = builder.add_root(NodeSpec::root(r"C:\")).expect("drive");
            let snapshot = builder.freeze().expect("snapshot");
            assert_eq!(
                TargetValidator::new(&snapshot, Some(&executable)).validate(
                    drive,
                    generation(1),
                    false,
                    ActionPurpose::Destructive
                ),
                Err(TargetRejection::FilesystemRoot { node: drive })
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn filesystem_root_detection_covers_drive_unc_and_volume_roots() {
        for root in [
            r"C:\",
            r"c:",
            r"\\?\C:\",
            r"\\server\share\",
            r"\\?\UNC\server\share",
            r"\\?\Volume{1}\",
        ] {
            assert!(is_filesystem_root(Path::new(root)), "{root}");
        }
        for path in [r"C:\Users", r"\\server\share\dir", r"\\?\C:\x", r"relative", ""] {
            assert!(!is_filesystem_root(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn rejects_the_running_executable_and_its_containing_directories() {
        let fixture = fixture(1);
        let executable = executable();
        let validator = validator(&fixture, Some(&executable));
        assert_eq!(
            validator.validate(fixture.exe, generation(1), false, ActionPurpose::Destructive),
            Err(TargetRejection::RunningExecutable { node: fixture.exe })
        );
        assert_eq!(
            validator.validate(fixture.bin, generation(1), false, ActionPurpose::Destructive),
            Err(TargetRejection::ContainsRunningExecutable { node: fixture.bin })
        );
        assert!(
            validator
                .validate(fixture.directory, generation(1), false, ActionPurpose::Destructive)
                .is_ok()
        );
        assert!(validator.validate(fixture.exe, generation(1), false, ActionPurpose::Open).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn executable_comparison_is_case_and_prefix_insensitive() {
        let fixture = fixture(1);
        let executable = PathBuf::from(r"\\?\c:\ZQX\BIN\DiskPie.EXE");
        let validator = validator(&fixture, Some(&executable));
        assert_eq!(
            validator.validate(fixture.exe, generation(1), false, ActionPurpose::Destructive),
            Err(TargetRejection::RunningExecutable { node: fixture.exe })
        );
    }

    #[test]
    fn destructive_validation_without_an_executable_path_fails_closed() {
        let fixture = fixture(1);
        let validator = validator(&fixture, None);
        assert_eq!(
            validator.validate(fixture.leaf, generation(1), false, ActionPurpose::Destructive),
            Err(TargetRejection::ExecutableUnknown { node: fixture.leaf })
        );
        assert!(
            validator.validate(fixture.leaf, generation(1), false, ActionPurpose::Reveal).is_ok()
        );
    }

    #[test]
    fn recycle_flow_issues_a_single_use_capability_bound_to_the_target() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        let mut flow = ConfirmationFlow::new();
        assert_eq!(flow.status(), FlowStatus::Idle);
        flow.begin_recycle(target.clone()).expect("idle flow accepts a review");
        assert_eq!(flow.status(), FlowStatus::Reviewing { kind: ActionKind::Recycle });
        let presentation = flow.presentation().expect("reviewing shows a presentation");
        let ConfirmationPresentation::Filesystem { kind, target: shown } = presentation else {
            panic!("filesystem presentation expected");
        };
        assert_eq!(*kind, ActionKind::Recycle);
        assert_eq!(shown.exact_path, target.path());
        assert_eq!(shown.display, r"C:\zqx\directory\leaf.bin");
        assert_eq!(shown.escaped_utf16, None);
        assert!(!shown.reparse_note);
        assert_eq!(flow.begin_recycle(target.clone()), Err(FlowError::NotIdle));

        assert_eq!(flow.accept_review(), Ok(FlowStatus::Confirmed { kind: ActionKind::Recycle }));
        assert_eq!(flow.presentation(), None);
        assert_eq!(flow.take_delete().expect_err("wrong kind"), FlowError::NothingConfirmed);
        let capability = flow.take_recycle().expect("recycle capability");
        assert_eq!(flow.status(), FlowStatus::Idle);
        assert_eq!(flow.take_recycle().expect_err("single use"), FlowError::NothingConfirmed);

        assert_eq!(capability.binding().kind(), ActionKind::Recycle);
        assert_eq!(capability.binding().verify_target(&target), Ok(()));
        let (request, pending) = capability.consume();
        assert_eq!(request.target(), &target);
        assert_eq!(
            pending.obligation(),
            &PostActionObligation::RescanParent {
                generation: generation(1),
                parent: fixture.directory,
                parent_path: PathBuf::from(r"C:\zqx\directory"),
            }
        );
        let report = pending.settle(DestructiveOutcome::CancelledBeforeMutation.into());
        assert_eq!(report.kind, ActionKind::Recycle);
        assert!(!report.requires_halt());
    }

    #[test]
    fn delete_flow_requires_the_exact_word() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.directory);
        let mut flow = ConfirmationFlow::new();
        flow.begin_delete(target.clone()).expect("begin");
        assert_eq!(flow.confirm_word(), Err(FlowError::NotAwaitingWord));
        assert_eq!(
            flow.accept_review(),
            Ok(FlowStatus::AwaitingWord {
                kind: ActionKind::DeletePermanently,
                required_word: "DELETE",
                matches: false
            })
        );
        assert!(flow.presentation().is_some(), "the exact path stays visible while typing");
        for wrong in
            ["delete", "Delete", " DELETE", "DELETE ", "DELETE\n", "DELET", "DELETED", "EMPTY"]
        {
            flow.set_typed_word(wrong).expect("typing is allowed");
            assert_eq!(flow.typed_word(), Some(wrong));
            assert_eq!(flow.confirm_word(), Err(FlowError::WordMismatch), "{wrong:?}");
            assert!(matches!(flow.status(), FlowStatus::AwaitingWord { matches: false, .. }));
        }
        flow.set_typed_word("DELETE").expect("typing");
        assert!(matches!(flow.status(), FlowStatus::AwaitingWord { matches: true, .. }));
        assert_eq!(
            flow.confirm_word(),
            Ok(FlowStatus::StronglyConfirmed { kind: ActionKind::DeletePermanently })
        );
        assert_eq!(flow.take_recycle().expect_err("wrong kind"), FlowError::NothingConfirmed);
        let capability = flow.take_delete().expect("delete capability");
        assert_eq!(capability.binding().kind(), ActionKind::DeletePermanently);
        let (request, pending) = capability.consume();
        assert_eq!(request.target().kind(), TargetKind::Directory);
        assert_eq!(pending.kind(), ActionKind::DeletePermanently);
    }

    #[test]
    fn empty_bin_flow_binds_the_exact_scope_and_word() {
        let scope = RecycleBinScope::Drive(DriveRoot::parse(r"d:\").expect("root"));
        let mut flow = ConfirmationFlow::new();
        flow.begin_empty_recycle_bin(
            scope.clone(),
            generation(4),
            Some(RecycleBinEstimate { items: 3, bytes: 900 }),
        )
        .expect("begin");
        let Some(ConfirmationPresentation::RecycleBin(presentation)) = flow.presentation() else {
            panic!("bin presentation expected");
        };
        assert_eq!(presentation.scope, scope);
        assert_eq!(presentation.estimate, Some(RecycleBinEstimate { items: 3, bytes: 900 }));
        assert!(presentation.cancellation_unavailable_after_dispatch);
        flow.accept_review().expect("accept");
        flow.set_typed_word("DELETE").expect("typing");
        assert_eq!(flow.confirm_word(), Err(FlowError::WordMismatch));
        flow.set_typed_word("EMPTY").expect("typing");
        assert_eq!(
            flow.confirm_word(),
            Ok(FlowStatus::StronglyConfirmed { kind: ActionKind::EmptyRecycleBin })
        );
        let capability = flow.take_empty_recycle_bin().expect("capability");
        assert_eq!(capability.binding().verify_scope(&scope, generation(4)), Ok(()));
        assert_eq!(
            capability.binding().verify_scope(&RecycleBinScope::AllDrives, generation(4)),
            Err(BindingMismatch::Scope)
        );
        assert_eq!(
            capability.binding().verify_scope(&scope, generation(5)),
            Err(BindingMismatch::Generation)
        );
        let (request, pending) = capability.consume();
        assert_eq!(request.scope(), &scope);
        assert_eq!(pending.obligation(), &PostActionObligation::RescanRecycleBin { scope });
    }

    #[test]
    fn cancel_is_available_from_every_state() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        let mut flow = ConfirmationFlow::new();
        flow.cancel();
        assert_eq!(flow.status(), FlowStatus::Idle);

        flow.begin_delete(target.clone()).expect("begin");
        flow.cancel();
        assert_eq!(flow.status(), FlowStatus::Idle);

        flow.begin_delete(target.clone()).expect("begin");
        flow.accept_review().expect("accept");
        flow.set_typed_word("DELETE").expect("typing");
        flow.cancel();
        assert_eq!(flow.status(), FlowStatus::Idle);

        flow.begin_delete(target.clone()).expect("begin");
        flow.accept_review().expect("accept");
        flow.set_typed_word("DELETE").expect("typing");
        flow.confirm_word().expect("confirm");
        flow.cancel();
        assert_eq!(flow.take_delete().expect_err("cancelled"), FlowError::NothingConfirmed);

        flow.begin_recycle(target).expect("begin");
        flow.accept_review().expect("accept");
        flow.cancel();
        assert_eq!(flow.take_recycle().expect_err("cancelled"), FlowError::NothingConfirmed);
    }

    #[test]
    fn binding_detects_every_changed_field() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(target.clone()).expect("begin");
        flow.accept_review().expect("accept");
        let capability = flow.take_recycle().expect("capability");
        let binding = capability.binding();

        let mut other_generation = target.clone();
        other_generation.generation = generation(2);
        assert_eq!(binding.verify_target(&other_generation), Err(BindingMismatch::Generation));

        let mut other_path = target.clone();
        other_path.path = PathBuf::from(r"C:\zqx\directory\other.bin");
        assert_eq!(binding.verify_target(&other_path), Err(BindingMismatch::Path));

        let mut other_kind = target.clone();
        other_kind.kind = TargetKind::Directory;
        assert_eq!(binding.verify_target(&other_kind), Err(BindingMismatch::TargetKind));

        let mut other_identity = target.clone();
        other_identity.identity = Some(identity(99));
        assert_eq!(binding.verify_target(&other_identity), Err(BindingMismatch::Identity));

        let mut missing_identity = target.clone();
        missing_identity.identity = None;
        assert_eq!(binding.verify_target(&missing_identity), Err(BindingMismatch::Identity));

        assert_eq!(
            binding.verify_scope(&RecycleBinScope::AllDrives, generation(1)),
            Err(BindingMismatch::Kind)
        );

        let rebuilt = self::fixture(1);
        let replaced = destructive_target(&rebuilt, fixture.leaf);
        assert_eq!(binding.verify_target(&replaced), Ok(()));
    }

    #[test]
    fn a_new_snapshot_with_a_renamed_target_no_longer_matches() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(target).expect("begin");
        flow.accept_review().expect("accept");
        let capability = flow.take_recycle().expect("capability");

        let mut builder = TreeBuilder::new(generation(1));
        let root = builder.add_root(NodeSpec::root(r"C:\zqx")).expect("root");
        let directory = builder.add_child(root, NodeSpec::directory("directory")).expect("dir");
        let renamed =
            builder.add_child(directory, file("renamed.bin", Some(identity(12)))).expect("leaf");
        let snapshot = builder.freeze().expect("snapshot");
        let executable = executable();
        let current = TargetValidator::new(&snapshot, Some(&executable))
            .validate(renamed, generation(1), false, ActionPurpose::Destructive)
            .expect("valid");
        assert_eq!(capability.binding().verify_target(&current), Err(BindingMismatch::Path));
    }

    #[test]
    fn capabilities_cannot_be_forged_cloned_or_reused() {
        // The request types have private fields and no public constructor,
        // and capabilities are neither `Clone` nor `Copy`, so the only source
        // is the flow and a consumed capability is gone. Those are compile-time
        // facts; this test exercises the runtime half of the contract.
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(target).expect("begin");
        flow.accept_review().expect("accept");
        let capability = flow.take_recycle().expect("capability");
        let (request, pending) = capability.consume();
        // `capability` has been moved: reusing it is a compile error.
        drop(request);
        let report = pending.settle(
            DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete { items: Vec::new() }
                .into(),
        );
        assert!(report.requires_halt());
    }

    #[test]
    fn reparse_target_presentation_carries_the_link_note() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.link);
        assert_eq!(target.kind(), TargetKind::ReparsePoint(ReparseKind::Directory));
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(target).expect("begin");
        let Some(ConfirmationPresentation::Filesystem { target: shown, .. }) = flow.presentation()
        else {
            panic!("filesystem presentation expected");
        };
        assert!(shown.reparse_note);
    }

    #[test]
    fn drive_root_scope_is_exact_and_never_empty() {
        assert_eq!(DriveRoot::parse(""), Err(DriveRootError::Empty));
        assert_eq!(DriveRoot::parse("C:"), Err(DriveRootError::NotADriveRoot));
        assert_eq!(DriveRoot::parse(r"C:\Users"), Err(DriveRootError::NotADriveRoot));
        assert_eq!(DriveRoot::parse(r"\\?\C:\"), Err(DriveRootError::NotADriveRoot));
        assert_eq!(DriveRoot::parse(r"1:\"), Err(DriveRootError::NotADriveRoot));
        assert_eq!(DriveRoot::from_letter('9'), Err(DriveRootError::NotADriveRoot));
        let root = DriveRoot::parse(r"e:\").expect("root");
        assert_eq!(root.letter(), 'E');
        assert_eq!(root.as_path(), Path::new(r"E:\"));
        assert!(!root.as_path().as_os_str().is_empty());
        assert_eq!(DriveRoot::from_letter('e').expect("root"), root);
    }

    #[test]
    fn utf16_escape_round_trips_unpaired_surrogates_and_literal_escapes() {
        let cases: [Vec<u16>; 6] = [
            "C:\\plain\\name.txt".encode_utf16().collect(),
            vec![b'C'.into(), b':'.into(), b'\\'.into(), 0xD800, b'x'.into()],
            vec![0xDC00, 0xD83D, 0xDE00, 0xD83D],
            "C:\\\\u{D800}literal".encode_utf16().collect(),
            "emoji \u{1F600} and \\u{".encode_utf16().collect(),
            Vec::new(),
        ];
        for units in cases {
            let escaped = escape_utf16(&units);
            let restored = unescape_utf16(&escaped).expect("round trip");
            assert_eq!(restored, units, "{escaped}");
        }
        assert_eq!(
            escape_utf16(&[b'a'.into(), 0xD800, b'\\'.into(), b'u'.into(), b'{'.into()]),
            "a\\u{D800}\\u{005C}u{"
        );
        assert_eq!(escape_utf16(&"C:\\dir".encode_utf16().collect::<Vec<_>>()), "C:\\dir");
        assert_eq!(unescape_utf16("\\u{"), Err(UnescapeError::MalformedEscape { at: 0 }));
        assert_eq!(unescape_utf16("\\u{12345}"), Err(UnescapeError::MalformedEscape { at: 0 }));
        assert_eq!(unescape_utf16("\\u{zz}"), Err(UnescapeError::MalformedEscape { at: 0 }));
    }

    #[cfg(windows)]
    #[test]
    fn lossy_native_path_gets_a_lossless_escaped_presentation() {
        use std::os::windows::ffi::OsStringExt;

        let units: Vec<u16> = "C:\\zqx\\".encode_utf16().chain([0xD800_u16, b'x'.into()]).collect();
        let path = PathBuf::from(OsString::from_wide(&units));
        assert_eq!(escaped_native_path(Path::new(r"C:\plain")), None);
        let escaped = escaped_native_path(&path).expect("lossy path is escaped");
        assert_eq!(escaped, "C:\\zqx\\\\u{D800}x");
        assert_eq!(unescape_utf16(&escaped).expect("round trip"), units);

        let mut builder = TreeBuilder::new(generation(1));
        let root = builder.add_root(NodeSpec::root(r"C:\zqx")).expect("root");
        let name = OsString::from_wide(&[0xD800_u16, b'x'.into()]);
        let leaf = builder.add_child(root, file(name, Some(identity(1)))).expect("leaf");
        let snapshot = builder.freeze().expect("snapshot");
        let executable = executable();
        let target = TargetValidator::new(&snapshot, Some(&executable))
            .validate(leaf, generation(1), false, ActionPurpose::Destructive)
            .expect("valid");
        let mut flow = ConfirmationFlow::new();
        flow.begin_delete(target.clone()).expect("begin");
        let Some(ConfirmationPresentation::Filesystem { target: shown, .. }) = flow.presentation()
        else {
            panic!("filesystem presentation expected");
        };
        assert_eq!(shown.exact_path, target.path());
        assert_eq!(shown.escaped_utf16.as_deref(), Some("C:\\zqx\\\\u{D800}x"));
        assert!(shown.display.contains('\u{FFFD}'));
    }

    #[test]
    fn debug_output_never_contains_paths() {
        let fixture = fixture(1);
        let target = destructive_target(&fixture, fixture.leaf);
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(target.clone()).expect("begin");
        let rendered = format!("{target:?} {flow:?} {:?}", flow.presentation());
        flow.accept_review().expect("accept");
        let capability = flow.take_recycle().expect("capability");
        let (_, pending) = capability.consume();
        let rendered = format!("{rendered} {pending:?} {:?}", pending.obligation());
        assert!(!rendered.contains("zqx"), "{rendered}");
        assert!(!rendered.contains("leaf"), "{rendered}");
        assert!(rendered.contains("path_present: true"));
    }

    #[test]
    fn open_reveal_and_installed_apps_bind_without_confirmation() {
        let fixture = fixture(1);
        let validator = validator(&fixture, None);
        let target = validator
            .validate(fixture.exe, generation(1), false, ActionPurpose::Open)
            .expect("open target");
        let open = OpenItem::new(target.clone());
        assert_eq!(open.target().generation(), generation(1));
        let reveal = RevealItem::new(target);
        assert_eq!(reveal.target().node(), fixture.exe);
        let apps = OpenInstalledApps::new(Some(generation(1)));
        assert_eq!(apps.generation(), Some(generation(1)));
        assert!(!ActionKind::Open.is_destructive());
        assert_eq!(ActionKind::Open.required_word(), None);
        assert_eq!(ActionKind::DeletePermanently.required_word(), Some("DELETE"));
        assert_eq!(ActionKind::EmptyRecycleBin.required_word(), Some("EMPTY"));
    }

    #[test]
    fn classification_completed_requires_recycle_bin_evidence() {
        let recycled = evidence(DeleteMode::Recycle);
        assert_eq!(
            classify_delete_evidence(&recycled),
            DestructiveOutcome::Completed {
                items: vec![ItemResult {
                    code: 0,
                    effect: ItemEffect::Recycled {
                        receipt: RecycleReceipt { display_name: Some("leaf.bin".into()) }
                    }
                }],
                assurance: IdentityAssurance::Exact,
            }
        );

        let mut permanent = evidence(DeleteMode::Permanent);
        permanent.post_delete[0].newly_created = None;
        permanent.assurance = IdentityAssurance::KindOnly;
        assert_eq!(
            classify_delete_evidence(&permanent),
            DestructiveOutcome::Completed {
                items: vec![ItemResult { code: 0, effect: ItemEffect::PermanentlyDeleted }],
                assurance: IdentityAssurance::KindOnly,
            }
        );
    }

    #[test]
    fn classification_recycle_without_bin_item_is_a_safety_violation() {
        let mut silent = evidence(DeleteMode::Recycle);
        silent.post_delete[0].newly_created = None;
        assert_eq!(
            classify_delete_evidence(&silent),
            DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete {
                items: vec![ItemResult { code: 0, effect: ItemEffect::PermanentlyDeleted }],
            }
        );
        // Even a later failure or abort does not hide the violation.
        silent.aborted = Ok(true);
        silent.perform = Err(-1);
        assert!(matches!(
            classify_delete_evidence(&silent),
            DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete { .. }
        ));
    }

    #[test]
    fn classification_success_hresult_with_abort_is_partial_not_completed() {
        let mut aborted = evidence(DeleteMode::Recycle);
        aborted.aborted = Ok(true);
        assert_eq!(
            classify_delete_evidence(&aborted),
            DestructiveOutcome::Partial {
                committed: 1,
                failed: 0,
                aborted: true,
                items: aborted
                    .post_delete
                    .iter()
                    .map(|_| ItemResult {
                        code: 0,
                        effect: ItemEffect::Recycled {
                            receipt: RecycleReceipt { display_name: Some("leaf.bin".into()) }
                        }
                    })
                    .collect(),
            }
        );
    }

    #[test]
    fn classification_missing_callbacks_are_unknown_not_success() {
        let mut none = evidence(DeleteMode::Permanent);
        none.post_delete.clear();
        assert_eq!(
            classify_delete_evidence(&none),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::MissingCallbacks,
                code: None
            }
        );

        let mut no_finish = evidence(DeleteMode::Permanent);
        no_finish.post_delete[0].newly_created = None;
        no_finish.finish = None;
        assert_eq!(
            classify_delete_evidence(&no_finish),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::MissingCallbacks,
                code: None
            }
        );

        let mut not_started = evidence(DeleteMode::Recycle);
        not_started.started = false;
        assert_eq!(
            classify_delete_evidence(&not_started),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::MissingCallbacks,
                code: None
            }
        );
    }

    #[test]
    fn classification_failed_item_depends_on_target_kind() {
        let mut file_failed = evidence(DeleteMode::Recycle);
        file_failed.post_delete[0] =
            PostDeleteEvidence { hresult: -2_147_024_891, newly_created: None };
        assert_eq!(
            classify_delete_evidence(&file_failed),
            DestructiveOutcome::Failed {
                stage: FailureStage::PostDeleteItem,
                code: Some(-2_147_024_891)
            }
        );

        let mut directory_failed = file_failed.clone();
        directory_failed.target_kind = TargetKind::Directory;
        assert_eq!(
            classify_delete_evidence(&directory_failed),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::PostDeleteItem,
                code: Some(-2_147_024_891)
            }
        );

        let mut mixed = evidence(DeleteMode::Permanent);
        mixed.post_delete = vec![
            PostDeleteEvidence { hresult: 0, newly_created: None },
            PostDeleteEvidence { hresult: -5, newly_created: None },
        ];
        assert_eq!(
            classify_delete_evidence(&mixed),
            DestructiveOutcome::Partial {
                committed: 1,
                failed: 1,
                aborted: false,
                items: vec![
                    ItemResult { code: 0, effect: ItemEffect::PermanentlyDeleted },
                    ItemResult { code: -5, effect: ItemEffect::Unchanged },
                ],
            }
        );
    }

    #[test]
    fn classification_cancellation_before_and_after_mutation() {
        let mut before = evidence(DeleteMode::Recycle);
        before.cancel_requested = true;
        before.pre_delete = vec![PreDeleteEvidence { cancelled: true }];
        before.post_delete.clear();
        before.perform = Err(-2_144_927_744);
        before.aborted = Ok(true);
        assert_eq!(classify_delete_evidence(&before), DestructiveOutcome::CancelledBeforeMutation);

        let mut never_started = evidence(DeleteMode::Permanent);
        never_started.cancel_requested = true;
        never_started.pre_delete.clear();
        never_started.post_delete.clear();
        never_started.started = false;
        assert_eq!(
            classify_delete_evidence(&never_started),
            DestructiveOutcome::CancelledBeforeMutation
        );

        let mut after = evidence(DeleteMode::Permanent);
        after.post_delete[0].newly_created = None;
        after.cancel_requested = true;
        after.pre_delete.push(PreDeleteEvidence { cancelled: true });
        after.aborted = Ok(true);
        assert_eq!(
            classify_delete_evidence(&after),
            DestructiveOutcome::Partial {
                committed: 1,
                failed: 0,
                aborted: true,
                items: vec![ItemResult { code: 0, effect: ItemEffect::PermanentlyDeleted }],
            }
        );
    }

    #[test]
    fn classification_perform_and_finish_failures() {
        let mut failed_early = evidence(DeleteMode::Permanent);
        failed_early.pre_delete.clear();
        failed_early.post_delete.clear();
        failed_early.perform = Err(-7);
        assert_eq!(
            classify_delete_evidence(&failed_early),
            DestructiveOutcome::Failed { stage: FailureStage::PerformOperations, code: Some(-7) }
        );

        let mut failed_late = evidence(DeleteMode::Permanent);
        failed_late.post_delete.clear();
        failed_late.perform = Err(-7);
        assert_eq!(
            classify_delete_evidence(&failed_late),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::PerformOperations,
                code: Some(-7)
            }
        );

        let mut perform_after_items = evidence(DeleteMode::Recycle);
        perform_after_items.perform = Err(-9);
        assert_eq!(
            classify_delete_evidence(&perform_after_items),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::PerformOperations,
                code: Some(-9)
            }
        );

        let mut finish_failed = evidence(DeleteMode::Recycle);
        finish_failed.finish = Some(-11);
        assert_eq!(
            classify_delete_evidence(&finish_failed),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::FinishOperations,
                code: Some(-11)
            }
        );

        let mut aborted_query_failed = evidence(DeleteMode::Recycle);
        aborted_query_failed.aborted = Err(-13);
        assert_eq!(
            classify_delete_evidence(&aborted_query_failed),
            DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::QueryAborted,
                code: Some(-13)
            }
        );

        let mut aborted_query_failed_early = evidence(DeleteMode::Recycle);
        aborted_query_failed_early.pre_delete.clear();
        aborted_query_failed_early.post_delete.clear();
        aborted_query_failed_early.aborted = Err(-13);
        assert_eq!(
            classify_delete_evidence(&aborted_query_failed_early),
            DestructiveOutcome::Failed { stage: FailureStage::QueryAborted, code: Some(-13) }
        );
    }

    #[test]
    fn identity_from_raw_widens_the_volume_serial() {
        let identity = identity_from_raw(0x1234, 0x99);
        assert_eq!(identity.volume(), VolumeKey::new(0x1234));
        assert_eq!(identity.file_id(), 0x99);
    }
}
