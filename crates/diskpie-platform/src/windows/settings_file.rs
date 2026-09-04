//! Explicit retained-handle settings transport (ADR 0019).
//!
//! The adapter binds the exact `app.ron` leaf under a retained, non-reparse,
//! local settings directory. Reads use only the retained parent and target
//! handles; publication creates an exclusively held sibling in the same
//! directory, flushes it, and commits with handle-relative
//! `FileRenameInformationEx` POSIX semantics. Every native call is followed by
//! an identity classification; an uncertain commit preserves every complete
//! copy and is reported instead of being guessed from an error code.
//!
//! Public errors and read outcomes never contain a path or arbitrary text.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    os::windows::ffi::OsStringExt,
    path::Path,
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
};
use windows::Win32::{
    Foundation::{GENERIC_WRITE, HANDLE},
    Storage::FileSystem::{
        CREATE_NEW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_READ_ATTRIBUTES,
        FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FlushFileBuffers, OPEN_EXISTING, WriteFile,
    },
};

use super::diagnostic_files::{
    DirectoryGuard, ExportDestinationKind, InspectedFile, NativeFileIdentity, RenameBuffer,
    classify_export_destination, delete_file_handle, extended_native_path_units_with_nul,
    file_handle, file_size, inspect_handle, inspect_optional_file, native_paths_equal,
    normalize_absolute, open_native_file, os_units, read_bounded_bytes, valid_plain_file_name,
    validate_regular_non_reparse,
};

/// Exact leaf name of the application-owned settings document.
pub const SETTINGS_FILE_NAME: &str = "app.ron";

/// Hard read/write bound for the whole settings document (ADR 0013, 64 KiB).
pub const MAX_SETTINGS_DOCUMENT_BYTES: usize = 64 * 1_024;

/// Maximum number of sibling candidates attempted per publication.
const MAX_SETTINGS_TEMPORARIES: usize = 4;
const TEMP_CREATE_ATTEMPTS: u64 = 128;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Native operation that produced a sanitized settings error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SettingsFileOperation {
    OpenParent,
    InspectTarget,
    ReadTarget,
    CreateTemporary,
    WriteTemporary,
    FlushTemporary,
    VerifyTemporary,
    RevalidateTarget,
    ReplaceTarget,
    ClassifyResult,
}

/// Stable error category that is safe to show without a filesystem path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SettingsFileErrorKind {
    AccessDenied,
    NotFound,
    InvalidDestination,
    ReparsePoint,
    OwnedDestination,
    InvalidContent,
    TooLarge,
    AlreadyExists,
    PartialReplacement,
    Unsupported,
    Io,
}

/// Sanitized settings-file failure without a path or arbitrary text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingsFileError {
    pub operation: SettingsFileOperation,
    pub kind: SettingsFileErrorKind,
    pub os_code: Option<i32>,
}

impl SettingsFileError {
    const fn new(operation: SettingsFileOperation, kind: SettingsFileErrorKind) -> Self {
        Self { operation, kind, os_code: None }
    }

    #[cfg(test)]
    const fn with_os_code(mut self, os_code: i32) -> Self {
        self.os_code = Some(os_code);
        self
    }

    fn from_diagnostic(
        operation: SettingsFileOperation,
        error: &super::diagnostic_files::DiagnosticFileError,
    ) -> Self {
        let kind = match error.kind {
            super::diagnostic_files::DiagnosticFileErrorKind::AccessDenied => {
                SettingsFileErrorKind::AccessDenied
            }
            super::diagnostic_files::DiagnosticFileErrorKind::NotFound => {
                SettingsFileErrorKind::NotFound
            }
            super::diagnostic_files::DiagnosticFileErrorKind::InvalidDestination => {
                SettingsFileErrorKind::InvalidDestination
            }
            super::diagnostic_files::DiagnosticFileErrorKind::ReparsePoint => {
                SettingsFileErrorKind::ReparsePoint
            }
            super::diagnostic_files::DiagnosticFileErrorKind::OwnedDestination => {
                SettingsFileErrorKind::OwnedDestination
            }
            super::diagnostic_files::DiagnosticFileErrorKind::InvalidContent => {
                SettingsFileErrorKind::InvalidContent
            }
            super::diagnostic_files::DiagnosticFileErrorKind::TooLarge => {
                SettingsFileErrorKind::TooLarge
            }
            super::diagnostic_files::DiagnosticFileErrorKind::AlreadyExists => {
                SettingsFileErrorKind::AlreadyExists
            }
            super::diagnostic_files::DiagnosticFileErrorKind::PartialReplacement => {
                SettingsFileErrorKind::PartialReplacement
            }
            super::diagnostic_files::DiagnosticFileErrorKind::Unsupported => {
                SettingsFileErrorKind::Unsupported
            }
            super::diagnostic_files::DiagnosticFileErrorKind::Io => SettingsFileErrorKind::Io,
        };
        Self { operation, kind, os_code: error.os_code }
    }
}

impl fmt::Display for SettingsFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "settings file {:?} failed: {:?}", self.operation, self.kind)?;
        if let Some(code) = self.os_code {
            write!(formatter, " (OS code {code})")?;
        }
        Ok(())
    }
}

impl Error for SettingsFileError {}

/// Bounded outcome of a settings read over the retained parent handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettingsFileRead {
    /// The exact leaf was absent at read time.
    Missing,
    /// A complete document of at most [`MAX_SETTINGS_DOCUMENT_BYTES`].
    Document { bytes: Box<[u8]> },
    /// The retained leaf exceeds the document bound; no bytes are returned.
    Oversized { bytes: usize },
    /// The exact leaf could not be inspected safely. The static code is a
    /// stable diagnostic identifier, never a path or native message.
    Unavailable { code: &'static str },
}

/// Classified result of one atomic settings publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsCommitOutcome {
    /// The new document is visible at the exact target name.
    Committed { replaced_existing: bool, bytes_written: u64 },
    /// The namespace may have changed but identity could not be proven. Every
    /// complete copy is preserved; the next publication re-inspects the target
    /// through the retained parent before trusting any earlier identity.
    CommittedButUnverified,
}

#[derive(Clone)]
struct PreparedSettingsCandidate {
    path: Box<[u16]>,
    recovery_rename: RenameBuffer,
}

/// Retained-handle writer for the explicit application-owned settings file.
///
/// Construction opens and validates the settings directory and the current
/// target exactly once. Reads and commits never re-resolve a user-supplied
/// path: the parent handle bounds every operation and every rename supplies
/// the retained parent as `RootDirectory` with a plain relative name.
pub struct WindowsSettingsFile {
    parent: DirectoryGuard,
    target_name: Box<[u16]>,
    target_present: bool,
    /// Set when a rename succeeded but post-commit verification could not
    /// prove the result; the next commit must re-inspect before comparing.
    target_unverified: bool,
    target: Option<InspectedFile>,
    target_rename: RenameBuffer,
    create_rename: RenameBuffer,
    temporary_candidates: Box<[PreparedSettingsCandidate]>,
}

impl WindowsSettingsFile {
    /// Validates the exact `app.ron` leaf under an existing local directory.
    ///
    /// The parent must be a direct local non-reparse directory. An existing
    /// target is inspected through a retained handle and must be a regular,
    /// single-link, non-reparse file.
    pub fn prepare(directory: &Path) -> Result<Self, SettingsFileError> {
        let operation = SettingsFileOperation::OpenParent;
        if classify_export_destination(directory).map_err(|error| {
            SettingsFileError::from_diagnostic(SettingsFileOperation::InspectTarget, &error)
        })? != ExportDestinationKind::Local
        {
            return Err(SettingsFileError::new(
                operation,
                SettingsFileErrorKind::InvalidDestination,
            ));
        }
        let parent = DirectoryGuard::open(directory, op(operation))
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        if !parent.is_local() {
            return Err(SettingsFileError::new(
                operation,
                SettingsFileErrorKind::InvalidDestination,
            ));
        }
        if !valid_plain_file_name(OsStr::new(SETTINGS_FILE_NAME)) {
            return Err(SettingsFileError::new(
                operation,
                SettingsFileErrorKind::InvalidDestination,
            ));
        }
        let target_name = os_units(OsStr::new(SETTINGS_FILE_NAME));
        let target_rename = RenameBuffer::new(parent.handle(), &target_name, true)
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        let create_rename = RenameBuffer::new(parent.handle(), &target_name, false)
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;

        let (target_present, target) = inspect_settings_target(&parent, &target_name)?;

        let normalized_parent = normalize_absolute(directory).map_err(|error| {
            SettingsFileError::from_diagnostic(SettingsFileOperation::InspectTarget, &error)
        })?;
        let normalized_target =
            normalize_absolute(&directory.join(SETTINGS_FILE_NAME)).map_err(|error| {
                SettingsFileError::from_diagnostic(SettingsFileOperation::InspectTarget, &error)
            })?;
        let mut prepared = Vec::with_capacity(MAX_SETTINGS_TEMPORARIES);
        let start = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, AtomicOrdering::Relaxed);
        for offset in 0..TEMP_CREATE_ATTEMPTS {
            if prepared.len() == MAX_SETTINGS_TEMPORARIES {
                break;
            }
            let name = OsString::from(format!(
                ".diskpie-settings-{}-{}.tmp",
                std::process::id(),
                start.wrapping_add(offset)
            ));
            let candidate = directory.join(&name);
            let valid = classify_export_destination(&candidate).is_ok_and(|kind| {
                kind == ExportDestinationKind::Local
                    && normalize_absolute(candidate.parent().unwrap_or(Path::new(""))).is_ok_and(
                        |parent_units| native_paths_equal(&normalized_parent, &parent_units),
                    )
                    && normalize_absolute(&candidate).is_ok_and(|candidate_units| {
                        !native_paths_equal(&normalized_target, &candidate_units)
                    })
            }) && valid_plain_file_name(&name);
            if !valid {
                continue;
            }
            prepared.push(PreparedSettingsCandidate {
                path: extended_native_path_units_with_nul(&candidate)
                    .map_err(|error| {
                        SettingsFileError::from_diagnostic(
                            SettingsFileOperation::InspectTarget,
                            &error,
                        )
                    })?
                    .into_boxed_slice(),
                recovery_rename: RenameBuffer::new(parent.handle(), &os_units(&name), false)
                    .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?,
            });
        }
        if prepared.is_empty() {
            return Err(SettingsFileError::new(operation, SettingsFileErrorKind::Io));
        }
        Ok(Self {
            parent,
            target_name: target_name.into_boxed_slice(),
            target_present,
            target_unverified: false,
            target,
            target_rename,
            create_rename,
            temporary_candidates: prepared.into_boxed_slice(),
        })
    }

    /// Reads the bounded document through the retained target handle.
    pub fn read(&self) -> SettingsFileRead {
        let Some(target) = self.target.as_ref() else {
            return SettingsFileRead::Missing;
        };
        let size = match file_size(target.file(), op(SettingsFileOperation::ReadTarget)) {
            Ok(size) => size,
            Err(_error) => {
                return SettingsFileRead::Unavailable { code: "settings.storage.read_failed" };
            }
        };
        if size > MAX_SETTINGS_DOCUMENT_BYTES as u64 {
            return SettingsFileRead::Oversized { bytes: size as usize };
        }
        match read_bounded_bytes(
            target.file(),
            MAX_SETTINGS_DOCUMENT_BYTES,
            op(SettingsFileOperation::ReadTarget),
        ) {
            Ok(bytes) => SettingsFileRead::Document { bytes },
            Err(_error) => SettingsFileRead::Unavailable { code: "settings.storage.read_failed" },
        }
    }

    /// Atomically publishes one complete bounded document.
    ///
    /// An initially missing target is committed without replacement flags, so
    /// a concurrently created entry makes the rename fail atomically. An
    /// initially present target is committed with replace + POSIX semantics
    /// after its retained identity is revalidated.
    pub fn commit(&mut self, bytes: &[u8]) -> Result<SettingsCommitOutcome, SettingsFileError> {
        if bytes.is_empty() {
            return Err(SettingsFileError::new(
                SettingsFileOperation::WriteTemporary,
                SettingsFileErrorKind::InvalidContent,
            ));
        }
        if bytes.len() > MAX_SETTINGS_DOCUMENT_BYTES {
            return Err(SettingsFileError::new(
                SettingsFileOperation::WriteTemporary,
                SettingsFileErrorKind::TooLarge,
            ));
        }

        let mut last_busy = None;
        let candidates = self.temporary_candidates.clone();
        for temporary in &candidates {
            let sibling = match SettingsSibling::create(&temporary.path) {
                Ok(sibling) => sibling,
                Err(error) if error.kind == SettingsFileErrorKind::AlreadyExists => {
                    last_busy = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            return self.commit_sibling(sibling, temporary, bytes);
        }
        Err(last_busy.unwrap_or_else(|| {
            SettingsFileError::new(
                SettingsFileOperation::CreateTemporary,
                SettingsFileErrorKind::AlreadyExists,
            )
        }))
    }

    fn commit_sibling(
        &mut self,
        sibling: SettingsSibling,
        temporary: &PreparedSettingsCandidate,
        bytes: &[u8],
    ) -> Result<SettingsCommitOutcome, SettingsFileError> {
        if let Err(error) = sibling.write_all(bytes).and_then(|()| sibling.flush()) {
            let _ignored = sibling.delete_exact();
            return Err(error);
        }
        if let Err(error) = sibling.verify_complete(bytes.len() as u64, &self.parent) {
            let _ignored = sibling.delete_exact();
            return Err(error);
        }
        self.parent.ensure_location_stable(op(SettingsFileOperation::RevalidateTarget)).map_err(
            |error| {
                SettingsFileError::from_diagnostic(SettingsFileOperation::RevalidateTarget, &error)
            },
        )?;

        if self.target_unverified {
            // The previous publication changed the namespace without a proven
            // result (ADR 0019 item 8). Re-inspect through the retained parent
            // instead of comparing against an identity that is known stale.
            match inspect_settings_target(&self.parent, &self.target_name) {
                Ok((present, target)) => {
                    self.target_present = present;
                    self.target = target;
                    self.target_unverified = false;
                }
                Err(error) => {
                    let _ignored = sibling.delete_exact();
                    return Err(error);
                }
            }
        }

        let expected_target = self.target.as_ref().map(InspectedFile::identity);
        let current = match self.current_target_inspection(SettingsFileOperation::RevalidateTarget)
        {
            Ok(current) => current,
            Err(error) => {
                let _ignored = sibling.delete_exact();
                return Err(SettingsFileError {
                    operation: SettingsFileOperation::RevalidateTarget,
                    kind: SettingsFileErrorKind::Unsupported,
                    os_code: error.os_code,
                });
            }
        };
        match (expected_target, current.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(current)) if expected.same_file(&current.identity()) => {}
            (None, Some(_)) => {
                let _ignored = sibling.delete_exact();
                return Err(SettingsFileError::new(
                    SettingsFileOperation::RevalidateTarget,
                    SettingsFileErrorKind::AlreadyExists,
                ));
            }
            (Some(_), None) => {
                // The inspected document disappeared after preparation. There
                // is no prior file left to protect, so publish with create-only
                // semantics; a concurrent recreation still fails atomically.
                self.target_present = false;
                self.target = None;
            }
            _ => {
                let _ignored = sibling.delete_exact();
                return Err(SettingsFileError::new(
                    SettingsFileOperation::RevalidateTarget,
                    SettingsFileErrorKind::OwnedDestination,
                ));
            }
        }

        let replaced_existing = self.target_present;
        let expected_target = self.target.as_ref().map(InspectedFile::identity);
        let rename = if replaced_existing { &self.target_rename } else { &self.create_rename };
        if let Err(error) = rename.apply(sibling.handle()) {
            let error =
                SettingsFileError::from_diagnostic(SettingsFileOperation::ReplaceTarget, &error);
            return self.classify_failed_replace(sibling, temporary, error, expected_target);
        }

        // The namespace has changed. From here on no ordinary pre-commit error
        // may be returned (ADR 0019 item 8); an unprovable result is reported
        // as such and nothing is deleted.
        self.target_present = true;
        match self.verify_committed(sibling) {
            Ok(()) => {
                self.target_unverified = false;
                Ok(SettingsCommitOutcome::Committed {
                    replaced_existing,
                    bytes_written: bytes.len() as u64,
                })
            }
            Err(_classification) => {
                self.target = None;
                self.target_unverified = true;
                Ok(SettingsCommitOutcome::CommittedButUnverified)
            }
        }
    }

    fn classify_failed_replace(
        &mut self,
        sibling: SettingsSibling,
        temporary: &PreparedSettingsCandidate,
        error: SettingsFileError,
        expected_target: Option<NativeFileIdentity>,
    ) -> Result<SettingsCommitOutcome, SettingsFileError> {
        if error.kind == SettingsFileErrorKind::PartialReplacement {
            // Windows errors 1176/1177 leave the sibling as a complete copy.
            // Preserve it and report the uncertain outcome without guessing.
            let _ignored = temporary.recovery_rename.apply(sibling.handle());
            return Ok(SettingsCommitOutcome::CommittedButUnverified);
        }
        let current = match self.current_target_inspection(SettingsFileOperation::ClassifyResult) {
            Ok(current) => current,
            Err(_classification_error) => {
                return Ok(SettingsCommitOutcome::CommittedButUnverified);
            }
        };
        match (expected_target, current.as_ref()) {
            (Some(expected), Some(current)) if expected.same_file(&current.identity()) => {
                let _ignored = sibling.delete_exact();
                Err(error)
            }
            (None, None) => {
                let _ignored = sibling.delete_exact();
                Err(error)
            }
            (_, Some(current)) if current.identity().same_file(&sibling.identity()) => {
                self.target_present = true;
                Ok(SettingsCommitOutcome::Committed { replaced_existing: true, bytes_written: 0 })
            }
            _ => Ok(SettingsCommitOutcome::CommittedButUnverified),
        }
    }

    fn verify_committed(&mut self, sibling: SettingsSibling) -> Result<(), SettingsFileError> {
        self.parent.ensure_location_stable(op(SettingsFileOperation::ClassifyResult)).map_err(
            |error| SettingsFileError {
                operation: SettingsFileOperation::ClassifyResult,
                kind: SettingsFileErrorKind::Unsupported,
                os_code: error.os_code,
            },
        )?;
        let Some(current) = self
            .current_target_inspection(SettingsFileOperation::ClassifyResult)
            .map_err(|error| SettingsFileError {
            operation: SettingsFileOperation::ClassifyResult,
            kind: SettingsFileErrorKind::Unsupported,
            os_code: error.os_code,
        })?
        else {
            return Err(SettingsFileError::new(
                SettingsFileOperation::ClassifyResult,
                SettingsFileErrorKind::InvalidDestination,
            ));
        };
        let name_matches = self
            .parent
            .direct_child_name(sibling.handle(), op(SettingsFileOperation::ClassifyResult))
            .is_ok_and(|name| native_paths_equal(&name, &self.target_name));
        if !current.identity().same_file(&sibling.identity()) || !name_matches {
            return Err(SettingsFileError::new(
                SettingsFileOperation::ClassifyResult,
                SettingsFileErrorKind::InvalidDestination,
            ));
        }

        // The commit is proven. The sibling handle denies every competing
        // open, so release it before retaining a readable target handle for
        // later `read` calls. If the readable reopen no longer shows the
        // committed identity, keep the attribute-only inspection: identity
        // tracking for the next commit stays exact and reads report
        // unavailability instead of foreign bytes.
        let committed = sibling.identity();
        drop(sibling);
        self.target = match inspect_settings_target(&self.parent, &self.target_name) {
            Ok((true, Some(readable))) if readable.identity().same_file(&committed) => {
                Some(readable)
            }
            _ => Some(current),
        };
        Ok(())
    }

    fn current_target_inspection(
        &self,
        operation: SettingsFileOperation,
    ) -> Result<Option<InspectedFile>, super::diagnostic_files::DiagnosticFileError> {
        let path =
            self.parent.current_child_path(OsString::from_wide(&self.target_name).as_os_str())?;
        inspect_optional_file(
            &path,
            op(operation),
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )
    }
}

impl fmt::Debug for WindowsSettingsFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsSettingsFile")
            .field("local_parent_bound", &true)
            .field("target_present", &self.target_present)
            .field("target_unverified", &self.target_unverified)
            .field("temporary_candidate_count", &self.temporary_candidates.len())
            .finish()
    }
}

fn op(operation: SettingsFileOperation) -> super::diagnostic_files::DiagnosticFileOperation {
    match operation {
        SettingsFileOperation::OpenParent => {
            super::diagnostic_files::DiagnosticFileOperation::OpenLog
        }
        SettingsFileOperation::InspectTarget => {
            super::diagnostic_files::DiagnosticFileOperation::InspectDestination
        }
        SettingsFileOperation::ReadTarget => {
            super::diagnostic_files::DiagnosticFileOperation::ReadLogDirectory
        }
        SettingsFileOperation::CreateTemporary => {
            super::diagnostic_files::DiagnosticFileOperation::CreateTemporary
        }
        SettingsFileOperation::WriteTemporary => {
            super::diagnostic_files::DiagnosticFileOperation::WriteTemporary
        }
        SettingsFileOperation::FlushTemporary => {
            super::diagnostic_files::DiagnosticFileOperation::FlushTemporary
        }
        SettingsFileOperation::VerifyTemporary => {
            super::diagnostic_files::DiagnosticFileOperation::InspectDestination
        }
        SettingsFileOperation::RevalidateTarget => {
            super::diagnostic_files::DiagnosticFileOperation::InspectDestination
        }
        SettingsFileOperation::ReplaceTarget => {
            super::diagnostic_files::DiagnosticFileOperation::ReplaceDestination
        }
        SettingsFileOperation::ClassifyResult => {
            super::diagnostic_files::DiagnosticFileOperation::ReplaceDestination
        }
    }
}

fn inspect_settings_target(
    parent: &DirectoryGuard,
    target_name: &[u16],
) -> Result<(bool, Option<InspectedFile>), SettingsFileError> {
    let operation = SettingsFileOperation::InspectTarget;
    let path = parent
        .current_child_path(OsString::from_wide(target_name).as_os_str())
        .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
    let raw = extended_native_path_units_with_nul(&path)
        .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
    // FILE_FLAG_BACKUP_SEMANTICS lets a directory-shaped entry open so the
    // attribute check below rejects it as an invalid destination instead of
    // surfacing the generic access-denied code CreateFileW returns otherwise.
    let file = match open_native_file(
        &raw,
        FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        op(operation),
    ) {
        Ok(file) => file,
        Err(error) if error.kind == super::diagnostic_files::DiagnosticFileErrorKind::NotFound => {
            return Ok((false, None));
        }
        Err(error) => return Err(SettingsFileError::from_diagnostic(operation, &error)),
    };
    let info = inspect_handle(&file, op(operation))
        .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
    validate_regular_non_reparse(&info, op(operation))
        .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
    if info.link_count() != 1 {
        return Err(SettingsFileError::new(operation, SettingsFileErrorKind::OwnedDestination));
    }
    parent
        .validate_direct_child_handle(file_handle(&file), op(operation))
        .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
    Ok((true, Some(InspectedFile::new(file, info.identity()))))
}

struct SettingsSibling {
    file: File,
    identity: NativeFileIdentity,
}

impl SettingsSibling {
    fn create(path: &[u16]) -> Result<Self, SettingsFileError> {
        let operation = SettingsFileOperation::CreateTemporary;
        let file = open_native_file(
            path,
            GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | DELETE.0,
            FILE_SHARE_NONE,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
            op(operation),
        )
        .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        let info = inspect_handle(&file, op(operation))
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        validate_regular_non_reparse(&info, op(operation))
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        if info.link_count() != 1 {
            let _ignored = delete_file_handle(&file, op(operation));
            return Err(SettingsFileError::new(operation, SettingsFileErrorKind::Unsupported));
        }
        Ok(Self { file, identity: info.identity() })
    }

    fn handle(&self) -> HANDLE {
        file_handle(&self.file)
    }

    fn identity(&self) -> NativeFileIdentity {
        self.identity
    }

    fn write_all(&self, mut contents: &[u8]) -> Result<(), SettingsFileError> {
        let operation = SettingsFileOperation::WriteTemporary;
        while !contents.is_empty() {
            let mut written = 0_u32;
            // SAFETY: the owned handle remains valid, `contents` is a live
            // bounded slice, `written` is an exact synchronous output slot,
            // and no OVERLAPPED pointer is used.
            unsafe { WriteFile(self.handle(), Some(contents), Some(&raw mut written), None) }
                .map_err(|error| {
                    SettingsFileError::from_diagnostic(
                        operation,
                        &super::diagnostic_files::DiagnosticFileError::from_windows(
                            super::diagnostic_files::DiagnosticFileOperation::WriteTemporary,
                            &error,
                        ),
                    )
                })?;
            if written == 0 || written as usize > contents.len() {
                return Err(SettingsFileError::new(operation, SettingsFileErrorKind::Io));
            }
            contents = &contents[written as usize..];
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), SettingsFileError> {
        // SAFETY: the handle remains owned and open for this synchronous call.
        unsafe { FlushFileBuffers(self.handle()) }.map_err(|error| {
            SettingsFileError::from_diagnostic(
                SettingsFileOperation::FlushTemporary,
                &super::diagnostic_files::DiagnosticFileError::from_windows(
                    super::diagnostic_files::DiagnosticFileOperation::FlushTemporary,
                    &error,
                ),
            )
        })
    }

    fn verify_complete(
        &self,
        expected_size: u64,
        parent: &DirectoryGuard,
    ) -> Result<(), SettingsFileError> {
        let operation = SettingsFileOperation::VerifyTemporary;
        let info = inspect_handle(&self.file, op(operation))
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        validate_regular_non_reparse(&info, op(operation))
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        if !self.identity.same_file(&info.identity())
            || info.link_count() != 1
            || file_size(&self.file, op(operation))
                .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?
                != expected_size
        {
            return Err(SettingsFileError::new(operation, SettingsFileErrorKind::InvalidContent));
        }
        parent
            .validate_direct_child_handle(self.handle(), op(operation))
            .map_err(|error| SettingsFileError::from_diagnostic(operation, &error))?;
        Ok(())
    }

    fn delete_exact(self) -> Result<(), SettingsFileError> {
        delete_file_handle(&self.file, op(SettingsFileOperation::CreateTemporary)).map_err(
            |error| {
                SettingsFileError::from_diagnostic(SettingsFileOperation::CreateTemporary, &error)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::windows::fs::symlink_file,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diskpie-settings-{label}-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn document(bytes: &[u8]) -> Box<[u8]> {
        bytes.to_vec().into_boxed_slice()
    }

    #[test]
    fn read_missing_and_commit_creates_atomically() {
        let directory = TestDirectory::new("create");
        let mut file = WindowsSettingsFile::prepare(directory.path()).expect("prepare empty dir");
        assert_eq!(file.read(), SettingsFileRead::Missing);

        let contents = document(b"(schema_version:1,settings:())");
        let outcome = file.commit(&contents).expect("commit first document");
        assert!(matches!(
            outcome,
            SettingsCommitOutcome::Committed { replaced_existing: false, .. }
        ));
        if let SettingsCommitOutcome::Committed { bytes_written, .. } = outcome {
            assert_eq!(bytes_written, contents.len() as u64);
        }
        assert_eq!(
            fs::read(directory.path().join(SETTINGS_FILE_NAME)).expect("read target"),
            contents.as_ref()
        );
        assert_eq!(file.read(), SettingsFileRead::Document { bytes: contents });
    }

    #[test]
    fn commit_replaces_an_existing_owned_document() {
        let directory = TestDirectory::new("replace");
        let first = document(b"(schema_version:1,settings:(ui_scale:Some(1.25)))");
        fs::write(directory.path().join(SETTINGS_FILE_NAME), first.as_ref())
            .expect("write initial document");

        let mut file = WindowsSettingsFile::prepare(directory.path()).expect("prepare existing");
        assert_eq!(file.read(), SettingsFileRead::Document { bytes: first });

        let second = document(b"(schema_version:1,settings:(theme:Dark))");
        let outcome = file.commit(&second).expect("commit replacement");
        assert!(matches!(
            outcome,
            SettingsCommitOutcome::Committed { replaced_existing: true, .. }
        ));
        assert_eq!(
            fs::read(directory.path().join(SETTINGS_FILE_NAME)).expect("read replaced target"),
            second.as_ref()
        );
    }

    #[test]
    fn repeated_commits_track_replacement_state_and_leave_no_sibling() {
        let directory = TestDirectory::new("repeat");
        let mut file = WindowsSettingsFile::prepare(directory.path()).expect("prepare empty dir");

        let first = document(b"(schema_version:1,settings:())");
        assert!(matches!(
            file.commit(&first).expect("first commit"),
            SettingsCommitOutcome::Committed { replaced_existing: false, .. }
        ));
        let second = document(b"(schema_version:1,settings:(theme:Light))");
        assert!(matches!(
            file.commit(&second).expect("second commit"),
            SettingsCommitOutcome::Committed { replaced_existing: true, .. }
        ));
        let third = document(b"(schema_version:1,settings:(theme:Dark))");
        assert!(matches!(
            file.commit(&third).expect("third commit"),
            SettingsCommitOutcome::Committed { replaced_existing: true, .. }
        ));

        assert_eq!(
            fs::read(directory.path().join(SETTINGS_FILE_NAME)).expect("read final target"),
            third.as_ref()
        );
        let leftovers = fs::read_dir(directory.path())
            .expect("list settings directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".diskpie-"))
            .count();
        assert_eq!(leftovers, 0, "every sibling must be consumed by the rename");
    }

    #[test]
    fn inspected_target_that_disappears_is_recreated_not_a_permanent_failure() {
        let directory = TestDirectory::new("vanished");
        let initial = document(b"(schema_version:1,settings:(ui_scale:Some(1.0)))");
        let target = directory.path().join(SETTINGS_FILE_NAME);
        fs::write(&target, initial.as_ref()).expect("write initial document");
        let mut file = WindowsSettingsFile::prepare(directory.path()).expect("prepare existing");
        assert_eq!(file.read(), SettingsFileRead::Document { bytes: initial });

        // The retained target handle shares delete access, so an external
        // deletion succeeds while the adapter still holds its inspection.
        fs::remove_file(&target).expect("delete inspected target");

        let replacement = document(b"(schema_version:1,settings:(theme:Dark))");
        assert!(matches!(
            file.commit(&replacement).expect("publish after external deletion"),
            SettingsCommitOutcome::Committed { replaced_existing: false, .. }
        ));
        assert_eq!(fs::read(&target).expect("read recreated target"), replacement.as_ref());

        let again = document(b"(schema_version:1,settings:(theme:Light))");
        assert!(matches!(
            file.commit(&again).expect("publish again"),
            SettingsCommitOutcome::Committed { replaced_existing: true, .. }
        ));
        assert_eq!(fs::read(&target).expect("read second replacement"), again.as_ref());
    }

    #[test]
    fn oversized_document_is_reported_without_reading_payload() {
        let directory = TestDirectory::new("oversized");
        let oversized = vec![b'x'; MAX_SETTINGS_DOCUMENT_BYTES + 1];
        fs::write(directory.path().join(SETTINGS_FILE_NAME), &oversized).expect("write oversized");

        let file = WindowsSettingsFile::prepare(directory.path()).expect("prepare oversized");
        assert_eq!(
            file.read(),
            SettingsFileRead::Oversized { bytes: MAX_SETTINGS_DOCUMENT_BYTES + 1 }
        );
    }

    #[test]
    fn commit_rejects_oversized_and_empty_payloads() {
        let directory = TestDirectory::new("reject-payload");
        let mut file = WindowsSettingsFile::prepare(directory.path()).expect("prepare dir");
        assert_eq!(
            file.commit(&[]).expect_err("empty document"),
            SettingsFileError::new(
                SettingsFileOperation::WriteTemporary,
                SettingsFileErrorKind::InvalidContent
            )
        );
        let oversized = vec![b'x'; MAX_SETTINGS_DOCUMENT_BYTES + 1];
        assert_eq!(
            file.commit(&oversized).expect_err("oversized document"),
            SettingsFileError::new(
                SettingsFileOperation::WriteTemporary,
                SettingsFileErrorKind::TooLarge
            )
        );
        assert_eq!(file.read(), SettingsFileRead::Missing);
    }

    #[test]
    fn initially_missing_target_that_appears_is_a_conflict() {
        let directory = TestDirectory::new("appears-race");
        let mut file = WindowsSettingsFile::prepare(directory.path()).expect("prepare missing");
        assert_eq!(file.read(), SettingsFileRead::Missing);

        let foreign = document(b"foreign");
        fs::write(directory.path().join(SETTINGS_FILE_NAME), foreign.as_ref())
            .expect("concurrent creator wins");
        let error = file
            .commit(&document(b"(schema_version:1,settings:())"))
            .expect_err("create-only rename must fail");
        assert_eq!(error.kind, SettingsFileErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(directory.path().join(SETTINGS_FILE_NAME)).expect("foreign target intact"),
            foreign.as_ref()
        );
    }

    #[test]
    fn reparse_target_is_rejected_without_touching_its_target() {
        let directory = TestDirectory::new("reparse-target");
        let sentinel = directory.path().join("sentinel.txt");
        let target = directory.path().join(SETTINGS_FILE_NAME);
        fs::write(&sentinel, b"must remain").expect("write sentinel");
        match symlink_file(&sentinel, &target) {
            Ok(()) => {}
            Err(error) if matches!(error.raw_os_error(), Some(5 | 1_314)) => return,
            Err(error) => panic!("create symlink fixture: {error}"),
        }

        let error = WindowsSettingsFile::prepare(directory.path())
            .expect_err("reparse target must fail closed");
        assert_eq!(error.kind, SettingsFileErrorKind::ReparsePoint);
        assert_eq!(fs::read(&sentinel).expect("sentinel survives"), b"must remain");
    }

    #[test]
    fn hard_linked_target_is_rejected() {
        let directory = TestDirectory::new("hardlink-target");
        let target = directory.path().join(SETTINGS_FILE_NAME);
        let alias = directory.path().join("alias.ron");
        fs::write(&target, b"private").expect("write target");
        fs::hard_link(&target, &alias).expect("create hard-link fixture");

        let error = WindowsSettingsFile::prepare(directory.path())
            .expect_err("multiply-linked target must fail closed");
        assert_eq!(error.kind, SettingsFileErrorKind::OwnedDestination);
        assert_eq!(fs::read(&alias).expect("alias survives"), b"private");
    }

    #[test]
    fn directory_target_is_rejected() {
        let directory = TestDirectory::new("directory-target");
        fs::create_dir(directory.path().join(SETTINGS_FILE_NAME))
            .expect("create directory-shaped target");

        let error = WindowsSettingsFile::prepare(directory.path())
            .expect_err("directory target must fail closed");
        assert!(matches!(
            error.kind,
            SettingsFileErrorKind::InvalidDestination | SettingsFileErrorKind::OwnedDestination
        ));
    }

    #[test]
    fn error_display_never_contains_a_path() {
        let error = SettingsFileError::new(
            SettingsFileOperation::InspectTarget,
            SettingsFileErrorKind::ReparsePoint,
        )
        .with_os_code(5);
        let rendered = format!("{error}");
        assert!(rendered.contains("InspectTarget"));
        assert!(!rendered.contains(r"C:\"));
        assert!(!rendered.contains("app.ron"));
    }

    #[test]
    fn committed_outcome_is_debug_and_eq_safe() {
        let committed =
            SettingsCommitOutcome::Committed { replaced_existing: true, bytes_written: 12 };
        assert_eq!(committed, committed);
        let unverified = SettingsCommitOutcome::CommittedButUnverified;
        assert_ne!(committed, unverified);
    }
}
