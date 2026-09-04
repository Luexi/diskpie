//! Portable policy for the reversible per-user Explorer verb selected by
//! ADR 0009.
//!
//! This module owns the exact key layout, the `REG_SZ` value set, the direct
//! command template, ownership classification, the install/repair/remove
//! decision table, and the staged commit with rollback. It never touches a
//! registry itself: every mutation flows through the [`IntegrationRegistry`]
//! port, whose Windows adapter lives in `diskpie-platform`, and every key path
//! is relative to a root the adapter chooses (`HKCU\Software\Classes` in
//! production, a disposable test root in tests).
//!
//! Paths are never rendered into `Debug`, `Display`, or error messages. The
//! only paths that leave this module are the typed executable paths that ADR
//! 0009 item 5 requires DiskPie to show for an owned-but-stale integration.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Vendor-qualified verb key name registered under `Directory\shell` and
/// `Drive\shell`.
pub const VERB_KEY_NAME: &str = "DiskPie.Scan";
/// Child key holding the verb command line in its default value.
pub const COMMAND_KEY_NAME: &str = "command";
/// Static verb container below each shell class.
pub const SHELL_KEY_NAME: &str = "shell";
/// Stable ownership marker stored in every DiskPie verb key.
pub const OWNER_MARKER: &str = "{3A5DC901-CCA6-4FD4-872B-3EC15508883A}";
/// Schema version of the owned value set.
pub const SCHEMA_VERSION: &str = "1";
/// Context-menu label stored in `MUIVerb`.
pub const VERB_LABEL: &str = "Scan with DiskPie";
/// Static verbs activate exactly one selected item.
pub const MULTI_SELECT_MODEL: &str = "Single";
/// Startup option that carries the selected folder or drive (ADR 0014).
pub const SCAN_PATH_OPTION: &str = "--scan-path";

/// Value names stored on the verb key.
pub const MUI_VERB_VALUE: &str = "MUIVerb";
pub const ICON_VALUE: &str = "Icon";
pub const MULTI_SELECT_MODEL_VALUE: &str = "MultiSelectModel";
pub const OWNER_VALUE: &str = "DiskPieOwner";
pub const SCHEMA_VALUE: &str = "DiskPieSchema";
pub const EXECUTABLE_VALUE: &str = "DiskPieExecutable";
/// The unnamed default value of the `command` key.
pub const DEFAULT_VALUE_NAME: &str = "";

const STAGING_INFIX: &str = "staging";
const PREVIOUS_INFIX: &str = "previous";
const KEY_SEPARATOR: char = '\\';

/// Shell class receiving the verb.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum VerbTarget {
    Directory,
    Drive,
}

impl VerbTarget {
    /// Every class DiskPie registers, in commit order.
    pub const ALL: [Self; 2] = [Self::Directory, Self::Drive];

    /// Registry class key below the classes root.
    #[must_use]
    pub const fn class_key_name(self) -> &'static str {
        match self {
            Self::Directory => "Directory",
            Self::Drive => "Drive",
        }
    }

    /// Quoted selection placeholder for this class.
    ///
    /// Explorer substitutes `%1` with the selected folder (no trailing
    /// separator) or the drive root (`C:\`, with a trailing separator). The
    /// Windows argv parser treats a backslash before a closing quote as an
    /// escaped quote, so `"C:\"` would arrive as `C:"`. The drive template
    /// therefore adds one literal backslash: `"C:\\"` parses back to `C:\`.
    #[must_use]
    pub const fn selection_argument(self) -> &'static str {
        match self {
            Self::Directory => "\"%1\"",
            Self::Drive => "\"%1\\\"",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Directory => 0,
            Self::Drive => 1,
        }
    }
}

/// Registry key path relative to the integration root.
///
/// Components are ASCII protocol names authored by this module; they are
/// never derived from user data.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyPath(Vec<String>);

impl KeyPath {
    /// Builds a path from non-empty components that contain no separator.
    #[must_use]
    pub fn try_new<I, S>(components: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let components: Vec<String> = components.into_iter().map(Into::into).collect();
        let valid = !components.is_empty()
            && components
                .iter()
                .all(|component| !component.is_empty() && !component.contains(KEY_SEPARATOR));
        valid.then_some(Self(components))
    }

    fn from_static(components: &[&str]) -> Self {
        Self(components.iter().map(|component| (*component).to_owned()).collect())
    }

    /// Relative components from the root downward.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.0
    }

    /// Final component.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.last().map_or("", String::as_str)
    }

    /// Path without the final component, if any remains.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        (self.0.len() > 1).then(|| Self(self.0[..self.0.len() - 1].to_vec()))
    }

    /// Appends one component.
    #[must_use]
    pub fn child(&self, name: &str) -> Self {
        let mut components = self.0.clone();
        components.push(name.to_owned());
        Self(components)
    }

    /// Replaces the final component.
    #[must_use]
    pub fn with_name(&self, name: &str) -> Self {
        let mut components = self.0.clone();
        if let Some(last) = components.last_mut() {
            *last = name.to_owned();
        }
        Self(components)
    }

    /// Backslash-joined form accepted by registry APIs.
    #[must_use]
    pub fn to_native(&self) -> String {
        self.0.join("\\")
    }
}

impl fmt::Debug for KeyPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "KeyPath({})", self.to_native())
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_native())
    }
}

/// `<Class>\shell` container for one target.
#[must_use]
pub fn shell_key(target: VerbTarget) -> KeyPath {
    KeyPath::from_static(&[target.class_key_name(), SHELL_KEY_NAME])
}

/// Final verb key for one target.
#[must_use]
pub fn verb_key(target: VerbTarget) -> KeyPath {
    shell_key(target).child(VERB_KEY_NAME)
}

/// `command` child of the final verb key.
#[must_use]
pub fn command_key(target: VerbTarget) -> KeyPath {
    verb_key(target).child(COMMAND_KEY_NAME)
}

/// Uniquely named staging sibling written before the final key exists.
#[must_use]
pub fn staging_verb_key(target: VerbTarget, token: &StagingToken) -> KeyPath {
    shell_key(target).child(&staging_name(token))
}

/// Sibling that temporarily holds the previous owned key during a repair.
#[must_use]
pub fn previous_verb_key(target: VerbTarget, token: &StagingToken) -> KeyPath {
    shell_key(target).child(&previous_name(token))
}

fn staging_name(token: &StagingToken) -> String {
    format!("{VERB_KEY_NAME}.{STAGING_INFIX}.{}", token.as_str())
}

fn previous_name(token: &StagingToken) -> String {
    format!("{VERB_KEY_NAME}.{PREVIOUS_INFIX}.{}", token.as_str())
}

/// Request-unique suffix for staging sibling names.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StagingToken(String);

static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl StagingToken {
    /// Builds a deterministic token from a process identity and a sequence.
    #[must_use]
    pub fn new(process_id: u32, sequence: u64) -> Self {
        Self(format!("{process_id:x}-{sequence:x}"))
    }

    /// Builds a token unique within this process and unlikely to collide
    /// across processes.
    #[must_use]
    pub fn generate() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
        let sequence = TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self::new(std::process::id(), nanos ^ sequence.rotate_left(32))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One registry value as observed through the port.
#[derive(Clone, Eq, PartialEq)]
pub enum RegistryValue {
    /// `REG_SZ` data without its terminator.
    String(OsString),
    /// Any other registry type, identified by its native type code.
    Other { type_id: u32 },
}

impl fmt::Debug for RegistryValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => write!(formatter, "String(len={})", value.len()),
            Self::Other { type_id } => write!(formatter, "Other(type={type_id})"),
        }
    }
}

/// Values and direct subkeys of one key.
///
/// Registry names are case-insensitive, so lookups ignore ASCII case while
/// the stored spelling is preserved for exact snapshots.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct KeySnapshot {
    values: BTreeMap<String, RegistryValue>,
    subkeys: Vec<String>,
}

impl KeySnapshot {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a value, keeping one entry per case-insensitive name.
    pub fn insert_value(&mut self, name: &str, value: RegistryValue) {
        self.values.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        self.values.insert(name.to_owned(), value);
    }

    /// Builder form of [`Self::insert_value`] for `REG_SZ` data.
    #[must_use]
    pub fn with_string(mut self, name: &str, value: impl Into<OsString>) -> Self {
        self.insert_value(name, RegistryValue::String(value.into()));
        self
    }

    /// Adds a direct subkey name once, ignoring ASCII case.
    pub fn add_subkey(&mut self, name: &str) {
        if !self.subkeys.iter().any(|existing| existing.eq_ignore_ascii_case(name)) {
            self.subkeys.push(name.to_owned());
            self.subkeys.sort_unstable();
        }
    }

    #[must_use]
    pub fn with_subkey(mut self, name: &str) -> Self {
        self.add_subkey(name);
        self
    }

    /// All values with their stored names.
    pub fn values(&self) -> impl Iterator<Item = (&str, &RegistryValue)> {
        self.values.iter().map(|(name, value)| (name.as_str(), value))
    }

    /// Sorted direct subkey names.
    #[must_use]
    pub fn subkeys(&self) -> &[String] {
        &self.subkeys
    }

    /// Case-insensitive value lookup.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&RegistryValue> {
        self.values
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// Case-insensitive lookup of `REG_SZ` data.
    #[must_use]
    pub fn string_value(&self, name: &str) -> Option<&OsStr> {
        match self.value(name)? {
            RegistryValue::String(value) => Some(value.as_os_str()),
            RegistryValue::Other { .. } => None,
        }
    }

    /// Whether this snapshot carries exactly the expected values and subkeys,
    /// comparing names without ASCII case and data exactly.
    #[must_use]
    pub fn matches(&self, expected: &Self) -> bool {
        self.values.len() == expected.values.len()
            && expected.values.iter().all(|(name, value)| self.value(name) == Some(value))
            && self.subkeys.len() == expected.subkeys.len()
            && expected
                .subkeys
                .iter()
                .all(|name| self.subkeys.iter().any(|existing| existing.eq_ignore_ascii_case(name)))
    }
}

impl fmt::Debug for KeySnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeySnapshot")
            .field("values", &self.values)
            .field("subkeys", &self.subkeys)
            .finish()
    }
}

/// Port operation that produced a registry failure.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RegistryOperation {
    Exists,
    Read,
    Create,
    Write,
    Rename,
    Delete,
}

/// Stable, path-free failure category reported by the port.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RegistryErrorKind {
    AccessDenied,
    NotFound,
    AlreadyExists,
    Unsupported,
    Other,
}

/// Sanitized registry failure. Key paths and values are never retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryError {
    pub operation: RegistryOperation,
    pub kind: RegistryErrorKind,
    pub code: Option<i32>,
}

impl RegistryError {
    #[must_use]
    pub const fn new(operation: RegistryOperation, kind: RegistryErrorKind) -> Self {
        Self { operation, kind, code: None }
    }

    #[must_use]
    pub const fn with_code(mut self, code: i32) -> Self {
        self.code = Some(code);
        self
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "registry {:?} failed: {:?}", self.operation, self.kind)?;
        if let Some(code) = self.code {
            write!(formatter, " (code 0x{:08X})", code as u32)?;
        }
        Ok(())
    }
}

impl Error for RegistryError {}

/// Narrow registry port used by the integration policy.
///
/// Every key is relative to the adapter's root. Implementations must never
/// widen a request: `delete_tree` removes exactly the named subtree,
/// `rename_key` renames in place under the same parent, and `create_key`
/// creates only the named key and its missing ancestors.
pub trait IntegrationRegistry {
    fn key_exists(&self, key: &KeyPath) -> Result<bool, RegistryError>;

    /// Returns `None` when the key does not exist.
    fn read_key(&self, key: &KeyPath) -> Result<Option<KeySnapshot>, RegistryError>;

    fn create_key(&mut self, key: &KeyPath) -> Result<(), RegistryError>;

    /// Writes one `REG_SZ` value on an existing key.
    fn write_string(
        &mut self,
        key: &KeyPath,
        name: &str,
        value: &OsStr,
    ) -> Result<(), RegistryError>;

    /// Renames the key to a sibling name under the same parent.
    fn rename_key(&mut self, key: &KeyPath, new_name: &str) -> Result<(), RegistryError>;

    /// Deletes the exact subtree. A missing key is not an error.
    fn delete_tree(&mut self, key: &KeyPath) -> Result<(), RegistryError>;

    /// Tells the shell that file associations changed.
    fn notify_association_changed(&mut self);
}

/// Reason an executable path cannot be stored in a verb command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutablePathError {
    Empty,
    ContainsQuote,
    ContainsNul,
    TrailingSeparator,
    /// A `%` followed by an alphanumeric, `*`, or `~` would be substituted by
    /// Explorer inside the command string, changing the executable path.
    ContainsPlaceholder,
}

impl fmt::Display for ExecutablePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "the executable path is empty",
            Self::ContainsQuote => "the executable path contains a double quote",
            Self::ContainsNul => "the executable path contains a NUL unit",
            Self::TrailingSeparator => "the executable path ends with a separator",
            Self::ContainsPlaceholder => {
                "the executable path contains a shell placeholder sequence"
            }
        })
    }
}

impl Error for ExecutablePathError {}

/// Exact value set DiskPie stores for one executable path.
#[derive(Clone, Eq, PartialEq)]
pub struct VerbRecord {
    executable: PathBuf,
}

impl VerbRecord {
    /// Validates that the path can be quoted verbatim in a command line.
    pub fn new(executable: &Path) -> Result<Self, ExecutablePathError> {
        let bytes = executable.as_os_str().as_encoded_bytes();
        if bytes.is_empty() {
            return Err(ExecutablePathError::Empty);
        }
        if bytes.contains(&b'"') {
            return Err(ExecutablePathError::ContainsQuote);
        }
        if bytes.contains(&0) {
            return Err(ExecutablePathError::ContainsNul);
        }
        if matches!(bytes.last(), Some(b'\\' | b'/')) {
            return Err(ExecutablePathError::TrailingSeparator);
        }
        // Explorer substitutes %0-%9, %L, %V, %D, %I, %S, %W, %* and %~
        // (in either case) anywhere in the command string, even inside the
        // quoted executable, so such a path cannot be templated verbatim.
        if bytes.windows(2).any(|pair| {
            pair[0] == b'%' && (pair[1].is_ascii_alphanumeric() || matches!(pair[1], b'*' | b'~'))
        }) {
            return Err(ExecutablePathError::ContainsPlaceholder);
        }
        Ok(Self { executable: executable.to_path_buf() })
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// `"<exe>" --scan-path "%1"` (or the drive-root form) without expansion.
    #[must_use]
    pub fn command_line(&self, target: VerbTarget) -> OsString {
        let mut line = OsString::from("\"");
        line.push(self.executable.as_os_str());
        line.push("\" ");
        line.push(SCAN_PATH_OPTION);
        line.push(" ");
        line.push(target.selection_argument());
        line
    }

    /// `"<exe>",0`: Explorer parses `Icon` as `path,index`, so the path is
    /// quoted to survive a comma inside it and the first icon group is named
    /// explicitly.
    #[must_use]
    pub fn icon_value(&self) -> OsString {
        let mut icon = OsString::from("\"");
        icon.push(self.executable.as_os_str());
        icon.push("\",0");
        icon
    }

    /// Expected content of the verb key.
    #[must_use]
    pub fn verb_snapshot(&self) -> KeySnapshot {
        KeySnapshot::new()
            .with_string(MUI_VERB_VALUE, VERB_LABEL)
            .with_string(ICON_VALUE, self.icon_value())
            .with_string(MULTI_SELECT_MODEL_VALUE, MULTI_SELECT_MODEL)
            .with_string(OWNER_VALUE, OWNER_MARKER)
            .with_string(SCHEMA_VALUE, SCHEMA_VERSION)
            .with_string(EXECUTABLE_VALUE, self.executable.as_os_str())
            .with_subkey(COMMAND_KEY_NAME)
    }

    /// Expected content of the `command` key.
    #[must_use]
    pub fn command_snapshot(&self, target: VerbTarget) -> KeySnapshot {
        KeySnapshot::new().with_string(DEFAULT_VALUE_NAME, self.command_line(target))
    }
}

impl fmt::Debug for VerbRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerbRecord")
            .field("executable_present", &!self.executable.as_os_str().is_empty())
            .finish()
    }
}

/// Why an existing key cannot be classified as owned.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AmbiguityReason {
    UnsupportedSchema,
    MissingValues,
    UnexpectedValues,
    InconsistentValues,
    InconsistentCommand,
    UnexpectedSubkeys,
    PartialInstallation,
    DivergentExecutables,
}

/// Ownership classification of one verb key.
#[derive(Clone, Eq, PartialEq)]
pub enum KeyState {
    NotInstalled,
    OwnedCurrent,
    OwnedStale { stored_executable: PathBuf },
    Foreign,
    Ambiguous(AmbiguityReason),
}

impl KeyState {
    #[must_use]
    pub const fn is_owned(&self) -> bool {
        matches!(self, Self::OwnedCurrent | Self::OwnedStale { .. })
    }

    #[must_use]
    pub const fn blocks_mutation(&self) -> bool {
        matches!(self, Self::Foreign | Self::Ambiguous(_))
    }
}

impl fmt::Debug for KeyState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("NotInstalled"),
            Self::OwnedCurrent => formatter.write_str("OwnedCurrent"),
            Self::OwnedStale { .. } => formatter.write_str("OwnedStale"),
            Self::Foreign => formatter.write_str("Foreign"),
            Self::Ambiguous(reason) => write!(formatter, "Ambiguous({reason:?})"),
        }
    }
}

/// Classifies one target from its verb and command snapshots.
#[must_use]
pub fn classify_key(
    target: VerbTarget,
    verb: Option<&KeySnapshot>,
    command: Option<&KeySnapshot>,
    current: &VerbRecord,
) -> KeyState {
    let Some(verb) = verb else {
        return KeyState::NotInstalled;
    };
    if verb.string_value(OWNER_VALUE) != Some(OsStr::new(OWNER_MARKER)) {
        return KeyState::Foreign;
    }
    if verb.string_value(SCHEMA_VALUE) != Some(OsStr::new(SCHEMA_VERSION)) {
        return KeyState::Ambiguous(AmbiguityReason::UnsupportedSchema);
    }
    let Some(stored) = verb.string_value(EXECUTABLE_VALUE) else {
        return KeyState::Ambiguous(AmbiguityReason::MissingValues);
    };
    let Ok(stored_record) = VerbRecord::new(Path::new(stored)) else {
        return KeyState::Ambiguous(AmbiguityReason::InconsistentValues);
    };
    let expected_verb = stored_record.verb_snapshot();
    if verb.values.len() < expected_verb.values.len() {
        return KeyState::Ambiguous(AmbiguityReason::MissingValues);
    }
    if verb.values.len() > expected_verb.values.len() {
        return KeyState::Ambiguous(AmbiguityReason::UnexpectedValues);
    }
    if !verb.subkeys.iter().any(|name| name.eq_ignore_ascii_case(COMMAND_KEY_NAME)) {
        return KeyState::Ambiguous(AmbiguityReason::InconsistentCommand);
    }
    if verb.subkeys.len() != expected_verb.subkeys.len() {
        return KeyState::Ambiguous(AmbiguityReason::UnexpectedSubkeys);
    }
    if !verb.matches(&expected_verb) {
        return KeyState::Ambiguous(AmbiguityReason::InconsistentValues);
    }
    if !command.is_some_and(|command| command.matches(&stored_record.command_snapshot(target))) {
        return KeyState::Ambiguous(AmbiguityReason::InconsistentCommand);
    }
    if stored_record.executable() == current.executable() {
        KeyState::OwnedCurrent
    } else {
        KeyState::OwnedStale { stored_executable: stored_record.executable }
    }
}

/// Combined classification across both targets.
#[derive(Clone, Eq, PartialEq)]
pub enum IntegrationState {
    NotInstalled,
    OwnedCurrent,
    OwnedStale { stored_executable: PathBuf, current_executable: PathBuf },
    Foreign,
    Ambiguous(AmbiguityReason),
}

impl fmt::Debug for IntegrationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("NotInstalled"),
            Self::OwnedCurrent => formatter.write_str("OwnedCurrent"),
            Self::OwnedStale { .. } => formatter.write_str("OwnedStale"),
            Self::Foreign => formatter.write_str("Foreign"),
            Self::Ambiguous(reason) => write!(formatter, "Ambiguous({reason:?})"),
        }
    }
}

/// Renderable status of the Explorer integration for the current executable.
#[derive(Clone, Eq, PartialEq)]
pub struct ExplorerIntegrationStatus {
    state: IntegrationState,
    keys: [KeyState; 2],
    strays: Vec<KeyPath>,
}

impl ExplorerIntegrationStatus {
    /// Folds per-target states into one reportable state.
    #[must_use]
    pub fn from_key_states(current_executable: &Path, keys: [KeyState; 2]) -> Self {
        let state = fold_states(current_executable, &keys);
        Self { state, keys, strays: Vec::new() }
    }

    /// Records DiskPie-owned staging or previous siblings left behind by an
    /// interrupted request.
    #[must_use]
    pub fn with_strays(mut self, strays: Vec<KeyPath>) -> Self {
        self.strays = strays;
        self
    }

    #[must_use]
    pub fn state(&self) -> &IntegrationState {
        &self.state
    }

    /// Owned `DiskPie.Scan.*` siblings that Remove will also delete.
    #[must_use]
    pub fn strays(&self) -> &[KeyPath] {
        &self.strays
    }

    #[must_use]
    pub fn key_state(&self, target: VerbTarget) -> &KeyState {
        &self.keys[target.index()]
    }

    /// Whether both verbs are owned and point at the current executable.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.state == IntegrationState::OwnedCurrent
    }

    /// Whether the user must choose Repair or Remove before Install can run.
    #[must_use]
    pub fn requires_decision(&self) -> bool {
        matches!(self.state, IntegrationState::OwnedStale { .. })
            || self.keys.iter().any(|key| matches!(key, KeyState::OwnedStale { .. }))
    }

    /// Whether a foreign or ambiguous key blocks every mutation.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        self.keys.iter().any(KeyState::blocks_mutation)
    }
}

impl fmt::Debug for ExplorerIntegrationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExplorerIntegrationStatus")
            .field("state", &self.state)
            .field("directory", &self.keys[VerbTarget::Directory.index()])
            .field("drive", &self.keys[VerbTarget::Drive.index()])
            .field("strays", &self.strays.len())
            .finish()
    }
}

fn fold_states(current_executable: &Path, keys: &[KeyState; 2]) -> IntegrationState {
    if keys.contains(&KeyState::Foreign) {
        return IntegrationState::Foreign;
    }
    if let Some(reason) = keys.iter().find_map(|key| match key {
        KeyState::Ambiguous(reason) => Some(*reason),
        _ => None,
    }) {
        return IntegrationState::Ambiguous(reason);
    }
    if keys.iter().all(|key| *key == KeyState::NotInstalled) {
        return IntegrationState::NotInstalled;
    }
    if keys.iter().all(|key| *key == KeyState::OwnedCurrent) {
        return IntegrationState::OwnedCurrent;
    }
    if keys.contains(&KeyState::NotInstalled) {
        return IntegrationState::Ambiguous(AmbiguityReason::PartialInstallation);
    }
    let mut stored: Option<&Path> = None;
    for key in keys {
        if let KeyState::OwnedStale { stored_executable } = key {
            match stored {
                None => stored = Some(stored_executable),
                Some(previous) if previous == stored_executable.as_path() => {}
                Some(_) => {
                    return IntegrationState::Ambiguous(AmbiguityReason::DivergentExecutables);
                }
            }
        }
    }
    stored.map_or(IntegrationState::Ambiguous(AmbiguityReason::InconsistentValues), |stored| {
        IntegrationState::OwnedStale {
            stored_executable: stored.to_path_buf(),
            current_executable: current_executable.to_path_buf(),
        }
    })
}

/// User-initiated integration change.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IntegrationRequest {
    Install,
    Repair,
    Remove,
}

/// Result category of a completed request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IntegrationOutcome {
    Installed,
    Repaired,
    Removed,
    AlreadyInstalled,
    AlreadyRemoved,
}

impl IntegrationOutcome {
    #[must_use]
    pub const fn mutated(self) -> bool {
        matches!(self, Self::Installed | Self::Repaired | Self::Removed)
    }
}

/// Why the decision table refuses a request without touching the registry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Refusal {
    /// A foreign or ambiguous key exists; DiskPie never overwrites it.
    Conflict,
    /// An owned key points at another executable; Repair or Remove is required.
    StaleRequiresDecision,
}

/// Per-target mutation chosen by the decision table.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TargetAction {
    Install,
    Replace,
    Remove,
}

/// Ordered per-target actions and the outcome they produce on success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    actions: Vec<(VerbTarget, TargetAction)>,
    /// Owned staging or previous siblings that Remove deletes after the
    /// final keys; always empty for Install and Repair.
    strays: Vec<KeyPath>,
    outcome: IntegrationOutcome,
}

impl Plan {
    #[must_use]
    pub fn actions(&self) -> &[(VerbTarget, TargetAction)] {
        &self.actions
    }

    #[must_use]
    pub fn strays(&self) -> &[KeyPath] {
        &self.strays
    }

    #[must_use]
    pub const fn outcome(&self) -> IntegrationOutcome {
        self.outcome
    }

    fn is_removal(&self) -> bool {
        self.actions.iter().all(|(_, action)| *action == TargetAction::Remove)
    }
}

/// Outcome of consulting the decision table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    NoChange(IntegrationOutcome),
    Perform(Plan),
    Refuse(Refusal),
}

/// ADR 0009 items 3-5 as a pure function of the inspected status.
#[must_use]
pub fn decide(request: IntegrationRequest, status: &ExplorerIntegrationStatus) -> Decision {
    if status.is_conflict() {
        return Decision::Refuse(Refusal::Conflict);
    }
    let mut actions = Vec::with_capacity(VerbTarget::ALL.len());
    for target in VerbTarget::ALL {
        let key = status.key_state(target);
        let action = match (request, key) {
            (IntegrationRequest::Install, KeyState::OwnedStale { .. }) => {
                return Decision::Refuse(Refusal::StaleRequiresDecision);
            }
            (IntegrationRequest::Install | IntegrationRequest::Repair, KeyState::NotInstalled) => {
                Some(TargetAction::Install)
            }
            (IntegrationRequest::Repair, KeyState::OwnedStale { .. }) => {
                Some(TargetAction::Replace)
            }
            (IntegrationRequest::Remove, KeyState::OwnedCurrent | KeyState::OwnedStale { .. }) => {
                Some(TargetAction::Remove)
            }
            _ => None,
        };
        if let Some(action) = action {
            actions.push((target, action));
        }
    }
    let (no_change, success) = match request {
        IntegrationRequest::Install => {
            (IntegrationOutcome::AlreadyInstalled, IntegrationOutcome::Installed)
        }
        IntegrationRequest::Repair => {
            (IntegrationOutcome::AlreadyInstalled, IntegrationOutcome::Repaired)
        }
        IntegrationRequest::Remove => {
            (IntegrationOutcome::AlreadyRemoved, IntegrationOutcome::Removed)
        }
    };
    let strays = match request {
        IntegrationRequest::Remove => status.strays().to_vec(),
        IntegrationRequest::Install | IntegrationRequest::Repair => Vec::new(),
    };
    if actions.is_empty() && strays.is_empty() {
        Decision::NoChange(no_change)
    } else {
        Decision::Perform(Plan { actions, strays, outcome: success })
    }
}

/// What rollback achieved after a failed staged mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RollbackReport {
    /// Nothing had been written yet.
    NotNeeded,
    /// Every key written by this request was removed or restored.
    Clean,
    /// Rollback itself failed; a staging or previous sibling may remain.
    Failed(RegistryError),
}

/// Failure of an inspection or a request.
#[derive(Clone, Debug, PartialEq)]
pub enum IntegrationError {
    InvalidExecutable(ExecutablePathError),
    /// Refused without mutation because a key is foreign or ambiguous.
    Conflict(Box<ExplorerIntegrationStatus>),
    /// Refused without mutation because an owned key is stale; the status
    /// carries both executable paths for the Repair/Remove prompt.
    StaleRequiresDecision(Box<ExplorerIntegrationStatus>),
    /// A staging sibling for this token already existed; nothing was written.
    StagingCollision {
        target: VerbTarget,
    },
    /// A staged key read back differently from what was written.
    Verification {
        target: VerbTarget,
        rollback: RollbackReport,
    },
    /// The final key changed between inspection and commit.
    ConcurrentModification {
        target: VerbTarget,
        rollback: RollbackReport,
    },
    Registry {
        error: RegistryError,
        rollback: RollbackReport,
    },
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExecutable(error) => write!(formatter, "invalid executable path: {error}"),
            Self::Conflict(status) => {
                write!(formatter, "a foreign or ambiguous verb key exists ({:?})", status.state())
            }
            Self::StaleRequiresDecision(_) => {
                formatter.write_str("the integration points at another executable")
            }
            Self::StagingCollision { target } => {
                write!(formatter, "a staging key already exists for {target:?}")
            }
            Self::Verification { target, rollback } => {
                write!(formatter, "staged {target:?} values did not read back ({rollback:?})")
            }
            Self::ConcurrentModification { target, rollback } => {
                write!(formatter, "the {target:?} key changed during commit ({rollback:?})")
            }
            Self::Registry { error, rollback } => write!(formatter, "{error} ({rollback:?})"),
        }
    }
}

impl Error for IntegrationError {}

impl From<ExecutablePathError> for IntegrationError {
    fn from(error: ExecutablePathError) -> Self {
        Self::InvalidExecutable(error)
    }
}

/// Successful request result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntegrationReport {
    pub outcome: IntegrationOutcome,
    /// The mutation succeeded, but a `previous` sibling from a repair could
    /// not be deleted. Remove and Repair remain available.
    pub incomplete_cleanup: Option<RegistryError>,
}

/// Reads both targets without mutation.
pub fn inspect(
    registry: &dyn IntegrationRegistry,
    current_executable: &Path,
) -> Result<ExplorerIntegrationStatus, IntegrationError> {
    let record = VerbRecord::new(current_executable)?;
    inspect_record(registry, &record)
}

fn inspect_record(
    registry: &dyn IntegrationRegistry,
    record: &VerbRecord,
) -> Result<ExplorerIntegrationStatus, IntegrationError> {
    let mut keys = [KeyState::NotInstalled, KeyState::NotInstalled];
    let mut strays = Vec::new();
    for target in VerbTarget::ALL {
        let inspected = classify_target(registry, target, record)
            .and_then(|state| Ok((state, owned_strays(registry, target)?)));
        let (state, mut target_strays) = inspected.map_err(|error| IntegrationError::Registry {
            error,
            rollback: RollbackReport::NotNeeded,
        })?;
        keys[target.index()] = state;
        strays.append(&mut target_strays);
    }
    Ok(ExplorerIntegrationStatus::from_key_states(record.executable(), keys).with_strays(strays))
}

/// Finds the staging and previous siblings DiskPie itself creates
/// (`DiskPie.Scan.staging.<token>` and `DiskPie.Scan.previous.<token>`) that
/// still carry DiskPie's complete ownership value set.
///
/// Such keys can only remain after an interrupted request whose rollback
/// failed. Any other sibling name, and any key that lacks the marker, schema,
/// or stored executable value, is not DiskPie's and is never touched.
fn owned_strays(
    registry: &dyn IntegrationRegistry,
    target: VerbTarget,
) -> Result<Vec<KeyPath>, RegistryError> {
    let shell = shell_key(target);
    let Some(snapshot) = registry.read_key(&shell)? else {
        return Ok(Vec::new());
    };
    let mut strays = Vec::new();
    for name in snapshot.subkeys() {
        if !is_diskpie_transient_name(name) {
            continue;
        }
        let candidate = shell.child(name);
        if is_marked_owned(registry, &candidate)? {
            strays.push(candidate);
        }
    }
    Ok(strays)
}

/// Whether `name` has the exact shape of a DiskPie staging or previous key
/// with a non-empty token, compared ASCII-case-insensitively like the
/// registry itself.
fn is_diskpie_transient_name(name: &str) -> bool {
    [STAGING_INFIX, PREVIOUS_INFIX].iter().any(|infix| {
        let prefix = format!("{VERB_KEY_NAME}.{infix}.");
        name.len() > prefix.len()
            && name.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
    })
}

/// A key counts as DiskPie's only when the marker, the schema, and the stored
/// executable value are all present: the marker alone is not ownership.
fn is_marked_owned(
    registry: &dyn IntegrationRegistry,
    key: &KeyPath,
) -> Result<bool, RegistryError> {
    Ok(registry.read_key(key)?.is_some_and(|snapshot| {
        snapshot.string_value(OWNER_VALUE) == Some(OsStr::new(OWNER_MARKER))
            && snapshot.string_value(SCHEMA_VALUE) == Some(OsStr::new(SCHEMA_VERSION))
            && snapshot.string_value(EXECUTABLE_VALUE).is_some_and(|value| !value.is_empty())
    }))
}

fn classify_target(
    registry: &dyn IntegrationRegistry,
    target: VerbTarget,
    record: &VerbRecord,
) -> Result<KeyState, RegistryError> {
    let verb = registry.read_key(&verb_key(target))?;
    let command = match verb {
        Some(_) => registry.read_key(&command_key(target))?,
        None => None,
    };
    Ok(classify_key(target, verb.as_ref(), command.as_ref(), record))
}

/// Inspects, decides, and performs one request end to end.
pub fn apply(
    registry: &mut dyn IntegrationRegistry,
    request: IntegrationRequest,
    current_executable: &Path,
    token: &StagingToken,
) -> Result<IntegrationReport, IntegrationError> {
    let record = VerbRecord::new(current_executable)?;
    let status = inspect_record(registry, &record)?;
    match decide(request, &status) {
        Decision::NoChange(outcome) => Ok(IntegrationReport { outcome, incomplete_cleanup: None }),
        Decision::Refuse(Refusal::Conflict) => Err(IntegrationError::Conflict(Box::new(status))),
        Decision::Refuse(Refusal::StaleRequiresDecision) => {
            Err(IntegrationError::StaleRequiresDecision(Box::new(status)))
        }
        Decision::Perform(plan) => {
            let report = perform(registry, &plan, &record, token)?;
            if report.outcome.mutated() {
                registry.notify_association_changed();
            }
            Ok(report)
        }
    }
}

fn perform(
    registry: &mut dyn IntegrationRegistry,
    plan: &Plan,
    record: &VerbRecord,
    token: &StagingToken,
) -> Result<IntegrationReport, IntegrationError> {
    if plan.is_removal() {
        perform_remove(registry, plan, record)
    } else {
        perform_staged(registry, plan, record, token)
    }
}

fn perform_remove(
    registry: &mut dyn IntegrationRegistry,
    plan: &Plan,
    record: &VerbRecord,
) -> Result<IntegrationReport, IntegrationError> {
    for (target, _) in plan.actions() {
        let state = classify_target(registry, *target, record).map_err(|error| {
            IntegrationError::Registry { error, rollback: RollbackReport::NotNeeded }
        })?;
        if !state.is_owned() {
            return Err(IntegrationError::ConcurrentModification {
                target: *target,
                rollback: RollbackReport::NotNeeded,
            });
        }
        registry.delete_tree(&verb_key(*target)).map_err(|error| IntegrationError::Registry {
            error,
            rollback: RollbackReport::NotNeeded,
        })?;
    }
    for stray in plan.strays() {
        // Re-verify the marker immediately before each exact deletion.
        let owned = is_marked_owned(registry, stray).map_err(|error| {
            IntegrationError::Registry { error, rollback: RollbackReport::NotNeeded }
        })?;
        if owned {
            registry.delete_tree(stray).map_err(|error| IntegrationError::Registry {
                error,
                rollback: RollbackReport::NotNeeded,
            })?;
        }
    }
    Ok(IntegrationReport { outcome: plan.outcome(), incomplete_cleanup: None })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Committed {
    /// The staged key became the final key of a previously absent target.
    Installed(VerbTarget),
    /// The owned final key was renamed aside; the staged key is not yet final.
    Aside(VerbTarget),
    /// The staged key became final and the previous key waits for deletion.
    Replaced(VerbTarget),
}

struct StagedCommit<'a> {
    registry: &'a mut dyn IntegrationRegistry,
    record: &'a VerbRecord,
    token: &'a StagingToken,
    staged: Vec<VerbTarget>,
    committed: Vec<Committed>,
}

impl StagedCommit<'_> {
    fn fail<F>(&mut self, build: F) -> IntegrationError
    where
        F: FnOnce(RollbackReport) -> IntegrationError,
    {
        let report = self.rollback();
        build(report)
    }

    fn registry_error(&mut self, error: RegistryError) -> IntegrationError {
        self.fail(|rollback| IntegrationError::Registry { error, rollback })
    }

    fn rollback(&mut self) -> RollbackReport {
        if self.staged.is_empty() && self.committed.is_empty() {
            return RollbackReport::NotNeeded;
        }
        let mut first_error = None;
        let mut note = |result: Result<(), RegistryError>| {
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        };
        while let Some(commit) = self.committed.pop() {
            match commit {
                Committed::Installed(target) => {
                    note(self.delete_fresh_final(target));
                }
                Committed::Aside(target) => {
                    note(
                        self.registry
                            .rename_key(&previous_verb_key(target, self.token), VERB_KEY_NAME),
                    );
                }
                Committed::Replaced(target) => {
                    note(self.delete_fresh_final(target));
                    note(
                        self.registry
                            .rename_key(&previous_verb_key(target, self.token), VERB_KEY_NAME),
                    );
                }
            }
        }
        for target in std::mem::take(&mut self.staged) {
            note(self.registry.delete_tree(&staging_verb_key(target, self.token)));
        }
        first_error.map_or(RollbackReport::Clean, RollbackReport::Failed)
    }

    /// Deletes a final key only while it still holds exactly what this
    /// request wrote.
    fn delete_fresh_final(&mut self, target: VerbTarget) -> Result<(), RegistryError> {
        let state = classify_target(self.registry, target, self.record)?;
        if state == KeyState::OwnedCurrent {
            self.registry.delete_tree(&verb_key(target))
        } else {
            Ok(())
        }
    }

    fn stage(&mut self, target: VerbTarget) -> Result<(), IntegrationError> {
        let staging = staging_verb_key(target, self.token);
        let command = staging.child(COMMAND_KEY_NAME);
        self.staged.push(target);
        let expected_verb = self.record.verb_snapshot();
        let expected_command = self.record.command_snapshot(target);

        let written: Result<(), RegistryError> = (|| {
            self.registry.create_key(&staging)?;
            for (name, value) in expected_verb.values() {
                if let RegistryValue::String(value) = value {
                    self.registry.write_string(&staging, name, value)?;
                }
            }
            self.registry.create_key(&command)?;
            self.registry.write_string(
                &command,
                DEFAULT_VALUE_NAME,
                &self.record.command_line(target),
            )
        })();
        if let Err(error) = written {
            return Err(self.registry_error(error));
        }

        let read_back = (|| {
            Ok::<_, RegistryError>((
                self.registry.read_key(&staging)?,
                self.registry.read_key(&command)?,
            ))
        })();
        let verified = match read_back {
            Ok((Some(verb), Some(command))) => {
                verb.matches(&expected_verb) && command.matches(&expected_command)
            }
            Ok(_) => false,
            Err(error) => return Err(self.registry_error(error)),
        };
        if !verified {
            return Err(self.fail(|rollback| IntegrationError::Verification { target, rollback }));
        }
        Ok(())
    }

    fn commit(&mut self, target: VerbTarget, action: TargetAction) -> Result<(), IntegrationError> {
        let staging = staging_verb_key(target, self.token);
        let final_key = verb_key(target);
        match action {
            TargetAction::Install => {
                match self.registry.key_exists(&final_key) {
                    Ok(false) => {}
                    Ok(true) => {
                        return Err(self.fail(|rollback| {
                            IntegrationError::ConcurrentModification { target, rollback }
                        }));
                    }
                    Err(error) => return Err(self.registry_error(error)),
                }
                if let Err(error) = self.registry.rename_key(&staging, VERB_KEY_NAME) {
                    return Err(self.registry_error(error));
                }
                self.committed.push(Committed::Installed(target));
            }
            TargetAction::Replace => {
                match classify_target(self.registry, target, self.record) {
                    Ok(state) if state.is_owned() => {}
                    Ok(_) => {
                        return Err(self.fail(|rollback| {
                            IntegrationError::ConcurrentModification { target, rollback }
                        }));
                    }
                    Err(error) => return Err(self.registry_error(error)),
                }
                if let Err(error) = self.registry.rename_key(&final_key, &previous_name(self.token))
                {
                    return Err(self.registry_error(error));
                }
                self.committed.push(Committed::Aside(target));
                if let Err(error) = self.registry.rename_key(&staging, VERB_KEY_NAME) {
                    return Err(self.registry_error(error));
                }
                self.committed.pop();
                self.committed.push(Committed::Replaced(target));
            }
            TargetAction::Remove => {}
        }
        self.staged.retain(|staged| *staged != target);
        Ok(())
    }

    fn cleanup(&mut self) -> Option<RegistryError> {
        let mut first_error = None;
        for commit in std::mem::take(&mut self.committed) {
            if let Committed::Replaced(target) = commit
                && let Err(error) =
                    self.registry.delete_tree(&previous_verb_key(target, self.token))
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error
    }
}

fn perform_staged(
    registry: &mut dyn IntegrationRegistry,
    plan: &Plan,
    record: &VerbRecord,
    token: &StagingToken,
) -> Result<IntegrationReport, IntegrationError> {
    for (target, _) in plan.actions() {
        for sibling in [staging_verb_key(*target, token), previous_verb_key(*target, token)] {
            let exists = registry.key_exists(&sibling).map_err(|error| {
                IntegrationError::Registry { error, rollback: RollbackReport::NotNeeded }
            })?;
            if exists {
                return Err(IntegrationError::StagingCollision { target: *target });
            }
        }
    }

    let mut commit =
        StagedCommit { registry, record, token, staged: Vec::new(), committed: Vec::new() };
    for (target, _) in plan.actions() {
        commit.stage(*target)?;
    }
    for (target, action) in plan.actions() {
        commit.commit(*target, *action)?;
    }
    let incomplete_cleanup = commit.cleanup();
    Ok(IntegrationReport { outcome: plan.outcome(), incomplete_cleanup })
}

/// In-memory registry for policy tests and fake-adapter wiring.
///
/// Every port call, including read-only ones, is counted so a failure can be
/// injected at any exact step of a request.
#[derive(Clone, Debug, Default)]
pub struct FakeRegistry {
    keys: BTreeMap<String, FakeKey>,
    failure: Option<InjectedFailure>,
    operations: RefCell<Vec<RegistryOperation>>,
    notifications: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct FakeKey {
    display_path: String,
    values: BTreeMap<String, (String, RegistryValue)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InjectedFailure {
    at_operation_index: usize,
    kind: RegistryErrorKind,
}

impl FakeRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fails exactly the `index`-th port call (zero-based, all operations
    /// counted, notifications excluded) and succeeds afterwards.
    pub fn fail_operation(&mut self, index: usize, kind: RegistryErrorKind) {
        self.failure = Some(InjectedFailure { at_operation_index: index, kind });
    }

    /// Port calls recorded so far, in order.
    #[must_use]
    pub fn operations(&self) -> Vec<RegistryOperation> {
        self.operations.borrow().clone()
    }

    pub fn clear_operations(&mut self) {
        self.operations.get_mut().clear();
    }

    #[must_use]
    pub fn notifications(&self) -> usize {
        self.notifications
    }

    /// Exact content of every key, for before/after comparisons.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, KeySnapshot> {
        self.keys
            .keys()
            .map(|lower| {
                let key = &self.keys[lower];
                (key.display_path.clone(), self.snapshot_of(lower))
            })
            .collect()
    }

    /// Direct subkey names below a path, as stored.
    #[must_use]
    pub fn subkeys_of(&self, key: &KeyPath) -> Vec<String> {
        self.snapshot_of(&Self::lower(key)).subkeys
    }

    fn lower(key: &KeyPath) -> String {
        key.to_native().to_ascii_lowercase()
    }

    fn snapshot_of(&self, lower: &str) -> KeySnapshot {
        let mut snapshot = KeySnapshot::new();
        if let Some(key) = self.keys.get(lower) {
            for (name, value) in key.values.values() {
                snapshot.insert_value(name, value.clone());
            }
        }
        let prefix = format!("{lower}\\");
        for (child, key) in self.keys.range(prefix.clone()..) {
            if !child.starts_with(&prefix) {
                break;
            }
            if !child[prefix.len()..].contains(KEY_SEPARATOR) {
                let name = key.display_path.rsplit(KEY_SEPARATOR).next().unwrap_or_default();
                snapshot.add_subkey(name);
            }
        }
        snapshot
    }

    fn record(&self, operation: RegistryOperation) -> Result<(), RegistryError> {
        let mut operations = self.operations.borrow_mut();
        let index = operations.len();
        operations.push(operation);
        match self.failure {
            Some(failure) if failure.at_operation_index == index => {
                Err(RegistryError::new(operation, failure.kind))
            }
            _ => Ok(()),
        }
    }

    fn descendants(&self, lower: &str) -> Vec<String> {
        let prefix = format!("{lower}\\");
        self.keys
            .range(prefix.clone()..)
            .take_while(|(child, _)| child.starts_with(&prefix))
            .map(|(child, _)| child.clone())
            .collect()
    }
}

impl IntegrationRegistry for FakeRegistry {
    fn key_exists(&self, key: &KeyPath) -> Result<bool, RegistryError> {
        self.record(RegistryOperation::Exists)?;
        Ok(self.keys.contains_key(&Self::lower(key)))
    }

    fn read_key(&self, key: &KeyPath) -> Result<Option<KeySnapshot>, RegistryError> {
        self.record(RegistryOperation::Read)?;
        let lower = Self::lower(key);
        Ok(self.keys.contains_key(&lower).then(|| self.snapshot_of(&lower)))
    }

    fn create_key(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
        self.record(RegistryOperation::Create)?;
        let components = key.components();
        for depth in 1..=components.len() {
            let display = components[..depth].join("\\");
            let lower = display.to_ascii_lowercase();
            self.keys
                .entry(lower)
                .or_insert_with(|| FakeKey { display_path: display, values: BTreeMap::new() });
        }
        Ok(())
    }

    fn write_string(
        &mut self,
        key: &KeyPath,
        name: &str,
        value: &OsStr,
    ) -> Result<(), RegistryError> {
        self.record(RegistryOperation::Write)?;
        let Some(entry) = self.keys.get_mut(&Self::lower(key)) else {
            return Err(RegistryError::new(RegistryOperation::Write, RegistryErrorKind::NotFound));
        };
        entry.values.insert(
            name.to_ascii_lowercase(),
            (name.to_owned(), RegistryValue::String(value.to_os_string())),
        );
        Ok(())
    }

    fn rename_key(&mut self, key: &KeyPath, new_name: &str) -> Result<(), RegistryError> {
        self.record(RegistryOperation::Rename)?;
        let from = Self::lower(key);
        let to_path = key.with_name(new_name);
        let to = Self::lower(&to_path);
        if !self.keys.contains_key(&from) {
            return Err(RegistryError::new(RegistryOperation::Rename, RegistryErrorKind::NotFound));
        }
        if self.keys.contains_key(&to) {
            return Err(RegistryError::new(
                RegistryOperation::Rename,
                RegistryErrorKind::AlreadyExists,
            ));
        }
        let mut moved = Vec::new();
        for lower in std::iter::once(from.clone()).chain(self.descendants(&from)) {
            let mut entry = self.keys.remove(&lower).unwrap_or_default();
            let suffix = entry.display_path[from.len()..].to_owned();
            entry.display_path = format!("{}{suffix}", to_path.to_native());
            moved.push((format!("{to}{}", &lower[from.len()..]), entry));
        }
        self.keys.extend(moved);
        Ok(())
    }

    fn delete_tree(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
        self.record(RegistryOperation::Delete)?;
        let lower = Self::lower(key);
        for descendant in self.descendants(&lower) {
            self.keys.remove(&descendant);
        }
        self.keys.remove(&lower);
        Ok(())
    }

    fn notify_association_changed(&mut self) {
        self.notifications += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Tools\Disk Pie\diskpie.exe";
    const MOVED_EXE: &str = r"D:\Portable\diskpie.exe";

    type AmbiguousCase<'a> = (&'a str, Box<dyn Fn(&mut FakeRegistry)>, AmbiguityReason);

    fn token() -> StagingToken {
        StagingToken::new(0x1234, 7)
    }

    fn record(path: &str) -> VerbRecord {
        VerbRecord::new(Path::new(path)).expect("fixture executable path is valid")
    }

    fn installed(path: &str) -> FakeRegistry {
        let mut registry = FakeRegistry::new();
        apply(&mut registry, IntegrationRequest::Install, Path::new(path), &token())
            .expect("fixture install succeeds");
        registry.clear_operations();
        registry
    }

    fn with_adjacent(registry: &mut FakeRegistry) -> KeyPath {
        let adjacent = shell_key(VerbTarget::Directory).child("Adjacent.Verb");
        registry.create_key(&adjacent).expect("fake create");
        registry
            .write_string(&adjacent, MUI_VERB_VALUE, OsStr::new("Adjacent"))
            .expect("fake write");
        let command = adjacent.child(COMMAND_KEY_NAME);
        registry.create_key(&command).expect("fake create");
        registry
            .write_string(&command, DEFAULT_VALUE_NAME, OsStr::new(r#""C:\other.exe" "%1""#))
            .expect("fake write");
        registry.create_key(&shell_key(VerbTarget::Drive)).expect("fake create");
        registry.clear_operations();
        adjacent
    }

    fn assert_no_mutation(registry: &FakeRegistry) {
        assert!(
            registry.operations().iter().all(|operation| matches!(
                operation,
                RegistryOperation::Exists | RegistryOperation::Read
            )),
            "only read operations expected: {:?}",
            registry.operations()
        );
    }

    #[test]
    fn key_paths_are_exact_and_vendor_qualified() {
        assert_eq!(verb_key(VerbTarget::Directory).to_native(), r"Directory\shell\DiskPie.Scan");
        assert_eq!(verb_key(VerbTarget::Drive).to_native(), r"Drive\shell\DiskPie.Scan");
        assert_eq!(
            command_key(VerbTarget::Directory).to_native(),
            r"Directory\shell\DiskPie.Scan\command"
        );
        assert_eq!(
            staging_verb_key(VerbTarget::Drive, &token()).to_native(),
            r"Drive\shell\DiskPie.Scan.staging.1234-7"
        );
        assert_eq!(
            previous_verb_key(VerbTarget::Drive, &token()).to_native(),
            r"Drive\shell\DiskPie.Scan.previous.1234-7"
        );
        assert_eq!(
            staging_verb_key(VerbTarget::Drive, &token()).parent(),
            Some(shell_key(VerbTarget::Drive))
        );
        assert!(KeyPath::try_new(["a", "b\\c"]).is_none());
        assert!(KeyPath::try_new(Vec::<String>::new()).is_none());
        assert!(KeyPath::try_new(["", "b"]).is_none());
    }

    #[test]
    fn verb_values_are_exact_reg_sz_entries() {
        let record = record(EXE);
        let verb = record.verb_snapshot();
        let names: Vec<&str> = verb.values().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                EXECUTABLE_VALUE,
                OWNER_VALUE,
                SCHEMA_VALUE,
                ICON_VALUE,
                MUI_VERB_VALUE,
                MULTI_SELECT_MODEL_VALUE
            ]
        );
        assert_eq!(verb.string_value("muiverb"), Some(OsStr::new(VERB_LABEL)));
        assert_eq!(
            verb.string_value(ICON_VALUE),
            Some(OsStr::new(&format!("\"{EXE}\",0"))),
            "the icon path is quoted so a comma inside it cannot be read as an index"
        );
        assert_eq!(verb.string_value(MULTI_SELECT_MODEL_VALUE), Some(OsStr::new("Single")));
        assert_eq!(verb.string_value(OWNER_VALUE), Some(OsStr::new(OWNER_MARKER)));
        assert_eq!(verb.string_value(SCHEMA_VALUE), Some(OsStr::new("1")));
        assert_eq!(verb.string_value(EXECUTABLE_VALUE), Some(OsStr::new(EXE)));
        assert_eq!(verb.subkeys(), [COMMAND_KEY_NAME]);
        assert!(verb.values().all(|(_, value)| matches!(value, RegistryValue::String(_))));

        let command = record.command_snapshot(VerbTarget::Directory);
        assert_eq!(
            command.string_value(DEFAULT_VALUE_NAME),
            Some(OsStr::new(r#""C:\Tools\Disk Pie\diskpie.exe" --scan-path "%1""#))
        );
        assert!(command.subkeys().is_empty());
    }

    #[test]
    fn command_template_quotes_verbatim_without_expansion() {
        let cases = [
            r"C:\Tools\Disk Pie\diskpie.exe",
            r"C:\🍕 pie\diskpie.exe",
            r"C:\100%\diskpie.exe",
            r"C:\a&b^c,d\diskpie.exe",
            r"C:\-leading dash\--diskpie.exe",
            r"\\server\share\diskpie.exe",
            r"\\?\C:\very\long\diskpie.exe",
            "/unix/style/diskpie",
        ];
        for path in cases {
            let record = record(path);
            let mut expected = OsString::from("\"");
            expected.push(path);
            expected.push("\" --scan-path \"%1\"");
            assert_eq!(record.command_line(VerbTarget::Directory), expected, "{path}");
            let mut drive = OsString::from("\"");
            drive.push(path);
            drive.push("\" --scan-path \"%1\\\"");
            assert_eq!(record.command_line(VerbTarget::Drive), drive, "{path}");
        }
        assert_eq!(VerbTarget::Directory.selection_argument(), "\"%1\"");
        assert_eq!(VerbTarget::Drive.selection_argument(), "\"%1\\\"");
    }

    #[test]
    fn executable_paths_that_cannot_be_quoted_are_rejected() {
        assert_eq!(VerbRecord::new(Path::new("")), Err(ExecutablePathError::Empty));
        assert_eq!(
            VerbRecord::new(Path::new(r#"C:\quote"here.exe"#)),
            Err(ExecutablePathError::ContainsQuote)
        );
        assert_eq!(
            VerbRecord::new(Path::new(r"C:\trailing\")),
            Err(ExecutablePathError::TrailingSeparator)
        );
        assert_eq!(
            VerbRecord::new(Path::new("C:/trailing/")),
            Err(ExecutablePathError::TrailingSeparator)
        );
        let mut nul = OsString::from(r"C:\nul");
        nul.push("\0");
        nul.push(".exe");
        assert_eq!(VerbRecord::new(Path::new(&nul)), Err(ExecutablePathError::ContainsNul));
        for placeholder in [r"C:\tools\100%1\diskpie.exe", r"C:\%L\diskpie.exe", r"C:\a%~b\d.exe"] {
            assert_eq!(
                VerbRecord::new(Path::new(placeholder)),
                Err(ExecutablePathError::ContainsPlaceholder),
                "{placeholder}"
            );
        }
        assert!(VerbRecord::new(Path::new(r"C:\100% tools\diskpie.exe")).is_ok());
        assert!(VerbRecord::new(Path::new(r"C:\a%\diskpie.exe")).is_ok());
        assert_eq!(
            apply(&mut FakeRegistry::new(), IntegrationRequest::Install, Path::new(""), &token()),
            Err(IntegrationError::InvalidExecutable(ExecutablePathError::Empty))
        );
    }

    #[test]
    fn install_writes_exact_keys_and_removes_staging() {
        let mut registry = FakeRegistry::new();
        let adjacent = with_adjacent(&mut registry);
        let before = registry.snapshot();

        let report = apply(&mut registry, IntegrationRequest::Install, Path::new(EXE), &token())
            .expect("install succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Installed);
        assert_eq!(report.incomplete_cleanup, None);
        assert_eq!(registry.notifications(), 1);

        let record = record(EXE);
        for target in VerbTarget::ALL {
            let verb = registry.read_key(&verb_key(target)).expect("read").expect("verb exists");
            assert_eq!(verb, record.verb_snapshot());
            let command =
                registry.read_key(&command_key(target)).expect("read").expect("command exists");
            assert_eq!(command, record.command_snapshot(target));
            assert_eq!(
                registry.key_exists(&staging_verb_key(target, &token())),
                Ok(false),
                "staging removed for {target:?}"
            );
        }
        assert_eq!(
            registry.subkeys_of(&shell_key(VerbTarget::Directory)),
            ["Adjacent.Verb", VERB_KEY_NAME]
        );
        assert_eq!(registry.subkeys_of(&shell_key(VerbTarget::Drive)), [VERB_KEY_NAME]);

        let after = registry.snapshot();
        for (path, snapshot) in &before {
            if path.eq_ignore_ascii_case(&shell_key(VerbTarget::Directory).to_native())
                || path.eq_ignore_ascii_case(&shell_key(VerbTarget::Drive).to_native())
            {
                continue;
            }
            assert_eq!(after.get(path), Some(snapshot), "{path} unchanged");
        }
        assert_eq!(after[&adjacent.to_native()], before[&adjacent.to_native()]);

        let status = inspect(&registry, Path::new(EXE)).expect("inspect");
        assert!(status.is_installed());
        assert_eq!(status.state(), &IntegrationState::OwnedCurrent);
    }

    #[test]
    fn matching_install_is_idempotent_without_writes() {
        let mut registry = installed(EXE);
        let before = registry.snapshot();
        let report = apply(&mut registry, IntegrationRequest::Install, Path::new(EXE), &token())
            .expect("repeated install succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::AlreadyInstalled);
        assert_no_mutation(&registry);
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.notifications(), 1);

        let report = apply(&mut registry, IntegrationRequest::Repair, Path::new(EXE), &token())
            .expect("repair of a current install is a no-op");
        assert_eq!(report.outcome, IntegrationOutcome::AlreadyInstalled);
        assert_eq!(registry.snapshot(), before);
    }

    #[test]
    fn foreign_and_ambiguous_keys_refuse_every_request_without_overwrite() {
        let mut foreign = FakeRegistry::new();
        let verb = verb_key(VerbTarget::Directory);
        foreign.create_key(&verb).expect("fake create");
        foreign.write_string(&verb, MUI_VERB_VALUE, OsStr::new("Someone else")).expect("write");
        foreign.create_key(&verb.child(COMMAND_KEY_NAME)).expect("fake create");
        foreign.clear_operations();
        let before = foreign.snapshot();
        for request in
            [IntegrationRequest::Install, IntegrationRequest::Repair, IntegrationRequest::Remove]
        {
            let error = apply(&mut foreign, request, Path::new(EXE), &token())
                .expect_err("foreign key refuses");
            let IntegrationError::Conflict(status) = error else {
                panic!("expected conflict, got {error:?}");
            };
            assert_eq!(status.state(), &IntegrationState::Foreign);
            assert_eq!(status.key_state(VerbTarget::Directory), &KeyState::Foreign);
            assert_eq!(status.key_state(VerbTarget::Drive), &KeyState::NotInstalled);
            assert_no_mutation(&foreign);
            assert_eq!(foreign.snapshot(), before);
        }
        assert_eq!(foreign.notifications(), 0);

        let other_owner = installed(EXE);
        let mut mutated = other_owner.clone();
        mutated
            .write_string(&verb, OWNER_VALUE, OsStr::new("{00000000-0000-0000-0000-000000000000}"))
            .expect("write");
        assert_eq!(
            inspect(&mutated, Path::new(EXE)).expect("inspect").key_state(VerbTarget::Directory),
            &KeyState::Foreign
        );

        let ambiguous_cases: Vec<AmbiguousCase<'_>> = vec![
            (
                "schema",
                Box::new(|registry| {
                    registry
                        .write_string(&verb_key(VerbTarget::Drive), SCHEMA_VALUE, OsStr::new("2"))
                        .expect("write");
                }),
                AmbiguityReason::UnsupportedSchema,
            ),
            (
                "extra value",
                Box::new(|registry| {
                    registry
                        .write_string(&verb_key(VerbTarget::Drive), "Extended", OsStr::new("x"))
                        .expect("write");
                }),
                AmbiguityReason::UnexpectedValues,
            ),
            (
                "inconsistent icon",
                Box::new(|registry| {
                    registry
                        .write_string(&verb_key(VerbTarget::Drive), ICON_VALUE, OsStr::new("x"))
                        .expect("write");
                }),
                AmbiguityReason::InconsistentValues,
            ),
            (
                "extra subkey",
                Box::new(|registry| {
                    registry
                        .create_key(&verb_key(VerbTarget::Drive).child("DropTarget"))
                        .expect("c");
                }),
                AmbiguityReason::UnexpectedSubkeys,
            ),
            (
                "edited command",
                Box::new(|registry| {
                    registry
                        .write_string(
                            &command_key(VerbTarget::Drive),
                            DEFAULT_VALUE_NAME,
                            OsStr::new("cmd.exe /c evil"),
                        )
                        .expect("write");
                }),
                AmbiguityReason::InconsistentCommand,
            ),
            (
                "missing command",
                Box::new(|registry| {
                    registry.delete_tree(&command_key(VerbTarget::Drive)).expect("delete");
                }),
                AmbiguityReason::InconsistentCommand,
            ),
        ];
        for (label, mutate, reason) in ambiguous_cases {
            let mut registry = installed(EXE);
            mutate(&mut registry);
            registry.clear_operations();
            let before = registry.snapshot();
            let status = inspect(&registry, Path::new(EXE)).expect("inspect");
            assert_eq!(status.state(), &IntegrationState::Ambiguous(reason), "{label}");
            assert_eq!(
                status.key_state(VerbTarget::Drive),
                &KeyState::Ambiguous(reason),
                "{label}"
            );
            assert!(status.is_conflict());
            for request in [
                IntegrationRequest::Install,
                IntegrationRequest::Repair,
                IntegrationRequest::Remove,
            ] {
                assert!(
                    matches!(
                        apply(&mut registry, request, Path::new(EXE), &token()),
                        Err(IntegrationError::Conflict(_))
                    ),
                    "{label} {request:?}"
                );
            }
            assert_no_mutation(&registry);
            assert_eq!(registry.snapshot(), before, "{label}");
        }
    }

    #[test]
    fn moved_executable_is_owned_stale_and_requires_a_decision() {
        let mut registry = installed(EXE);
        let before = registry.snapshot();

        let status = inspect(&registry, Path::new(MOVED_EXE)).expect("inspect");
        assert_eq!(
            status.state(),
            &IntegrationState::OwnedStale {
                stored_executable: PathBuf::from(EXE),
                current_executable: PathBuf::from(MOVED_EXE),
            }
        );
        assert!(status.requires_decision());
        assert!(!status.is_conflict());
        assert_eq!(
            status.key_state(VerbTarget::Directory),
            &KeyState::OwnedStale { stored_executable: PathBuf::from(EXE) }
        );

        let error =
            apply(&mut registry, IntegrationRequest::Install, Path::new(MOVED_EXE), &token())
                .expect_err("install refuses a stale integration");
        assert!(matches!(error, IntegrationError::StaleRequiresDecision(_)));
        assert_no_mutation(&registry);
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.notifications(), 1);
        assert_eq!(
            decide(IntegrationRequest::Install, &status),
            Decision::Refuse(Refusal::StaleRequiresDecision)
        );
    }

    #[test]
    fn repair_replaces_stale_keys_and_leaves_no_siblings() {
        let mut registry = installed(EXE);
        let adjacent = with_adjacent(&mut registry);
        let adjacent_before = registry.read_key(&adjacent).expect("read");
        let repair_token = StagingToken::new(9, 9);

        let report =
            apply(&mut registry, IntegrationRequest::Repair, Path::new(MOVED_EXE), &repair_token)
                .expect("repair succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Repaired);
        assert_eq!(report.incomplete_cleanup, None);
        assert_eq!(registry.notifications(), 2);

        let moved = record(MOVED_EXE);
        for target in VerbTarget::ALL {
            assert_eq!(
                registry.read_key(&verb_key(target)).expect("read"),
                Some(moved.verb_snapshot())
            );
            assert_eq!(
                registry.read_key(&command_key(target)).expect("read"),
                Some(moved.command_snapshot(target))
            );
            assert_eq!(registry.key_exists(&staging_verb_key(target, &repair_token)), Ok(false));
            assert_eq!(registry.key_exists(&previous_verb_key(target, &repair_token)), Ok(false));
        }
        assert_eq!(
            registry.subkeys_of(&shell_key(VerbTarget::Directory)),
            ["Adjacent.Verb", VERB_KEY_NAME]
        );
        assert_eq!(registry.read_key(&adjacent).expect("read"), adjacent_before);
        assert!(inspect(&registry, Path::new(MOVED_EXE)).expect("inspect").is_installed());
    }

    #[test]
    fn remove_deletes_only_the_owned_subtrees_and_is_idempotent() {
        let mut registry = installed(EXE);
        let adjacent = with_adjacent(&mut registry);
        let before = registry.snapshot();

        let report = apply(&mut registry, IntegrationRequest::Remove, Path::new(EXE), &token())
            .expect("remove succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Removed);
        assert_eq!(registry.notifications(), 2);
        assert_eq!(
            registry
                .operations()
                .iter()
                .filter(|operation| **operation == RegistryOperation::Delete)
                .count(),
            2
        );
        for target in VerbTarget::ALL {
            assert_eq!(registry.key_exists(&verb_key(target)), Ok(false));
            assert_eq!(registry.key_exists(&shell_key(target)), Ok(true), "shell parent kept");
            assert_eq!(
                registry.key_exists(&KeyPath::try_new([target.class_key_name()]).expect("path")),
                Ok(true),
                "class parent kept"
            );
        }
        let after = registry.snapshot();
        for (path, snapshot) in &before {
            let owned = VerbTarget::ALL.iter().any(|target| {
                let prefix = verb_key(*target).to_native().to_ascii_lowercase();
                path.to_ascii_lowercase().starts_with(&prefix)
            });
            let shell = VerbTarget::ALL
                .iter()
                .any(|target| path.eq_ignore_ascii_case(&shell_key(*target).to_native()));
            if owned || shell {
                assert!(shell || !after.contains_key(path), "{path} removed");
            } else {
                assert_eq!(after.get(path), Some(snapshot), "{path} unchanged");
            }
        }
        assert_eq!(after[&adjacent.to_native()], before[&adjacent.to_native()]);
        assert_eq!(
            inspect(&registry, Path::new(EXE)).expect("inspect").state(),
            &IntegrationState::NotInstalled
        );

        registry.clear_operations();
        let report = apply(&mut registry, IntegrationRequest::Remove, Path::new(EXE), &token())
            .expect("repeated remove succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::AlreadyRemoved);
        assert_no_mutation(&registry);
        assert_eq!(registry.notifications(), 2);

        let mut stale = installed(EXE);
        let report = apply(&mut stale, IntegrationRequest::Remove, Path::new(MOVED_EXE), &token())
            .expect("remove works on a stale but owned integration");
        assert_eq!(report.outcome, IntegrationOutcome::Removed);
        assert_eq!(
            inspect(&stale, Path::new(MOVED_EXE)).expect("inspect").state(),
            &IntegrationState::NotInstalled
        );
    }

    #[test]
    fn partial_installation_is_reported_and_completed_by_install() {
        let mut registry = installed(EXE);
        registry.delete_tree(&verb_key(VerbTarget::Drive)).expect("delete");
        let status = inspect(&registry, Path::new(EXE)).expect("inspect");
        assert_eq!(
            status.state(),
            &IntegrationState::Ambiguous(AmbiguityReason::PartialInstallation)
        );
        assert!(!status.is_conflict());
        assert_eq!(
            decide(IntegrationRequest::Install, &status),
            Decision::Perform(Plan {
                actions: vec![(VerbTarget::Drive, TargetAction::Install)],
                strays: vec![],
                outcome: IntegrationOutcome::Installed,
            })
        );
        let report = apply(&mut registry, IntegrationRequest::Install, Path::new(EXE), &token())
            .expect("install completes the missing key");
        assert_eq!(report.outcome, IntegrationOutcome::Installed);
        assert!(inspect(&registry, Path::new(EXE)).expect("inspect").is_installed());

        let mut divergent = installed(EXE);
        divergent
            .write_string(&verb_key(VerbTarget::Drive), EXECUTABLE_VALUE, OsStr::new(MOVED_EXE))
            .expect("write");
        divergent
            .write_string(&verb_key(VerbTarget::Drive), ICON_VALUE, &record(MOVED_EXE).icon_value())
            .expect("write");
        divergent
            .write_string(
                &command_key(VerbTarget::Drive),
                DEFAULT_VALUE_NAME,
                &record(MOVED_EXE).command_line(VerbTarget::Drive),
            )
            .expect("write");
        let status = inspect(&divergent, Path::new(r"E:\third.exe")).expect("inspect");
        assert_eq!(
            status.state(),
            &IntegrationState::Ambiguous(AmbiguityReason::DivergentExecutables)
        );
        assert!(!status.is_conflict());
        assert_eq!(
            decide(IntegrationRequest::Remove, &status),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Remove),
                    (VerbTarget::Drive, TargetAction::Remove)
                ],
                strays: vec![],
                outcome: IntegrationOutcome::Removed,
            })
        );
    }

    #[test]
    fn staging_collision_refuses_without_writing() {
        let mut registry = FakeRegistry::new();
        registry.create_key(&staging_verb_key(VerbTarget::Drive, &token())).expect("create");
        registry.clear_operations();
        let before = registry.snapshot();
        let error = apply(&mut registry, IntegrationRequest::Install, Path::new(EXE), &token())
            .expect_err("collision refuses");
        assert_eq!(error, IntegrationError::StagingCollision { target: VerbTarget::Drive });
        assert_no_mutation(&registry);
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.notifications(), 0);
    }

    fn assert_rollback_on_every_step(seed: &FakeRegistry, request: IntegrationRequest, exe: &str) {
        let mut happy = seed.clone();
        happy.clear_operations();
        let report = apply(&mut happy, request, Path::new(exe), &token()).expect("happy path");
        assert!(report.outcome.mutated());
        let steps = happy.operations().len();
        let first_mutation = happy
            .operations()
            .iter()
            .position(|operation| {
                !matches!(operation, RegistryOperation::Exists | RegistryOperation::Read)
            })
            .expect("the happy path mutates");
        assert!(steps > first_mutation + 4, "enough steps to inject into");

        for step in 0..steps {
            for kind in [RegistryErrorKind::AccessDenied, RegistryErrorKind::Other] {
                let mut registry = seed.clone();
                registry.clear_operations();
                registry.fail_operation(step, kind);
                let error = match apply(&mut registry, request, Path::new(exe), &token()) {
                    Err(error) => error,
                    Ok(report) => {
                        // Only the post-commit deletion of a `previous`
                        // sibling may fail after the mutation succeeded; the
                        // leftover is then visible as an owned stray.
                        assert_eq!(report.outcome, IntegrationOutcome::Repaired, "step {step}");
                        let cleanup = report.incomplete_cleanup.expect("cleanup failure reported");
                        assert_eq!(cleanup.operation, RegistryOperation::Delete);
                        assert_eq!(cleanup.kind, kind);
                        let status = inspect(&registry, Path::new(exe)).expect("inspect");
                        assert!(status.is_installed(), "step {step}");
                        assert!(!status.strays().is_empty(), "step {step}");
                        assert!(
                            status
                                .strays()
                                .iter()
                                .all(|stray| stray.name().contains(PREVIOUS_INFIX))
                        );
                        assert_eq!(registry.notifications(), seed.notifications() + 1);
                        continue;
                    }
                };
                let rollback = match &error {
                    IntegrationError::Registry { error, rollback } => {
                        assert_eq!(error.kind, kind, "step {step}");
                        *rollback
                    }
                    IntegrationError::Verification { rollback, .. }
                    | IntegrationError::ConcurrentModification { rollback, .. } => *rollback,
                    other => panic!("unexpected error at step {step}: {other:?}"),
                };
                assert!(
                    matches!(rollback, RollbackReport::NotNeeded | RollbackReport::Clean),
                    "step {step}: {rollback:?}"
                );
                assert_eq!(
                    registry.snapshot(),
                    seed.snapshot(),
                    "step {step} {kind:?} restores state"
                );
                assert_eq!(registry.notifications(), seed.notifications(), "step {step}");
            }
        }
    }

    #[test]
    fn install_rolls_back_on_every_injected_failure() {
        let mut seed = FakeRegistry::new();
        with_adjacent(&mut seed);
        assert_rollback_on_every_step(&seed, IntegrationRequest::Install, EXE);
    }

    #[test]
    fn repair_rolls_back_on_every_injected_failure() {
        let mut seed = installed(EXE);
        with_adjacent(&mut seed);
        assert_rollback_on_every_step(&seed, IntegrationRequest::Repair, MOVED_EXE);
    }

    #[test]
    fn partial_install_rolls_back_on_every_injected_failure() {
        let mut seed = installed(EXE);
        seed.delete_tree(&verb_key(VerbTarget::Drive)).expect("delete");
        with_adjacent(&mut seed);
        assert_rollback_on_every_step(&seed, IntegrationRequest::Install, EXE);
    }

    #[test]
    fn owned_strays_are_reported_and_removed_only_when_marked() {
        let mut registry = installed(EXE);
        let owned_stray = staging_verb_key(VerbTarget::Drive, &StagingToken::new(1, 1));
        registry.create_key(&owned_stray.child(COMMAND_KEY_NAME)).expect("create");
        registry.write_string(&owned_stray, OWNER_VALUE, OsStr::new(OWNER_MARKER)).expect("write");
        registry
            .write_string(&owned_stray, SCHEMA_VALUE, OsStr::new(SCHEMA_VERSION))
            .expect("write");
        registry.write_string(&owned_stray, EXECUTABLE_VALUE, OsStr::new(EXE)).expect("write");
        let unmarked = shell_key(VerbTarget::Directory).child("DiskPie.Scan.previous.someone");
        registry.create_key(&unmarked).expect("create");
        registry.write_string(&unmarked, MUI_VERB_VALUE, OsStr::new("Not ours")).expect("write");
        // A transient name carrying only the marker is not proof of ownership.
        let marker_only = previous_verb_key(VerbTarget::Directory, &StagingToken::new(2, 2));
        registry.create_key(&marker_only).expect("create");
        registry.write_string(&marker_only, OWNER_VALUE, OsStr::new(OWNER_MARKER)).expect("write");
        // A fully marked key under a non-transient sibling name is never a stray.
        let unrelated = shell_key(VerbTarget::Directory).child("DiskPie.Scanner");
        registry.create_key(&unrelated).expect("create");
        registry.write_string(&unrelated, OWNER_VALUE, OsStr::new(OWNER_MARKER)).expect("write");
        registry.write_string(&unrelated, SCHEMA_VALUE, OsStr::new(SCHEMA_VERSION)).expect("write");
        registry.write_string(&unrelated, EXECUTABLE_VALUE, OsStr::new(EXE)).expect("write");
        registry.clear_operations();

        let status = inspect(&registry, Path::new(EXE)).expect("inspect");
        assert!(status.is_installed());
        assert_eq!(status.strays(), std::slice::from_ref(&owned_stray));
        assert!(format!("{status:?}").contains("strays: 1"));
        assert_eq!(
            decide(IntegrationRequest::Install, &status),
            Decision::NoChange(IntegrationOutcome::AlreadyInstalled)
        );
        assert_eq!(
            decide(IntegrationRequest::Remove, &status),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Remove),
                    (VerbTarget::Drive, TargetAction::Remove)
                ],
                strays: vec![owned_stray.clone()],
                outcome: IntegrationOutcome::Removed,
            })
        );

        let report = apply(&mut registry, IntegrationRequest::Remove, Path::new(EXE), &token())
            .expect("remove succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Removed);
        assert_eq!(registry.key_exists(&owned_stray), Ok(false));
        assert_eq!(registry.key_exists(&unmarked), Ok(true), "unmarked sibling is not ours");
        assert_eq!(registry.key_exists(&marker_only), Ok(true), "a marker alone is not ownership");
        assert_eq!(registry.key_exists(&unrelated), Ok(true), "other prefixes are ignored");
        let status = inspect(&registry, Path::new(EXE)).expect("inspect");
        assert_eq!(status.state(), &IntegrationState::NotInstalled);
        assert!(status.strays().is_empty());

        // A fully marked stray alone is enough for Remove to act and to
        // notify Explorer.
        registry.create_key(&owned_stray).expect("create");
        registry.write_string(&owned_stray, OWNER_VALUE, OsStr::new(OWNER_MARKER)).expect("write");
        registry
            .write_string(&owned_stray, SCHEMA_VALUE, OsStr::new(SCHEMA_VERSION))
            .expect("write");
        registry.write_string(&owned_stray, EXECUTABLE_VALUE, OsStr::new(EXE)).expect("write");
        let notifications = registry.notifications();
        let report = apply(&mut registry, IntegrationRequest::Remove, Path::new(EXE), &token())
            .expect("stray-only remove succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Removed);
        assert_eq!(registry.key_exists(&owned_stray), Ok(false));
        assert_eq!(registry.notifications(), notifications + 1);
    }

    #[test]
    fn staged_readback_mismatch_rolls_back() {
        struct Corrupting {
            inner: FakeRegistry,
        }
        impl IntegrationRegistry for Corrupting {
            fn key_exists(&self, key: &KeyPath) -> Result<bool, RegistryError> {
                self.inner.key_exists(key)
            }
            fn read_key(&self, key: &KeyPath) -> Result<Option<KeySnapshot>, RegistryError> {
                let mut snapshot = self.inner.read_key(key)?;
                if key.name().contains(STAGING_INFIX)
                    && let Some(snapshot) = snapshot.as_mut()
                {
                    snapshot.insert_value(ICON_VALUE, RegistryValue::Other { type_id: 3 });
                }
                Ok(snapshot)
            }
            fn create_key(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
                self.inner.create_key(key)
            }
            fn write_string(
                &mut self,
                key: &KeyPath,
                name: &str,
                value: &OsStr,
            ) -> Result<(), RegistryError> {
                self.inner.write_string(key, name, value)
            }
            fn rename_key(&mut self, key: &KeyPath, new_name: &str) -> Result<(), RegistryError> {
                self.inner.rename_key(key, new_name)
            }
            fn delete_tree(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
                self.inner.delete_tree(key)
            }
            fn notify_association_changed(&mut self) {
                self.inner.notify_association_changed();
            }
        }

        let mut seed = FakeRegistry::new();
        with_adjacent(&mut seed);
        let mut registry = Corrupting { inner: seed };
        let before = registry.inner.snapshot();
        let error = apply(&mut registry, IntegrationRequest::Install, Path::new(EXE), &token())
            .expect_err("corrupted read-back fails verification");
        assert_eq!(
            error,
            IntegrationError::Verification {
                target: VerbTarget::Directory,
                rollback: RollbackReport::Clean
            }
        );
        assert_eq!(registry.inner.snapshot(), before);
        assert_eq!(registry.inner.notifications(), 0);
    }

    #[test]
    fn failed_rollback_is_reported_not_hidden() {
        let mut registry = FakeRegistry::new();
        let mut probe = registry.clone();
        apply(&mut probe, IntegrationRequest::Install, Path::new(EXE), &token()).expect("probe");
        let last_rename = probe
            .operations()
            .iter()
            .rposition(|operation| *operation == RegistryOperation::Rename)
            .expect("rename happens");
        registry.fail_operation(last_rename, RegistryErrorKind::AccessDenied);
        // The final rename fails; rollback deletes the already-installed
        // Directory key. Make that rollback delete fail as well.
        struct FailingDelete {
            inner: FakeRegistry,
            fail_next_delete: bool,
        }
        impl IntegrationRegistry for FailingDelete {
            fn key_exists(&self, key: &KeyPath) -> Result<bool, RegistryError> {
                self.inner.key_exists(key)
            }
            fn read_key(&self, key: &KeyPath) -> Result<Option<KeySnapshot>, RegistryError> {
                self.inner.read_key(key)
            }
            fn create_key(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
                self.inner.create_key(key)
            }
            fn write_string(
                &mut self,
                key: &KeyPath,
                name: &str,
                value: &OsStr,
            ) -> Result<(), RegistryError> {
                self.inner.write_string(key, name, value)
            }
            fn rename_key(&mut self, key: &KeyPath, new_name: &str) -> Result<(), RegistryError> {
                let result = self.inner.rename_key(key, new_name);
                if result.is_err() {
                    self.fail_next_delete = true;
                }
                result
            }
            fn delete_tree(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
                if std::mem::take(&mut self.fail_next_delete) {
                    return Err(RegistryError::new(
                        RegistryOperation::Delete,
                        RegistryErrorKind::AccessDenied,
                    ));
                }
                self.inner.delete_tree(key)
            }
            fn notify_association_changed(&mut self) {
                self.inner.notify_association_changed();
            }
        }
        let mut failing = FailingDelete { inner: registry, fail_next_delete: false };
        let error = apply(&mut failing, IntegrationRequest::Install, Path::new(EXE), &token())
            .expect_err("rename failure surfaces");
        let IntegrationError::Registry { error, rollback } = error else {
            panic!("expected registry error, got {error:?}");
        };
        assert_eq!(error.operation, RegistryOperation::Rename);
        assert!(
            matches!(rollback, RollbackReport::Failed(failed) if failed.operation == RegistryOperation::Delete)
        );
        assert_eq!(failing.inner.notifications(), 0);
    }

    #[test]
    fn decision_table_matches_adr_items_three_to_five() {
        let current = Path::new(EXE);
        let stale = KeyState::OwnedStale { stored_executable: PathBuf::from(MOVED_EXE) };
        let status =
            |keys: [KeyState; 2]| ExplorerIntegrationStatus::from_key_states(current, keys);

        let none = status([KeyState::NotInstalled, KeyState::NotInstalled]);
        assert_eq!(
            decide(IntegrationRequest::Install, &none),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Install),
                    (VerbTarget::Drive, TargetAction::Install)
                ],
                strays: vec![],
                outcome: IntegrationOutcome::Installed,
            })
        );
        assert_eq!(
            decide(IntegrationRequest::Remove, &none),
            Decision::NoChange(IntegrationOutcome::AlreadyRemoved)
        );
        assert_eq!(
            decide(IntegrationRequest::Repair, &none),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Install),
                    (VerbTarget::Drive, TargetAction::Install)
                ],
                strays: vec![],
                outcome: IntegrationOutcome::Repaired,
            })
        );

        let current_status = status([KeyState::OwnedCurrent, KeyState::OwnedCurrent]);
        assert_eq!(
            decide(IntegrationRequest::Install, &current_status),
            Decision::NoChange(IntegrationOutcome::AlreadyInstalled)
        );
        assert_eq!(
            decide(IntegrationRequest::Remove, &current_status),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Remove),
                    (VerbTarget::Drive, TargetAction::Remove)
                ],
                strays: vec![],
                outcome: IntegrationOutcome::Removed,
            })
        );

        let stale_status = status([stale.clone(), stale.clone()]);
        assert_eq!(
            decide(IntegrationRequest::Install, &stale_status),
            Decision::Refuse(Refusal::StaleRequiresDecision)
        );
        assert_eq!(
            decide(IntegrationRequest::Repair, &stale_status),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Replace),
                    (VerbTarget::Drive, TargetAction::Replace)
                ],
                strays: vec![],
                outcome: IntegrationOutcome::Repaired,
            })
        );
        assert_eq!(
            decide(IntegrationRequest::Remove, &stale_status),
            Decision::Perform(Plan {
                actions: vec![
                    (VerbTarget::Directory, TargetAction::Remove),
                    (VerbTarget::Drive, TargetAction::Remove)
                ],
                strays: vec![],
                outcome: IntegrationOutcome::Removed,
            })
        );

        for blocking in [KeyState::Foreign, KeyState::Ambiguous(AmbiguityReason::MissingValues)] {
            let conflict = status([KeyState::OwnedCurrent, blocking]);
            for request in [
                IntegrationRequest::Install,
                IntegrationRequest::Repair,
                IntegrationRequest::Remove,
            ] {
                assert_eq!(decide(request, &conflict), Decision::Refuse(Refusal::Conflict));
            }
        }
    }

    #[test]
    fn debug_and_display_output_never_contain_executable_paths() {
        let registry = installed(EXE);
        let status = inspect(&registry, Path::new(MOVED_EXE)).expect("inspect");
        let rendered = format!("{status:?}");
        assert!(rendered.contains("OwnedStale"));
        assert!(!rendered.contains("diskpie.exe"));
        assert!(!rendered.contains("Tools"));
        assert!(!rendered.contains("Portable"));

        let error = IntegrationError::StaleRequiresDecision(Box::new(status.clone()));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("diskpie.exe"));
        let IntegrationState::OwnedStale { stored_executable, current_executable } = status.state()
        else {
            panic!("expected stale state");
        };
        assert_eq!(stored_executable, Path::new(EXE));
        assert_eq!(current_executable, Path::new(MOVED_EXE));

        let record = record(EXE);
        assert!(!format!("{record:?}").contains("Tools"));
        let snapshot = record.verb_snapshot();
        let rendered = format!("{snapshot:?}");
        assert!(rendered.contains(MUI_VERB_VALUE));
        assert!(!rendered.contains("Tools"));
        assert!(!rendered.contains(EXE));
    }

    #[test]
    fn tokens_are_unique_and_registry_safe() {
        let first = StagingToken::generate();
        let second = StagingToken::generate();
        assert_ne!(first, second);
        for token in [first, second, token()] {
            assert!(
                token.as_str().chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{}",
                token.as_str()
            );
            assert!(KeyPath::try_new([staging_name(&token)]).is_some());
        }
    }

    #[test]
    fn fake_registry_names_are_case_insensitive_and_exact_subtree_scoped() {
        let mut registry = FakeRegistry::new();
        let upper = KeyPath::try_new(["Directory", "Shell", "Verb"]).expect("path");
        let lower = KeyPath::try_new(["directory", "shell", "verb"]).expect("path");
        registry.create_key(&upper).expect("create");
        assert_eq!(registry.key_exists(&lower), Ok(true));
        registry.write_string(&lower, "Name", OsStr::new("value")).expect("write");
        let snapshot = registry.read_key(&upper).expect("read").expect("exists");
        assert_eq!(snapshot.string_value("NAME"), Some(OsStr::new("value")));

        let sibling = KeyPath::try_new(["Directory", "shell", "Verb2"]).expect("path");
        registry.create_key(&sibling.child("command")).expect("create");
        registry.delete_tree(&upper).expect("delete");
        assert_eq!(registry.key_exists(&upper), Ok(false));
        assert_eq!(registry.key_exists(&sibling), Ok(true));
        assert_eq!(registry.key_exists(&sibling.child("command")), Ok(true));
        assert_eq!(registry.delete_tree(&upper), Ok(()), "missing key deletion is idempotent");

        registry.rename_key(&sibling, "Renamed").expect("rename");
        assert_eq!(registry.key_exists(&sibling), Ok(false));
        let renamed = sibling.with_name("Renamed");
        assert_eq!(registry.key_exists(&renamed.child("command")), Ok(true));
        assert_eq!(
            registry.read_key(&renamed).expect("read").expect("exists").subkeys(),
            ["command"]
        );
        registry.create_key(&sibling).expect("create");
        assert_eq!(
            registry.rename_key(&renamed, "Verb2"),
            Err(RegistryError::new(RegistryOperation::Rename, RegistryErrorKind::AlreadyExists))
        );
    }
}
