//! Narrow Windows filesystem operations for local diagnostics.
//!
//! The adapter owns three security-sensitive details from ADR 0016:
//! exact-name log retention, native destination classification, and durable
//! sibling-file replacement. Public errors intentionally omit every path.

use std::{
    cmp::Ordering,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{self, Write},
    mem::{offset_of, size_of, size_of_val},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};
use windows::{
    Wdk::Storage::FileSystem::{FileRenameInformationEx, NtSetInformationFile},
    Win32::{
        Foundation::{
            ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
            ERROR_NOT_SUPPORTED, ERROR_UNABLE_TO_MOVE_REPLACEMENT,
            ERROR_UNABLE_TO_MOVE_REPLACEMENT_2, ERROR_UNABLE_TO_REMOVE_REPLACED, GENERIC_WRITE,
            HANDLE, RtlNtStatusToDosError,
        },
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, DELETE, FILE_APPEND_DATA,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_TAG_INFO, FILE_CREATION_DISPOSITION, FILE_DISPOSITION_INFO,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
            FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY,
            FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO,
            FILE_RENAME_INFO_0, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_NONE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileDispositionInfo,
            FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FlushFileBuffers,
            GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileSizeEx,
            GetFinalPathNameByHandleW, GetFullPathNameW, OPEN_EXISTING, ReadFile,
            SetFileInformationByHandle, WriteFile,
        },
        System::{IO::IO_STATUS_BLOCK, SystemInformation::GetSystemTime},
    },
    core::PCWSTR,
};

/// Stable documented `FileRenameInformationEx` flag values (ntifs.h). They
/// are plain integers in the windows crate's `WindowsProgramming` module; the
/// values are defined locally so the adapter does not enable that feature.
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

/// Maximum number of exact daily log files retained by DiskPie.
pub const MAX_RETAINED_LOG_FILES: usize = 8;

/// Hard maximum for one active UTC daily log file.
pub const MAX_DAILY_LOG_BYTES: u64 = 16 * 1_024 * 1_024;

/// Maximum size of a user-created plain-text diagnostic export.
pub const MAX_DIAGNOSTIC_EXPORT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum payload accepted by the allocation-free native panic-marker writer.
pub const MAX_PANIC_MARKER_BYTES: usize = 4 * 1024;

const LOG_PREFIX: &str = "diskpie.";
const LOG_SUFFIX: &str = ".log";
const LOG_NAME_LENGTH: usize = 22;
const MAX_RECORDED_RETENTION_ISSUES: usize = 16;
const TEMP_CREATE_ATTEMPTS: u64 = 128;
const MAX_PANIC_MARKER_TEMPORARIES: usize = 4;
const MAX_COMMIT_RECORD_CLAIM_ATTEMPTS: u8 = 8;
const MAX_NATIVE_PATH_UNITS: usize = 32_768;
const DIRECTORY_QUERY_BUFFER_BYTES: usize = 32 * 1_024;
const MAX_DIRECTORY_RECORDS_PER_QUERY: usize = 256;
const MAX_DIRECTORY_ENTRIES_PER_CYCLE: u64 = 65_536;
const MAX_ACTIVE_LOOKUP_QUERIES: usize = 256;
const MAX_SYNCHRONOUS_RETENTION_STEPS: usize = 512;
const MAX_RETENTION_PROGRESS_PASSES: u8 = 2;
const ACTIVE_RETENTION_SLOTS: usize = MAX_RETAINED_LOG_FILES - 1;
const RETENTION_CONTINUATION_DELAY: Duration = Duration::from_millis(10);
const RETENTION_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Operation that produced a path-redacted diagnostic filesystem error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiagnosticFileOperation {
    OpenLog,
    WriteLog,
    ReadLogDirectory,
    InspectLogEntry,
    RemoveOldLog,
    NormalizeDestination,
    InspectDestination,
    InspectOwnedPath,
    CreateTemporary,
    WriteTemporary,
    FlushTemporary,
    ReplaceDestination,
    RemoveDestination,
}

/// Stable error category that is safe to show without a filesystem path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiagnosticFileErrorKind {
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

/// Sanitized diagnostic-file failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticFileError {
    pub operation: DiagnosticFileOperation,
    pub kind: DiagnosticFileErrorKind,
    pub os_code: Option<i32>,
}

impl DiagnosticFileError {
    const fn new(operation: DiagnosticFileOperation, kind: DiagnosticFileErrorKind) -> Self {
        Self { operation, kind, os_code: None }
    }

    fn from_io(operation: DiagnosticFileOperation, error: &io::Error) -> Self {
        Self { operation, kind: classify_io_error(error), os_code: error.raw_os_error() }
    }

    pub(super) fn from_windows(
        operation: DiagnosticFileOperation,
        error: &windows::core::Error,
    ) -> Self {
        Self::from_os_code(operation, windows_os_code(error))
    }

    /// Classifies a raw Win32 error code, including one translated from an
    /// `NTSTATUS` by `RtlNtStatusToDosError`.
    pub(super) fn from_os_code(operation: DiagnosticFileOperation, os_code: i32) -> Self {
        let io_error = io::Error::from_raw_os_error(os_code);
        let kind = if is_partial_replace_code(os_code) {
            DiagnosticFileErrorKind::PartialReplacement
        } else {
            classify_io_error(&io_error)
        };
        Self { operation, kind, os_code: Some(os_code) }
    }
}

impl fmt::Display for DiagnosticFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "diagnostic file {:?} failed: {:?}", self.operation, self.kind)?;
        if let Some(code) = self.os_code {
            write!(formatter, " (OS code {code})")?;
        }
        Ok(())
    }
}

impl Error for DiagnosticFileError {}

/// Bounded summary of a retention pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticRetentionReport {
    pub exact_files_seen: u64,
    pub files_removed: u64,
    pub issue_count: u64,
    pub issues: Vec<DiagnosticFileError>,
}

impl DiagnosticRetentionReport {
    fn record_issue(&mut self, issue: DiagnosticFileError) {
        self.issue_count = self.issue_count.saturating_add(1);
        if self.issues.len() < MAX_RECORDED_RETENTION_ISSUES {
            self.issues.push(issue);
        }
    }
}

/// Filesystem transport implied by a user-selected export path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExportDestinationKind {
    Local,
    Unc,
}

/// Result of a completed atomic export write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportWriteOutcome {
    pub destination_kind: ExportDestinationKind,
    pub replaced_existing: bool,
    pub bytes_written: u64,
}

/// Precomputed Windows paths for a bounded native panic-marker replacement.
///
/// Construction performs normalization and allocation before the panic hook
/// is installed. [`Self::replace`] uses only these fixed UTF-16 buffers, a
/// stack counter, and direct synchronous Win32 calls. It promises atomic
/// namespace visibility, but deliberately does not claim power-loss durability
/// for the directory entry because handle-relative rename documents no such
/// guarantee.
pub struct WindowsAtomicMarkerWriter {
    parent: DirectoryGuard,
    target_rename: RenameBuffer,
    temporary_candidates: Box<[PreparedMarkerCandidate]>,
}

struct PreparedMarkerCandidate {
    path: Box<[u16]>,
    recovery_rename: RenameBuffer,
}

impl WindowsAtomicMarkerWriter {
    /// Validates and precomputes one target and up to four sibling candidates.
    pub fn prepare(
        target: &Path,
        temporary_candidates: &[PathBuf],
    ) -> Result<Self, DiagnosticFileError> {
        let Some(target_name) = target.file_name() else {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        };
        if temporary_candidates.is_empty()
            || temporary_candidates.len() > MAX_PANIC_MARKER_TEMPORARIES
            || !valid_plain_file_name(target_name)
        {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        if classify_export_destination(target)? != ExportDestinationKind::Local {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        let target_parent = target.parent().ok_or_else(|| {
            DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            )
        })?;
        let normalized_target = normalize_absolute(target)?;
        let normalized_parent = normalize_absolute(target_parent)?;
        let parent =
            DirectoryGuard::open(target_parent, DiagnosticFileOperation::InspectDestination)?;
        if parent.transport != ExportDestinationKind::Local {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        let target_units = os_units(target_name);
        let target_rename = RenameBuffer::new(parent.handle(), &target_units, true)?;
        let current_target = parent.current_child_path(target_name)?;
        let _validated_target = inspect_optional_file(
            &current_target,
            DiagnosticFileOperation::InspectDestination,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )?;
        let mut prepared = Vec::with_capacity(temporary_candidates.len());
        for candidate in temporary_candidates {
            if classify_export_destination(candidate)? != ExportDestinationKind::Local {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::NormalizeDestination,
                    DiagnosticFileErrorKind::InvalidDestination,
                ));
            }
            let candidate_parent = candidate.parent().ok_or_else(|| {
                DiagnosticFileError::new(
                    DiagnosticFileOperation::NormalizeDestination,
                    DiagnosticFileErrorKind::InvalidDestination,
                )
            })?;
            let Some(candidate_name) = candidate.file_name() else {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::NormalizeDestination,
                    DiagnosticFileErrorKind::InvalidDestination,
                ));
            };
            if !valid_plain_file_name(candidate_name)
                || !native_paths_equal(&normalized_parent, &normalize_absolute(candidate_parent)?)
                || native_paths_equal(&normalized_target, &normalize_absolute(candidate)?)
            {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::NormalizeDestination,
                    DiagnosticFileErrorKind::InvalidDestination,
                ));
            }
            prepared.push(PreparedMarkerCandidate {
                path: extended_native_path_units_with_nul(candidate)?.into_boxed_slice(),
                recovery_rename: RenameBuffer::new(
                    parent.handle(),
                    &os_units(candidate_name),
                    false,
                )?,
            });
        }
        Ok(Self { parent, target_rename, temporary_candidates: prepared.into_boxed_slice() })
    }

    /// Atomically replaces the marker using a fixed temporary sibling.
    ///
    /// The caller remains responsible for validating the application-specific
    /// marker schema. This layer independently enforces a nonempty ASCII
    /// payload of at most 4 KiB before touching the filesystem.
    pub fn replace(&self, contents: &[u8]) -> Result<(), DiagnosticFileError> {
        if contents.is_empty() || contents.len() > MAX_PANIC_MARKER_BYTES || !contents.is_ascii() {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::WriteTemporary,
                if contents.len() > MAX_PANIC_MARKER_BYTES {
                    DiagnosticFileErrorKind::TooLarge
                } else {
                    DiagnosticFileErrorKind::InvalidContent
                },
            ));
        }

        let mut last_busy = None;
        for temporary in &self.temporary_candidates {
            let file = match NativeMarkerFile::create(&temporary.path) {
                Ok(file) => file,
                Err(error) if error.kind == DiagnosticFileErrorKind::AlreadyExists => {
                    last_busy = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = self.parent.validate_direct_child_handle(
                file.handle(),
                DiagnosticFileOperation::CreateTemporary,
            ) {
                let _ignored = file.delete_exact();
                return Err(error);
            }
            let result = write_native_marker(
                file,
                &self.parent,
                &self.target_rename,
                &temporary.recovery_rename,
                contents,
            );
            if result.is_ok() {
                return result;
            }
            return result;
        }
        Err(last_busy.unwrap_or_else(|| {
            DiagnosticFileError::new(
                DiagnosticFileOperation::CreateTemporary,
                DiagnosticFileErrorKind::AlreadyExists,
            )
        }))
    }
}

impl fmt::Debug for WindowsAtomicMarkerWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsAtomicMarkerWriter")
            .field("local_parent_bound", &true)
            .field("temporary_candidate_count", &self.temporary_candidates.len())
            .finish()
    }
}

/// Bounded view of one fixed marker slot captured while the session was
/// prepared. The native identity behind a present slot stays private to the
/// session so later operations can only be identity-conditional.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerSlotState<'a> {
    /// The exact name was absent at preparation time.
    Missing,
    /// A regular, single-link, non-reparse direct child holding at most
    /// [`MAX_PANIC_MARKER_BYTES`] ASCII bytes.
    Present(&'a [u8]),
    /// The slot could not be inspected or read safely; it is never touched.
    Unavailable(DiagnosticFileError),
}

/// Classified result of one panic-hook commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerCommitStatus {
    /// The new marker is proven visible at the target name.
    Committed,
    /// Nothing at the target name changed; a sibling created by this call was
    /// removed through its own handle before returning.
    NotCommitted(DiagnosticFileError),
    /// The namespace may have changed but the result could not be proven.
    /// Every known copy, including a preserved sibling, is left untouched.
    CommittedButUnverified(DiagnosticFileError),
}

/// Mutation requested for a marker still owned by the current session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerReconcileAction<'a> {
    /// Replace the session marker with the previous marker bytes.
    Restore(&'a [u8]),
    /// Remove the session marker.
    Delete,
}

/// Classified result of an identity-conditional reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerReconcileOutcome {
    RestoredPrevious,
    RemovedSessionMarker,
    /// The target no longer carries the identity this session committed, or
    /// its bytes failed the caller's ownership check. Nothing was touched.
    OwnershipLost,
    /// The target name was absent. Nothing was touched.
    MarkerMissing,
    /// The outcome could not be proven from post-operation identities, or the
    /// session's own commit record is not a single verified commit. Every
    /// known copy is preserved.
    PreservedUncertain,
}

const RECORD_NONE: u8 = 0;
const RECORD_RECORDING: u8 = 1;
const RECORD_COMMITTED: u8 = 2;
const RECORD_UNVERIFIED: u8 = 3;

/// Lock-free record of the identity this session committed at the target.
///
/// The panic hook may run on any thread and must not wait. A writer claims
/// the record with one compare-exchange; a concurrent claim marks the record
/// contended so shutdown treats it as uncertain instead of trusting a torn
/// identity. `Unverified` is sticky: once a commit could not be proven, no
/// later commit can re-enable reconciliation.
struct CommitRecord {
    phase: AtomicU8,
    contended: AtomicBool,
    legacy_volume: AtomicU64,
    legacy_file_id: AtomicU64,
    extended_present: AtomicBool,
    extended_volume: AtomicU64,
    extended_low: AtomicU64,
    extended_high: AtomicU64,
}

impl CommitRecord {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(RECORD_NONE),
            contended: AtomicBool::new(false),
            legacy_volume: AtomicU64::new(0),
            legacy_file_id: AtomicU64::new(0),
            extended_present: AtomicBool::new(false),
            extended_volume: AtomicU64::new(0),
            extended_low: AtomicU64::new(0),
            extended_high: AtomicU64::new(0),
        }
    }

    fn record_committed(&self, identity: NativeFileIdentity) {
        self.record(RECORD_COMMITTED, Some(identity));
    }

    fn record_unverified(&self) {
        self.record(RECORD_UNVERIFIED, None);
    }

    fn record(&self, phase: u8, identity: Option<NativeFileIdentity>) {
        let mut observed = self.phase.load(AtomicOrdering::Acquire);
        let mut attempts = 0_u8;
        loop {
            if observed == RECORD_RECORDING || attempts >= MAX_COMMIT_RECORD_CLAIM_ATTEMPTS {
                self.contended.store(true, AtomicOrdering::Release);
                return;
            }
            match self.phase.compare_exchange(
                observed,
                RECORD_RECORDING,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_previous) => break,
                Err(current) => {
                    observed = current;
                    attempts = attempts.saturating_add(1);
                }
            }
        }
        let final_phase = if observed == RECORD_UNVERIFIED { RECORD_UNVERIFIED } else { phase };
        if final_phase == RECORD_COMMITTED
            && let Some(identity) = identity
        {
            self.legacy_volume.store(u64::from(identity.legacy_volume), AtomicOrdering::Relaxed);
            self.legacy_file_id.store(identity.legacy_file_id, AtomicOrdering::Relaxed);
            match identity.extended {
                Some((volume, id)) => {
                    let mut low = [0_u8; 8];
                    let mut high = [0_u8; 8];
                    low.copy_from_slice(&id[..8]);
                    high.copy_from_slice(&id[8..]);
                    self.extended_volume.store(volume, AtomicOrdering::Relaxed);
                    self.extended_low.store(u64::from_ne_bytes(low), AtomicOrdering::Relaxed);
                    self.extended_high.store(u64::from_ne_bytes(high), AtomicOrdering::Relaxed);
                    self.extended_present.store(true, AtomicOrdering::Relaxed);
                }
                None => self.extended_present.store(false, AtomicOrdering::Relaxed),
            }
        }
        self.phase.store(final_phase, AtomicOrdering::Release);
    }

    /// Returns the committed identity only for a single, uncontended,
    /// verified commit.
    fn committed_identity(&self) -> Option<NativeFileIdentity> {
        if self.phase.load(AtomicOrdering::Acquire) != RECORD_COMMITTED
            || self.contended.load(AtomicOrdering::Acquire)
        {
            return None;
        }
        let extended = self.extended_present.load(AtomicOrdering::Relaxed).then(|| {
            let mut id = [0_u8; 16];
            id[..8].copy_from_slice(&self.extended_low.load(AtomicOrdering::Relaxed).to_ne_bytes());
            id[8..]
                .copy_from_slice(&self.extended_high.load(AtomicOrdering::Relaxed).to_ne_bytes());
            (self.extended_volume.load(AtomicOrdering::Relaxed), id)
        });
        Some(NativeFileIdentity {
            legacy_volume: self.legacy_volume.load(AtomicOrdering::Relaxed) as u32,
            legacy_file_id: self.legacy_file_id.load(AtomicOrdering::Relaxed),
            extended,
        })
    }
}

/// Post-operation classification of a rename that reported failure.
enum FailedRenameClassification {
    /// The sibling identity is visible at the target name.
    Committed(NativeFileIdentity),
    /// The target is unchanged; the sibling was deleted through its handle.
    NotCommitted(DiagnosticFileError),
    /// The result cannot be proven; the sibling is preserved.
    Unverified(DiagnosticFileError),
}

enum PreparedMarkerSlot {
    Missing,
    Present { identity: NativeFileIdentity, bytes: Box<[u8]> },
    Unavailable(DiagnosticFileError),
}

impl PreparedMarkerSlot {
    fn state(&self) -> MarkerSlotState<'_> {
        match self {
            Self::Missing => MarkerSlotState::Missing,
            Self::Present { bytes, .. } => MarkerSlotState::Present(bytes),
            Self::Unavailable(error) => MarkerSlotState::Unavailable(*error),
        }
    }
}

/// Retained-handle panic-marker session (ADR 0017 transport with the ADR 0019
/// commit discipline).
///
/// Construction happens exactly once before the process panic hook is
/// installed. It retains the local non-reparse parent directory handle,
/// inspects the fixed target and sibling slots as regular single-link direct
/// children of that parent, reads each present slot with a hard bound, and
/// precomputes every UTF-16 buffer a later operation needs. [`Self::commit`]
/// therefore performs only direct synchronous Win32 calls on retained handles
/// and fixed buffers: it never allocates, re-resolves an untrusted path,
/// acquires a lock, or waits.
///
/// Namespace changes go through `NtSetInformationFile` with
/// `FileRenameInformationEx` relative to the retained parent (see
/// [`RenameBuffer`]); every outcome is classified from post-operation
/// identities and never inferred from a return code alone. Atomic namespace
/// visibility is promised; directory-entry durability across power loss is
/// deliberately not claimed.
pub struct WindowsMarkerSession {
    parent: DirectoryGuard,
    target_name: Box<[u16]>,
    sibling_name: Box<[u16]>,
    target_path: Box<[u16]>,
    sibling_path: Box<[u16]>,
    target_rename: RenameBuffer,
    create_rename: RenameBuffer,
    sibling_recovery_rename: RenameBuffer,
    initial_target: PreparedMarkerSlot,
    initial_sibling: PreparedMarkerSlot,
    record: CommitRecord,
}

impl WindowsMarkerSession {
    /// Validates the fixed target and sibling names, retains their common
    /// local parent, and captures the bounded initial view of both slots.
    ///
    /// Only the parent and the precomputed rename requests are required for
    /// success. A slot that is a directory, reparse point, hard-linked file,
    /// oversized or non-ASCII object is reported as
    /// [`MarkerSlotState::Unavailable`] and is never touched afterwards.
    pub fn prepare(target: &Path, sibling: &Path) -> Result<Self, DiagnosticFileError> {
        let invalid = || {
            DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            )
        };
        let (Some(target_file_name), Some(sibling_file_name)) =
            (target.file_name(), sibling.file_name())
        else {
            return Err(invalid());
        };
        if !valid_plain_file_name(target_file_name) || !valid_plain_file_name(sibling_file_name) {
            return Err(invalid());
        }
        for path in [target, sibling] {
            if classify_export_destination(path)? != ExportDestinationKind::Local {
                return Err(invalid());
            }
        }
        let (Some(target_parent), Some(sibling_parent)) = (target.parent(), sibling.parent())
        else {
            return Err(invalid());
        };
        let normalized_parent = normalize_absolute(target_parent)?;
        if !native_paths_equal(&normalized_parent, &normalize_absolute(sibling_parent)?)
            || native_paths_equal(&normalize_absolute(target)?, &normalize_absolute(sibling)?)
        {
            return Err(invalid());
        }

        let parent =
            DirectoryGuard::open(target_parent, DiagnosticFileOperation::InspectDestination)?;
        if !parent.is_local() {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        let target_name = os_units(target_file_name);
        let sibling_name = os_units(sibling_file_name);
        let target_rename = RenameBuffer::new(parent.handle(), &target_name, true)?;
        let create_rename = RenameBuffer::new(parent.handle(), &target_name, false)?;
        let sibling_recovery_rename = RenameBuffer::new(parent.handle(), &sibling_name, false)?;
        let target_path =
            extended_native_path_units_with_nul(&parent.current_child_path(target_file_name)?)?;
        let sibling_path =
            extended_native_path_units_with_nul(&parent.current_child_path(sibling_file_name)?)?;

        let initial_target = read_marker_slot(&parent, &target_path, &target_name);
        let initial_sibling = read_marker_slot(&parent, &sibling_path, &sibling_name);

        Ok(Self {
            parent,
            target_name: target_name.into_boxed_slice(),
            sibling_name: sibling_name.into_boxed_slice(),
            target_path: target_path.into_boxed_slice(),
            sibling_path: sibling_path.into_boxed_slice(),
            target_rename,
            create_rename,
            sibling_recovery_rename,
            initial_target,
            initial_sibling,
            record: CommitRecord::new(),
        })
    }

    /// Bounded initial view of the target slot.
    pub fn target_slot(&self) -> MarkerSlotState<'_> {
        self.initial_target.state()
    }

    /// Bounded initial view of the fixed sibling slot.
    pub fn sibling_slot(&self) -> MarkerSlotState<'_> {
        self.initial_sibling.state()
    }

    /// Commits `contents` at the target name from the panic hook.
    ///
    /// The exclusive sibling is created from the precomputed path, written,
    /// flushed, and verified through its own handle; the target is then
    /// revalidated against the prepared identity through the retained parent
    /// and replaced (or created when it was and still is missing) with a
    /// handle-relative rename. The result is classified by re-inspecting
    /// identities. A sibling this call created and never renamed is deleted
    /// through its handle; a sibling that may have been the only complete copy
    /// after an attempted replacement is preserved.
    pub fn commit(&self, contents: &[u8]) -> MarkerCommitStatus {
        if let Err(error) = validate_marker_payload(contents) {
            return MarkerCommitStatus::NotCommitted(error);
        }
        // A later panic in the same session replaces the marker this session
        // already proved at the target; otherwise the prepared inspection is
        // the only identity this call may replace.
        let expected_target = match self.record.committed_identity() {
            Some(identity) => Some(identity),
            None => match &self.initial_target {
                PreparedMarkerSlot::Missing => None,
                PreparedMarkerSlot::Present { identity, .. } => Some(*identity),
                PreparedMarkerSlot::Unavailable(error) => {
                    return MarkerCommitStatus::NotCommitted(*error);
                }
            },
        };

        let file = match self.create_verified_sibling(contents) {
            Ok(file) => file,
            Err(error) => return MarkerCommitStatus::NotCommitted(error),
        };
        if let Err(error) =
            self.parent.ensure_location_stable(DiagnosticFileOperation::ReplaceDestination)
        {
            let _cleanup = file.delete_exact();
            return MarkerCommitStatus::NotCommitted(error);
        }

        // Revalidate the target through the retained parent and keep the
        // inspection open through the namespace commit (ADR 0019 item 7).
        let current =
            match self.inspect_target_attributes(DiagnosticFileOperation::InspectDestination) {
                Ok(current) => current,
                Err(error) => {
                    let _cleanup = file.delete_exact();
                    return MarkerCommitStatus::NotCommitted(error);
                }
            };
        let rename = match (expected_target, current.as_ref()) {
            (None, None) => &self.create_rename,
            (Some(expected), Some((_, identity))) if expected.same_file(identity) => {
                &self.target_rename
            }
            (None, Some(_)) => {
                let _cleanup = file.delete_exact();
                return MarkerCommitStatus::NotCommitted(DiagnosticFileError::new(
                    DiagnosticFileOperation::InspectDestination,
                    DiagnosticFileErrorKind::AlreadyExists,
                ));
            }
            // The inspected marker disappeared: there is no prior object left
            // to protect, so create-only semantics still fail atomically if a
            // concurrent creator wins.
            (Some(_), None) => &self.create_rename,
            _ => {
                let _cleanup = file.delete_exact();
                return MarkerCommitStatus::NotCommitted(DiagnosticFileError::new(
                    DiagnosticFileOperation::InspectDestination,
                    DiagnosticFileErrorKind::OwnedDestination,
                ));
            }
        };

        match rename.apply(file.handle()) {
            Ok(()) => {
                drop(current);
                match self.verify_at_target(&file, contents.len() as u64) {
                    Ok(identity) => {
                        self.record.record_committed(identity);
                        MarkerCommitStatus::Committed
                    }
                    Err(error) => {
                        self.record.record_unverified();
                        MarkerCommitStatus::CommittedButUnverified(error)
                    }
                }
            }
            Err(error) => {
                drop(current);
                match self.classify_failed_rename(file, expected_target, error) {
                    FailedRenameClassification::Committed(identity) => {
                        self.record.record_committed(identity);
                        MarkerCommitStatus::Committed
                    }
                    FailedRenameClassification::NotCommitted(error) => {
                        MarkerCommitStatus::NotCommitted(error)
                    }
                    FailedRenameClassification::Unverified(error) => {
                        self.record.record_unverified();
                        MarkerCommitStatus::CommittedButUnverified(error)
                    }
                }
            }
        }
    }

    /// Reconciles the target only while it still carries the identity this
    /// session committed and `owner_check` accepts its current bytes.
    ///
    /// The inspected target handle (with delete sharing) is retained through
    /// the mutation: a delete is applied to that very object through
    /// `FileDispositionInfo`, and a restore renames a fresh verified sibling
    /// over it relative to the retained parent. The outcome is classified
    /// from post-operation identities; any uncertainty preserves every copy.
    ///
    /// A sibling preserved by this session exists only in the
    /// `CommittedButUnverified` state, which this method refuses to touch, so
    /// there is never an owned leftover sibling to clean here. A foreign
    /// sibling is never touched.
    pub fn reconcile_if_owned(
        &self,
        action: MarkerReconcileAction<'_>,
        owner_check: &dyn Fn(&[u8]) -> bool,
    ) -> Result<MarkerReconcileOutcome, DiagnosticFileError> {
        let Some(committed) = self.record.committed_identity() else {
            return Ok(MarkerReconcileOutcome::PreservedUncertain);
        };
        if let MarkerReconcileAction::Restore(previous) = action {
            validate_marker_payload(previous)?;
        }
        self.parent.ensure_location_stable(DiagnosticFileOperation::InspectDestination)?;

        let access = FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0 | DELETE.0;
        let inspected = match inspect_marker_slot(
            &self.parent,
            &self.target_path,
            &self.target_name,
            access,
            DiagnosticFileOperation::InspectDestination,
        ) {
            Ok(Some(inspected)) => inspected,
            Ok(None) => return Ok(MarkerReconcileOutcome::MarkerMissing),
            Err(error)
                if matches!(
                    error.kind,
                    DiagnosticFileErrorKind::ReparsePoint
                        | DiagnosticFileErrorKind::InvalidDestination
                        | DiagnosticFileErrorKind::OwnedDestination
                ) =>
            {
                return Ok(MarkerReconcileOutcome::OwnershipLost);
            }
            Err(error) => return Err(error),
        };
        let (file, info) = inspected;
        if !info.identity.same_file(&committed) {
            return Ok(MarkerReconcileOutcome::OwnershipLost);
        }
        let bytes = read_bounded_bytes(
            &file,
            MAX_PANIC_MARKER_BYTES,
            DiagnosticFileOperation::InspectDestination,
        )?;
        if !bytes.is_ascii() || !owner_check(&bytes) {
            return Ok(MarkerReconcileOutcome::OwnershipLost);
        }

        match action {
            MarkerReconcileAction::Delete => {
                delete_file_handle(&file, DiagnosticFileOperation::RemoveDestination)?;
                drop(file);
                match self.inspect_target_attributes(DiagnosticFileOperation::RemoveDestination) {
                    Ok(None) => Ok(MarkerReconcileOutcome::RemovedSessionMarker),
                    Ok(Some((_, identity))) if identity.same_file(&committed) => {
                        Ok(MarkerReconcileOutcome::PreservedUncertain)
                    }
                    Ok(Some(_)) => Ok(MarkerReconcileOutcome::RemovedSessionMarker),
                    Err(_classification) => Ok(MarkerReconcileOutcome::PreservedUncertain),
                }
            }
            MarkerReconcileAction::Restore(previous) => {
                let sibling = self.create_verified_sibling(previous)?;
                if let Err(error) =
                    self.parent.ensure_location_stable(DiagnosticFileOperation::ReplaceDestination)
                {
                    let _cleanup = sibling.delete_exact();
                    return Err(error);
                }
                match self.target_rename.apply(sibling.handle()) {
                    Ok(()) => {
                        drop(file);
                        match self.verify_at_target(&sibling, previous.len() as u64) {
                            Ok(_identity) => Ok(MarkerReconcileOutcome::RestoredPrevious),
                            Err(_classification) => Ok(MarkerReconcileOutcome::PreservedUncertain),
                        }
                    }
                    Err(error) => {
                        drop(file);
                        // The session record is deliberately left alone: the
                        // restored object is the previous marker, not one this
                        // session may reconcile again.
                        match self.classify_failed_rename(sibling, Some(committed), error) {
                            FailedRenameClassification::Committed(_identity) => {
                                Ok(MarkerReconcileOutcome::RestoredPrevious)
                            }
                            FailedRenameClassification::NotCommitted(error) => Err(error),
                            FailedRenameClassification::Unverified(_error) => {
                                Ok(MarkerReconcileOutcome::PreservedUncertain)
                            }
                        }
                    }
                }
            }
        }
    }

    /// Deletes the slots captured at preparation through freshly inspected
    /// handles whose identities still match that inspection.
    ///
    /// Both slots are inspected before anything is deleted. A slot whose
    /// identity changed, an entry that appeared under an initially missing
    /// name, or a slot that was unavailable at preparation preserves every
    /// file and is reported as an ownership conflict. Returns whether any
    /// file was deleted.
    pub fn delete_explicit(&self) -> Result<bool, DiagnosticFileError> {
        self.parent.ensure_location_stable(DiagnosticFileOperation::InspectDestination)?;
        let access = FILE_READ_ATTRIBUTES.0 | DELETE.0;
        let mut owned = Vec::with_capacity(2);
        for (slot, path, name) in [
            (&self.initial_target, &self.target_path, &self.target_name),
            (&self.initial_sibling, &self.sibling_path, &self.sibling_name),
        ] {
            let conflict = DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::OwnedDestination,
            );
            let current = inspect_marker_slot(
                &self.parent,
                path,
                name,
                access,
                DiagnosticFileOperation::InspectDestination,
            )?;
            match (slot, current) {
                (PreparedMarkerSlot::Unavailable(error), _) => return Err(*error),
                (PreparedMarkerSlot::Missing, None)
                | (PreparedMarkerSlot::Present { .. }, None) => {}
                (PreparedMarkerSlot::Missing, Some(_)) => return Err(conflict),
                (PreparedMarkerSlot::Present { identity, .. }, Some((file, info)))
                    if info.identity.same_file(identity) =>
                {
                    owned.push(file);
                }
                (PreparedMarkerSlot::Present { .. }, Some(_)) => return Err(conflict),
            }
        }
        let mut deleted = false;
        for file in &owned {
            delete_file_handle(file, DiagnosticFileOperation::RemoveDestination)?;
            deleted = true;
        }
        Ok(deleted)
    }

    fn create_verified_sibling(
        &self,
        contents: &[u8],
    ) -> Result<NativeMarkerFile, DiagnosticFileError> {
        let file = NativeMarkerFile::create(&self.sibling_path)?;
        if let Err(error) = self.parent.validate_direct_child_handle_units(
            file.handle(),
            &self.sibling_name,
            DiagnosticFileOperation::CreateTemporary,
        ) {
            let _cleanup = file.delete_exact();
            return Err(error);
        }
        if let Err(error) = file.fill_and_verify(contents) {
            let _cleanup = file.delete_exact();
            return Err(error);
        }
        Ok(file)
    }

    /// Opens the target name with attribute-only access, which no sharing
    /// mode can deny, so it also works while the exclusive sibling handle is
    /// still open at that name after the rename.
    fn inspect_target_attributes(
        &self,
        operation: DiagnosticFileOperation,
    ) -> Result<Option<(File, NativeFileIdentity)>, DiagnosticFileError> {
        inspect_marker_slot(
            &self.parent,
            &self.target_path,
            &self.target_name,
            FILE_READ_ATTRIBUTES.0,
            operation,
        )
        .map(|inspected| inspected.map(|(file, info)| (file, info.identity)))
    }

    fn verify_at_target(
        &self,
        file: &NativeMarkerFile,
        expected_size: u64,
    ) -> Result<NativeFileIdentity, DiagnosticFileError> {
        let operation = DiagnosticFileOperation::ReplaceDestination;
        self.parent.validate_direct_child_handle_units(
            file.handle(),
            &self.target_name,
            operation,
        )?;
        file.verify_complete(expected_size)?;
        let Some((_current, identity)) = self.inspect_target_attributes(operation)? else {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        };
        if !identity.same_file(&file.identity()) {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        Ok(identity)
    }

    /// Classifies a rename that reported failure without touching the
    /// session record. Observing the sibling identity at the target name is a
    /// commit; an unchanged target (or a still-missing target) is not, and the
    /// never-renamed sibling is deleted through its handle. Anything else
    /// preserves the sibling as a possible only complete copy.
    fn classify_failed_rename(
        &self,
        file: NativeMarkerFile,
        expected_target: Option<NativeFileIdentity>,
        error: DiagnosticFileError,
    ) -> FailedRenameClassification {
        if error.kind == DiagnosticFileErrorKind::PartialReplacement {
            // Windows errors 1176/1177: the sibling may be the only complete
            // copy. Move it back under its fixed name and report uncertainty.
            let _recovery = self.sibling_recovery_rename.apply(file.handle());
            return FailedRenameClassification::Unverified(error);
        }
        let current =
            match self.inspect_target_attributes(DiagnosticFileOperation::ReplaceDestination) {
                Ok(current) => current,
                Err(_classification) => return FailedRenameClassification::Unverified(error),
            };
        match (expected_target, current.as_ref()) {
            (Some(expected), Some((_, identity))) if expected.same_file(identity) => {
                let _cleanup = file.delete_exact();
                FailedRenameClassification::NotCommitted(error)
            }
            (None, None) => {
                let _cleanup = file.delete_exact();
                FailedRenameClassification::NotCommitted(error)
            }
            (_, Some((_, identity))) if identity.same_file(&file.identity()) => {
                FailedRenameClassification::Committed(*identity)
            }
            _ => FailedRenameClassification::Unverified(error),
        }
    }
}

impl fmt::Debug for WindowsMarkerSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsMarkerSession")
            .field("local_parent_bound", &true)
            .field(
                "target_present",
                &matches!(self.initial_target, PreparedMarkerSlot::Present { .. }),
            )
            .field(
                "sibling_present",
                &matches!(self.initial_sibling, PreparedMarkerSlot::Present { .. }),
            )
            .field("committed_identity_recorded", &self.record.committed_identity().is_some())
            .finish()
    }
}

fn validate_marker_payload(contents: &[u8]) -> Result<(), DiagnosticFileError> {
    if contents.is_empty() || !contents.is_ascii() {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::WriteTemporary,
            DiagnosticFileErrorKind::InvalidContent,
        ));
    }
    if contents.len() > MAX_PANIC_MARKER_BYTES {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::WriteTemporary,
            DiagnosticFileErrorKind::TooLarge,
        ));
    }
    Ok(())
}

/// Opens one fixed marker slot through its precomputed path after proving the
/// retained parent has not moved, then requires a regular, single-link,
/// non-reparse direct child. `FILE_FLAG_BACKUP_SEMANTICS` lets a directory
/// open so it is rejected as an invalid destination rather than surfacing the
/// generic access-denied code.
fn inspect_marker_slot(
    parent: &DirectoryGuard,
    path: &[u16],
    name: &[u16],
    access: u32,
    operation: DiagnosticFileOperation,
) -> Result<Option<(File, HandleInspection)>, DiagnosticFileError> {
    parent.ensure_location_stable(operation)?;
    let file = match open_native_file(
        path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        operation,
    ) {
        Ok(file) => file,
        Err(error) if error.kind == DiagnosticFileErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let info = inspect_handle(&file, operation)?;
    validate_regular_non_reparse(&info, operation)?;
    if info.link_count != 1 {
        return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::OwnedDestination));
    }
    parent.validate_direct_child_handle_units(file_handle(&file), name, operation)?;
    Ok(Some((file, info)))
}

fn read_marker_slot(parent: &DirectoryGuard, path: &[u16], name: &[u16]) -> PreparedMarkerSlot {
    let operation = DiagnosticFileOperation::InspectDestination;
    let (file, info) = match inspect_marker_slot(
        parent,
        path,
        name,
        FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0,
        operation,
    ) {
        Ok(Some(inspected)) => inspected,
        Ok(None) => return PreparedMarkerSlot::Missing,
        Err(error) => return PreparedMarkerSlot::Unavailable(error),
    };
    let bytes = match read_bounded_bytes(&file, MAX_PANIC_MARKER_BYTES, operation) {
        Ok(bytes) => bytes,
        Err(error) => return PreparedMarkerSlot::Unavailable(error),
    };
    if !bytes.is_ascii() {
        return PreparedMarkerSlot::Unavailable(DiagnosticFileError::new(
            operation,
            DiagnosticFileErrorKind::InvalidContent,
        ));
    }
    PreparedMarkerSlot::Present { identity: info.identity, bytes }
}

/// Path-free health shared with the asynchronous diagnostics worker.
#[derive(Clone, Default)]
pub struct WindowsDailyLogHealth {
    retention_issues: Arc<AtomicU64>,
}

impl WindowsDailyLogHealth {
    /// Returns the number of retention failures observed by the writer.
    pub fn retention_issue_count(&self) -> u64 {
        self.retention_issues.load(AtomicOrdering::Acquire)
    }

    fn record_issue(&self) {
        saturating_atomic_add(&self.retention_issues, 1);
    }
}

impl fmt::Debug for WindowsDailyLogHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsDailyLogHealth")
            .field("retention_issue_count", &self.retention_issue_count())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UtcDate {
    year: u16,
    month: u16,
    day: u16,
}

impl UtcDate {
    fn log_name(self) -> String {
        format!("{LOG_PREFIX}{:04}-{:02}-{:02}{LOG_SUFFIX}", self.year, self.month, self.day)
    }
}

type UtcClock = Arc<dyn Fn() -> UtcDate + Send + Sync>;

/// Secure append-only UTC daily writer for DiskPie's private diagnostics.
///
/// The directory and active leaf stay open without delete sharing. Every
/// operation is bound to the directory's resolved local identity; no fallback
/// directory or terminal sink is used. Retention is incremental and uses only
/// handle enumeration from that same directory object.
pub struct WindowsDailyLogWriter {
    root: DirectoryGuard,
    active: ActiveLog,
    clock: UtcClock,
    retention: RetentionController,
    health: WindowsDailyLogHealth,
    query_buffer: Box<DirectoryQueryBuffer>,
    next_maintenance: Instant,
}

impl WindowsDailyLogWriter {
    /// Opens today's UTC log in an already-created private local directory.
    pub fn open(directory: &Path) -> Result<Self, DiagnosticFileError> {
        Self::open_with_clock(directory, Arc::new(system_utc_date))
    }

    fn open_with_clock(directory: &Path, clock: UtcClock) -> Result<Self, DiagnosticFileError> {
        let root = DirectoryGuard::open(directory, DiagnosticFileOperation::OpenLog)?;
        if root.transport != ExportDestinationKind::Local {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::OpenLog,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        root.extended_identity(DiagnosticFileOperation::OpenLog)?;
        let date = clock();
        let active = ActiveLog::open(&root, date)?;
        Ok(Self {
            root,
            active,
            clock,
            retention: RetentionController::default(),
            health: WindowsDailyLogHealth::default(),
            query_buffer: Box::new(DirectoryQueryBuffer::new()),
            next_maintenance: Instant::now(),
        })
    }

    /// Returns a path-free health handle that remains valid on the worker.
    pub fn health(&self) -> WindowsDailyLogHealth {
        self.health.clone()
    }

    /// Time until the worker should wake even if no diagnostic event arrives.
    pub fn maintenance_delay(&self) -> Duration {
        self.next_maintenance.saturating_duration_since(Instant::now())
    }

    /// Performs at most one bounded native directory-query buffer of work.
    pub fn maintain_if_due(&mut self) {
        if Instant::now() < self.next_maintenance {
            return;
        }
        if let Err(error) = self.ensure_active() {
            self.record_retention_issue(error);
            self.retention.finish_cycle();
            self.next_maintenance = Instant::now() + RETENTION_INTERVAL;
            return;
        }
        self.maintenance_step();
        self.schedule_next_maintenance();
    }

    fn ensure_active(&mut self) -> Result<(), DiagnosticFileError> {
        self.root.ensure_location_stable(DiagnosticFileOperation::OpenLog)?;
        self.active.validate(&self.root)?;
        let date = (self.clock)();
        if date != self.active.date {
            // Open and fully validate the successor before releasing the
            // previous active handle. A sharing or validation failure leaves
            // the old handle retained and makes this operation fail closed.
            let replacement = ActiveLog::open(&self.root, date)?;
            self.active = replacement;
            self.retention.start_cycle();
            self.next_maintenance = Instant::now();
        }
        Ok(())
    }

    fn maintenance_step(&mut self) {
        if self.retention.is_idle() {
            self.retention.start_cycle();
        }
        let phase = std::mem::replace(&mut self.retention.phase, RetentionPhase::Idle);
        match phase {
            RetentionPhase::Idle => {}
            RetentionPhase::Cleanup(mut state) => {
                match query_directory_records(&self.root, state.restart, self.query_buffer.as_mut())
                {
                    Ok(DirectoryQuery::Records { records, scanned }) => {
                        state.restart = false;
                        if !state.admit_scanned(scanned) {
                            self.record_retention_issue(DiagnosticFileError::new(
                                DiagnosticFileOperation::ReadLogDirectory,
                                DiagnosticFileErrorKind::Unsupported,
                            ));
                            self.retention.finish_cycle();
                            return;
                        }
                        for record in records {
                            self.process_cleanup_record(&mut state, record);
                        }
                        self.retention.phase = RetentionPhase::Cleanup(state);
                    }
                    Ok(DirectoryQuery::Done) => {
                        if !state.active_seen {
                            self.record_retention_issue(DiagnosticFileError::new(
                                DiagnosticFileOperation::InspectLogEntry,
                                DiagnosticFileErrorKind::InvalidDestination,
                            ));
                        }
                        drop(state.keep);
                        self.retention.phase = RetentionPhase::Verify(VerifyPass {
                            restart: true,
                            exact_seen: 0,
                            active_seen: false,
                            removed_before_verify: state.removed,
                            scanned_entries: state.scanned_entries,
                        });
                    }
                    Err(error) => {
                        self.record_retention_issue(error);
                        self.retention.finish_cycle();
                    }
                }
            }
            RetentionPhase::Verify(mut state) => {
                match query_directory_records(&self.root, state.restart, self.query_buffer.as_mut())
                {
                    Ok(DirectoryQuery::Records { records, scanned }) => {
                        state.restart = false;
                        if !state.admit_scanned(scanned) {
                            self.record_retention_issue(DiagnosticFileError::new(
                                DiagnosticFileOperation::ReadLogDirectory,
                                DiagnosticFileErrorKind::Unsupported,
                            ));
                            self.retention.finish_cycle();
                            return;
                        }
                        for record in records {
                            state.exact_seen = state.exact_seen.saturating_add(1);
                            if record.name_key == self.active.name_key {
                                state.active_seen |= record.file_id == self.active.extended_id();
                            }
                        }
                        self.retention.phase = RetentionPhase::Verify(state);
                    }
                    Ok(DirectoryQuery::Done) => self.finish_verification(state),
                    Err(error) => {
                        self.record_retention_issue(error);
                        self.retention.finish_cycle();
                    }
                }
            }
        }
    }

    fn process_cleanup_record(&mut self, state: &mut CleanupPass, record: DirectoryRecord) {
        if record.name_key == self.active.name_key {
            if record.file_id == self.active.extended_id() {
                state.active_seen = true;
            } else {
                self.record_retention_issue(DiagnosticFileError::new(
                    DiagnosticFileOperation::InspectLogEntry,
                    DiagnosticFileErrorKind::InvalidDestination,
                ));
            }
            return;
        }
        if record.attributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0 {
            self.record_retention_issue(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectLogEntry,
                if record.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                    DiagnosticFileErrorKind::ReparsePoint
                } else {
                    DiagnosticFileErrorKind::InvalidDestination
                },
            ));
            return;
        }
        let candidate = match LockedLog::open(&self.root, &record) {
            Ok(candidate) => candidate,
            Err(error) => {
                self.record_retention_issue(error);
                return;
            }
        };
        if state.keep.len() < ACTIVE_RETENTION_SLOTS {
            state.keep.push(candidate);
            return;
        }
        let worst_index = state
            .keep
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                retention_preference(&left.name_key, &right.name_key, &self.active.name_key)
            })
            .map_or(0, |(index, _)| index);
        if retention_preference(
            &candidate.name_key,
            &state.keep[worst_index].name_key,
            &self.active.name_key,
        ) == Ordering::Greater
        {
            let obsolete = std::mem::replace(&mut state.keep[worst_index], candidate);
            self.remove_locked_log(state, obsolete);
        } else {
            self.remove_locked_log(state, candidate);
        }
    }

    fn remove_locked_log(&mut self, state: &mut CleanupPass, candidate: LockedLog) {
        match candidate.delete() {
            Ok(()) => {
                state.removed = state.removed.saturating_add(1);
                self.retention.report.files_removed =
                    self.retention.report.files_removed.saturating_add(1);
            }
            Err(error) => self.record_retention_issue(error),
        }
    }

    fn finish_verification(&mut self, state: VerifyPass) {
        self.retention.report.exact_files_seen = state.exact_seen;
        if !state.active_seen {
            self.record_retention_issue(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectLogEntry,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        if state.exact_seen > MAX_RETAINED_LOG_FILES as u64
            && state.removed_before_verify != 0
            && self.retention.progress_passes < MAX_RETENTION_PROGRESS_PASSES
        {
            self.retention.progress_passes += 1;
            self.retention.phase =
                RetentionPhase::Cleanup(CleanupPass::with_scanned(state.scanned_entries));
            return;
        }
        if state.exact_seen > MAX_RETAINED_LOG_FILES as u64 {
            // Unsafe or concurrently replaced exact entries are preserved. A
            // path-free issue makes the residual explicit and the pass stops
            // instead of spinning forever under an adversarial namespace.
            self.record_retention_issue(DiagnosticFileError::new(
                DiagnosticFileOperation::RemoveOldLog,
                DiagnosticFileErrorKind::Unsupported,
            ));
        }
        self.retention.finish_cycle();
    }

    fn record_retention_issue(&mut self, error: DiagnosticFileError) {
        self.retention.report.record_issue(error);
        self.health.record_issue();
    }

    fn schedule_next_maintenance(&mut self) {
        let delay = if self.retention.is_idle() {
            RETENTION_INTERVAL
        } else {
            RETENTION_CONTINUATION_DELAY
        };
        self.next_maintenance = Instant::now() + delay;
    }

    fn force_retention_step(&mut self) -> bool {
        if let Err(error) = self.ensure_active() {
            self.record_retention_issue(error);
            self.retention.finish_cycle();
            return true;
        }
        self.maintenance_step();
        self.retention.is_idle()
    }
}

impl io::Write for WindowsDailyLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.ensure_active().map_err(diagnostic_io_error)?;
        if Instant::now() >= self.next_maintenance {
            self.maintenance_step();
            self.schedule_next_maintenance();
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        let current_size = file_size(&self.active.file, DiagnosticFileOperation::WriteLog)
            .map_err(diagnostic_io_error)?;
        let incoming = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if current_size > MAX_DAILY_LOG_BYTES
            || incoming > MAX_DAILY_LOG_BYTES.saturating_sub(current_size)
        {
            return Err(diagnostic_io_error(DiagnosticFileError::new(
                DiagnosticFileOperation::WriteLog,
                DiagnosticFileErrorKind::TooLarge,
            )));
        }
        let length = buffer.len().min(u32::MAX as usize);
        let mut written = 0_u32;
        // SAFETY: the active handle remains owned for the duration of this
        // synchronous call; `buffer` is live and `written` is an exact output.
        unsafe {
            WriteFile(
                file_handle(&self.active.file),
                Some(&buffer[..length]),
                Some(&raw mut written),
                None,
            )
        }
        .map_err(|error| {
            diagnostic_io_error(DiagnosticFileError::from_windows(
                DiagnosticFileOperation::WriteLog,
                &error,
            ))
        })?;
        if written == 0 && length != 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "diagnostic log write failed"));
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ensure_active().map_err(diagnostic_io_error)?;
        // SAFETY: the active file handle stays live for this synchronous call.
        unsafe { FlushFileBuffers(file_handle(&self.active.file)) }.map_err(|error| {
            diagnostic_io_error(DiagnosticFileError::from_windows(
                DiagnosticFileOperation::WriteLog,
                &error,
            ))
        })
    }
}

impl fmt::Debug for WindowsDailyLogWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsDailyLogWriter")
            .field("local_root_bound", &true)
            .field("active_identity_bound", &true)
            .field("retention_in_progress", &!self.retention.is_idle())
            .field("health", &self.health)
            .finish()
    }
}

struct ActiveLog {
    file: File,
    identity: NativeFileIdentity,
    name_key: String,
    date: UtcDate,
}

impl ActiveLog {
    fn open(root: &DirectoryGuard, date: UtcDate) -> Result<Self, DiagnosticFileError> {
        let name_key = date.log_name();
        let mut path = root.absolute_child_units_for(
            &os_units(OsStr::new(&name_key)),
            DiagnosticFileOperation::OpenLog,
        )?;
        path.push(0);
        let access = FILE_APPEND_DATA.0 | FILE_READ_ATTRIBUTES.0 | DELETE.0;
        let flags =
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
        let created = match open_native_file(
            &path,
            access,
            FILE_SHARE_READ,
            CREATE_NEW,
            flags,
            DiagnosticFileOperation::OpenLog,
        ) {
            Ok(file) => Some(CreatedLogCleanup::new(file)),
            Err(error) if error.kind == DiagnosticFileErrorKind::AlreadyExists => None,
            Err(error) => return Err(error),
        };
        let file = if let Some(created) = created {
            let identity = validate_active_file(created.file(), root, &name_key)?;
            validate_active_membership(root, &name_key, identity)?;
            return Ok(Self { file: created.commit(), identity, name_key, date });
        } else {
            open_native_file(
                &path,
                access,
                FILE_SHARE_READ,
                OPEN_EXISTING,
                flags,
                DiagnosticFileOperation::OpenLog,
            )?
        };
        let identity = validate_active_file(&file, root, &name_key)?;
        validate_active_membership(root, &name_key, identity)?;
        Ok(Self { file, identity, name_key, date })
    }

    fn validate(&self, root: &DirectoryGuard) -> Result<(), DiagnosticFileError> {
        let identity = validate_active_file(&self.file, root, &self.name_key)?;
        if !self.identity.same_file(&identity) {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::OpenLog,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        Ok(())
    }

    fn extended_id(&self) -> [u8; 16] {
        self.identity.extended.expect("active logs require FileIdInfo").1
    }
}

fn validate_active_file(
    file: &File,
    root: &DirectoryGuard,
    name: &str,
) -> Result<NativeFileIdentity, DiagnosticFileError> {
    let info = inspect_handle(file, DiagnosticFileOperation::OpenLog)?;
    validate_regular_non_reparse(&info, DiagnosticFileOperation::OpenLog)?;
    if info.link_count != 1 {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::OpenLog,
            DiagnosticFileErrorKind::Unsupported,
        ));
    }
    let (root_volume, _) = root.extended_identity(DiagnosticFileOperation::OpenLog)?;
    let Some((volume, _file_id)) = info.identity.extended else {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::OpenLog,
            DiagnosticFileErrorKind::Unsupported,
        ));
    };
    if volume != root_volume {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::OpenLog,
            DiagnosticFileErrorKind::InvalidDestination,
        ));
    }
    root.validate_direct_child_handle_named(
        file_handle(file),
        OsStr::new(name),
        DiagnosticFileOperation::OpenLog,
    )?;
    Ok(info.identity)
}

fn validate_active_membership(
    root: &DirectoryGuard,
    name: &str,
    identity: NativeFileIdentity,
) -> Result<(), DiagnosticFileError> {
    let Some((_volume, expected_id)) = identity.extended else {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::OpenLog,
            DiagnosticFileErrorKind::Unsupported,
        ));
    };
    let mut buffer = Box::new(DirectoryQueryBuffer::new());
    let mut scanned_entries = 0_u64;
    for query_index in 0..MAX_ACTIVE_LOOKUP_QUERIES {
        match query_directory_records(root, query_index == 0, buffer.as_mut())? {
            DirectoryQuery::Records { records, scanned } => {
                scanned_entries = scanned_entries.saturating_add(scanned as u64);
                if scanned_entries > MAX_DIRECTORY_ENTRIES_PER_CYCLE {
                    return Err(DiagnosticFileError::new(
                        DiagnosticFileOperation::OpenLog,
                        DiagnosticFileErrorKind::Unsupported,
                    ));
                }
                for record in records {
                    if record.name_key != name {
                        continue;
                    }
                    if record.file_id == expected_id
                        && record.attributes
                            & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
                            == 0
                    {
                        return Ok(());
                    }
                    return Err(DiagnosticFileError::new(
                        DiagnosticFileOperation::OpenLog,
                        DiagnosticFileErrorKind::InvalidDestination,
                    ));
                }
            }
            DirectoryQuery::Done => {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::OpenLog,
                    DiagnosticFileErrorKind::InvalidDestination,
                ));
            }
        }
    }
    Err(DiagnosticFileError::new(
        DiagnosticFileOperation::OpenLog,
        DiagnosticFileErrorKind::Unsupported,
    ))
}

struct CreatedLogCleanup {
    file: Option<File>,
}

impl CreatedLogCleanup {
    fn new(file: File) -> Self {
        Self { file: Some(file) }
    }

    fn file(&self) -> &File {
        self.file.as_ref().expect("created log cleanup owns a file")
    }

    fn commit(mut self) -> File {
        self.file.take().expect("created log cleanup owns a file")
    }
}

impl Drop for CreatedLogCleanup {
    fn drop(&mut self) {
        if let Some(file) = self.file.as_ref() {
            let _ignored = delete_file_handle(file, DiagnosticFileOperation::OpenLog);
        }
    }
}

#[derive(Default)]
struct RetentionController {
    phase: RetentionPhase,
    report: DiagnosticRetentionReport,
    progress_passes: u8,
}

impl RetentionController {
    fn is_idle(&self) -> bool {
        matches!(self.phase, RetentionPhase::Idle)
    }

    fn start_cycle(&mut self) {
        self.phase = RetentionPhase::Cleanup(CleanupPass::default());
        self.report = DiagnosticRetentionReport::default();
        self.progress_passes = 1;
    }

    fn finish_cycle(&mut self) {
        self.phase = RetentionPhase::Idle;
    }
}

#[derive(Default)]
enum RetentionPhase {
    #[default]
    Idle,
    Cleanup(CleanupPass),
    Verify(VerifyPass),
}

struct CleanupPass {
    restart: bool,
    keep: Vec<LockedLog>,
    active_seen: bool,
    removed: u64,
    scanned_entries: u64,
}

impl Default for CleanupPass {
    fn default() -> Self {
        Self::with_scanned(0)
    }
}

impl CleanupPass {
    fn with_scanned(scanned_entries: u64) -> Self {
        Self {
            restart: true,
            keep: Vec::with_capacity(ACTIVE_RETENTION_SLOTS),
            active_seen: false,
            removed: 0,
            scanned_entries,
        }
    }

    fn admit_scanned(&mut self, scanned: usize) -> bool {
        self.scanned_entries =
            self.scanned_entries.saturating_add(u64::try_from(scanned).unwrap_or(u64::MAX));
        self.scanned_entries <= MAX_DIRECTORY_ENTRIES_PER_CYCLE
    }
}

struct VerifyPass {
    restart: bool,
    exact_seen: u64,
    active_seen: bool,
    removed_before_verify: u64,
    scanned_entries: u64,
}

impl VerifyPass {
    fn admit_scanned(&mut self, scanned: usize) -> bool {
        self.scanned_entries =
            self.scanned_entries.saturating_add(u64::try_from(scanned).unwrap_or(u64::MAX));
        self.scanned_entries <= MAX_DIRECTORY_ENTRIES_PER_CYCLE
    }
}

struct LockedLog {
    file: File,
    name_key: String,
}

impl LockedLog {
    fn open(root: &DirectoryGuard, record: &DirectoryRecord) -> Result<Self, DiagnosticFileError> {
        let mut path = root.absolute_child_units_for(
            &os_units(OsStr::new(&record.name_key)),
            DiagnosticFileOperation::InspectLogEntry,
        )?;
        path.push(0);
        let file = open_native_file(
            &path,
            FILE_READ_ATTRIBUTES.0 | DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            DiagnosticFileOperation::InspectLogEntry,
        )?;
        let info = inspect_handle(&file, DiagnosticFileOperation::InspectLogEntry)?;
        validate_regular_non_reparse(&info, DiagnosticFileOperation::InspectLogEntry)?;
        if info.link_count != 1 {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectLogEntry,
                DiagnosticFileErrorKind::Unsupported,
            ));
        }
        let (root_volume, _) = root.extended_identity(DiagnosticFileOperation::InspectLogEntry)?;
        if info.identity.extended != Some((root_volume, record.file_id)) {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectLogEntry,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        root.validate_direct_child_handle_named(
            file_handle(&file),
            OsStr::new(&record.name_key),
            DiagnosticFileOperation::InspectLogEntry,
        )?;
        Ok(Self { file, name_key: record.name_key.clone() })
    }

    fn delete(self) -> Result<(), DiagnosticFileError> {
        delete_file_handle(&self.file, DiagnosticFileOperation::RemoveOldLog)
    }
}

fn retention_preference(left: &str, right: &str, active: &str) -> Ordering {
    let left_future = left > active;
    let right_future = right > active;
    match (left_future, right_future) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => left.cmp(right),
        // Future names are managed but cannot crowd out history; among them,
        // the closest upcoming date wins over an arbitrarily distant date.
        (true, true) => right.cmp(left),
    }
}

#[repr(C, align(8))]
struct DirectoryQueryBuffer([u8; DIRECTORY_QUERY_BUFFER_BYTES]);

impl DirectoryQueryBuffer {
    fn new() -> Self {
        Self([0; DIRECTORY_QUERY_BUFFER_BYTES])
    }
}

struct DirectoryRecord {
    name_key: String,
    attributes: u32,
    file_id: [u8; 16],
}

enum DirectoryQuery {
    Records { records: Vec<DirectoryRecord>, scanned: usize },
    Done,
}

fn query_directory_records(
    root: &DirectoryGuard,
    restart: bool,
    buffer: &mut DirectoryQueryBuffer,
) -> Result<DirectoryQuery, DiagnosticFileError> {
    root.ensure_location_stable(DiagnosticFileOperation::ReadLogDirectory)?;
    buffer.0.fill(0);
    let class = if restart { FileIdExtdDirectoryRestartInfo } else { FileIdExtdDirectoryInfo };
    // SAFETY: the directory handle is retained by `root`; the buffer is
    // 8-byte aligned, writable, and its exact bounded size is supplied.
    match unsafe {
        GetFileInformationByHandleEx(
            root.handle(),
            class,
            buffer.0.as_mut_ptr().cast(),
            DIRECTORY_QUERY_BUFFER_BYTES as u32,
        )
    } {
        Ok(()) => parse_directory_query_buffer(&buffer.0).map(|parsed| DirectoryQuery::Records {
            records: parsed.records,
            scanned: parsed.scanned,
        }),
        Err(error) if windows_os_code(&error) as u32 == ERROR_NO_MORE_FILES.0 => {
            Ok(DirectoryQuery::Done)
        }
        Err(error) => Err(DiagnosticFileError::from_windows(
            DiagnosticFileOperation::ReadLogDirectory,
            &error,
        )),
    }
}

struct ParsedDirectoryRecords {
    records: Vec<DirectoryRecord>,
    scanned: usize,
}

fn parse_directory_query_buffer(
    buffer: &[u8; DIRECTORY_QUERY_BUFFER_BYTES],
) -> Result<ParsedDirectoryRecords, DiagnosticFileError> {
    const HEADER_BYTES: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
    const ATTRIBUTES_OFFSET: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes);
    const NAME_LENGTH_OFFSET: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength);
    const FILE_ID_OFFSET: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);

    let invalid = || {
        DiagnosticFileError::new(
            DiagnosticFileOperation::ReadLogDirectory,
            DiagnosticFileErrorKind::InvalidDestination,
        )
    };
    let mut records = Vec::new();
    let mut offset = 0_usize;
    let mut entries = 0_usize;
    loop {
        if !offset.is_multiple_of(8)
            || offset.checked_add(HEADER_BYTES).is_none_or(|end| end > buffer.len())
        {
            return Err(invalid());
        }
        entries += 1;
        if entries > MAX_DIRECTORY_RECORDS_PER_QUERY {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::ReadLogDirectory,
                DiagnosticFileErrorKind::Unsupported,
            ));
        }
        let next = read_buffer_u32(buffer, offset).ok_or_else(invalid)? as usize;
        let attributes = read_buffer_u32(buffer, offset + ATTRIBUTES_OFFSET).ok_or_else(invalid)?;
        let name_bytes =
            read_buffer_u32(buffer, offset + NAME_LENGTH_OFFSET).ok_or_else(invalid)? as usize;
        if name_bytes == 0 || !name_bytes.is_multiple_of(2) || name_bytes > 255 * size_of::<u16>() {
            return Err(invalid());
        }
        let record_bytes = HEADER_BYTES.checked_add(name_bytes).ok_or_else(invalid)?;
        let record_end = offset.checked_add(record_bytes).ok_or_else(invalid)?;
        if record_end > buffer.len() {
            return Err(invalid());
        }
        let name_start = offset + HEADER_BYTES;
        let mut name = Vec::with_capacity(name_bytes / 2);
        for chunk in buffer[name_start..record_end].chunks_exact(2) {
            name.push(u16::from_ne_bytes([chunk[0], chunk[1]]));
        }
        if let Some(name_key) = exact_log_name_units(&name) {
            let id_start = offset + FILE_ID_OFFSET;
            let id_end = id_start + 16;
            let mut file_id = [0_u8; 16];
            file_id.copy_from_slice(buffer.get(id_start..id_end).ok_or_else(invalid)?);
            records.push(DirectoryRecord { name_key, attributes, file_id });
        }
        if next == 0 {
            break;
        }
        if !next.is_multiple_of(8) || next < record_bytes {
            return Err(invalid());
        }
        offset = offset.checked_add(next).ok_or_else(invalid)?;
        if offset >= buffer.len() {
            return Err(invalid());
        }
    }
    Ok(ParsedDirectoryRecords { records, scanned: entries })
}

fn read_buffer_u32(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = buffer.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_ne_bytes(bytes))
}

/// Runs the same handle-bound retention engine synchronously for callers that
/// need an explicit one-shot report. The production logger uses incremental
/// worker maintenance instead.
pub fn prune_diagnostic_logs(directory: &Path) -> DiagnosticRetentionReport {
    let mut writer = match WindowsDailyLogWriter::open(directory) {
        Ok(writer) => writer,
        Err(error) => {
            let mut report = DiagnosticRetentionReport::default();
            report.record_issue(error);
            return report;
        }
    };
    for _ in 0..MAX_SYNCHRONOUS_RETENTION_STEPS {
        if writer.force_retention_step() {
            return writer.retention.report.clone();
        }
    }
    writer.record_retention_issue(DiagnosticFileError::new(
        DiagnosticFileOperation::ReadLogDirectory,
        DiagnosticFileErrorKind::Unsupported,
    ));
    writer.retention.finish_cycle();
    writer.retention.report.clone()
}

fn system_utc_date() -> UtcDate {
    // SAFETY: GetSystemTime has no input pointers and returns initialized UTC.
    let now = unsafe { GetSystemTime() };
    UtcDate { year: now.wYear, month: now.wMonth, day: now.wDay }
}

#[cfg(test)]
fn current_utc_log_key() -> String {
    system_utc_date().log_name()
}

#[cfg(test)]
fn exact_log_name_key(name: &OsStr) -> Option<String> {
    exact_log_name_units(&os_units(name))
}

fn exact_log_name_units(name: &[u16]) -> Option<String> {
    if name.len() != LOG_NAME_LENGTH || !name.iter().all(|unit| *unit <= u16::from(u8::MAX)) {
        return None;
    }
    let bytes: Vec<u8> = name.iter().map(|unit| *unit as u8).collect();
    if !bytes.starts_with(LOG_PREFIX.as_bytes()) || !bytes.ends_with(LOG_SUFFIX.as_bytes()) {
        return None;
    }
    let date = &bytes[LOG_PREFIX.len()..bytes.len() - LOG_SUFFIX.len()];
    valid_iso_date(date).then(|| String::from_utf8(bytes).expect("ASCII log name"))
}

fn valid_iso_date(date: &[u8]) -> bool {
    if date.len() != 10 || date[4] != b'-' || date[7] != b'-' {
        return false;
    }
    if date
        .iter()
        .enumerate()
        .any(|(index, byte)| index != 4 && index != 7 && !byte.is_ascii_digit())
    {
        return false;
    }
    let year = parse_digits(&date[0..4]);
    let month = parse_digits(&date[5..7]);
    let day = parse_digits(&date[8..10]);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn parse_digits(value: &[u8]) -> u32 {
    value.iter().fold(0, |number, digit| number * 10 + u32::from(digit - b'0'))
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Classifies a native absolute path without opening it.
///
/// UNC classification is exposed before export so the UI can state that the
/// user-selected filesystem destination may use the network.
pub fn classify_export_destination(
    destination: &Path,
) -> Result<ExportDestinationKind, DiagnosticFileError> {
    let mut units = path_units_without_nul(destination).map_err(|error| {
        DiagnosticFileError::from_io(DiagnosticFileOperation::NormalizeDestination, &error)
    })?;
    normalize_separators(&mut units);
    if !destination.is_absolute() || has_device_namespace(&units) {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::NormalizeDestination,
            DiagnosticFileErrorKind::InvalidDestination,
        ));
    }
    if is_unc_units(&units) {
        Ok(ExportDestinationKind::Unc)
    } else {
        Ok(ExportDestinationKind::Local)
    }
}

/// Writes a bounded diagnostic artifact through a temporary sibling.
///
/// `owned_files` lists settings, logs, and crash markers that an export must
/// never overwrite. Both normalized path equality and existing-file identity
/// (including hard links) are checked and locked through replacement. A newly
/// created, exclusively held file object is renamed over the destination, so
/// alternate streams from a previous destination object are not inherited.
/// The native Save dialog remains responsible for overwrite confirmation.
pub fn write_diagnostic_export(
    destination: &Path,
    bytes: &[u8],
    owned_files: &[&Path],
) -> Result<ExportWriteOutcome, DiagnosticFileError> {
    if bytes.len() > MAX_DIAGNOSTIC_EXPORT_BYTES {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::WriteTemporary,
            DiagnosticFileErrorKind::TooLarge,
        ));
    }
    if std::str::from_utf8(bytes).is_err() {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::WriteTemporary,
            DiagnosticFileErrorKind::InvalidContent,
        ));
    }
    classify_export_destination(destination)?;
    let parent =
        destination.parent().filter(|parent| !parent.as_os_str().is_empty()).ok_or_else(|| {
            DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            )
        })?;
    let file_name = destination.file_name().ok_or_else(|| {
        DiagnosticFileError::new(
            DiagnosticFileOperation::NormalizeDestination,
            DiagnosticFileErrorKind::InvalidDestination,
        )
    })?;
    let normalized_destination = normalize_absolute(destination)?;
    let destination_name = os_units(file_name);
    let parent_guard = DirectoryGuard::open(parent, DiagnosticFileOperation::InspectDestination)?;
    let destination_kind = parent_guard.transport;
    let current_destination = parent_guard.current_child_path(file_name)?;
    let destination_inspection = inspect_optional_file(
        &current_destination,
        DiagnosticFileOperation::InspectDestination,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;

    let mut owned_file_guards = Vec::with_capacity(owned_files.len());
    let mut owned_parent_guards = Vec::with_capacity(owned_files.len());

    for owned in owned_files {
        let normalized_owned = normalize_absolute(owned).map_err(|mut error| {
            error.operation = DiagnosticFileOperation::InspectOwnedPath;
            error
        })?;
        if native_paths_equal(&normalized_destination, &normalized_owned) {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectOwnedPath,
                DiagnosticFileErrorKind::OwnedDestination,
            ));
        }
        if let (Some(owned_parent), Some(owned_name)) = (owned.parent(), owned.file_name())
            && let Some(owned_parent_guard) = DirectoryGuard::open_optional(
                owned_parent,
                DiagnosticFileOperation::InspectOwnedPath,
            )?
        {
            if parent_guard.identity.same_file(&owned_parent_guard.identity)
                && native_paths_equal(&destination_name, &os_units(owned_name))
            {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::InspectOwnedPath,
                    DiagnosticFileErrorKind::OwnedDestination,
                ));
            }
            owned_parent_guards.push(owned_parent_guard);
        }
        if let Some(owned_file) = inspect_optional_file(
            owned,
            DiagnosticFileOperation::InspectOwnedPath,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )? {
            if destination_inspection.as_ref().is_some_and(|destination_file| {
                destination_file.identity.same_file(&owned_file.identity)
            }) {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::InspectOwnedPath,
                    DiagnosticFileErrorKind::OwnedDestination,
                ));
            }
            owned_file_guards.push(owned_file);
        }
    }

    if !valid_plain_file_name(file_name)
        || !destination
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
    {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::NormalizeDestination,
            DiagnosticFileErrorKind::InvalidDestination,
        ));
    }

    let mut temporary = TemporarySibling::create(&parent_guard)?;
    temporary.write_all(bytes)?;
    temporary.flush_and_sync()?;
    temporary.verify_complete(bytes.len() as u64, &parent_guard)?;

    let expected_destination = destination_inspection.as_ref().map(|file| file.identity);
    let revalidated_path = parent_guard.current_child_path(file_name)?;
    let revalidated_destination = inspect_optional_file(
        &revalidated_path,
        DiagnosticFileOperation::InspectDestination,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    match (expected_destination, revalidated_destination.as_ref()) {
        (None, None) => {}
        (Some(expected), Some(current)) if expected.same_file(&current.identity) => {}
        (None, Some(_)) => {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::AlreadyExists,
            ));
        }
        _ => {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
    }
    for owned in &owned_file_guards {
        if revalidated_destination
            .as_ref()
            .is_some_and(|destination| destination.identity.same_file(&owned.identity))
        {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectOwnedPath,
                DiagnosticFileErrorKind::OwnedDestination,
            ));
        }
    }
    let destination_existed = expected_destination.is_some();
    drop(revalidated_destination);
    drop(destination_inspection);
    temporary.replace(&parent_guard, &destination_name, destination_existed)?;

    Ok(ExportWriteOutcome {
        destination_kind,
        replaced_existing: destination_existed,
        bytes_written: bytes.len() as u64,
    })
}

pub(super) fn os_units(value: &OsStr) -> Vec<u16> {
    value.encode_wide().collect()
}

/// Reads at most `max` bytes from a retained, already-validated file handle.
///
/// The size is read from the same retained handle and the exact expected
/// length is read back; a short or changed read is an integrity failure. This
/// is the shared bounded reader for settings and panic-marker snapshots.
pub(super) fn read_bounded_bytes(
    file: &File,
    max: usize,
    operation: DiagnosticFileOperation,
) -> Result<Box<[u8]>, DiagnosticFileError> {
    let size = file_size(file, operation)?;
    let bounded = usize::try_from(size).unwrap_or(usize::MAX).min(max);
    let mut bytes = vec![0_u8; bounded];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let mut read = 0_u32;
        // SAFETY: the handle remains owned by `file` for this synchronous
        // call; `bytes[offset..]` is a live writable slice and `read` is an
        // exact synchronous output slot; no OVERLAPPED pointer is supplied.
        unsafe {
            ReadFile(file_handle(file), Some(&mut bytes[offset..]), Some(&raw mut read), None)
        }
        .map_err(|error| DiagnosticFileError::from_windows(operation, &error))?;
        if read == 0 {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidContent,
            ));
        }
        offset = offset.checked_add(read as usize).filter(|end| *end <= bytes.len()).ok_or_else(
            || DiagnosticFileError::new(operation, DiagnosticFileErrorKind::InvalidContent),
        )?;
    }
    if size > max as u64 {
        return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::TooLarge));
    }
    Ok(bytes.into_boxed_slice())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativeFileIdentity {
    legacy_volume: u32,
    legacy_file_id: u64,
    extended: Option<(u64, [u8; 16])>,
}

impl NativeFileIdentity {
    pub(super) fn same_file(self, other: &Self) -> bool {
        match (self.extended, other.extended) {
            (Some(left), Some(right)) => left == right,
            _ => {
                self.legacy_volume == other.legacy_volume
                    && self.legacy_file_id == other.legacy_file_id
            }
        }
    }
}

pub(super) struct InspectedFile {
    _file: File,
    identity: NativeFileIdentity,
}

impl InspectedFile {
    pub(super) fn new(file: File, identity: NativeFileIdentity) -> Self {
        Self { _file: file, identity }
    }

    pub(super) fn identity(&self) -> NativeFileIdentity {
        self.identity
    }

    pub(super) fn file(&self) -> &File {
        &self._file
    }
}

pub(super) struct HandleInspection {
    identity: NativeFileIdentity,
    attributes: u32,
    link_count: u32,
}

impl HandleInspection {
    pub(super) fn identity(&self) -> NativeFileIdentity {
        self.identity
    }

    pub(super) fn link_count(&self) -> u32 {
        self.link_count
    }
}

pub(super) fn inspect_optional_file(
    path: &Path,
    operation: DiagnosticFileOperation,
    share_mode: FILE_SHARE_MODE,
) -> Result<Option<InspectedFile>, DiagnosticFileError> {
    let raw = extended_native_path_units_with_nul(path)?;
    let file = match open_native_file(
        &raw,
        FILE_READ_ATTRIBUTES.0,
        share_mode,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        operation,
    ) {
        Ok(file) => file,
        Err(error) if error.kind == DiagnosticFileErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let info = inspect_handle(&file, operation)?;
    validate_regular_non_reparse(&info, operation)?;
    Ok(Some(InspectedFile { _file: file, identity: info.identity }))
}

pub(super) fn inspect_handle(
    file: &File,
    operation: DiagnosticFileOperation,
) -> Result<HandleInspection, DiagnosticFileError> {
    let handle = file_handle(file);
    let mut legacy = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is owned by `file`; `legacy` is the exact output type.
    unsafe { GetFileInformationByHandle(handle, &raw mut legacy) }
        .map_err(|error| DiagnosticFileError::from_windows(operation, &error))?;

    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the handle stays valid and `tag` is an exact writable output.
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|error| DiagnosticFileError::from_windows(operation, &error))?;

    let mut id = FILE_ID_INFO::default();
    // SAFETY: the handle stays valid and `id` is an exact writable output.
    let extended = match unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut id).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } {
        Ok(()) => Some((id.VolumeSerialNumber, id.FileId.Identifier)),
        Err(error) if file_id_fallback_allowed(windows_os_code(&error)) => None,
        Err(error) => return Err(DiagnosticFileError::from_windows(operation, &error)),
    };
    let legacy_file_id = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
    if extended.is_none() && legacy_file_id == 0 {
        return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::Unsupported));
    }
    Ok(HandleInspection {
        identity: NativeFileIdentity {
            legacy_volume: legacy.dwVolumeSerialNumber,
            legacy_file_id,
            extended,
        },
        attributes: tag.FileAttributes,
        link_count: legacy.nNumberOfLinks,
    })
}

pub(super) fn validate_regular_non_reparse(
    info: &HandleInspection,
    operation: DiagnosticFileOperation,
) -> Result<(), DiagnosticFileError> {
    if info.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::ReparsePoint));
    }
    if info.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return Err(DiagnosticFileError::new(
            operation,
            DiagnosticFileErrorKind::InvalidDestination,
        ));
    }
    Ok(())
}

fn file_id_fallback_allowed(code: i32) -> bool {
    let code = code as u32;
    code == ERROR_INVALID_FUNCTION.0
        || code == ERROR_INVALID_PARAMETER.0
        || code == ERROR_NOT_SUPPORTED.0
}

pub(super) struct DirectoryGuard {
    file: File,
    identity: NativeFileIdentity,
    final_path: Box<[u16]>,
    transport: ExportDestinationKind,
}

impl DirectoryGuard {
    pub(super) fn open(
        path: &Path,
        operation: DiagnosticFileOperation,
    ) -> Result<Self, DiagnosticFileError> {
        let raw = extended_native_path_units_with_nul(path)?;
        let file = open_native_file(
            &raw,
            FILE_READ_ATTRIBUTES.0 | FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            operation,
        )?;
        let info = inspect_handle(&file, operation)?;
        if info.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::ReparsePoint));
        }
        if info.attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        let mut buffer = [0_u16; MAX_NATIVE_PATH_UNITS];
        let length = final_path_into(file_handle(&file), &mut buffer, operation)?;
        let final_path = trim_final_path(&buffer[..length]).to_vec().into_boxed_slice();
        let transport = classify_final_transport(&final_path).ok_or_else(|| {
            DiagnosticFileError::new(operation, DiagnosticFileErrorKind::InvalidDestination)
        })?;
        Ok(Self { file, identity: info.identity, final_path, transport })
    }

    fn open_optional(
        path: &Path,
        operation: DiagnosticFileOperation,
    ) -> Result<Option<Self>, DiagnosticFileError> {
        match Self::open(path, operation) {
            Ok(directory) => Ok(Some(directory)),
            Err(error) if error.kind == DiagnosticFileErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn handle(&self) -> HANDLE {
        file_handle(&self.file)
    }

    pub(super) fn is_local(&self) -> bool {
        self.transport == ExportDestinationKind::Local
    }

    pub(super) fn extended_identity(
        &self,
        operation: DiagnosticFileOperation,
    ) -> Result<(u64, [u8; 16]), DiagnosticFileError> {
        self.identity.extended.ok_or_else(|| {
            DiagnosticFileError::new(operation, DiagnosticFileErrorKind::Unsupported)
        })
    }

    pub(super) fn ensure_location_stable(
        &self,
        operation: DiagnosticFileOperation,
    ) -> Result<(), DiagnosticFileError> {
        let mut current = [0_u16; MAX_NATIVE_PATH_UNITS];
        let length = final_path_into(self.handle(), &mut current, operation)?;
        let current = trim_final_path(&current[..length]);
        if !native_paths_equal(&self.final_path, current) {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        Ok(())
    }

    pub(super) fn validate_direct_child_handle(
        &self,
        child: HANDLE,
        operation: DiagnosticFileOperation,
    ) -> Result<(), DiagnosticFileError> {
        self.ensure_location_stable(operation)?;
        let mut child_path = [0_u16; MAX_NATIVE_PATH_UNITS];
        let length = final_path_into(child, &mut child_path, operation)?;
        let child_path = trim_final_path(&child_path[..length]);
        let Some(separator) = child_path.iter().rposition(|unit| *unit == b'\\' as u16) else {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        };
        if !native_paths_equal(&self.final_path, &child_path[..separator]) {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        Ok(())
    }

    pub(super) fn direct_child_name(
        &self,
        child: HANDLE,
        operation: DiagnosticFileOperation,
    ) -> Result<Box<[u16]>, DiagnosticFileError> {
        self.ensure_location_stable(operation)?;
        let mut child_path = [0_u16; MAX_NATIVE_PATH_UNITS];
        let length = final_path_into(child, &mut child_path, operation)?;
        let child_path = trim_final_path(&child_path[..length]);
        let Some(separator) = child_path.iter().rposition(|unit| *unit == b'\\' as u16) else {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        };
        if !native_paths_equal(&self.final_path, &child_path[..separator]) {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        Ok(child_path[separator + 1..].into())
    }

    fn validate_direct_child_handle_named(
        &self,
        child: HANDLE,
        expected_name: &OsStr,
        operation: DiagnosticFileOperation,
    ) -> Result<(), DiagnosticFileError> {
        self.validate_direct_child_handle_units(child, &os_units(expected_name), operation)
    }

    /// Allocation-free form of [`Self::validate_direct_child_handle_named`]
    /// for callers that precomputed the expected UTF-16 name, such as the
    /// panic hook.
    pub(super) fn validate_direct_child_handle_units(
        &self,
        child: HANDLE,
        expected_name: &[u16],
        operation: DiagnosticFileOperation,
    ) -> Result<(), DiagnosticFileError> {
        self.ensure_location_stable(operation)?;
        let mut child_path = [0_u16; MAX_NATIVE_PATH_UNITS];
        let length = final_path_into(child, &mut child_path, operation)?;
        let child_path = trim_final_path(&child_path[..length]);
        let Some(separator) = child_path.iter().rposition(|unit| *unit == b'\\' as u16) else {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        };
        if !native_paths_equal(&self.final_path, &child_path[..separator])
            || !native_paths_equal(expected_name, &child_path[separator + 1..])
        {
            return Err(DiagnosticFileError::new(
                operation,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        Ok(())
    }

    pub(super) fn current_child_path(&self, name: &OsStr) -> Result<PathBuf, DiagnosticFileError> {
        let units = self.absolute_child_units(&os_units(name))?;
        Ok(PathBuf::from(OsString::from_wide(&units)))
    }

    fn absolute_child_units(&self, name: &[u16]) -> Result<Vec<u16>, DiagnosticFileError> {
        self.absolute_child_units_for(name, DiagnosticFileOperation::InspectDestination)
    }

    fn absolute_child_units_for(
        &self,
        name: &[u16],
        operation: DiagnosticFileOperation,
    ) -> Result<Vec<u16>, DiagnosticFileError> {
        self.ensure_location_stable(operation)?;
        let mut units = self.final_path.to_vec();
        if !matches!(units.last(), Some(unit) if *unit == b'\\' as u16) {
            units.push(b'\\' as u16);
        }
        units.extend_from_slice(name);
        if units.len() >= MAX_NATIVE_PATH_UNITS {
            return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::TooLarge));
        }
        Ok(units)
    }
}

pub(super) fn open_native_file(
    path: &[u16],
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: FILE_CREATION_DISPOSITION,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
    operation: DiagnosticFileOperation,
) -> Result<File, DiagnosticFileError> {
    // SAFETY: `path` is a live NUL-terminated buffer. The returned handle is
    // immediately transferred into `OwnedHandle`, establishing single-owner
    // RAII before any fallible operation can occur.
    let handle = unsafe {
        CreateFileW(PCWSTR(path.as_ptr()), access, share, None, disposition, flags, None)
    }
    .map_err(|error| DiagnosticFileError::from_windows(operation, &error))?;
    // SAFETY: `handle` was just returned as owned by CreateFileW and has not
    // been copied into another owner.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    Ok(File::from(owned))
}

pub(super) fn file_handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

fn final_path_into(
    handle: HANDLE,
    output: &mut [u16],
    operation: DiagnosticFileOperation,
) -> Result<usize, DiagnosticFileError> {
    // SAFETY: `handle` is a live file handle and `output` is a valid exact
    // mutable slice for this synchronous call.
    let written = unsafe { GetFinalPathNameByHandleW(handle, output, FILE_NAME_NORMALIZED) };
    if written == 0 {
        return Err(DiagnosticFileError::from_io(operation, &io::Error::last_os_error()));
    }
    if written as usize >= output.len() {
        return Err(DiagnosticFileError::new(operation, DiagnosticFileErrorKind::TooLarge));
    }
    Ok(written as usize)
}

fn trim_final_path(mut path: &[u16]) -> &[u16] {
    while path.len() > 3 && matches!(path.last(), Some(unit) if *unit == b'\\' as u16) {
        path = &path[..path.len() - 1];
    }
    path
}

fn classify_final_transport(path: &[u16]) -> Option<ExportDestinationKind> {
    if starts_with_ascii_case_insensitive(path, r"\\?\UNC\")
        || (path.starts_with(&[b'\\' as u16, b'\\' as u16])
            && !starts_with_ascii_case_insensitive(path, r"\\?\"))
    {
        Some(ExportDestinationKind::Unc)
    } else if starts_with_ascii_case_insensitive(path, r"\\?\")
        || (path.len() >= 3
            && u8::try_from(path[0]).is_ok_and(|unit| unit.is_ascii_alphabetic())
            && path[1] == b':' as u16
            && path[2] == b'\\' as u16)
    {
        Some(ExportDestinationKind::Local)
    } else {
        None
    }
}

pub(super) fn delete_file_handle(
    file: &File,
    operation: DiagnosticFileOperation,
) -> Result<(), DiagnosticFileError> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle remains live; `disposition` is the exact input type
    // for FileDispositionInfo and its size is passed verbatim.
    unsafe {
        SetFileInformationByHandle(
            file_handle(file),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|error| DiagnosticFileError::from_windows(operation, &error))
}

#[derive(Clone)]
pub(super) struct RenameBuffer {
    words: Box<[usize]>,
    byte_len: u32,
}
impl RenameBuffer {
    /// Precomputes a handle-relative `FileRenameInformationEx` request (ADR
    /// 0019). The retained parent handle is supplied as `RootDirectory` and
    /// `name` is a plain relative component, so the namespace commit never
    /// re-resolves an untrusted path. A replacing rename uses
    /// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` with POSIX semantics; a
    /// create-only rename uses no flags, making a concurrently created entry
    /// fail the rename atomically.
    ///
    /// The request is applied through `NtSetInformationFile`. The Win32
    /// wrapper `SetFileInformationByHandle` rejects every rename whose
    /// `RootDirectory` is not null with `ERROR_INVALID_PARAMETER`, for both
    /// `FileRenameInfo` and `FileRenameInfoEx`, so it cannot express the
    /// retained-parent contract at all.
    pub(super) fn new(
        root: HANDLE,
        name: &[u16],
        replace_existing: bool,
    ) -> Result<Self, DiagnosticFileError> {
        if name.is_empty() || name.len() >= MAX_NATIVE_PATH_UNITS || name.contains(&0) {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::NormalizeDestination,
                DiagnosticFileErrorKind::InvalidDestination,
            ));
        }
        let byte_len = size_of::<FILE_RENAME_INFO>()
            .saturating_sub(size_of::<u16>())
            .checked_add(name.len().saturating_mul(size_of::<u16>()))
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| {
                DiagnosticFileError::new(
                    DiagnosticFileOperation::NormalizeDestination,
                    DiagnosticFileErrorKind::TooLarge,
                )
            })?;
        let word_count = (byte_len as usize).div_ceil(size_of::<usize>());
        let mut words = vec![0_usize; word_count].into_boxed_slice();
        // SAFETY: `words` is suitably aligned and large enough for the header
        // plus every UTF-16 name unit. The `FILE_RENAME_INFO` layout matches
        // the documented `FILE_RENAME_INFO_EX` layout (4-byte flags union,
        // then an aligned `HANDLE`, length, and inline name), so the same
        // buffer is valid for the extended information class. All fields are
        // initialized before use.
        unsafe {
            let info = words.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            let flags = if replace_existing {
                FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS
            } else {
                0
            };
            (*info).Anonymous = FILE_RENAME_INFO_0 { Flags: flags };
            (*info).RootDirectory = root;
            (*info).FileNameLength = size_of_val(name) as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                (&raw mut (*info).FileName).cast::<u16>(),
                name.len(),
            );
        }
        Ok(Self { words, byte_len })
    }

    pub(super) fn apply(&self, file: HANDLE) -> Result<(), DiagnosticFileError> {
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: the aligned buffer was fully initialized for
        // FileRenameInformationEx, the root directory and source handles
        // remain live, the exact byte length is retained with the allocation,
        // and the status block outlives the synchronous call.
        let status = unsafe {
            NtSetInformationFile(
                file,
                &raw mut status_block,
                self.words.as_ptr().cast(),
                self.byte_len,
                FileRenameInformationEx,
            )
        };
        if status.is_ok() {
            return Ok(());
        }
        // SAFETY: pure table lookup on a status value; no memory is touched.
        let os_code = unsafe { RtlNtStatusToDosError(status) };
        Err(DiagnosticFileError::from_os_code(
            DiagnosticFileOperation::ReplaceDestination,
            os_code as i32,
        ))
    }
}

pub(super) fn valid_plain_file_name(name: &OsStr) -> bool {
    let units = os_units(name);
    if units.is_empty()
        || units.len() > 255
        || units.iter().any(|unit| *unit < 32 || matches!(*unit, 47 | 58 | 92))
        || matches!(units.last(), Some(unit) if matches!(*unit, 32 | 46))
    {
        return false;
    }
    let base_end = units.iter().position(|unit| *unit == b'.' as u16).unwrap_or(units.len());
    let Some(base) = units[..base_end]
        .iter()
        .map(|unit| u8::try_from(*unit).ok().map(|byte| byte.to_ascii_uppercase()))
        .collect::<Option<Vec<_>>>()
    else {
        return true;
    };
    !matches!(base.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL")
        && !(base.len() == 4
            && matches!(&base[..3], b"COM" | b"LPT")
            && matches!(base[3], b'1'..=b'9'))
}

pub(super) fn normalize_absolute(path: &Path) -> Result<Vec<u16>, DiagnosticFileError> {
    let mut input = path_units_with_nul(path).map_err(|error| {
        DiagnosticFileError::from_io(DiagnosticFileOperation::NormalizeDestination, &error)
    })?;
    let without_nul = input.len().saturating_sub(1);
    normalize_separators(&mut input[..without_nul]);
    if !path.is_absolute() || has_device_namespace(&input[..input.len().saturating_sub(1)]) {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::NormalizeDestination,
            DiagnosticFileErrorKind::InvalidDestination,
        ));
    }

    // A zero-sized query returns the required capacity including the NUL.
    // SAFETY: `input` is NUL-terminated and remains live throughout the call.
    let required = unsafe { GetFullPathNameW(PCWSTR(input.as_ptr()), None, None) };
    if required == 0 {
        return Err(DiagnosticFileError::from_io(
            DiagnosticFileOperation::NormalizeDestination,
            &io::Error::last_os_error(),
        ));
    }
    let mut output = vec![0_u16; required as usize];
    // SAFETY: the input is live and `output` is a writable buffer described by
    // its actual slice length. No file-part pointer is requested.
    let written = unsafe { GetFullPathNameW(PCWSTR(input.as_ptr()), Some(&mut output), None) };
    if written == 0 || written as usize >= output.len() {
        return Err(DiagnosticFileError::from_io(
            DiagnosticFileOperation::NormalizeDestination,
            &io::Error::last_os_error(),
        ));
    }
    output.truncate(written as usize);
    normalize_extended_prefix(&mut output);
    while output.len() > 3 && matches!(output.last(), Some(unit) if *unit == b'\\' as u16) {
        output.pop();
    }
    Ok(output)
}

pub(super) fn extended_native_path_units_with_nul(
    path: &Path,
) -> Result<Vec<u16>, DiagnosticFileError> {
    let normalized = normalize_absolute(path)?;
    let mut native = Vec::with_capacity(normalized.len().saturating_add(8));
    if normalized.starts_with(&[b'\\' as u16, b'\\' as u16]) {
        native.extend(r"\\?\UNC\".encode_utf16());
        native.extend_from_slice(&normalized[2..]);
    } else {
        native.extend(r"\\?\".encode_utf16());
        native.extend_from_slice(&normalized);
    }
    if native.len() >= MAX_NATIVE_PATH_UNITS {
        return Err(DiagnosticFileError::new(
            DiagnosticFileOperation::NormalizeDestination,
            DiagnosticFileErrorKind::TooLarge,
        ));
    }
    native.push(0);
    Ok(native)
}

pub(super) fn native_paths_equal(left: &[u16], right: &[u16]) -> bool {
    // SAFETY: both slices are live, explicitly sized UTF-16 strings. Ordinal
    // comparison does not require NUL termination and performs no allocation.
    unsafe { CompareStringOrdinal(left, right, true) == CSTR_EQUAL }
}

fn normalize_extended_prefix(units: &mut Vec<u16>) {
    normalize_separators(units);
    if starts_with_ascii_case_insensitive(units, r"\\?\UNC\") {
        units.drain(..8);
        units.splice(0..0, [b'\\' as u16, b'\\' as u16]);
    } else if starts_with_ascii_case_insensitive(units, r"\\?\") {
        units.drain(..4);
    }
}

fn normalize_separators(units: &mut [u16]) {
    for unit in units {
        if *unit == b'/' as u16 {
            *unit = b'\\' as u16;
        }
    }
}

fn path_units_without_nul(path: &Path) -> io::Result<Vec<u16>> {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    } else {
        Ok(units)
    }
}

fn path_units_with_nul(path: &Path) -> io::Result<Vec<u16>> {
    let mut units = path_units_without_nul(path)?;
    units.push(0);
    Ok(units)
}

fn has_device_namespace(units: &[u16]) -> bool {
    starts_with_ascii_case_insensitive(units, r"\\.\")
        || (starts_with_ascii_case_insensitive(units, r"\\?\")
            && !starts_with_ascii_case_insensitive(units, r"\\?\UNC\")
            && !(units.len() >= 7
                && u8::try_from(units[4]).is_ok_and(|unit| unit.is_ascii_alphabetic())
                && units[5] == b':' as u16
                && units[6] == b'\\' as u16))
}

fn is_unc_units(units: &[u16]) -> bool {
    (units.starts_with(&[b'\\' as u16, b'\\' as u16])
        && !starts_with_ascii_case_insensitive(units, r"\\?\"))
        || starts_with_ascii_case_insensitive(units, r"\\?\UNC\")
}

fn starts_with_ascii_case_insensitive(units: &[u16], prefix: &str) -> bool {
    let prefix = prefix.as_bytes();
    units.len() >= prefix.len()
        && units.iter().zip(prefix).all(|(unit, byte)| {
            u8::try_from(*unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(byte))
        })
}

struct TemporarySibling {
    file: Option<File>,
    identity: NativeFileIdentity,
    recovery_rename: RenameBuffer,
    expected_size: Option<u64>,
    active: bool,
}

impl TemporarySibling {
    fn create(parent: &DirectoryGuard) -> Result<Self, DiagnosticFileError> {
        let start = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, AtomicOrdering::Relaxed);
        for offset in 0..TEMP_CREATE_ATTEMPTS {
            let name = OsString::from(format!(
                ".diskpie-diagnostics-{}-{}.tmp",
                std::process::id(),
                start.wrapping_add(offset)
            ));
            let path = parent.current_child_path(&name)?;
            let raw = extended_native_path_units_with_nul(&path)?;
            let file = match open_native_file(
                &raw,
                GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | DELETE.0,
                FILE_SHARE_NONE,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
                DiagnosticFileOperation::CreateTemporary,
            ) {
                Ok(file) => file,
                Err(error) if error.kind == DiagnosticFileErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let info = inspect_handle(&file, DiagnosticFileOperation::CreateTemporary)?;
            validate_regular_non_reparse(&info, DiagnosticFileOperation::CreateTemporary)?;
            if info.link_count != 1 {
                let _ignored = delete_file_handle(&file, DiagnosticFileOperation::CreateTemporary);
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::CreateTemporary,
                    DiagnosticFileErrorKind::Unsupported,
                ));
            }
            if let Err(error) = parent.validate_direct_child_handle(
                file_handle(&file),
                DiagnosticFileOperation::CreateTemporary,
            ) {
                let _ignored = delete_file_handle(&file, DiagnosticFileOperation::CreateTemporary);
                return Err(error);
            }
            let recovery_rename = match RenameBuffer::new(parent.handle(), &os_units(&name), false)
            {
                Ok(rename) => rename,
                Err(error) => {
                    let _ignored =
                        delete_file_handle(&file, DiagnosticFileOperation::CreateTemporary);
                    return Err(error);
                }
            };
            return Ok(Self {
                file: Some(file),
                identity: info.identity,
                recovery_rename,
                expected_size: None,
                active: true,
            });
        }
        Err(DiagnosticFileError::new(
            DiagnosticFileOperation::CreateTemporary,
            DiagnosticFileErrorKind::AlreadyExists,
        ))
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), DiagnosticFileError> {
        let Some(file) = self.file.as_mut() else {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::WriteTemporary,
                DiagnosticFileErrorKind::Io,
            ));
        };
        file.write_all(bytes).map_err(|error| {
            DiagnosticFileError::from_io(DiagnosticFileOperation::WriteTemporary, &error)
        })
    }

    fn flush_and_sync(&mut self) -> Result<(), DiagnosticFileError> {
        let Some(file) = self.file.as_mut() else {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::FlushTemporary,
                DiagnosticFileErrorKind::Io,
            ));
        };
        file.flush().and_then(|()| file.sync_all()).map_err(|error| {
            DiagnosticFileError::from_io(DiagnosticFileOperation::FlushTemporary, &error)
        })
    }

    fn verify_complete(
        &mut self,
        expected_size: u64,
        parent: &DirectoryGuard,
    ) -> Result<(), DiagnosticFileError> {
        let Some(file) = self.file.as_ref() else {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::Io,
            ));
        };
        let info = inspect_handle(file, DiagnosticFileOperation::InspectDestination)?;
        validate_regular_non_reparse(&info, DiagnosticFileOperation::InspectDestination)?;
        if !self.identity.same_file(&info.identity)
            || info.link_count != 1
            || file_size(file, DiagnosticFileOperation::InspectDestination)? != expected_size
        {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::InvalidContent,
            ));
        }
        parent.validate_direct_child_handle(
            file_handle(file),
            DiagnosticFileOperation::InspectDestination,
        )?;
        self.expected_size = Some(expected_size);
        Ok(())
    }

    fn replace(
        &mut self,
        parent: &DirectoryGuard,
        destination_name: &[u16],
        destination_exists: bool,
    ) -> Result<(), DiagnosticFileError> {
        parent.ensure_location_stable(DiagnosticFileOperation::ReplaceDestination)?;
        let Some(file) = self.file.as_ref() else {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::ReplaceDestination,
                DiagnosticFileErrorKind::Io,
            ));
        };
        let rename = RenameBuffer::new(parent.handle(), destination_name, destination_exists)?;
        if let Err(error) = rename.apply(file_handle(file)) {
            if preserve_after_failed_replacement(&error, || {
                let _recovery = self.recovery_rename.apply(file_handle(file));
            }) {
                self.active = false;
            }
            return Err(error);
        }
        self.active = false;
        let info = inspect_handle(file, DiagnosticFileOperation::ReplaceDestination)?;
        let size = file_size(file, DiagnosticFileOperation::ReplaceDestination)?;
        if !self.identity.same_file(&info.identity)
            || self.expected_size != Some(size)
            || info.link_count != 1
        {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::ReplaceDestination,
                DiagnosticFileErrorKind::InvalidContent,
            ));
        }
        Ok(())
    }
}

impl Drop for TemporarySibling {
    fn drop(&mut self) {
        if self.active
            && let Some(file) = self.file.as_ref()
        {
            let _ignored = delete_file_handle(file, DiagnosticFileOperation::ReplaceDestination);
        }
        self.file = None;
    }
}

pub(super) fn file_size(
    file: &File,
    operation: DiagnosticFileOperation,
) -> Result<u64, DiagnosticFileError> {
    let mut size = 0_i64;
    // SAFETY: the file handle stays valid and `size` is an exact output slot.
    unsafe { GetFileSizeEx(file_handle(file), &raw mut size) }
        .map_err(|error| DiagnosticFileError::from_windows(operation, &error))?;
    u64::try_from(size).map_err(|_error| {
        DiagnosticFileError::new(operation, DiagnosticFileErrorKind::InvalidContent)
    })
}

fn preserve_after_failed_replacement(error: &DiagnosticFileError, recover: impl FnOnce()) -> bool {
    if error.kind != DiagnosticFileErrorKind::PartialReplacement {
        return false;
    }
    recover();
    true
}

struct NativeMarkerFile {
    file: File,
    identity: NativeFileIdentity,
}

impl NativeMarkerFile {
    fn create(path: &[u16]) -> Result<Self, DiagnosticFileError> {
        // SAFETY: `path` is a live NUL-terminated buffer precomputed before
        // hook installation. CREATE_NEW and FILE_SHARE_NONE prevent another
        // handle from swapping, extending, or adding an ADS to the sibling.
        let file = open_native_file(
            path,
            GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | DELETE.0,
            FILE_SHARE_NONE,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
            DiagnosticFileOperation::CreateTemporary,
        )?;
        let info = inspect_handle(&file, DiagnosticFileOperation::CreateTemporary)?;
        validate_regular_non_reparse(&info, DiagnosticFileOperation::CreateTemporary)?;
        if info.link_count != 1 {
            let _ignored = delete_file_handle(&file, DiagnosticFileOperation::CreateTemporary);
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::CreateTemporary,
                DiagnosticFileErrorKind::Unsupported,
            ));
        }
        Ok(Self { file, identity: info.identity })
    }

    fn handle(&self) -> HANDLE {
        file_handle(&self.file)
    }

    fn identity(&self) -> NativeFileIdentity {
        self.identity
    }

    /// Writes, flushes, and verifies the complete payload through the owned
    /// handle without touching the namespace.
    fn fill_and_verify(&self, contents: &[u8]) -> Result<(), DiagnosticFileError> {
        self.write_all(contents)?;
        self.flush()?;
        self.verify_complete(contents.len() as u64)
    }

    fn write_all(&self, mut contents: &[u8]) -> Result<(), DiagnosticFileError> {
        while !contents.is_empty() {
            let mut written = 0_u32;
            // SAFETY: the owned handle remains valid, `contents` is a live
            // bounded slice, `written` is an exact synchronous output slot,
            // and no OVERLAPPED pointer is used.
            unsafe { WriteFile(self.handle(), Some(contents), Some(&raw mut written), None) }
                .map_err(|error| {
                    DiagnosticFileError::from_windows(
                        DiagnosticFileOperation::WriteTemporary,
                        &error,
                    )
                })?;
            if written == 0 || written as usize > contents.len() {
                return Err(DiagnosticFileError::new(
                    DiagnosticFileOperation::WriteTemporary,
                    DiagnosticFileErrorKind::Io,
                ));
            }
            contents = &contents[written as usize..];
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), DiagnosticFileError> {
        // SAFETY: the handle remains owned and open for this synchronous call.
        unsafe { FlushFileBuffers(self.handle()) }.map_err(|error| {
            DiagnosticFileError::from_windows(DiagnosticFileOperation::FlushTemporary, &error)
        })
    }

    fn verify_complete(&self, expected_size: u64) -> Result<(), DiagnosticFileError> {
        let info = inspect_handle(&self.file, DiagnosticFileOperation::InspectDestination)?;
        if !self.identity.same_file(&info.identity)
            || info.link_count != 1
            || file_size(&self.file, DiagnosticFileOperation::InspectDestination)? != expected_size
        {
            return Err(DiagnosticFileError::new(
                DiagnosticFileOperation::InspectDestination,
                DiagnosticFileErrorKind::InvalidContent,
            ));
        }
        validate_regular_non_reparse(&info, DiagnosticFileOperation::InspectDestination)
    }

    fn delete_exact(self) -> Result<(), DiagnosticFileError> {
        delete_file_handle(&self.file, DiagnosticFileOperation::ReplaceDestination)
    }
}

fn write_native_marker(
    file: NativeMarkerFile,
    parent: &DirectoryGuard,
    target_rename: &RenameBuffer,
    recovery_rename: &RenameBuffer,
    contents: &[u8],
) -> Result<(), DiagnosticFileError> {
    if let Err(error) = file.fill_and_verify(contents) {
        let _cleanup = file.delete_exact();
        return Err(error);
    }
    if let Err(error) = parent.ensure_location_stable(DiagnosticFileOperation::ReplaceDestination) {
        let _cleanup = file.delete_exact();
        return Err(error);
    }
    if let Err(error) = target_rename.apply(file.handle()) {
        if preserve_after_failed_replacement(&error, || {
            let _recovery = recovery_rename.apply(file.handle());
        }) {
            drop(file);
        } else {
            let _cleanup = file.delete_exact();
        }
        return Err(error);
    }
    file.verify_complete(contents.len() as u64)
}

fn diagnostic_io_error(error: DiagnosticFileError) -> io::Error {
    io::Error::other(error)
}

fn saturating_atomic_add(counter: &AtomicU64, amount: u64) {
    let _ignored = counter.fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |value| {
        Some(value.saturating_add(amount))
    });
}

fn windows_os_code(error: &windows::core::Error) -> i32 {
    let hresult = error.code().0;
    let raw = hresult as u32;
    if raw & 0xFFFF_0000 == 0x8007_0000 { (raw & 0xFFFF) as i32 } else { hresult }
}

fn is_partial_replace_code(code: i32) -> bool {
    let code = code as u32;
    code == ERROR_UNABLE_TO_REMOVE_REPLACED.0
        || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT.0
        || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT_2.0
}

fn classify_io_error(error: &io::Error) -> DiagnosticFileErrorKind {
    match error.kind() {
        io::ErrorKind::PermissionDenied => DiagnosticFileErrorKind::AccessDenied,
        io::ErrorKind::NotFound => DiagnosticFileErrorKind::NotFound,
        io::ErrorKind::AlreadyExists => DiagnosticFileErrorKind::AlreadyExists,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
            DiagnosticFileErrorKind::InvalidDestination
        }
        io::ErrorKind::Unsupported => DiagnosticFileErrorKind::Unsupported,
        _ => DiagnosticFileErrorKind::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cmp::Ordering,
        ffi::OsString,
        fs::{self, File, OpenOptions},
        os::windows::{
            ffi::OsStringExt,
            fs::{OpenOptionsExt, symlink_dir, symlink_file},
        },
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
        },
    };
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diskpie-diagnostics-{label}-{}-{sequence}", std::process::id()));
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

    enum PrivilegedFixture {
        Exercised,
        Skipped(i32),
    }

    fn classify_privileged_fixture(result: io::Result<()>) -> PrivilegedFixture {
        match result {
            Ok(()) => PrivilegedFixture::Exercised,
            Err(error) if matches!(error.raw_os_error(), Some(5 | 1_314)) => {
                PrivilegedFixture::Skipped(error.raw_os_error().expect("matched OS code"))
            }
            Err(error) => panic!("create privileged filesystem fixture: {error}"),
        }
    }

    fn require_exercised(fixture: PrivilegedFixture) -> bool {
        match fixture {
            PrivilegedFixture::Exercised => true,
            PrivilegedFixture::Skipped(code) => {
                assert!(matches!(code, 5 | 1_314));
                eprintln!("privilege-dependent fixture skipped with expected OS code {code}");
                false
            }
        }
    }

    fn exact_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(directory)
            .expect("read test directory")
            .filter_map(Result::ok)
            .filter_map(|entry| exact_log_name_key(&entry.file_name()))
            .collect();
        names.sort();
        names
    }

    fn injected_clock(date: UtcDate) -> (Arc<Mutex<UtcDate>>, UtcClock) {
        let state = Arc::new(Mutex::new(date));
        let clock_state = Arc::clone(&state);
        let clock: UtcClock = Arc::new(move || *clock_state.lock().expect("test UTC clock"));
        (state, clock)
    }

    fn enable_case_sensitive_directory(path: &Path) -> Result<(), DiagnosticFileError> {
        use windows::Win32::Storage::FileSystem::{
            FILE_CASE_SENSITIVE_INFO, FileCaseSensitiveInfo,
        };

        let raw = extended_native_path_units_with_nul(path)?;
        let file = open_native_file(
            &raw,
            GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            DiagnosticFileOperation::InspectDestination,
        )?;
        let information = FILE_CASE_SENSITIVE_INFO { Flags: 1 };
        // SAFETY: `file` remains live and `information` is the exact input
        // structure documented for FileCaseSensitiveInfo.
        unsafe {
            SetFileInformationByHandle(
                file_handle(&file),
                FileCaseSensitiveInfo,
                (&raw const information).cast(),
                size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
            )
        }
        .map_err(|error| {
            DiagnosticFileError::from_windows(DiagnosticFileOperation::InspectDestination, &error)
        })
    }

    fn write_directory_record(
        buffer: &mut DirectoryQueryBuffer,
        offset: usize,
        next: usize,
        name: &[u16],
        attributes: u32,
        id_byte: u8,
    ) {
        let name_start = offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        buffer.0[offset..offset + 4].copy_from_slice(&(next as u32).to_ne_bytes());
        let attributes_offset = offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes);
        buffer.0[attributes_offset..attributes_offset + 4]
            .copy_from_slice(&attributes.to_ne_bytes());
        let name_length_offset = offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength);
        buffer.0[name_length_offset..name_length_offset + 4]
            .copy_from_slice(&((name.len() * 2) as u32).to_ne_bytes());
        let id_start = offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
        buffer.0[id_start..id_start + 16].fill(id_byte);
        for (index, unit) in name.iter().enumerate() {
            let start = name_start + index * 2;
            buffer.0[start..start + 2].copy_from_slice(&unit.to_ne_bytes());
        }
    }

    const fn aligned_record_bytes(name_units: usize) -> usize {
        let raw = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) + name_units * 2;
        (raw + 7) & !7
    }

    #[test]
    fn exact_log_name_requires_a_real_calendar_date() {
        assert!(exact_log_name_key(OsStr::new("diskpie.2024-02-29.log")).is_some());
        assert!(exact_log_name_key(OsStr::new("diskpie.2023-02-29.log")).is_none());
        assert!(exact_log_name_key(OsStr::new("diskpie.2024-13-01.log")).is_none());
        assert!(exact_log_name_key(OsStr::new("DiskPie.2024-01-01.log")).is_none());
        assert!(exact_log_name_key(OsStr::new("diskpie.2024-01-01.log.bak")).is_none());
    }

    #[test]
    fn retention_keeps_only_eight_newest_exact_regular_files() {
        let directory = TestDirectory::new("retention");
        for day in 1..=12 {
            fs::write(directory.path().join(format!("diskpie.2000-07-{day:02}.log")), b"log")
                .expect("write log fixture");
        }
        fs::write(directory.path().join("diskpie.2000-07-01.log.bak"), b"unrelated")
            .expect("write unrelated fixture");

        let report = prune_diagnostic_logs(directory.path());

        assert_eq!(report.exact_files_seen, 8);
        assert_eq!(report.files_removed, 5);
        assert_eq!(report.issue_count, 0);
        for day in 1..=5 {
            assert!(!directory.path().join(format!("diskpie.2000-07-{day:02}.log")).exists());
        }
        for day in 6..=12 {
            assert!(directory.path().join(format!("diskpie.2000-07-{day:02}.log")).exists());
        }
        assert!(directory.path().join(current_utc_log_key()).exists());
        assert_eq!(exact_names(directory.path()).len(), MAX_RETAINED_LOG_FILES);
        assert!(directory.path().join("diskpie.2000-07-01.log.bak").exists());
    }

    #[test]
    fn retention_never_follows_a_matching_reparse_point() {
        let directory = TestDirectory::new("retention-reparse");
        let target = directory.path().join("target.txt");
        let link = directory.path().join("diskpie.2000-01-01.log");
        fs::write(&target, b"must remain").expect("write target");
        if !require_exercised(classify_privileged_fixture(symlink_file(&target, &link))) {
            return;
        }
        for day in 1..=9 {
            fs::write(directory.path().join(format!("diskpie.2000-08-{day:02}.log")), b"log")
                .expect("write log fixture");
        }

        let report = prune_diagnostic_logs(directory.path());

        assert_eq!(report.files_removed, 2);
        assert!(report.issue_count >= 1);
        assert!(link.symlink_metadata().is_ok());
        assert_eq!(fs::read(target).expect("read target"), b"must remain");
    }

    #[test]
    fn retention_rejects_a_reparse_root_without_touching_its_target() {
        let directory = TestDirectory::new("retention-root-reparse");
        let target = directory.path().join("real-logs");
        let link = directory.path().join("linked-logs");
        fs::create_dir(&target).expect("create real log root");
        fs::write(target.join("diskpie.2000-01-01.log"), b"must remain").expect("write target log");
        if !require_exercised(classify_privileged_fixture(symlink_dir(&target, &link))) {
            return;
        }

        let report = prune_diagnostic_logs(&link);

        assert_eq!(report.files_removed, 0);
        assert_eq!(report.issue_count, 1);
        assert_eq!(report.issues[0].kind, DiagnosticFileErrorKind::ReparsePoint);
        assert_eq!(fs::read(target.join("diskpie.2000-01-01.log")).unwrap(), b"must remain");
    }

    #[test]
    fn retention_caps_safe_future_names_without_evicting_the_active_log() {
        let directory = TestDirectory::new("retention-future");
        let active_key = current_utc_log_key();
        for day in 1..=9 {
            fs::write(directory.path().join(format!("diskpie.2000-01-{day:02}.log")), b"old")
                .expect("write old log");
        }
        let future = directory.path().join("diskpie.9999-12-31.log");
        fs::write(&future, b"untrusted future").expect("write future log");

        let report = prune_diagnostic_logs(directory.path());

        assert!(directory.path().join(active_key).exists(), "the active file is always retained");
        assert!(!future.exists(), "safe future names are managed by the same eight-file cap");
        assert_eq!(report.exact_files_seen, 8);
        assert_eq!(report.files_removed, 3);
        assert_eq!(report.issue_count, 0);
        assert_eq!(exact_names(directory.path()).len(), MAX_RETAINED_LOG_FILES);
    }

    #[test]
    fn writer_uses_injected_utc_date_and_rolls_before_the_next_record() {
        let directory = TestDirectory::new("writer-rollover");
        let first = UtcDate { year: 2040, month: 12, day: 31 };
        let second = UtcDate { year: 2041, month: 1, day: 1 };
        let (clock_state, clock) = injected_clock(first);
        let mut writer = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect("open first UTC log");

        writer.write_all(b"first-record\n").expect("write first date");
        *clock_state.lock().expect("set test UTC clock") = second;
        writer.write_all(b"second-record\n").expect("roll and write second date");
        drop(writer);

        assert_eq!(fs::read(directory.path().join(first.log_name())).unwrap(), b"first-record\n");
        assert_eq!(fs::read(directory.path().join(second.log_name())).unwrap(), b"second-record\n");
    }

    #[test]
    fn writer_real_clock_creates_the_current_utc_name() {
        let directory = TestDirectory::new("writer-real-utc");
        let expected = current_utc_log_key();
        let writer = WindowsDailyLogWriter::open(directory.path()).expect("open current UTC log");
        assert!(directory.path().join(expected).exists());
        drop(writer);
    }

    #[test]
    fn writer_enforces_complete_record_daily_byte_cap() {
        let directory = TestDirectory::new("writer-byte-cap");
        let date = UtcDate { year: 2042, month: 2, day: 2 };
        let (_clock_state, clock) = injected_clock(date);
        let mut writer = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect("open capped log");
        let almost_full = vec![b'a'; MAX_DAILY_LOG_BYTES as usize - 1];
        writer.write_all(&almost_full).expect("write below cap");
        writer.write_all(b"b").expect("write exact boundary");
        let error = writer.write_all(b"whole-record-must-fail").expect_err("reject over cap");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        drop(writer);
        assert_eq!(
            fs::metadata(directory.path().join(date.log_name())).unwrap().len(),
            MAX_DAILY_LOG_BYTES
        );
    }

    #[test]
    fn writer_rejects_append_when_existing_active_file_is_oversized() {
        let directory = TestDirectory::new("writer-existing-oversized");
        let date = UtcDate { year: 2043, month: 3, day: 3 };
        let active = directory.path().join(date.log_name());
        let file = File::create(&active).expect("create oversized active fixture");
        file.set_len(MAX_DAILY_LOG_BYTES + 1).expect("extend active fixture");
        drop(file);
        let (_clock_state, clock) = injected_clock(date);
        let mut writer = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect("open oversized active object");

        writer.write_all(b"rejected").expect_err("existing oversized log stays immutable");
        drop(writer);
        assert_eq!(fs::metadata(active).unwrap().len(), MAX_DAILY_LOG_BYTES + 1);
    }

    #[test]
    fn writer_refuses_a_second_concurrent_active_writer() {
        let directory = TestDirectory::new("writer-sharing");
        let date = UtcDate { year: 2044, month: 4, day: 4 };
        let (_first_state, first_clock) = injected_clock(date);
        let _first = WindowsDailyLogWriter::open_with_clock(directory.path(), first_clock)
            .expect("open first writer");
        let (_second_state, second_clock) = injected_clock(date);
        let error = WindowsDailyLogWriter::open_with_clock(directory.path(), second_clock)
            .expect_err("second writer must fail sharing checks");
        assert_eq!(error.os_code, Some(32));
    }

    #[test]
    fn writer_refuses_an_existing_competing_write_handle() {
        let directory = TestDirectory::new("writer-competing-handle");
        let date = UtcDate { year: 2045, month: 5, day: 5 };
        let active = directory.path().join(date.log_name());
        fs::write(&active, b"fixture").expect("create active fixture");
        let _competing = OpenOptions::new()
            .append(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(&active)
            .expect("open competing writer");
        let (_clock_state, clock) = injected_clock(date);
        let error = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect_err("writer must not weaken delete/write sharing");
        assert_eq!(error.os_code, Some(32));
    }

    #[test]
    fn writer_rejects_active_leaf_reparse_without_touching_target() {
        let directory = TestDirectory::new("writer-leaf-reparse");
        let date = UtcDate { year: 2046, month: 6, day: 6 };
        let target = directory.path().join("target.txt");
        let active = directory.path().join(date.log_name());
        fs::write(&target, b"private").expect("write target");
        if !require_exercised(classify_privileged_fixture(symlink_file(&target, &active))) {
            return;
        }
        let (_clock_state, clock) = injected_clock(date);
        let error = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect_err("active reparse leaf must fail closed");
        assert_eq!(error.kind, DiagnosticFileErrorKind::ReparsePoint);
        assert_eq!(fs::read(target).unwrap(), b"private");
    }

    #[test]
    fn writer_rejects_active_hard_link_identity() {
        let directory = TestDirectory::new("writer-active-hardlink");
        let date = UtcDate { year: 2047, month: 7, day: 7 };
        let active = directory.path().join(date.log_name());
        let alias = directory.path().join("alias.log");
        fs::write(&active, b"private").expect("write active fixture");
        fs::hard_link(&active, &alias).expect("create hard-link fixture");
        let (_clock_state, clock) = injected_clock(date);
        let error = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect_err("multiply-linked active file must fail closed");
        assert_eq!(error.kind, DiagnosticFileErrorKind::Unsupported);
        assert_eq!(fs::read(alias).unwrap(), b"private");
    }

    #[test]
    fn retention_preserves_multiply_linked_exact_leaf_and_reports_residual() {
        let directory = TestDirectory::new("retention-hardlink");
        let linked = directory.path().join("diskpie.2000-01-01.log");
        let alias = directory.path().join("hardlink-alias");
        fs::write(&linked, b"private").expect("write hard-link source");
        fs::hard_link(&linked, &alias).expect("create hard-link fixture");
        for day in 2..=10 {
            fs::write(directory.path().join(format!("diskpie.2000-01-{day:02}.log")), b"log")
                .expect("write safe log fixture");
        }

        let report = prune_diagnostic_logs(directory.path());

        assert!(linked.exists());
        assert_eq!(fs::read(alias).unwrap(), b"private");
        assert!(report.issue_count >= 1);
        assert_eq!(exact_names(directory.path()).len(), MAX_RETAINED_LOG_FILES + 1);
    }

    #[test]
    fn retention_preserves_a_leaf_when_delete_sharing_is_denied() {
        let directory = TestDirectory::new("retention-sharing");
        let held_path = directory.path().join("diskpie.2000-01-01.log");
        fs::write(&held_path, b"held").expect("write held log");
        let _held = OpenOptions::new()
            .append(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(&held_path)
            .expect("deny delete sharing");
        for day in 2..=9 {
            fs::write(directory.path().join(format!("diskpie.2000-01-{day:02}.log")), b"log")
                .expect("write safe log fixture");
        }

        let report = prune_diagnostic_logs(directory.path());

        assert!(held_path.exists());
        assert!(report.issue_count >= 1);
        assert_eq!(exact_names(directory.path()).len(), MAX_RETAINED_LOG_FILES + 1);
    }

    #[test]
    fn active_created_file_cleanup_is_armed_before_validation() {
        let directory = TestDirectory::new("writer-create-cleanup");
        let root = DirectoryGuard::open(directory.path(), DiagnosticFileOperation::OpenLog)
            .expect("open guarded root");
        let name = OsStr::new("cleanup-fixture.log");
        let mut path = root
            .absolute_child_units_for(&os_units(name), DiagnosticFileOperation::OpenLog)
            .expect("build stable child path");
        path.push(0);
        let file = open_native_file(
            &path,
            FILE_READ_ATTRIBUTES.0 | DELETE.0,
            FILE_SHARE_READ,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            DiagnosticFileOperation::OpenLog,
        )
        .expect("create cleanup fixture");
        drop(CreatedLogCleanup::new(file));
        assert!(!directory.path().join(name).exists());
    }

    #[test]
    fn writer_detects_or_blocks_ancestor_namespace_replacement() {
        let directory = TestDirectory::new("writer-ancestor-replacement");
        let ancestor = directory.path().join("ancestor");
        let log_root = ancestor.join("logs");
        let moved = directory.path().join("moved-ancestor");
        fs::create_dir(&ancestor).expect("create ancestor");
        fs::create_dir(&log_root).expect("create log root");
        let date = UtcDate { year: 2048, month: 8, day: 8 };
        let (_clock_state, clock) = injected_clock(date);
        let mut writer =
            WindowsDailyLogWriter::open_with_clock(&log_root, clock).expect("open guarded writer");

        match fs::rename(&ancestor, &moved) {
            Ok(()) => {
                writer.write_all(b"must-fail\n").expect_err("renamed ancestry must be detected");
                drop(writer);
                fs::rename(&moved, &ancestor).expect("restore renamed fixture");
            }
            Err(error) => {
                assert!(matches!(error.raw_os_error(), Some(5 | 32)));
                writer.write_all(b"still-stable\n").expect("blocked rename leaves writer valid");
            }
        }
    }

    #[test]
    fn writer_supports_long_and_non_unicode_directory_names() {
        let directory = TestDirectory::new("writer-long-nonunicode");
        let mut root = directory.path().to_path_buf();
        let mut segment = 0_u32;
        while root.as_os_str().encode_wide().count() < 300 {
            root.push(format!("long-writer-segment-{segment:03}-abcdefgh"));
            segment += 1;
        }
        fs::create_dir_all(&root).expect("create long writer root");
        let mut non_unicode: Vec<u16> = OsStr::new("non-unicode-").encode_wide().collect();
        non_unicode.push(0xD800);
        root.push(OsString::from_wide(&non_unicode));
        fs::create_dir(&root).expect("create non-Unicode writer root");
        let date = UtcDate { year: 2049, month: 9, day: 9 };
        let (_clock_state, clock) = injected_clock(date);
        let mut writer =
            WindowsDailyLogWriter::open_with_clock(&root, clock).expect("open native long writer");
        writer.write_all(b"native-path\n").expect("write native long path");
        drop(writer);
        assert_eq!(fs::read(root.join(date.log_name())).unwrap(), b"native-path\n");
    }

    #[test]
    fn case_sensitive_directory_keeps_exact_active_spelling() {
        let directory = TestDirectory::new("writer-case-sensitive");
        if let Err(error) = enable_case_sensitive_directory(directory.path()) {
            assert!(
                matches!(
                    error.kind,
                    DiagnosticFileErrorKind::AccessDenied | DiagnosticFileErrorKind::Unsupported
                ) || matches!(error.os_code, Some(1 | 5 | 50 | 87))
            );
            eprintln!("case-sensitive directory fixture unsupported: OS code {:?}", error.os_code);
            return;
        }
        let date = UtcDate { year: 2050, month: 10, day: 10 };
        let exact = date.log_name();
        let wrong_case = exact.replacen("diskpie", "DiskPie", 1);
        fs::write(directory.path().join(&wrong_case), b"wrong-case")
            .expect("write wrong-case peer");
        let (_clock_state, clock) = injected_clock(date);
        let mut writer = WindowsDailyLogWriter::open_with_clock(directory.path(), clock)
            .expect("open exact case-sensitive active file");
        writer.write_all(b"exact-case\n").expect("write exact active");
        drop(writer);
        assert_eq!(fs::read(directory.path().join(exact)).unwrap(), b"exact-case\n");
        assert_eq!(fs::read(directory.path().join(wrong_case)).unwrap(), b"wrong-case");
    }

    #[test]
    fn directory_parser_is_bounded_and_makes_forward_progress() {
        let mut buffer = DirectoryQueryBuffer::new();
        let first_name: Vec<u16> = OsStr::new("diskpie.2051-11-11.log").encode_wide().collect();
        let second_name: Vec<u16> = OsStr::new("unrelated").encode_wide().collect();
        let first_bytes = aligned_record_bytes(first_name.len());
        write_directory_record(&mut buffer, 0, first_bytes, &first_name, 0, 7);
        write_directory_record(&mut buffer, first_bytes, 0, &second_name, 0, 8);

        let parsed = parse_directory_query_buffer(&buffer.0).expect("parse valid linked records");
        assert_eq!(parsed.scanned, 2);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].name_key, "diskpie.2051-11-11.log");

        let mut malformed = DirectoryQueryBuffer::new();
        write_directory_record(&mut malformed, 0, first_bytes - 1, &first_name, 0, 1);
        assert!(parse_directory_query_buffer(&malformed.0).is_err());
    }

    #[test]
    fn directory_parser_enforces_per_buffer_record_budget() {
        let mut buffer = DirectoryQueryBuffer::new();
        let name = [b'x' as u16];
        let stride = aligned_record_bytes(name.len());
        for index in 0..=MAX_DIRECTORY_RECORDS_PER_QUERY {
            let next = if index == MAX_DIRECTORY_RECORDS_PER_QUERY { 0 } else { stride };
            write_directory_record(&mut buffer, index * stride, next, &name, 0, 1);
        }
        let Err(error) = parse_directory_query_buffer(&buffer.0) else {
            panic!("record budget must be strict");
        };
        assert_eq!(error.kind, DiagnosticFileErrorKind::Unsupported);
    }

    #[test]
    fn retention_cycle_enforces_total_entry_budget_without_wraparound() {
        let mut cleanup = CleanupPass::with_scanned(MAX_DIRECTORY_ENTRIES_PER_CYCLE - 1);
        assert!(cleanup.admit_scanned(1));
        assert!(!cleanup.admit_scanned(1));
        let mut verification = VerifyPass {
            restart: true,
            exact_seen: 0,
            active_seen: false,
            removed_before_verify: 0,
            scanned_entries: MAX_DIRECTORY_ENTRIES_PER_CYCLE,
        };
        assert!(!verification.admit_scanned(1));
    }

    #[test]
    fn destination_classification_distinguishes_local_and_unc_paths() {
        assert_eq!(
            classify_export_destination(Path::new(r"C:\reports\diskpie.txt")),
            Ok(ExportDestinationKind::Local)
        );
        assert_eq!(
            classify_export_destination(Path::new(r"\\server\share\diskpie.txt")),
            Ok(ExportDestinationKind::Unc)
        );
        assert_eq!(
            classify_export_destination(Path::new(r"\\?\UNC\server\share\diskpie.txt")),
            Ok(ExportDestinationKind::Unc)
        );
        assert_eq!(
            classify_export_destination(Path::new("//server/share/diskpie.txt")),
            Ok(ExportDestinationKind::Unc)
        );
        assert_eq!(
            classify_export_destination(Path::new("//?/UNC/server/share/diskpie.txt")),
            Ok(ExportDestinationKind::Unc)
        );
        assert_eq!(
            classify_export_destination(Path::new("C:/reports/diskpie.txt")),
            Ok(ExportDestinationKind::Local)
        );
        assert!(classify_export_destination(Path::new(r"relative\diskpie.txt")).is_err());
        assert!(classify_export_destination(Path::new(r"\\.\PhysicalDrive0")).is_err());
        assert!(classify_export_destination(Path::new("//./PhysicalDrive0")).is_err());
        assert!(classify_export_destination(Path::new("//?/GLOBALROOT/Device/Harddisk0")).is_err());
    }

    #[test]
    fn export_creates_and_atomically_replaces_a_regular_destination() {
        let directory = TestDirectory::new("export");
        let destination = directory.path().join("report.txt");

        let created = write_diagnostic_export(&destination, b"first", &[]).expect("create export");
        assert!(!created.replaced_existing);
        assert_eq!(created.bytes_written, 5);
        assert_eq!(fs::read(&destination).expect("read created export"), b"first");

        let replaced =
            write_diagnostic_export(&destination, b"second", &[]).expect("replace export");
        assert!(replaced.replaced_existing);
        assert_eq!(fs::read(&destination).expect("read replaced export"), b"second");
        let temporary_count = fs::read_dir(directory.path())
            .expect("list directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".diskpie-"))
            .count();
        assert_eq!(temporary_count, 0);
    }

    #[test]
    fn export_replacement_does_not_inherit_old_alternate_data_streams() {
        let directory = TestDirectory::new("export-ads");
        let destination = directory.path().join("report.txt");
        fs::write(&destination, b"old").expect("write old destination");
        let mut ads_name = destination.as_os_str().to_os_string();
        ads_name.push(":private-stream");
        let ads = PathBuf::from(ads_name);
        if let Err(error) = fs::write(&ads, b"must disappear") {
            if matches!(error.kind(), io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput) {
                return;
            }
            panic!("create NTFS ADS fixture: {error}");
        }

        write_diagnostic_export(&destination, b"new", &[]).expect("replace export");

        assert_eq!(fs::read(&destination).expect("read replacement"), b"new");
        let error = fs::read(&ads).expect_err("the old file object's ADS must not survive");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn export_temporary_denies_every_competing_open() {
        let directory = TestDirectory::new("export-exclusive");
        let parent =
            DirectoryGuard::open(directory.path(), DiagnosticFileOperation::InspectDestination)
                .expect("open parent guard");
        let temporary = TemporarySibling::create(&parent).expect("create exclusive temporary");
        let mut path_buffer = [0_u16; MAX_NATIVE_PATH_UNITS];
        let length = final_path_into(
            file_handle(temporary.file.as_ref().expect("temporary handle")),
            &mut path_buffer,
            DiagnosticFileOperation::InspectDestination,
        )
        .expect("resolve temporary path");
        let path = PathBuf::from(OsString::from_wide(&path_buffer[..length]));

        let error = File::open(path).expect_err("FILE_SHARE_NONE must reject a competing open");
        assert_eq!(error.raw_os_error(), Some(32));
    }

    #[test]
    fn export_supports_native_paths_longer_than_max_path() {
        let directory = TestDirectory::new("export-long-path");
        let mut parent = directory.path().to_path_buf();
        let mut segment = 0_u32;
        while parent.as_os_str().encode_wide().count() < 300 {
            parent.push(format!("long-segment-{segment:03}-abcdefgh"));
            segment += 1;
        }
        fs::create_dir_all(&parent).expect("create long native directory");
        let destination = parent.join("report.txt");
        assert!(destination.as_os_str().encode_wide().count() > 260);

        write_diagnostic_export(&destination, b"long-path", &[]).expect("write long-path export");

        assert_eq!(fs::read(destination).expect("read long-path export"), b"long-path");
    }

    #[test]
    fn export_refuses_owned_path_and_hard_link_identity() {
        let directory = TestDirectory::new("owned");
        let owned = directory.path().join("owned.log");
        let link = directory.path().join("selected.txt");
        fs::write(&owned, b"private").expect("write owned file");
        fs::hard_link(&owned, &link).expect("create hard link");

        let direct = write_diagnostic_export(&owned, b"report", &[&owned])
            .expect_err("direct owned file must be rejected");
        assert_eq!(direct.kind, DiagnosticFileErrorKind::OwnedDestination);

        let hard_link = write_diagnostic_export(&link, b"report", &[&owned])
            .expect_err("hard-link identity must be rejected");
        assert_eq!(hard_link.kind, DiagnosticFileErrorKind::OwnedDestination);
        assert_eq!(fs::read(owned).expect("owned content intact"), b"private");
    }

    #[test]
    fn export_refuses_reparse_destination_and_oversized_input() {
        let directory = TestDirectory::new("refuse");
        let target = directory.path().join("target.txt");
        let link = directory.path().join("selected.txt");
        fs::write(&target, b"private").expect("write target");
        if let Err(error) = symlink_file(&target, &link) {
            if matches!(error.raw_os_error(), Some(5 | 1_314)) {
                return;
            }
            panic!("create symlink: {error}");
        }

        let reparse = write_diagnostic_export(&link, b"report", &[])
            .expect_err("reparse destination must be rejected");
        assert_eq!(reparse.kind, DiagnosticFileErrorKind::ReparsePoint);
        assert_eq!(fs::read(target).expect("target intact"), b"private");

        let too_large = vec![0_u8; MAX_DIAGNOSTIC_EXPORT_BYTES + 1];
        let oversized =
            write_diagnostic_export(&directory.path().join("large.txt"), &too_large, &[])
                .expect_err("oversized export must be rejected");
        assert_eq!(oversized.kind, DiagnosticFileErrorKind::TooLarge);

        let invalid_utf8 =
            write_diagnostic_export(&directory.path().join("invalid.txt"), &[0xFF], &[])
                .expect_err("non-UTF-8 export must be rejected");
        assert_eq!(invalid_utf8.kind, DiagnosticFileErrorKind::InvalidContent);

        let wrong_extension =
            write_diagnostic_export(&directory.path().join("report.bin"), b"report", &[])
                .expect_err("non-text export must be rejected");
        assert_eq!(wrong_extension.kind, DiagnosticFileErrorKind::InvalidDestination);
    }

    #[test]
    fn public_errors_never_render_paths() {
        let directory = TestDirectory::new("redaction");
        let secret = directory.path().join("secret-customer-name");
        fs::create_dir(&secret).expect("create directory destination");
        let error = write_diagnostic_export(&secret, b"report", &[])
            .expect_err("directory destination must fail");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("secret-customer-name"));
        assert!(!rendered.contains(&directory.path().to_string_lossy().to_string()));
    }

    #[test]
    fn native_marker_writer_creates_and_replaces_complete_content() {
        let directory = TestDirectory::new("native-marker");
        let target = directory.path().join("last-panic.txt");
        let temporary = directory.path().join("last-panic.write");
        let writer = WindowsAtomicMarkerWriter::prepare(&target, std::slice::from_ref(&temporary))
            .expect("prepare marker writer");

        writer.replace(b"schema=fixed\n").expect("create marker");
        assert_eq!(fs::read(&target).expect("read marker"), b"schema=fixed\n");
        assert!(!temporary.exists());

        writer.replace(b"schema=replaced\n").expect("replace marker");
        assert_eq!(fs::read(&target).expect("read marker"), b"schema=replaced\n");
        assert!(!temporary.exists());
    }

    #[test]
    fn native_marker_writer_is_bounded_redacted_and_sibling_only() {
        let directory = TestDirectory::new("native-marker-policy");
        let target = directory.path().join("private-customer-last-panic.txt");
        let temporary = directory.path().join("private-customer-last-panic.write");
        let writer = WindowsAtomicMarkerWriter::prepare(&target, std::slice::from_ref(&temporary))
            .expect("prepare marker writer");
        let rendered = format!("{writer:?}");
        assert!(!rendered.contains("private-customer"));
        assert!(writer.replace(b"").is_err());
        assert!(writer.replace(&[0xFF]).is_err());
        assert!(writer.replace(&vec![b'x'; MAX_PANIC_MARKER_BYTES + 1]).is_err());
        assert!(!target.exists());
        assert!(!temporary.exists());

        let different_parent = directory.path().join("child");
        fs::create_dir(&different_parent).expect("create second parent");
        let error = WindowsAtomicMarkerWriter::prepare(
            &target,
            &[different_parent.join("last-panic.write")],
        )
        .expect_err("temporary must be a sibling");
        assert_eq!(error.kind, DiagnosticFileErrorKind::InvalidDestination);

        let unc_error = WindowsAtomicMarkerWriter::prepare(
            Path::new(r"\\server\share\last-panic.txt"),
            &[PathBuf::from(r"\\server\share\last-panic.write")],
        )
        .expect_err("panic markers must reject network destinations before opening them");
        assert_eq!(unc_error.kind, DiagnosticFileErrorKind::InvalidDestination);
    }

    #[test]
    fn native_marker_writer_refuses_reparse_or_directory_target_and_cleans_sibling() {
        let directory = TestDirectory::new("native-marker-target");
        let target_file = directory.path().join("private-target.txt");
        let target_link = directory.path().join("last-panic.txt");
        let temporary = directory.path().join("last-panic.write");
        fs::write(&target_file, b"private").expect("write target");
        if let Err(error) = symlink_file(&target_file, &target_link) {
            if matches!(error.raw_os_error(), Some(5 | 1_314)) {
                return;
            }
            panic!("create symlink: {error}");
        }
        let error =
            WindowsAtomicMarkerWriter::prepare(&target_link, std::slice::from_ref(&temporary))
                .expect_err("reparse target rejected during preparation");
        assert_eq!(error.kind, DiagnosticFileErrorKind::ReparsePoint);
        assert!(!temporary.exists());
        assert_eq!(fs::read(target_file).expect("target intact"), b"private");

        fs::remove_file(&target_link).expect("remove link");
        fs::create_dir(&target_link).expect("create target directory");
        let error =
            WindowsAtomicMarkerWriter::prepare(&target_link, std::slice::from_ref(&temporary))
                .expect_err("directory target rejected during preparation");
        assert_eq!(error.kind, DiagnosticFileErrorKind::InvalidDestination);
        assert!(!temporary.exists());
    }

    #[test]
    fn wide_helpers_preserve_non_scalar_units() {
        let mut units: Vec<u16> = OsStr::new(r"C:\reports\").encode_wide().collect();
        units.extend_from_slice(&[0xD800, b'.' as u16, b't' as u16, b'x' as u16, b't' as u16]);
        let path = PathBuf::from(OsString::from_wide(&units));
        assert_eq!(path_units_without_nul(&path).expect("encode path"), units);
    }

    #[test]
    fn windows_not_found_codes_map_without_localized_text() {
        for code in [ERROR_FILE_NOT_FOUND.0 as i32, ERROR_PATH_NOT_FOUND.0 as i32] {
            let error = DiagnosticFileError::from_io(
                DiagnosticFileOperation::InspectDestination,
                &io::Error::from_raw_os_error(code),
            );
            assert_eq!(error.kind, DiagnosticFileErrorKind::NotFound);
            assert_eq!(error.os_code, Some(code));
        }
    }

    #[test]
    fn injected_partial_replacement_runs_recovery_and_preserves_the_complete_sibling() {
        let recovered = AtomicBool::new(false);
        let partial = DiagnosticFileError::new(
            DiagnosticFileOperation::ReplaceDestination,
            DiagnosticFileErrorKind::PartialReplacement,
        );
        let ordinary = DiagnosticFileError::new(
            DiagnosticFileOperation::ReplaceDestination,
            DiagnosticFileErrorKind::Io,
        );

        assert!(preserve_after_failed_replacement(&partial, || {
            recovered.store(true, AtomicOrdering::Relaxed);
        }));
        assert!(recovered.load(AtomicOrdering::Relaxed));
        recovered.store(false, AtomicOrdering::Relaxed);
        assert!(!preserve_after_failed_replacement(&ordinary, || {
            recovered.store(true, AtomicOrdering::Relaxed);
        }));
        assert!(!recovered.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn ordering_of_valid_names_matches_date_order() {
        let left = exact_log_name_key(OsStr::new("diskpie.2025-12-31.log")).expect("left");
        let right = exact_log_name_key(OsStr::new("diskpie.2026-01-01.log")).expect("right");
        assert_eq!(left.cmp(&right), Ordering::Less);
    }

    const MARKER_TARGET: &str = "last-panic.txt";
    const MARKER_SIBLING: &str = "last-panic.write";

    fn marker_paths(directory: &Path) -> (PathBuf, PathBuf) {
        (directory.join(MARKER_TARGET), directory.join(MARKER_SIBLING))
    }

    fn prepare_marker_session(directory: &Path) -> WindowsMarkerSession {
        let (target, sibling) = marker_paths(directory);
        WindowsMarkerSession::prepare(&target, &sibling).expect("prepare marker session")
    }

    fn accept_owner(_bytes: &[u8]) -> bool {
        true
    }

    #[test]
    fn marker_session_creates_and_replaces_without_leaving_a_sibling() {
        let directory = TestDirectory::new("marker-session-create");
        let (target, sibling) = marker_paths(directory.path());

        let session = prepare_marker_session(directory.path());
        assert_eq!(session.target_slot(), MarkerSlotState::Missing);
        assert_eq!(session.sibling_slot(), MarkerSlotState::Missing);
        assert_eq!(session.commit(b"schema=first\n"), MarkerCommitStatus::Committed);
        assert_eq!(fs::read(&target).expect("read created marker"), b"schema=first\n");
        assert!(!sibling.exists(), "a successful commit consumes the sibling");
        assert_eq!(session.commit(b"schema=second\n"), MarkerCommitStatus::Committed);
        assert_eq!(fs::read(&target).expect("read replaced marker"), b"schema=second\n");
        assert!(!sibling.exists());
        drop(session);

        let session = prepare_marker_session(directory.path());
        assert_eq!(session.target_slot(), MarkerSlotState::Present(b"schema=second\n"));
        assert_eq!(session.commit(b"schema=third\n"), MarkerCommitStatus::Committed);
        assert_eq!(fs::read(&target).expect("read third marker"), b"schema=third\n");
        assert!(!sibling.exists());
    }

    #[test]
    fn marker_session_rejects_invalid_payloads_before_touching_the_filesystem() {
        let directory = TestDirectory::new("marker-session-payload");
        let (target, sibling) = marker_paths(directory.path());
        let session = prepare_marker_session(directory.path());
        for (payload, kind) in [
            (Vec::new(), DiagnosticFileErrorKind::InvalidContent),
            (vec![0xFF], DiagnosticFileErrorKind::InvalidContent),
            (vec![b'x'; MAX_PANIC_MARKER_BYTES + 1], DiagnosticFileErrorKind::TooLarge),
        ] {
            match session.commit(&payload) {
                MarkerCommitStatus::NotCommitted(error) => assert_eq!(error.kind, kind),
                other => panic!("invalid payload must not commit: {other:?}"),
            }
        }
        assert!(!target.exists());
        assert!(!sibling.exists());
    }

    #[test]
    fn marker_session_does_not_replace_an_initially_missing_target_that_appears() {
        let directory = TestDirectory::new("marker-session-appears");
        let (target, sibling) = marker_paths(directory.path());
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.target_slot(), MarkerSlotState::Missing);

        fs::write(&target, b"foreign").expect("concurrent creator wins");
        match session.commit(b"schema=late\n") {
            MarkerCommitStatus::NotCommitted(error) => {
                assert_eq!(error.kind, DiagnosticFileErrorKind::AlreadyExists);
            }
            other => panic!("an appeared target must not be replaced: {other:?}"),
        }
        assert_eq!(fs::read(&target).expect("foreign target intact"), b"foreign");
        assert!(!sibling.exists(), "the never-renamed sibling is removed by handle");
        assert_eq!(
            session.reconcile_if_owned(MarkerReconcileAction::Delete, &accept_owner),
            Ok(MarkerReconcileOutcome::PreservedUncertain),
            "nothing was committed, so nothing may be reconciled"
        );
        assert_eq!(fs::read(&target).expect("foreign target still intact"), b"foreign");
    }

    #[test]
    fn marker_session_refuses_a_swapped_target_identity_at_commit() {
        let directory = TestDirectory::new("marker-session-swapped");
        let (target, sibling) = marker_paths(directory.path());
        fs::write(&target, b"schema=previous\n").expect("write previous marker");
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.target_slot(), MarkerSlotState::Present(b"schema=previous\n"));

        fs::remove_file(&target).expect("remove inspected target");
        fs::write(&target, b"schema=foreign\n").expect("recreate with a new identity");
        match session.commit(b"schema=session\n") {
            MarkerCommitStatus::NotCommitted(error) => {
                assert_eq!(error.kind, DiagnosticFileErrorKind::OwnedDestination);
            }
            other => panic!("a swapped identity must not be replaced: {other:?}"),
        }
        assert_eq!(fs::read(&target).expect("foreign marker intact"), b"schema=foreign\n");
        assert!(!sibling.exists());
    }

    #[test]
    fn marker_session_recreates_an_inspected_target_that_disappeared() {
        let directory = TestDirectory::new("marker-session-vanished");
        let (target, sibling) = marker_paths(directory.path());
        fs::write(&target, b"schema=previous\n").expect("write previous marker");
        let session = prepare_marker_session(directory.path());
        fs::remove_file(&target).expect("delete inspected target");

        assert_eq!(session.commit(b"schema=session\n"), MarkerCommitStatus::Committed);
        assert_eq!(fs::read(&target).expect("recreated marker"), b"schema=session\n");
        assert!(!sibling.exists());
    }

    #[test]
    fn marker_session_reports_reparse_directory_and_hard_link_targets_untouched() {
        let directory = TestDirectory::new("marker-session-slots");
        let (target, sibling) = marker_paths(directory.path());
        let sentinel = directory.path().join("sentinel.txt");
        fs::write(&sentinel, b"must remain").expect("write sentinel");

        if require_exercised(classify_privileged_fixture(symlink_file(&sentinel, &target))) {
            let session = prepare_marker_session(directory.path());
            match session.target_slot() {
                MarkerSlotState::Unavailable(error) => {
                    assert_eq!(error.kind, DiagnosticFileErrorKind::ReparsePoint);
                }
                other => panic!("reparse target must be unavailable: {other:?}"),
            }
            assert!(matches!(
                session.commit(b"schema=x\n"),
                MarkerCommitStatus::NotCommitted(DiagnosticFileError {
                    kind: DiagnosticFileErrorKind::ReparsePoint,
                    ..
                })
            ));
            assert_eq!(
                session.reconcile_if_owned(MarkerReconcileAction::Delete, &accept_owner),
                Ok(MarkerReconcileOutcome::PreservedUncertain)
            );
            assert!(session.delete_explicit().is_err());
            assert!(target.symlink_metadata().is_ok(), "the link itself survives");
            assert_eq!(fs::read(&sentinel).expect("sentinel intact"), b"must remain");
            fs::remove_file(&target).expect("remove link fixture");
        }

        fs::create_dir(&target).expect("create directory-shaped target");
        let session = prepare_marker_session(directory.path());
        match session.target_slot() {
            MarkerSlotState::Unavailable(error) => {
                assert_eq!(error.kind, DiagnosticFileErrorKind::InvalidDestination);
            }
            other => panic!("directory target must be unavailable: {other:?}"),
        }
        assert!(matches!(session.commit(b"schema=x\n"), MarkerCommitStatus::NotCommitted(_)));
        assert!(session.delete_explicit().is_err());
        assert!(target.is_dir());
        assert!(!sibling.exists());
        fs::remove_dir(&target).expect("remove directory fixture");

        fs::write(&target, b"schema=linked\n").expect("write hard-link source");
        let alias = directory.path().join("alias.txt");
        fs::hard_link(&target, &alias).expect("create hard-link fixture");
        let session = prepare_marker_session(directory.path());
        match session.target_slot() {
            MarkerSlotState::Unavailable(error) => {
                assert_eq!(error.kind, DiagnosticFileErrorKind::OwnedDestination);
            }
            other => panic!("hard-linked target must be unavailable: {other:?}"),
        }
        assert!(matches!(session.commit(b"schema=x\n"), MarkerCommitStatus::NotCommitted(_)));
        assert!(session.delete_explicit().is_err());
        assert_eq!(fs::read(&alias).expect("alias intact"), b"schema=linked\n");
        assert_eq!(fs::read(&target).expect("linked target intact"), b"schema=linked\n");
        assert!(!sibling.exists());
    }

    #[test]
    fn marker_session_reports_oversized_and_non_ascii_slots_without_bytes() {
        let directory = TestDirectory::new("marker-session-bounds");
        let (target, sibling) = marker_paths(directory.path());
        fs::write(&target, vec![b'x'; MAX_PANIC_MARKER_BYTES + 1]).expect("write oversized");
        fs::write(&sibling, [0xFF, b'\n']).expect("write non-ascii sibling");
        let session = prepare_marker_session(directory.path());
        assert!(matches!(
            session.target_slot(),
            MarkerSlotState::Unavailable(DiagnosticFileError {
                kind: DiagnosticFileErrorKind::TooLarge,
                ..
            })
        ));
        assert!(matches!(
            session.sibling_slot(),
            MarkerSlotState::Unavailable(DiagnosticFileError {
                kind: DiagnosticFileErrorKind::InvalidContent,
                ..
            })
        ));
        assert!(session.delete_explicit().is_err());
        assert!(target.exists());
        assert!(sibling.exists());
    }

    #[test]
    fn marker_session_preserves_a_leftover_sibling_and_refuses_to_commit_over_it() {
        let directory = TestDirectory::new("marker-session-leftover");
        let (target, sibling) = marker_paths(directory.path());
        fs::write(&sibling, b"schema=leftover\n").expect("write leftover sibling");
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.sibling_slot(), MarkerSlotState::Present(b"schema=leftover\n"));
        match session.commit(b"schema=session\n") {
            MarkerCommitStatus::NotCommitted(error) => {
                assert_eq!(error.kind, DiagnosticFileErrorKind::AlreadyExists);
            }
            other => panic!("an occupied sibling name must not be reused: {other:?}"),
        }
        assert_eq!(fs::read(&sibling).expect("leftover intact"), b"schema=leftover\n");
        assert!(!target.exists());
    }

    #[test]
    fn marker_session_reconcile_restores_previous_or_removes_owned_marker() {
        let directory = TestDirectory::new("marker-session-reconcile");
        let (target, sibling) = marker_paths(directory.path());
        fs::write(&target, b"schema=previous\n").expect("write previous marker");
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.commit(b"schema=session\n"), MarkerCommitStatus::Committed);
        assert_eq!(
            session.reconcile_if_owned(
                MarkerReconcileAction::Restore(b"schema=previous\n"),
                &accept_owner
            ),
            Ok(MarkerReconcileOutcome::RestoredPrevious)
        );
        assert_eq!(fs::read(&target).expect("restored marker"), b"schema=previous\n");
        assert!(!sibling.exists());
        drop(session);

        fs::remove_file(&target).expect("clear previous marker");
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.commit(b"schema=session\n"), MarkerCommitStatus::Committed);
        assert_eq!(
            session.reconcile_if_owned(MarkerReconcileAction::Delete, &accept_owner),
            Ok(MarkerReconcileOutcome::RemovedSessionMarker)
        );
        assert!(!target.exists());
        assert!(!sibling.exists());
        assert_eq!(
            session.reconcile_if_owned(MarkerReconcileAction::Delete, &accept_owner),
            Ok(MarkerReconcileOutcome::MarkerMissing)
        );
    }

    #[test]
    fn marker_session_reconcile_preserves_a_foreign_identity_or_owner() {
        let directory = TestDirectory::new("marker-session-foreign");
        let (target, sibling) = marker_paths(directory.path());
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.commit(b"schema=session\n"), MarkerCommitStatus::Committed);

        assert_eq!(
            session.reconcile_if_owned(MarkerReconcileAction::Delete, &|_bytes| false),
            Ok(MarkerReconcileOutcome::OwnershipLost),
            "a rejected owner check preserves the marker"
        );
        assert_eq!(fs::read(&target).expect("marker intact"), b"schema=session\n");

        fs::remove_file(&target).expect("remove session marker");
        fs::write(&target, b"schema=foreign\n").expect("foreign process writes a new object");
        assert_eq!(
            session.reconcile_if_owned(
                MarkerReconcileAction::Restore(b"schema=previous\n"),
                &accept_owner
            ),
            Ok(MarkerReconcileOutcome::OwnershipLost)
        );
        assert_eq!(
            session.reconcile_if_owned(MarkerReconcileAction::Delete, &accept_owner),
            Ok(MarkerReconcileOutcome::OwnershipLost)
        );
        assert_eq!(fs::read(&target).expect("foreign marker intact"), b"schema=foreign\n");
        assert!(!sibling.exists());
    }

    #[test]
    fn marker_session_delete_explicit_deletes_only_identity_matched_slots() {
        let directory = TestDirectory::new("marker-session-delete");
        let (target, sibling) = marker_paths(directory.path());
        fs::write(&target, b"schema=target\n").expect("write target");
        fs::write(&sibling, b"schema=sibling\n").expect("write sibling");
        let session = prepare_marker_session(directory.path());
        assert_eq!(session.delete_explicit(), Ok(true));
        assert!(!target.exists());
        assert!(!sibling.exists());
        assert_eq!(session.delete_explicit(), Ok(false), "already deleted slots are not an error");

        fs::write(&target, b"schema=target\n").expect("write target again");
        let session = prepare_marker_session(directory.path());
        fs::remove_file(&target).expect("remove inspected target");
        fs::write(&target, b"schema=swapped\n").expect("swap the object under the name");
        match session.delete_explicit() {
            Err(error) => assert_eq!(error.kind, DiagnosticFileErrorKind::OwnedDestination),
            Ok(deleted) => panic!("a swapped file must not be deleted: {deleted}"),
        }
        assert_eq!(fs::read(&target).expect("swapped file intact"), b"schema=swapped\n");

        let session = prepare_marker_session(directory.path());
        fs::write(&sibling, b"schema=appeared\n").expect("sibling appears after preparation");
        match session.delete_explicit() {
            Err(error) => assert_eq!(error.kind, DiagnosticFileErrorKind::OwnedDestination),
            Ok(deleted) => panic!("an appeared sibling must block deletion: {deleted}"),
        }
        assert!(target.exists(), "nothing is deleted when any slot conflicts");
        assert!(sibling.exists());
    }

    #[test]
    fn marker_session_supports_native_paths_longer_than_max_path() {
        let directory = TestDirectory::new("marker-session-long-path");
        let mut parent = directory.path().to_path_buf();
        let mut segment = 0_u32;
        while parent.as_os_str().encode_wide().count() < 300 {
            parent.push(format!("long-marker-segment-{segment:03}-abcdefgh"));
            segment += 1;
        }
        fs::create_dir_all(&parent).expect("create long native directory");
        let (target, sibling) = marker_paths(&parent);
        assert!(target.as_os_str().encode_wide().count() > 260);

        let session = prepare_marker_session(&parent);
        assert_eq!(session.commit(b"schema=long\n"), MarkerCommitStatus::Committed);
        assert_eq!(fs::read(&target).expect("long-path marker"), b"schema=long\n");
        assert!(!sibling.exists());
        assert_eq!(
            session.reconcile_if_owned(MarkerReconcileAction::Delete, &accept_owner),
            Ok(MarkerReconcileOutcome::RemovedSessionMarker)
        );
        assert!(!target.exists());
    }

    #[test]
    fn marker_session_rejects_non_sibling_network_and_same_name_layouts() {
        let directory = TestDirectory::new("marker-session-layout");
        let target = directory.path().join(MARKER_TARGET);
        let child = directory.path().join("child");
        fs::create_dir(&child).expect("create second parent");
        assert_eq!(
            WindowsMarkerSession::prepare(&target, &child.join(MARKER_SIBLING))
                .expect_err("sibling must share the parent")
                .kind,
            DiagnosticFileErrorKind::InvalidDestination
        );
        assert_eq!(
            WindowsMarkerSession::prepare(&target, &target)
                .expect_err("sibling must differ from the target")
                .kind,
            DiagnosticFileErrorKind::InvalidDestination
        );
        assert_eq!(
            WindowsMarkerSession::prepare(
                Path::new(r"\\server\share\last-panic.txt"),
                Path::new(r"\\server\share\last-panic.write"),
            )
            .expect_err("network markers are rejected before any open")
            .kind,
            DiagnosticFileErrorKind::InvalidDestination
        );
        assert!(!target.exists());
    }

    #[test]
    fn marker_session_debug_and_errors_never_render_paths() {
        let directory = TestDirectory::new("marker-session-redaction");
        let private = directory.path().join("private-customer-crashes");
        fs::create_dir(&private).expect("create private parent");
        let session = prepare_marker_session(&private);
        let status = session.commit(&[0xFF]);
        let rendered = format!("{session:?} {status:?}");
        assert!(!rendered.contains("private-customer"));
        assert!(!rendered.contains(&directory.path().to_string_lossy().to_string()));
        let error = session.delete_explicit().err().map(|error| format!("{error} {error:?}"));
        assert!(error.is_none_or(|text| !text.contains("private-customer")));
    }

    #[test]
    fn commit_record_is_monotonic_and_reports_contention_as_uncertain() {
        let record = CommitRecord::new();
        assert_eq!(record.committed_identity(), None);
        let first = NativeFileIdentity {
            legacy_volume: 7,
            legacy_file_id: 9,
            extended: Some((11, [3; 16])),
        };
        record.record_committed(first);
        assert_eq!(record.committed_identity(), Some(first));
        let second = NativeFileIdentity { legacy_volume: 8, legacy_file_id: 10, extended: None };
        record.record_committed(second);
        assert_eq!(record.committed_identity(), Some(second));
        record.record_unverified();
        assert_eq!(record.committed_identity(), None);
        record.record_committed(first);
        assert_eq!(record.committed_identity(), None, "unverified is sticky");

        let contended = CommitRecord::new();
        contended.phase.store(RECORD_RECORDING, AtomicOrdering::Release);
        contended.record_committed(first);
        assert!(contended.contended.load(AtomicOrdering::Acquire));
        contended.phase.store(RECORD_COMMITTED, AtomicOrdering::Release);
        assert_eq!(contended.committed_identity(), None);
    }
}
