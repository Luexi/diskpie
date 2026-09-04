//! Minimal, local-only panic markers.
//!
//! This module deliberately has no route from [`std::panic::PanicHookInfo`] to
//! marker contents. The hook records only compile-time metadata, a bounded
//! timestamp, and an application-assigned [`ThreadRole`].

#![forbid(unsafe_code)]

use std::{
    cell::Cell,
    collections::hash_map::RandomState,
    error::Error,
    fmt,
    hash::{BuildHasher, Hasher},
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
    str,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(any(test, not(windows)))]
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
};

#[cfg(windows)]
use crate::local_diagnostics::DiagnosticsQuiescenceReceipt;

/// Maximum size of a complete panic marker, in bytes.
pub const MAX_MARKER_BYTES: usize = 4 * 1024;

/// Fixed marker leaf name inside the ADR 0018 crash directory.
pub const MARKER_FILE_NAME: &str = "last-panic.txt";

const MARKER_SCHEMA: &str = "diskpie-panic-marker-v2";
const PANIC_CODE: &str = "uncaught_panic";
const TEMPORARY_FILE_NAMES: [&str; 1] = ["last-panic.write"];

// Preview is deliberately an exact compile-time allowlist, not a generic
// semver parser. When a later release must preview a shipped older marker, add
// its exact version/target tuple here and retain the matching schema parser.
const PREVIEWABLE_PRIOR_IDENTITIES: &[MarkerIdentity] = &[MarkerIdentity {
    schema: "diskpie-panic-marker-v2",
    app_version: "0.1.0",
    target_os: "windows",
    target_arch: "x86_64",
}];

static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static NEXT_SESSION_TOKEN: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static THREAD_ROLE: Cell<ThreadRole> = const { Cell::new(ThreadRole::Unknown) };
}

#[derive(Clone, Copy)]
struct MarkerIdentity {
    schema: &'static str,
    app_version: &'static str,
    target_os: &'static str,
    target_arch: &'static str,
}

const CURRENT_MARKER_IDENTITY: MarkerIdentity = MarkerIdentity {
    schema: MARKER_SCHEMA,
    app_version: env!("CARGO_PKG_VERSION"),
    target_os: std::env::consts::OS,
    target_arch: std::env::consts::ARCH,
};

/// Opaque per-install ownership identity generated before the hook is set.
///
/// It is not a security credential and carries no user-derived input. Two
/// independently seeded standard-library hashers mix a monotonic in-process
/// ordinal and the current system-clock value into a fixed 128-bit token.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SessionToken([u8; 16]);

impl SessionToken {
    pub(crate) fn as_bytes(self) -> [u8; 16] {
        self.0
    }

    fn generate() -> Self {
        let random_state = RandomState::new();
        let ordinal = NEXT_SESSION_TOKEN.fetch_add(1, Ordering::Relaxed);
        let clock_nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos(),
            Err(error) => error.duration().as_nanos(),
        };

        let mut first = random_state.build_hasher();
        first.write_u64(0x4450_4D4B_525F_3031);
        first.write_u64(ordinal);
        first.write_u128(clock_nanos);

        let mut second = random_state.build_hasher();
        second.write_u64(0x4450_4D4B_525F_3032);
        second.write_u64(ordinal.rotate_left(29));
        second.write_u128(clock_nanos.rotate_left(61));

        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&first.finish().to_be_bytes());
        bytes[8..].copy_from_slice(&second.finish().to_be_bytes());
        Self(bytes)
    }

    fn parse(value: &str) -> Option<Self> {
        if value.len() != 32 || !value.is_ascii() {
            return None;
        }

        let input = value.as_bytes();
        let mut bytes = [0_u8; 16];
        for (index, output) in bytes.iter_mut().enumerate() {
            let high = decode_lower_hex(input[index * 2])?;
            let low = decode_lower_hex(input[index * 2 + 1])?;
            *output = (high << 4) | low;
        }
        Some(Self(bytes))
    }
}

fn decode_lower_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Static, allowlisted role recorded for the thread that panicked.
///
/// Raw operating-system or Rust thread names are never inspected or written.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ThreadRole {
    Main,
    Ui,
    ScanCoordinator,
    ScanWorker,
    LayoutWorker,
    ShellWorker,
    #[default]
    Unknown,
}

impl ThreadRole {
    /// Returns the fixed marker representation for this role.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Ui => "ui",
            Self::ScanCoordinator => "scan_coordinator",
            Self::ScanWorker => "scan_worker",
            Self::LayoutWorker => "layout_worker",
            Self::ShellWorker => "shell_worker",
            Self::Unknown => "unknown",
        }
    }

    fn from_marker(value: &str) -> Option<Self> {
        match value {
            "main" => Some(Self::Main),
            "ui" => Some(Self::Ui),
            "scan_coordinator" => Some(Self::ScanCoordinator),
            "scan_worker" => Some(Self::ScanWorker),
            "layout_worker" => Some(Self::LayoutWorker),
            "shell_worker" => Some(Self::ShellWorker),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Restores the previous static role when dropped on the assigning thread.
///
/// The guard is intentionally neither `Send` nor `Sync`, preventing a role
/// assignment from being restored on a different thread.
#[must_use = "keep the guard alive for the full lifetime of the assigned thread role"]
pub struct ThreadRoleGuard {
    previous: Option<ThreadRole>,
    not_send_or_sync: PhantomData<Rc<()>>,
}

impl Drop for ThreadRoleGuard {
    fn drop(&mut self) {
        let Some(previous) = self.previous else {
            return;
        };
        let _ignored = THREAD_ROLE.try_with(|role| role.set(previous));
    }
}

/// Assigns an allowlisted role to the current thread for the guard lifetime.
pub fn enter_thread_role(role: ThreadRole) -> ThreadRoleGuard {
    let previous = THREAD_ROLE
        .try_with(|current| {
            let previous = current.get();
            current.set(role);
            previous
        })
        .ok();

    ThreadRoleGuard { previous, not_send_or_sync: PhantomData }
}

/// Runs one task with a static role and restores the previous role afterward.
pub fn with_thread_role<T>(role: ThreadRole, task: impl FnOnce() -> T) -> T {
    let _role = enter_thread_role(role);
    task()
}

/// Replacement and durability level promised by the installed store.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReplacementSemantics {
    /// A bounded same-directory temporary is flushed and renamed, but the
    /// platform API contract is not claimed to provide atomic visibility or
    /// crash durability.
    BestEffortRename,
    /// Readers see either the old or new complete marker, without a durability
    /// promise across power loss.
    AtomicVisibility,
    /// Replacement is atomically visible and durably committed before return.
    AtomicAndDurable,
}

/// Strength of the store's token/identity-conditional shutdown operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OwnershipReconciliation {
    /// Shutdown reconciliation is disabled; an owned marker is preserved.
    Unsupported,
    /// A bounded token check is performed, but another process could race the
    /// subsequent filesystem mutation. DiskPie will not use this at shutdown.
    TokenCheckedBestEffort,
    /// The adapter commits only while the target's native identity still
    /// belongs to the expected session, and preserves mismatches untouched.
    AtomicFileIdentity,
}

/// Stable store failure category that contains no path or arbitrary text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MarkerStoreFailureKind {
    AccessDenied,
    NotFound,
    InvalidData,
    Busy,
    StorageFull,
    Unsupported,
    MarkerTooLarge,
    InvalidMarker,
    PreservedSibling,
    OwnershipConflict,
    Other,
}

/// Sanitized failure returned by the marker replacement port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarkerStoreFailure {
    pub kind: MarkerStoreFailureKind,
    pub os_code: Option<i32>,
}

impl MarkerStoreFailure {
    /// Constructs a sanitized failure for a platform adapter.
    pub const fn new(kind: MarkerStoreFailureKind, os_code: Option<i32>) -> Self {
        Self { kind, os_code }
    }

    #[cfg(any(test, not(windows)))]
    fn from_io(error: &io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::PermissionDenied => MarkerStoreFailureKind::AccessDenied,
            io::ErrorKind::NotFound => MarkerStoreFailureKind::NotFound,
            io::ErrorKind::InvalidData
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::WriteZero
            | io::ErrorKind::UnexpectedEof => MarkerStoreFailureKind::InvalidData,
            io::ErrorKind::AlreadyExists | io::ErrorKind::ResourceBusy => {
                MarkerStoreFailureKind::Busy
            }
            io::ErrorKind::StorageFull => MarkerStoreFailureKind::StorageFull,
            io::ErrorKind::Unsupported => MarkerStoreFailureKind::Unsupported,
            _ => MarkerStoreFailureKind::Other,
        };
        Self { kind, os_code: error.raw_os_error() }
    }
}

impl fmt::Display for MarkerStoreFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "panic marker store failed: {:?}", self.kind)?;
        if let Some(os_code) = self.os_code {
            write!(formatter, " (OS code {os_code})")?;
        }
        Ok(())
    }
}

impl Error for MarkerStoreFailure {}

/// Semantic result of one prepared marker commit.
///
/// The platform adapter determines this from the native operation contract,
/// not from an OS error-code allowlist in this module. An uncertain commit is
/// permanently conservative for the session: shutdown will preserve every
/// known copy rather than attempt reconciliation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum MarkerCommitOutcome {
    NotCommitted,
    Committed,
    CommittedButUnverified,
}

/// Allocation-free result returned to the panic hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkerCommitResult {
    pub(crate) outcome: MarkerCommitOutcome,
    pub(crate) failure: Option<MarkerStoreFailure>,
}

impl MarkerCommitResult {
    pub(crate) const fn not_committed(failure: MarkerStoreFailure) -> Self {
        Self { outcome: MarkerCommitOutcome::NotCommitted, failure: Some(failure) }
    }

    pub(crate) const fn committed() -> Self {
        Self { outcome: MarkerCommitOutcome::Committed, failure: None }
    }

    pub(crate) const fn committed_but_unverified(failure: MarkerStoreFailure) -> Self {
        Self { outcome: MarkerCommitOutcome::CommittedButUnverified, failure: Some(failure) }
    }
}

/// Explicit result of validating/recovering the two fixed marker slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparationRecovery {
    Ready,
    KnownCopyPreserved(MarkerStoreFailure),
    Uncertain(MarkerStoreFailure),
}

/// Validated, bounded marker bytes captured while the native capability was
/// prepared. The private fields prevent adapters from bypassing validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarkerSnapshot {
    bytes: Vec<u8>,
    owner: SessionToken,
}

impl MarkerSnapshot {
    /// Only the Windows bridge and the tests build snapshots from raw slot
    /// bytes; other targets have no native store.
    #[cfg(any(windows, test))]
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self, MarkerStoreFailure> {
        if bytes.len() > MAX_MARKER_BYTES {
            return Err(MarkerStoreFailure::new(MarkerStoreFailureKind::MarkerTooLarge, None));
        }
        let validated =
            validate_marker(&bytes, ValidationPolicy::Previewable).map_err(|_error| {
                MarkerStoreFailure::new(MarkerStoreFailureKind::InvalidMarker, None)
            })?;
        Ok(Self { bytes, owner: validated.owner })
    }
}

/// State of one fixed slot captured by `MarkerStore::prepare`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MarkerSlotSnapshot {
    Missing,
    Present(MarkerSnapshot),
    Unavailable(MarkerStoreFailure),
}

/// Bounded initial view supplied with the prepared native capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedMarkerView {
    pub(crate) target: MarkerSlotSnapshot,
    pub(crate) sibling: MarkerSlotSnapshot,
}

/// Prepared session returned before the process hook is installed.
///
/// `capability` must retain every directory/file handle and native identity
/// needed by commit, preview/delete policy, and conditional reconciliation.
/// Those later operations receive no filesystem paths and may not reopen a
/// target by an untrusted or newly resolved path.
pub(crate) struct PreparedMarkerSession {
    pub(crate) capability: Arc<dyn PreparedMarkerCapability>,
    pub(crate) initial: PreparedMarkerView,
    pub(crate) recovery: PreparationRecovery,
}

impl PreparedMarkerSession {
    pub(crate) fn new(
        capability: Arc<dyn PreparedMarkerCapability>,
        initial: PreparedMarkerView,
        recovery: PreparationRecovery,
    ) -> Self {
        Self { capability, initial, recovery }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code, reason = "constructed by the native Windows bridge")]
pub(crate) enum OwnedReconcileOutcome {
    RestoredPrevious,
    RemovedSessionMarker,
    OwnershipLost,
    MarkerMissing,
    PreservedUncertain,
}

#[allow(dead_code, reason = "read by the native Windows bridge")]
pub(crate) enum OwnedReconcileAction<'a> {
    Restore(&'a [u8]),
    Delete,
}

pub(crate) mod sealed {
    pub(crate) trait Store {}
    pub(crate) trait PreparedCapability {}
}

/// Factory for a crate-controlled, pathless marker session capability.
///
/// Path resolution and bounded initial reads happen exactly once here, before
/// hook installation. A production Windows adapter must retain native handles
/// and identities in the returned capability. Preparation failure means there
/// is no safe hook target and aborts installation.
pub(crate) trait MarkerStore: sealed::Store + Send + Sync + 'static {
    fn prepare(
        &self,
        target: &Path,
        sibling: &Path,
    ) -> Result<PreparedMarkerSession, MarkerStoreFailure>;
}

/// Operations available only through a previously prepared session.
///
/// This trait is crate-private and sealed. `commit` is the only method invoked
/// by the process hook; it must not allocate, acquire application locks, panic,
/// inspect process/user data, or resolve fallback paths. Native partial success
/// must be represented as `CommittedButUnverified`; the adapter must retain the
/// only known complete copy. In particular, Windows errors 1176/1177 are not
/// interpreted here and must never cause a sibling to be discarded.
pub(crate) trait PreparedMarkerCapability:
    sealed::PreparedCapability + Send + Sync + 'static
{
    fn replacement_semantics(&self) -> ReplacementSemantics;

    fn ownership_reconciliation(&self) -> OwnershipReconciliation;

    /// Commits through retained native ownership only. If a target that was
    /// initially missing appears, or a prepared identity changes, the adapter
    /// must preserve the foreign object and return `NotCommitted`. A native
    /// operation that may have committed but cannot prove its final identity
    /// returns `CommittedButUnverified` and preserves every known copy.
    fn commit(&self, contents: &[u8]) -> MarkerCommitResult;

    /// Reconciles only the expected session marker after shutdown quiescence.
    /// `AtomicFileIdentity` implementations must check identity and token on
    /// a retained handle that pins the object against unlink and replacement,
    /// must apply `Delete` to that very inspected object as one native
    /// conditional operation, and may apply `Restore` only by an atomic
    /// rename over the pinned object immediately after releasing that pin;
    /// the adapter must document any residual window of that release. They
    /// must conditionally clean an owned sibling without touching any other
    /// token or file identity. The bridge must flush explicitly and must not
    /// use unsupported `REPLACEFILE_WRITE_THROUGH`. Any post-operation
    /// uncertainty returns `PreservedUncertain` and preserves every known
    /// complete copy.
    fn reconcile_if_owned(
        &self,
        expected_owner: SessionToken,
        action: OwnedReconcileAction<'_>,
    ) -> Result<OwnedReconcileOutcome, MarkerStoreFailure>;

    /// Explicit user deletion using retained handles/identities. An identity
    /// mismatch or uncertain mutation preserves the file and is reported.
    fn delete_explicit(&self) -> Result<bool, MarkerStoreFailure>;
}

/// Standard-library, same-directory best-effort marker store.
///
/// This adapter syncs the temporary file and calls [`fs::rename`]. It does not
/// claim stronger atomic replacement or directory-entry durability than the
/// portable standard-library contract provides. It deliberately does not
/// perform shutdown reconciliation: a portable
/// token-check followed by rename/delete has a cross-process TOCTOU window.
/// It is deliberately unavailable in production Windows builds; Windows must
/// inject a native prepared capability before enabling the process hook.
#[cfg(any(test, not(windows)))]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StdMarkerStore;

#[cfg(any(test, not(windows)))]
impl sealed::Store for StdMarkerStore {}

#[cfg(any(test, not(windows)))]
impl MarkerStore for StdMarkerStore {
    fn prepare(
        &self,
        target: &Path,
        sibling: &Path,
    ) -> Result<PreparedMarkerSession, MarkerStoreFailure> {
        let paths = MarkerPaths { target: target.to_owned(), sibling: sibling.to_owned() };
        let target = read_snapshot_slot(&paths.target);
        let sibling = read_snapshot_slot(&paths.sibling);
        let recovery = classify_recovery(&target, &sibling);
        let target_present = matches!(target, MarkerSlotSnapshot::Present(_));
        let sibling_present = matches!(sibling, MarkerSlotSnapshot::Present(_));
        let capability =
            Arc::new(StdPreparedMarkerCapability { paths, target_present, sibling_present });
        Ok(PreparedMarkerSession::new(capability, PreparedMarkerView { target, sibling }, recovery))
    }
}

#[cfg(any(test, not(windows)))]
struct StdPreparedMarkerCapability {
    paths: MarkerPaths,
    target_present: bool,
    sibling_present: bool,
}

#[cfg(any(test, not(windows)))]
impl sealed::PreparedCapability for StdPreparedMarkerCapability {}

#[cfg(any(test, not(windows)))]
impl PreparedMarkerCapability for StdPreparedMarkerCapability {
    fn replacement_semantics(&self) -> ReplacementSemantics {
        ReplacementSemantics::BestEffortRename
    }

    fn ownership_reconciliation(&self) -> OwnershipReconciliation {
        OwnershipReconciliation::TokenCheckedBestEffort
    }

    fn commit(&self, contents: &[u8]) -> MarkerCommitResult {
        if contents.len() > MAX_MARKER_BYTES {
            return MarkerCommitResult::not_committed(MarkerStoreFailure::new(
                MarkerStoreFailureKind::MarkerTooLarge,
                None,
            ));
        }
        if validate_marker(contents, ValidationPolicy::Current).is_err() {
            return MarkerCommitResult::not_committed(MarkerStoreFailure::new(
                MarkerStoreFailureKind::InvalidMarker,
                None,
            ));
        }

        let mut file =
            match OpenOptions::new().write(true).create_new(true).open(&self.paths.sibling) {
                Ok(file) => file,
                Err(error) => {
                    return MarkerCommitResult::not_committed(MarkerStoreFailure::from_io(&error));
                }
            };

        let write_result =
            file.write_all(contents).and_then(|()| file.flush()).and_then(|()| file.sync_all());
        drop(file);

        if let Err(error) = write_result {
            // This process exclusively created the incomplete sibling and no
            // replacement was attempted, so removing this partial write cannot
            // discard a previously complete marker.
            let _ignored = fs::remove_file(&self.paths.sibling);
            return MarkerCommitResult::not_committed(MarkerStoreFailure::from_io(&error));
        }

        // A rename/ReplaceFile-style failure can leave the sibling as the only
        // complete copy. Preserve it for validated startup recovery.
        match fs::rename(&self.paths.sibling, &self.paths.target) {
            Ok(()) => MarkerCommitResult::committed(),
            Err(error) => {
                MarkerCommitResult::committed_but_unverified(MarkerStoreFailure::from_io(&error))
            }
        }
    }

    fn reconcile_if_owned(
        &self,
        _expected_owner: SessionToken,
        _action: OwnedReconcileAction<'_>,
    ) -> Result<OwnedReconcileOutcome, MarkerStoreFailure> {
        Err(MarkerStoreFailure::new(MarkerStoreFailureKind::Unsupported, None))
    }

    fn delete_explicit(&self) -> Result<bool, MarkerStoreFailure> {
        let mut deleted = false;
        if self.target_present {
            fs::remove_file(&self.paths.target)
                .map_err(|error| MarkerStoreFailure::from_io(&error))?;
            deleted = true;
        }
        if self.sibling_present {
            fs::remove_file(&self.paths.sibling)
                .map_err(|error| MarkerStoreFailure::from_io(&error))?;
            deleted = true;
        }
        Ok(deleted)
    }
}

/// Classifies the two fixed slots captured by a store's preparation.
///
/// A present sibling is a preserved known copy that blocks cleanup until an
/// explicit decision; an unavailable slot makes the whole preparation
/// uncertain.
fn classify_recovery(
    target: &MarkerSlotSnapshot,
    sibling: &MarkerSlotSnapshot,
) -> PreparationRecovery {
    match (target, sibling) {
        (_, MarkerSlotSnapshot::Unavailable(failure))
        | (MarkerSlotSnapshot::Unavailable(failure), MarkerSlotSnapshot::Present(_)) => {
            PreparationRecovery::Uncertain(*failure)
        }
        (MarkerSlotSnapshot::Present(target), MarkerSlotSnapshot::Present(sibling))
            if target.owner != sibling.owner =>
        {
            PreparationRecovery::KnownCopyPreserved(MarkerStoreFailure::new(
                MarkerStoreFailureKind::OwnershipConflict,
                None,
            ))
        }
        (_, MarkerSlotSnapshot::Present(_)) => PreparationRecovery::KnownCopyPreserved(
            MarkerStoreFailure::new(MarkerStoreFailureKind::PreservedSibling, None),
        ),
        (_, MarkerSlotSnapshot::Missing) => PreparationRecovery::Ready,
    }
}

/// Windows bridge from the sealed capability contract to the retained-handle
/// platform session.
///
/// The bridge forwards the two fixed paths exactly once during preparation.
/// Afterwards it holds no path: commit, reconciliation, and explicit deletion
/// operate on the handles, identities, and precomputed buffers retained by
/// [`WindowsMarkerSession`](diskpie_platform::WindowsMarkerSession). The
/// bridge contains no `unsafe` code.
#[cfg(windows)]
mod native {
    use std::{path::Path, sync::Arc};

    use diskpie_platform::{
        DiagnosticFileError, DiagnosticFileErrorKind, MarkerCommitStatus, MarkerReconcileAction,
        MarkerReconcileOutcome, MarkerSlotState, WindowsMarkerSession,
    };

    use super::{
        MAX_MARKER_BYTES, MarkerCommitResult, MarkerSlotSnapshot, MarkerSnapshot, MarkerStore,
        MarkerStoreFailure, MarkerStoreFailureKind, OwnedReconcileAction, OwnedReconcileOutcome,
        OwnershipReconciliation, PreparedMarkerCapability, PreparedMarkerSession,
        PreparedMarkerView, ReplacementSemantics, SessionToken, ValidationPolicy,
        classify_recovery, sealed, validate_marker,
    };

    /// Production Windows store: prepares one native session per install.
    #[derive(Clone, Copy, Debug, Default)]
    pub(super) struct WindowsMarkerStore;

    impl sealed::Store for WindowsMarkerStore {}

    impl MarkerStore for WindowsMarkerStore {
        fn prepare(
            &self,
            target: &Path,
            sibling: &Path,
        ) -> Result<PreparedMarkerSession, MarkerStoreFailure> {
            let session =
                WindowsMarkerSession::prepare(target, sibling).map_err(failure_from_native)?;
            let target = snapshot_from_slot(session.target_slot());
            let sibling = snapshot_from_slot(session.sibling_slot());
            let recovery = classify_recovery(&target, &sibling);
            Ok(PreparedMarkerSession::new(
                Arc::new(WindowsPreparedCapability { session }),
                PreparedMarkerView { target, sibling },
                recovery,
            ))
        }
    }

    fn snapshot_from_slot(slot: MarkerSlotState<'_>) -> MarkerSlotSnapshot {
        match slot {
            MarkerSlotState::Missing => MarkerSlotSnapshot::Missing,
            MarkerSlotState::Present(bytes) => match MarkerSnapshot::from_bytes(bytes.to_vec()) {
                Ok(snapshot) => MarkerSlotSnapshot::Present(snapshot),
                Err(failure) => MarkerSlotSnapshot::Unavailable(failure),
            },
            MarkerSlotState::Unavailable(error) => {
                MarkerSlotSnapshot::Unavailable(failure_from_native(error))
            }
        }
    }

    /// Maps the path-free platform error onto the store failure vocabulary.
    pub(super) const fn failure_from_native(error: DiagnosticFileError) -> MarkerStoreFailure {
        let kind = match error.kind {
            DiagnosticFileErrorKind::AccessDenied => MarkerStoreFailureKind::AccessDenied,
            DiagnosticFileErrorKind::NotFound => MarkerStoreFailureKind::NotFound,
            DiagnosticFileErrorKind::InvalidDestination | DiagnosticFileErrorKind::ReparsePoint => {
                MarkerStoreFailureKind::InvalidData
            }
            DiagnosticFileErrorKind::OwnedDestination => MarkerStoreFailureKind::OwnershipConflict,
            DiagnosticFileErrorKind::InvalidContent => MarkerStoreFailureKind::InvalidMarker,
            DiagnosticFileErrorKind::TooLarge => MarkerStoreFailureKind::MarkerTooLarge,
            DiagnosticFileErrorKind::AlreadyExists => MarkerStoreFailureKind::Busy,
            DiagnosticFileErrorKind::PartialReplacement => MarkerStoreFailureKind::PreservedSibling,
            DiagnosticFileErrorKind::Unsupported => MarkerStoreFailureKind::Unsupported,
            DiagnosticFileErrorKind::Io => MarkerStoreFailureKind::Other,
        };
        MarkerStoreFailure::new(kind, error.os_code)
    }

    struct WindowsPreparedCapability {
        session: WindowsMarkerSession,
    }

    impl sealed::PreparedCapability for WindowsPreparedCapability {}

    impl PreparedMarkerCapability for WindowsPreparedCapability {
        /// Handle-relative `FileRenameInformationEx` with POSIX semantics makes
        /// the replacement atomically visible; the platform session does not
        /// claim directory-entry durability across power loss.
        fn replacement_semantics(&self) -> ReplacementSemantics {
            ReplacementSemantics::AtomicVisibility
        }

        /// The session checks identity and token on a handle opened without
        /// delete sharing, deletes through that very handle, and restores by
        /// an atomic rename issued immediately after releasing it (Windows
        /// has no compare-and-rename; the platform documents that single-call
        /// window). Results are classified from post-operation identities.
        fn ownership_reconciliation(&self) -> OwnershipReconciliation {
            OwnershipReconciliation::AtomicFileIdentity
        }

        fn commit(&self, contents: &[u8]) -> MarkerCommitResult {
            if contents.len() > MAX_MARKER_BYTES {
                return MarkerCommitResult::not_committed(MarkerStoreFailure::new(
                    MarkerStoreFailureKind::MarkerTooLarge,
                    None,
                ));
            }
            if validate_marker(contents, ValidationPolicy::Current).is_err() {
                return MarkerCommitResult::not_committed(MarkerStoreFailure::new(
                    MarkerStoreFailureKind::InvalidMarker,
                    None,
                ));
            }
            match self.session.commit(contents) {
                MarkerCommitStatus::Committed => MarkerCommitResult::committed(),
                MarkerCommitStatus::NotCommitted(error) => {
                    MarkerCommitResult::not_committed(failure_from_native(error))
                }
                // The platform preserved every known copy; the runtime keeps
                // this session permanently conservative.
                MarkerCommitStatus::CommittedButUnverified(error) => {
                    MarkerCommitResult::committed_but_unverified(failure_from_native(error))
                }
            }
        }

        fn reconcile_if_owned(
            &self,
            expected_owner: SessionToken,
            action: OwnedReconcileAction<'_>,
        ) -> Result<OwnedReconcileOutcome, MarkerStoreFailure> {
            let native_action = match action {
                OwnedReconcileAction::Restore(previous) => MarkerReconcileAction::Restore(previous),
                OwnedReconcileAction::Delete => MarkerReconcileAction::Delete,
            };
            let owner_check = |bytes: &[u8]| {
                validate_marker(bytes, ValidationPolicy::Previewable)
                    .is_ok_and(|validated| validated.owner == expected_owner)
            };
            self.session
                .reconcile_if_owned(native_action, &owner_check)
                .map(|outcome| match outcome {
                    MarkerReconcileOutcome::RestoredPrevious => {
                        OwnedReconcileOutcome::RestoredPrevious
                    }
                    MarkerReconcileOutcome::RemovedSessionMarker => {
                        OwnedReconcileOutcome::RemovedSessionMarker
                    }
                    MarkerReconcileOutcome::OwnershipLost => OwnedReconcileOutcome::OwnershipLost,
                    MarkerReconcileOutcome::MarkerMissing => OwnedReconcileOutcome::MarkerMissing,
                    MarkerReconcileOutcome::PreservedUncertain => {
                        OwnedReconcileOutcome::PreservedUncertain
                    }
                })
                .map_err(failure_from_native)
        }

        fn delete_explicit(&self) -> Result<bool, MarkerStoreFailure> {
            self.session.delete_explicit().map_err(failure_from_native)
        }
    }
}

/// Operation that produced a sanitized public error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CrashMarkerOperation {
    PrepareStore,
    ReadPreviousMarker,
    InstallHook,
    Preview,
    Delete,
    ReconcileOwnedMarker,
}

/// Stable public marker error category.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CrashMarkerErrorKind {
    AccessDenied,
    NotFound,
    InvalidData,
    Busy,
    StorageFull,
    Unsupported,
    MarkerTooLarge,
    InvalidMarker,
    PreservedSibling,
    OwnershipConflict,
    HookAlreadyInstalled,
    PanickingThread,
    Other,
}

/// Sanitized panic-marker error without a filesystem path or arbitrary text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrashMarkerError {
    pub operation: CrashMarkerOperation,
    pub kind: CrashMarkerErrorKind,
    pub os_code: Option<i32>,
}

impl CrashMarkerError {
    const fn new(operation: CrashMarkerOperation, kind: CrashMarkerErrorKind) -> Self {
        Self { operation, kind, os_code: None }
    }

    const fn from_store(operation: CrashMarkerOperation, failure: MarkerStoreFailure) -> Self {
        let kind = match failure.kind {
            MarkerStoreFailureKind::AccessDenied => CrashMarkerErrorKind::AccessDenied,
            MarkerStoreFailureKind::NotFound => CrashMarkerErrorKind::NotFound,
            MarkerStoreFailureKind::InvalidData => CrashMarkerErrorKind::InvalidData,
            MarkerStoreFailureKind::Busy => CrashMarkerErrorKind::Busy,
            MarkerStoreFailureKind::StorageFull => CrashMarkerErrorKind::StorageFull,
            MarkerStoreFailureKind::Unsupported => CrashMarkerErrorKind::Unsupported,
            MarkerStoreFailureKind::MarkerTooLarge => CrashMarkerErrorKind::MarkerTooLarge,
            MarkerStoreFailureKind::InvalidMarker => CrashMarkerErrorKind::InvalidMarker,
            MarkerStoreFailureKind::PreservedSibling => CrashMarkerErrorKind::PreservedSibling,
            MarkerStoreFailureKind::OwnershipConflict => CrashMarkerErrorKind::OwnershipConflict,
            MarkerStoreFailureKind::Other => CrashMarkerErrorKind::Other,
        };
        Self { operation, kind, os_code: failure.os_code }
    }
}

impl fmt::Display for CrashMarkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "panic marker {:?} failed: {:?}", self.operation, self.kind)?;
        if let Some(os_code) = self.os_code {
            write!(formatter, " (OS code {os_code})")?;
        }
        Ok(())
    }
}

impl Error for CrashMarkerError {}

/// Validated, bounded text suitable for an explicit user preview or export.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkerPreview {
    text: Box<str>,
}

impl MarkerPreview {
    /// Returns the complete validated marker text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns the complete validated marker bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// Consumes the preview and returns its bounded text.
    pub fn into_string(self) -> String {
        self.text.into()
    }
}

struct MarkerPaths {
    target: PathBuf,
    sibling: PathBuf,
}

impl MarkerPaths {
    fn new(crashes_directory: &Path) -> Self {
        Self {
            target: crashes_directory.join(MARKER_FILE_NAME),
            sibling: crashes_directory.join(TEMPORARY_FILE_NAMES[0]),
        }
    }
}

enum PreviousMarkerState {
    Missing,
    Snapshot(MarkerSnapshot),
    Unavailable,
}

struct SessionPreparation {
    capability: Arc<dyn PreparedMarkerCapability>,
    previous_marker: PreviousMarkerState,
    failures: Vec<CrashMarkerError>,
    recovery_degraded: bool,
}

fn prepare_session(prepared: PreparedMarkerSession) -> SessionPreparation {
    let mut failures = Vec::with_capacity(2);
    let recovery_degraded = match prepared.recovery {
        PreparationRecovery::Ready => false,
        PreparationRecovery::KnownCopyPreserved(failure)
        | PreparationRecovery::Uncertain(failure) => {
            failures
                .push(CrashMarkerError::from_store(CrashMarkerOperation::PrepareStore, failure));
            true
        }
    };

    let previous_marker = match prepared.initial.target {
        MarkerSlotSnapshot::Present(snapshot) => PreviousMarkerState::Snapshot(snapshot),
        MarkerSlotSnapshot::Missing => PreviousMarkerState::Missing,
        MarkerSlotSnapshot::Unavailable(failure) => {
            failures.push(CrashMarkerError::from_store(
                CrashMarkerOperation::ReadPreviousMarker,
                failure,
            ));
            PreviousMarkerState::Unavailable
        }
    };

    SessionPreparation {
        capability: prepared.capability,
        previous_marker,
        failures,
        recovery_degraded,
    }
}

struct HookState {
    capability: Arc<dyn PreparedMarkerCapability>,
    markers: HookMarkers,
    replacement_semantics: ReplacementSemantics,
    ownership_reconciliation: OwnershipReconciliation,
    session_token: SessionToken,
    session_panicked: AtomicBool,
    commit_observation: AtomicU8,
}

const COMMIT_NOT_ATTEMPTED: u8 = 0;
const COMMIT_NOT_COMMITTED: u8 = 1;
const COMMIT_COMMITTED: u8 = 2;
const COMMIT_UNVERIFIED: u8 = 3;

struct HookInstallClaim {
    committed: bool,
}

impl HookInstallClaim {
    fn acquire() -> Result<Self, CrashMarkerError> {
        if thread::panicking() {
            return Err(CrashMarkerError::new(
                CrashMarkerOperation::InstallHook,
                CrashMarkerErrorKind::PanickingThread,
            ));
        }
        if HOOK_INSTALLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(CrashMarkerError::new(
                CrashMarkerOperation::InstallHook,
                CrashMarkerErrorKind::HookAlreadyInstalled,
            ));
        }
        Ok(Self { committed: false })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for HookInstallClaim {
    fn drop(&mut self) {
        if !self.committed {
            HOOK_INSTALLED.store(false, Ordering::Release);
        }
    }
}

/// Owns one process panic-marker session.
///
/// The hook remains installed until process exit. `Drop` intentionally performs
/// no cleanup because unwinding is not evidence of a clean exit. Shutdown must
/// supply a [`ShutdownQuiescenceProof`] issued only by the composition root.
#[must_use = "retain the crash runtime through application shutdown"]
pub struct CrashRuntime {
    state: Arc<HookState>,
    previous_marker: PreviousMarkerState,
    preparation_failures: Vec<CrashMarkerError>,
    recovery_degraded: bool,
    _installing_thread_role: ThreadRoleGuard,
}

/// Explicit proof that every panic-capable provider has stopped.
///
/// The private field makes the value unforgeable to API callers. The only
/// production constructor is [`Self::from_receipts`], which demands one typed
/// receipt per owned worker family; a receipt can only be minted from real
/// join evidence. A detached provider invalidates any future proof.
///
/// Only the Windows composition root and the tests coordinate a shutdown,
/// so the proof and its receipts exist on no other target.
#[cfg(any(windows, test))]
#[derive(Debug)]
pub(crate) struct ShutdownQuiescenceProof {
    _private: (),
}

#[cfg(any(windows, test))]
impl ShutdownQuiescenceProof {
    /// Assembles the proof from the receipts of every worker family the
    /// composition root owns. Adding a worker family means adding a receipt
    /// parameter here, so a caller cannot forget new evidence silently.
    ///
    /// The diagnostics receipt is minted only by
    /// [`LocalDiagnostics::finish`](crate::local_diagnostics::LocalDiagnostics::finish)
    /// when the worker guard received the worker's own completion message;
    /// `HandedOff` and `WorkerFailed` leave a thread that may still run (or
    /// may have died panicking) under the process-wide reaper and never carry
    /// one, so the crash runtime is dropped and the marker is preserved.
    #[cfg(windows)]
    pub(crate) const fn from_receipts(
        diagnostics: DiagnosticsQuiescenceReceipt,
        runtime: RuntimeQuiescenceReceipt,
    ) -> Self {
        // The receipts are consumed, not inspected: their private fields are
        // the evidence, and only their minting modules can produce them.
        let _joined_diagnostics_worker = diagnostics;
        let RuntimeQuiescenceReceipt { _private: () } = runtime;
        Self { _private: () }
    }

    #[cfg(test)]
    const fn for_test() -> Self {
        Self { _private: () }
    }
}

/// Receipt for the scan, layout, and Shell runtime workers.
///
/// The scan runtime, layout worker, and Shell service are not yet composed by
/// the executable: today the shell spawns no thread, so the only owned worker
/// is the diagnostics writer. This receipt is therefore a placeholder that
/// proves nothing beyond "no runtime was composed". The UI wiring that starts
/// those services MUST replace [`Self::no_runtime_composed`] with a
/// constructor that consumes the joined receipts of every runtime worker;
/// keeping the placeholder after that point would forge quiescence.
#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct RuntimeQuiescenceReceipt {
    _private: (),
}

/// Facts the composition root observed during its ordered shutdown.
///
/// Every field must hold for a receipt to exist; the struct only names them so
/// a caller cannot pass a new kind of worker without deciding what it proves.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeShutdownEvidence {
    /// `RuntimeController::is_shutdown_complete` was observed after a
    /// requested shutdown, within the root's bounded wait.
    pub(crate) runtime_shutdown_complete: bool,
    /// Scan sessions still held by background retirement at that moment. A
    /// retiring session may still be inside a blocking provider call, so only
    /// zero counts as every scan worker having stopped.
    pub(crate) retiring_scans: usize,
    /// The Shell STA was never started, or `finish` joined it in time. A
    /// handed-off or reaper-unavailable STA may still be running.
    pub(crate) shell_stopped: bool,
    /// The resolver worker thread was joined within its deadline.
    pub(crate) resolver_joined: bool,
    /// The diagnostic bridge thread was joined within its deadline.
    pub(crate) bridge_joined: bool,
}

#[cfg(windows)]
impl RuntimeQuiescenceReceipt {
    /// Evidence that no scan/layout/Shell runtime was ever started in this
    /// process. Valid only when startup failed before the composition root
    /// created any of them.
    pub(crate) const fn no_runtime_composed() -> Self {
        Self { _private: () }
    }

    /// Mints the receipt only when every UI-owned worker family provably
    /// stopped. Any timed-out join, handed-off worker, or retiring scan
    /// session yields `None`, and the caller then preserves the marker.
    pub(crate) const fn from_shutdown(evidence: RuntimeShutdownEvidence) -> Option<Self> {
        if evidence.runtime_shutdown_complete
            && evidence.retiring_scans == 0
            && evidence.shell_stopped
            && evidence.resolver_joined
            && evidence.bridge_joined
        {
            Some(Self { _private: () })
        } else {
            None
        }
    }
}

/// Result of clean-session reconciliation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FinishCleanOutcome {
    NoSessionPanic,
    RestoredPrevious,
    RemovedSessionMarker,
    OwnershipLost,
    SessionMarkerMissing,
    PreservedPreviousUnavailable,
    PreservedPreparationDegraded,
    PreservedWithoutAtomicReconciliation,
    NoSessionMarkerCommitted,
    PreservedCommitUncertain,
    PreservedReconciliationUncertain,
}

impl CrashRuntime {
    /// Installs the local hook with this target's production store.
    ///
    /// The crash directory must already have been resolved and created by
    /// the project-path adapter (ADR 0017 item 1). On Windows the
    /// retained-handle session is prepared once here: it keeps the parent
    /// directory handle and the bounded previous-marker snapshot, and the
    /// hook afterwards commits only prebuilt bounded data through it. Other
    /// targets use the standard-library best-effort store, which never
    /// reconciles at shutdown.
    pub fn install(crashes_directory: &Path) -> Result<Self, CrashMarkerError> {
        Self::install_with_store(crashes_directory, Arc::new(production_store()))
    }

    /// Installs the hook with a sealed crate-controlled platform adapter.
    pub(crate) fn install_with_store(
        crashes_directory: &Path,
        store: Arc<dyn MarkerStore>,
    ) -> Result<Self, CrashMarkerError> {
        let paths = MarkerPaths::new(crashes_directory);

        // Claim installation before the first store call or marker read.
        let install_claim = HookInstallClaim::acquire()?;
        let session_token = SessionToken::generate();
        let prepared = store.prepare(&paths.target, &paths.sibling).map_err(|failure| {
            CrashMarkerError::from_store(CrashMarkerOperation::PrepareStore, failure)
        })?;
        let preparation = prepare_session(prepared);
        let replacement_semantics = preparation.capability.replacement_semantics();
        let ownership_reconciliation = preparation.capability.ownership_reconciliation();
        let markers = HookMarkers::new(unix_seconds_now(), session_token).ok_or_else(|| {
            CrashMarkerError::new(
                CrashMarkerOperation::InstallHook,
                CrashMarkerErrorKind::InvalidData,
            )
        })?;

        if thread::panicking() {
            // The claim guard rolls back only because hook installation can no
            // longer occur safely. Store/read failures remain nonfatal.
            return Err(CrashMarkerError::new(
                CrashMarkerOperation::InstallHook,
                CrashMarkerErrorKind::PanickingThread,
            ));
        }

        let installing_thread_role = enter_thread_role(ThreadRole::Main);
        let state = Arc::new(HookState {
            capability: preparation.capability,
            markers,
            replacement_semantics,
            ownership_reconciliation,
            session_token,
            session_panicked: AtomicBool::new(false),
            commit_observation: AtomicU8::new(COMMIT_NOT_ATTEMPTED),
        });
        install_process_hook(Arc::clone(&state));
        install_claim.commit();

        Ok(Self {
            state,
            previous_marker: preparation.previous_marker,
            preparation_failures: preparation.failures,
            recovery_degraded: preparation.recovery_degraded,
            _installing_thread_role: installing_thread_role,
        })
    }

    /// Reports the exact replacement contract of the installed store.
    pub fn replacement_semantics(&self) -> ReplacementSemantics {
        self.state.replacement_semantics
    }

    /// Reports whether shutdown reconciliation is native identity-conditional.
    pub fn ownership_reconciliation(&self) -> OwnershipReconciliation {
        self.state.ownership_reconciliation
    }

    /// Returns every sanitized preparation issue (currently at most two).
    pub fn preparation_failures(&self) -> &[CrashMarkerError] {
        &self.preparation_failures
    }

    /// Returns the previous-read issue when present, otherwise the first issue.
    pub fn preparation_failure(&self) -> Option<CrashMarkerError> {
        self.preparation_failures
            .iter()
            .copied()
            .find(|failure| failure.operation == CrashMarkerOperation::ReadPreviousMarker)
            .or_else(|| self.preparation_failures.first().copied())
    }

    /// Returns the bounded previous marker captured by the installed session.
    ///
    /// This never prepares or reopens a filesystem path. Windows UI/export
    /// integration must use this retained snapshot while the native session
    /// capability is live instead of opening a second capability for preview.
    pub fn prepared_previous_marker(&self) -> Result<Option<MarkerPreview>, CrashMarkerError> {
        match &self.previous_marker {
            PreviousMarkerState::Missing => Ok(None),
            PreviousMarkerState::Unavailable => match self.preparation_failure() {
                Some(failure) => Err(failure),
                None => Err(CrashMarkerError::new(
                    CrashMarkerOperation::ReadPreviousMarker,
                    CrashMarkerErrorKind::Other,
                )),
            },
            PreviousMarkerState::Snapshot(previous) => {
                match String::from_utf8(previous.bytes.clone()) {
                    Ok(text) => Ok(Some(MarkerPreview { text: text.into_boxed_str() })),
                    Err(_error) => Err(CrashMarkerError::new(
                        CrashMarkerOperation::ReadPreviousMarker,
                        CrashMarkerErrorKind::InvalidMarker,
                    )),
                }
            }
        }
    }

    /// Returns whether any panic hook ran during this application session.
    pub fn session_panicked(&self) -> bool {
        self.state.session_panicked.load(Ordering::Acquire)
    }

    /// Reconciles only a marker still owned by this session.
    ///
    /// The proof must come from the composite shutdown coordinator after every
    /// panic-capable worker has stopped. Without it there is no cleanup API;
    /// dropping the runtime preserves all marker state.
    #[cfg(any(windows, test))]
    pub(crate) fn finish_clean(
        self,
        _proof: ShutdownQuiescenceProof,
    ) -> Result<FinishCleanOutcome, CrashMarkerError> {
        if !self.state.session_panicked.load(Ordering::Acquire) {
            return Ok(FinishCleanOutcome::NoSessionPanic);
        }

        match self.state.commit_observation.load(Ordering::Acquire) {
            COMMIT_NOT_ATTEMPTED | COMMIT_NOT_COMMITTED => {
                return Ok(FinishCleanOutcome::NoSessionMarkerCommitted);
            }
            COMMIT_UNVERIFIED => return Ok(FinishCleanOutcome::PreservedCommitUncertain),
            COMMIT_COMMITTED => {}
            _unexpected => return Ok(FinishCleanOutcome::PreservedCommitUncertain),
        }

        if matches!(self.previous_marker, PreviousMarkerState::Unavailable) {
            return Ok(FinishCleanOutcome::PreservedPreviousUnavailable);
        }
        if self.recovery_degraded {
            return Ok(FinishCleanOutcome::PreservedPreparationDegraded);
        }
        if self.state.ownership_reconciliation != OwnershipReconciliation::AtomicFileIdentity {
            return Ok(FinishCleanOutcome::PreservedWithoutAtomicReconciliation);
        }

        let action = match &self.previous_marker {
            PreviousMarkerState::Snapshot(previous) => {
                OwnedReconcileAction::Restore(&previous.bytes)
            }
            PreviousMarkerState::Missing => OwnedReconcileAction::Delete,
            PreviousMarkerState::Unavailable => {
                return Ok(FinishCleanOutcome::PreservedPreviousUnavailable);
            }
        };

        self.state
            .capability
            .reconcile_if_owned(self.state.session_token, action)
            .map(|outcome| match outcome {
                OwnedReconcileOutcome::RestoredPrevious => FinishCleanOutcome::RestoredPrevious,
                OwnedReconcileOutcome::RemovedSessionMarker => {
                    FinishCleanOutcome::RemovedSessionMarker
                }
                OwnedReconcileOutcome::OwnershipLost => FinishCleanOutcome::OwnershipLost,
                OwnedReconcileOutcome::MarkerMissing => FinishCleanOutcome::SessionMarkerMissing,
                OwnedReconcileOutcome::PreservedUncertain => {
                    FinishCleanOutcome::PreservedReconciliationUncertain
                }
            })
            .map_err(|failure| {
                CrashMarkerError::from_store(CrashMarkerOperation::ReconcileOwnedMarker, failure)
            })
    }
}

impl fmt::Debug for CrashRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CrashRuntime")
            .field("replacement_semantics", &self.state.replacement_semantics)
            .field("ownership_reconciliation", &self.state.ownership_reconciliation)
            .field("session_panicked", &self.session_panicked())
            .field(
                "has_valid_previous_marker",
                &matches!(self.previous_marker, PreviousMarkerState::Snapshot(_)),
            )
            .field("preparation_failure_count", &self.preparation_failures.len())
            .finish_non_exhaustive()
    }
}

/// The store used by every production entry point on this target.
#[cfg(windows)]
fn production_store() -> impl MarkerStore {
    native::WindowsMarkerStore
}

#[cfg(not(windows))]
fn production_store() -> impl MarkerStore {
    StdMarkerStore
}

/// Reads a bounded marker for explicit preview/export.
///
/// The schema/version/OS/architecture tuple must match an exact compile-time
/// allowlist. Generic semver strings are rejected; a cross-version/architecture
/// marker is accepted only when that exact shipped tuple remains listed.
/// If the target is missing, a valid preserved sibling remains previewable.
/// While a [`CrashRuntime`] is live, prefer its retained
/// [`CrashRuntime::prepared_previous_marker`] over preparing a second session.
pub fn preview_marker(crashes_directory: &Path) -> Result<Option<MarkerPreview>, CrashMarkerError> {
    let paths = MarkerPaths::new(crashes_directory);
    let prepared = production_store()
        .prepare(&paths.target, &paths.sibling)
        .map_err(|failure| CrashMarkerError::from_store(CrashMarkerOperation::Preview, failure))?;
    preview_prepared(&prepared)
}

/// Reads only the bounded view supplied by an existing prepared capability.
pub(crate) fn preview_prepared(
    prepared: &PreparedMarkerSession,
) -> Result<Option<MarkerPreview>, CrashMarkerError> {
    let snapshot = match &prepared.initial.target {
        MarkerSlotSnapshot::Present(snapshot) => Some(snapshot),
        MarkerSlotSnapshot::Unavailable(failure) => {
            return Err(CrashMarkerError::from_store(CrashMarkerOperation::Preview, *failure));
        }
        MarkerSlotSnapshot::Missing => match &prepared.initial.sibling {
            MarkerSlotSnapshot::Present(snapshot) => Some(snapshot),
            MarkerSlotSnapshot::Missing => None,
            MarkerSlotSnapshot::Unavailable(failure) => {
                return Err(CrashMarkerError::from_store(CrashMarkerOperation::Preview, *failure));
            }
        },
    };
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };

    match String::from_utf8(snapshot.bytes.clone()) {
        Ok(text) => Ok(Some(MarkerPreview { text: text.into_boxed_str() })),
        Err(_error) => Err(CrashMarkerError::new(
            CrashMarkerOperation::Preview,
            CrashMarkerErrorKind::InvalidMarker,
        )),
    }
}

#[cfg(test)]
fn preview_marker_with_store(
    crashes_directory: &Path,
    store: &dyn MarkerStore,
) -> Result<Option<MarkerPreview>, CrashMarkerError> {
    let paths = MarkerPaths::new(crashes_directory);
    let prepared = store
        .prepare(&paths.target, &paths.sibling)
        .map_err(|failure| CrashMarkerError::from_store(CrashMarkerOperation::Preview, failure))?;
    preview_prepared(&prepared)
}

/// Explicitly deletes validated fixed target/sibling markers.
///
/// A fresh session inspects both slots and deletes only through handles whose
/// identities still match that inspection; a swapped object is preserved and
/// reported as an ownership conflict.
pub fn delete_marker(crashes_directory: &Path) -> Result<bool, CrashMarkerError> {
    let paths = MarkerPaths::new(crashes_directory);
    let prepared = production_store()
        .prepare(&paths.target, &paths.sibling)
        .map_err(|failure| CrashMarkerError::from_store(CrashMarkerOperation::Delete, failure))?;
    delete_prepared(prepared)
}

/// Deletes only through an existing prepared capability.
pub(crate) fn delete_prepared(prepared: PreparedMarkerSession) -> Result<bool, CrashMarkerError> {
    match prepared.recovery {
        PreparationRecovery::Ready => {}
        PreparationRecovery::KnownCopyPreserved(failure)
        | PreparationRecovery::Uncertain(failure) => {
            return Err(CrashMarkerError::from_store(CrashMarkerOperation::Delete, failure));
        }
    }
    for slot in [&prepared.initial.target, &prepared.initial.sibling] {
        if let MarkerSlotSnapshot::Unavailable(failure) = slot {
            return Err(CrashMarkerError::from_store(CrashMarkerOperation::Delete, *failure));
        }
    }
    prepared
        .capability
        .delete_explicit()
        .map_err(|failure| CrashMarkerError::from_store(CrashMarkerOperation::Delete, failure))
}

#[cfg(test)]
fn delete_marker_with_store(
    crashes_directory: &Path,
    store: &dyn MarkerStore,
) -> Result<bool, CrashMarkerError> {
    let paths = MarkerPaths::new(crashes_directory);
    let prepared = store
        .prepare(&paths.target, &paths.sibling)
        .map_err(|failure| CrashMarkerError::from_store(CrashMarkerOperation::Delete, failure))?;
    delete_prepared(prepared)
}

fn install_process_hook(state: Arc<HookState>) {
    std::panic::set_hook(Box::new(move |_panic_info| {
        record_panic(&state);
    }));
}

fn unix_seconds_now() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_clock_error) => 0,
    }
}

fn record_panic(state: &HookState) {
    let role = match THREAD_ROLE.try_with(Cell::get) {
        Ok(role) => role,
        Err(_access_error) => ThreadRole::Unknown,
    };
    record_panic_for_role(state, role);
}

fn record_panic_for_role(state: &HookState, role: ThreadRole) {
    state.session_panicked.store(true, Ordering::Release);
    let result = state.capability.commit(state.markers.for_role(role));
    let observation = match result.outcome {
        MarkerCommitOutcome::NotCommitted => COMMIT_NOT_COMMITTED,
        MarkerCommitOutcome::Committed => COMMIT_COMMITTED,
        MarkerCommitOutcome::CommittedButUnverified => COMMIT_UNVERIFIED,
    };
    state.commit_observation.fetch_max(observation, Ordering::AcqRel);
}

struct FixedMarker {
    bytes: [u8; MAX_MARKER_BYTES],
    len: usize,
    overflowed: bool,
}

impl FixedMarker {
    fn new() -> Self {
        Self { bytes: [0; MAX_MARKER_BYTES], len: 0, overflowed: false }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        let Some(end) = self.len.checked_add(bytes.len()) else {
            self.overflowed = true;
            return;
        };
        if end > self.bytes.len() {
            self.overflowed = true;
            return;
        }
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    fn push_line(&mut self, key: &[u8], value: &[u8]) {
        self.push(key);
        self.push(b"=");
        self.push(value);
        self.push(b"\n");
    }

    fn push_decimal(&mut self, mut value: u64) {
        let mut digits = [0_u8; 20];
        let mut cursor = digits.len();
        if value == 0 {
            cursor -= 1;
            digits[cursor] = b'0';
        } else {
            while value != 0 {
                cursor -= 1;
                digits[cursor] = b'0' + (value % 10) as u8;
                value /= 10;
            }
        }
        self.push(&digits[cursor..]);
    }

    fn push_token(&mut self, token: SessionToken) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in token.as_bytes() {
            self.push(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0x0f)]]);
        }
    }

    fn finish(self) -> Option<Self> {
        (!self.overflowed).then_some(self)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Fully formatted marker variants allocated before hook installation.
///
/// The panic path only selects one immutable slice; it never calls the clock,
/// allocates, or formats dynamic input.
struct HookMarkers {
    main: FixedMarker,
    ui: FixedMarker,
    scan_coordinator: FixedMarker,
    scan_worker: FixedMarker,
    layout_worker: FixedMarker,
    shell_worker: FixedMarker,
    unknown: FixedMarker,
}

impl HookMarkers {
    fn new(timestamp: u64, token: SessionToken) -> Option<Self> {
        Some(Self {
            main: build_marker(timestamp, ThreadRole::Main, token)?,
            ui: build_marker(timestamp, ThreadRole::Ui, token)?,
            scan_coordinator: build_marker(timestamp, ThreadRole::ScanCoordinator, token)?,
            scan_worker: build_marker(timestamp, ThreadRole::ScanWorker, token)?,
            layout_worker: build_marker(timestamp, ThreadRole::LayoutWorker, token)?,
            shell_worker: build_marker(timestamp, ThreadRole::ShellWorker, token)?,
            unknown: build_marker(timestamp, ThreadRole::Unknown, token)?,
        })
    }

    fn for_role(&self, role: ThreadRole) -> &[u8] {
        match role {
            ThreadRole::Main => self.main.as_bytes(),
            ThreadRole::Ui => self.ui.as_bytes(),
            ThreadRole::ScanCoordinator => self.scan_coordinator.as_bytes(),
            ThreadRole::ScanWorker => self.scan_worker.as_bytes(),
            ThreadRole::LayoutWorker => self.layout_worker.as_bytes(),
            ThreadRole::ShellWorker => self.shell_worker.as_bytes(),
            ThreadRole::Unknown => self.unknown.as_bytes(),
        }
    }
}

fn build_marker(timestamp: u64, role: ThreadRole, token: SessionToken) -> Option<FixedMarker> {
    let mut marker = FixedMarker::new();
    marker.push_line(b"schema", CURRENT_MARKER_IDENTITY.schema.as_bytes());
    marker.push(b"timestamp_unix_seconds=");
    marker.push_decimal(timestamp);
    marker.push(b"\n");
    marker.push_line(b"app_version", CURRENT_MARKER_IDENTITY.app_version.as_bytes());
    marker.push_line(b"target_os", CURRENT_MARKER_IDENTITY.target_os.as_bytes());
    marker.push_line(b"target_arch", CURRENT_MARKER_IDENTITY.target_arch.as_bytes());
    marker.push_line(b"code", PANIC_CODE.as_bytes());
    marker.push_line(b"thread_role", role.as_str().as_bytes());
    marker.push(b"session_token=");
    marker.push_token(token);
    marker.push(b"\n");
    marker.finish()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerValidationError {
    Invalid,
}

#[derive(Clone, Copy)]
enum ValidationPolicy {
    Current,
    Previewable,
}

struct ValidatedMarker {
    owner: SessionToken,
}

fn validate_marker(
    bytes: &[u8],
    policy: ValidationPolicy,
) -> Result<ValidatedMarker, MarkerValidationError> {
    if bytes.is_empty() || bytes.len() > MAX_MARKER_BYTES || !bytes.is_ascii() {
        return Err(MarkerValidationError::Invalid);
    }
    let text = str::from_utf8(bytes).map_err(|_error| MarkerValidationError::Invalid)?;
    let body = text.strip_suffix('\n').ok_or(MarkerValidationError::Invalid)?;
    let mut lines = body.split('\n');

    let schema = next_field(&mut lines, "schema=")?;
    let timestamp = next_field(&mut lines, "timestamp_unix_seconds=")?;
    let version = next_field(&mut lines, "app_version=")?;
    let target_os = next_field(&mut lines, "target_os=")?;
    let target_arch = next_field(&mut lines, "target_arch=")?;
    let code = next_field(&mut lines, "code=")?;
    let role = next_field(&mut lines, "thread_role=")?;
    let session_token = next_field(&mut lines, "session_token=")?;

    if lines.next().is_some()
        || !valid_timestamp(timestamp)
        || !identity_allowed(policy, schema, version, target_os, target_arch)
        || code != PANIC_CODE
        || ThreadRole::from_marker(role).is_none()
    {
        return Err(MarkerValidationError::Invalid);
    }

    let owner = SessionToken::parse(session_token).ok_or(MarkerValidationError::Invalid)?;
    Ok(ValidatedMarker { owner })
}

fn identity_allowed(
    policy: ValidationPolicy,
    schema: &str,
    app_version: &str,
    target_os: &str,
    target_arch: &str,
) -> bool {
    let is_current =
        identity_matches(CURRENT_MARKER_IDENTITY, schema, app_version, target_os, target_arch);
    match policy {
        ValidationPolicy::Current => is_current,
        ValidationPolicy::Previewable => {
            is_current
                || PREVIEWABLE_PRIOR_IDENTITIES.iter().copied().any(|identity| {
                    identity_matches(identity, schema, app_version, target_os, target_arch)
                })
        }
    }
}

fn identity_matches(
    identity: MarkerIdentity,
    schema: &str,
    app_version: &str,
    target_os: &str,
    target_arch: &str,
) -> bool {
    schema == identity.schema
        && app_version == identity.app_version
        && target_os == identity.target_os
        && target_arch == identity.target_arch
}

fn next_field<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    prefix: &str,
) -> Result<&'a str, MarkerValidationError> {
    lines.next().and_then(|line| line.strip_prefix(prefix)).ok_or(MarkerValidationError::Invalid)
}

fn valid_timestamp(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok()
}

#[cfg(any(test, not(windows)))]
enum SnapshotReadFailure {
    Io(MarkerStoreFailure),
    TooLarge,
    Invalid,
}

#[cfg(any(test, not(windows)))]
impl SnapshotReadFailure {
    const fn into_store_failure(self) -> MarkerStoreFailure {
        match self {
            Self::Io(failure) => failure,
            Self::TooLarge => MarkerStoreFailure::new(MarkerStoreFailureKind::MarkerTooLarge, None),
            Self::Invalid => MarkerStoreFailure::new(MarkerStoreFailureKind::InvalidMarker, None),
        }
    }
}

#[cfg(any(test, not(windows)))]
fn read_snapshot(
    path: &Path,
    policy: ValidationPolicy,
) -> Result<Option<MarkerSnapshot>, SnapshotReadFailure> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SnapshotReadFailure::Io(MarkerStoreFailure::from_io(&error))),
    };

    let mut bytes = Vec::with_capacity(MAX_MARKER_BYTES + 1);
    file.take((MAX_MARKER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| SnapshotReadFailure::Io(MarkerStoreFailure::from_io(&error)))?;

    if bytes.len() > MAX_MARKER_BYTES {
        return Err(SnapshotReadFailure::TooLarge);
    }
    let validated =
        validate_marker(&bytes, policy).map_err(|_error| SnapshotReadFailure::Invalid)?;
    Ok(Some(MarkerSnapshot { bytes, owner: validated.owner }))
}

#[cfg(any(test, not(windows)))]
fn read_snapshot_slot(path: &Path) -> MarkerSlotSnapshot {
    match read_snapshot(path, ValidationPolicy::Previewable) {
        Ok(Some(snapshot)) => MarkerSlotSnapshot::Present(snapshot),
        Ok(None) => MarkerSlotSnapshot::Missing,
        Err(failure) => MarkerSlotSnapshot::Unavailable(failure.into_store_failure()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        process::{self, Command, ExitStatus, Stdio},
        sync::{
            Mutex,
            atomic::{AtomicU64, AtomicUsize},
        },
        time::{Duration, Instant},
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    static PRIOR_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

    const TEST_TOKEN: SessionToken = SessionToken([0x11; 16]);
    const CHILD_CRASH_DIRECTORY_ENV: &str = "DISKPIE_PANIC_MARKER_TEST_DIRECTORY";
    const CHILD_PRIVATE_ENV: &str = "DISKPIE_PANIC_MARKER_PRIVATE_ENV";
    const PRIVATE_ENV_SENTINEL: &str = "private-env-sentinel-7bd1";
    const PRIVATE_PAYLOAD_SENTINEL: &str = "private-payload-sentinel-8ac2";
    const PRIVATE_THREAD_SENTINEL: &str = "private-thread-sentinel-9fe3";

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diskpie-panic-marker-v2-{label}-{}-{sequence}", process::id()));
            fs::create_dir(&path).expect("create isolated panic marker test directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }

    fn run_subprocess_helper(
        filter: &str,
        crash_directory: &Path,
        current_directory: &Path,
    ) -> ExitStatus {
        let executable = std::env::current_exe().expect("resolve current test executable");
        let mut child = Command::new(executable)
            .arg(filter)
            .arg("--ignored")
            .arg("--test-threads=1")
            .env(CHILD_CRASH_DIRECTORY_ENV, crash_directory)
            .env(CHILD_PRIVATE_ENV, PRIVATE_ENV_SENTINEL)
            .current_dir(current_directory)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn panic marker subprocess helper");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait().expect("poll panic marker subprocess") {
                Some(status) => return status,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                None => {
                    let _ignored = child.kill();
                    let _ignored = child.wait();
                    panic!("panic marker subprocess timed out");
                }
            }
        }
    }

    fn marker_bytes(timestamp: u64, role: ThreadRole) -> Vec<u8> {
        marker_bytes_with_token(timestamp, role, TEST_TOKEN)
    }

    fn marker_bytes_with_token(timestamp: u64, role: ThreadRole, token: SessionToken) -> Vec<u8> {
        build_marker(timestamp, role, token).expect("the fixed marker fits").as_bytes().to_vec()
    }

    fn snapshot(bytes: Vec<u8>) -> MarkerSnapshot {
        MarkerSnapshot::from_bytes(bytes).expect("test snapshot is valid")
    }

    fn empty_view() -> PreparedMarkerView {
        PreparedMarkerView {
            target: MarkerSlotSnapshot::Missing,
            sibling: MarkerSlotSnapshot::Missing,
        }
    }

    struct FakeCapability {
        replacement_semantics: ReplacementSemantics,
        ownership_reconciliation: OwnershipReconciliation,
        commit_result: MarkerCommitResult,
        commit_calls: AtomicUsize,
        reconcile_calls: AtomicUsize,
        delete_calls: AtomicUsize,
        current: Mutex<Option<Vec<u8>>>,
        reconcile_override: Mutex<Option<OwnedReconcileOutcome>>,
    }

    impl FakeCapability {
        fn new(commit_result: MarkerCommitResult) -> Arc<Self> {
            Arc::new(Self {
                replacement_semantics: ReplacementSemantics::AtomicAndDurable,
                ownership_reconciliation: OwnershipReconciliation::AtomicFileIdentity,
                commit_result,
                commit_calls: AtomicUsize::new(0),
                reconcile_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
                current: Mutex::new(None),
                reconcile_override: Mutex::new(None),
            })
        }

        fn set_current(&self, bytes: Option<Vec<u8>>) {
            *self.current.lock().expect("lock fake marker") = bytes;
        }

        fn current(&self) -> Option<Vec<u8>> {
            self.current.lock().expect("lock fake marker").clone()
        }

        fn set_reconcile_override(&self, outcome: OwnedReconcileOutcome) {
            *self.reconcile_override.lock().expect("lock fake reconcile result") = Some(outcome);
        }
    }

    impl sealed::PreparedCapability for FakeCapability {}

    impl PreparedMarkerCapability for FakeCapability {
        fn replacement_semantics(&self) -> ReplacementSemantics {
            self.replacement_semantics
        }

        fn ownership_reconciliation(&self) -> OwnershipReconciliation {
            self.ownership_reconciliation
        }

        fn commit(&self, contents: &[u8]) -> MarkerCommitResult {
            self.commit_calls.fetch_add(1, Ordering::Relaxed);
            if matches!(
                self.commit_result.outcome,
                MarkerCommitOutcome::Committed | MarkerCommitOutcome::CommittedButUnverified
            ) {
                *self.current.lock().expect("lock fake marker") = Some(contents.to_vec());
            }
            self.commit_result
        }

        fn reconcile_if_owned(
            &self,
            expected_owner: SessionToken,
            action: OwnedReconcileAction<'_>,
        ) -> Result<OwnedReconcileOutcome, MarkerStoreFailure> {
            self.reconcile_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(outcome) =
                *self.reconcile_override.lock().expect("lock fake reconcile result")
            {
                return Ok(outcome);
            }

            let mut current = self.current.lock().expect("lock fake marker");
            let Some(bytes) = current.as_ref() else {
                return Ok(OwnedReconcileOutcome::MarkerMissing);
            };
            let validated = match validate_marker(bytes, ValidationPolicy::Previewable) {
                Ok(validated) => validated,
                Err(_error) => return Ok(OwnedReconcileOutcome::PreservedUncertain),
            };
            if validated.owner != expected_owner {
                return Ok(OwnedReconcileOutcome::OwnershipLost);
            }

            match action {
                OwnedReconcileAction::Restore(previous) => {
                    *current = Some(previous.to_vec());
                    Ok(OwnedReconcileOutcome::RestoredPrevious)
                }
                OwnedReconcileAction::Delete => {
                    *current = None;
                    Ok(OwnedReconcileOutcome::RemovedSessionMarker)
                }
            }
        }

        fn delete_explicit(&self) -> Result<bool, MarkerStoreFailure> {
            self.delete_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.current.lock().expect("lock fake marker").take().is_some())
        }
    }

    struct FakeStore {
        prepare_calls: AtomicUsize,
        capability: Arc<FakeCapability>,
        initial: PreparedMarkerView,
        recovery: PreparationRecovery,
        failure: Option<MarkerStoreFailure>,
    }

    impl FakeStore {
        fn ready(capability: Arc<FakeCapability>, initial: PreparedMarkerView) -> Arc<Self> {
            Arc::new(Self {
                prepare_calls: AtomicUsize::new(0),
                capability,
                initial,
                recovery: PreparationRecovery::Ready,
                failure: None,
            })
        }

        fn with_recovery(
            capability: Arc<FakeCapability>,
            initial: PreparedMarkerView,
            recovery: PreparationRecovery,
        ) -> Arc<Self> {
            Arc::new(Self {
                prepare_calls: AtomicUsize::new(0),
                capability,
                initial,
                recovery,
                failure: None,
            })
        }

        fn failing(capability: Arc<FakeCapability>, failure: MarkerStoreFailure) -> Arc<Self> {
            Arc::new(Self {
                prepare_calls: AtomicUsize::new(0),
                capability,
                initial: empty_view(),
                recovery: PreparationRecovery::Ready,
                failure: Some(failure),
            })
        }
    }

    impl sealed::Store for FakeStore {}

    impl MarkerStore for FakeStore {
        fn prepare(
            &self,
            _target: &Path,
            _sibling: &Path,
        ) -> Result<PreparedMarkerSession, MarkerStoreFailure> {
            self.prepare_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(failure) = self.failure {
                return Err(failure);
            }
            Ok(PreparedMarkerSession::new(
                Arc::clone(&self.capability) as Arc<dyn PreparedMarkerCapability>,
                self.initial.clone(),
                self.recovery,
            ))
        }
    }

    fn test_runtime(
        capability: Arc<FakeCapability>,
        previous_marker: PreviousMarkerState,
        recovery_degraded: bool,
        session_panicked: bool,
        commit_observation: u8,
    ) -> CrashRuntime {
        runtime_with_capability(
            capability,
            previous_marker,
            recovery_degraded,
            session_panicked,
            commit_observation,
        )
    }

    /// Builds a runtime around any sealed capability, including the real
    /// platform adapter, without installing the process hook.
    fn runtime_with_capability(
        capability: Arc<dyn PreparedMarkerCapability>,
        previous_marker: PreviousMarkerState,
        recovery_degraded: bool,
        session_panicked: bool,
        commit_observation: u8,
    ) -> CrashRuntime {
        let state = Arc::new(HookState {
            capability,
            markers: HookMarkers::new(42, TEST_TOKEN).expect("prebuild hook markers"),
            replacement_semantics: ReplacementSemantics::AtomicAndDurable,
            ownership_reconciliation: OwnershipReconciliation::AtomicFileIdentity,
            session_token: TEST_TOKEN,
            session_panicked: AtomicBool::new(session_panicked),
            commit_observation: AtomicU8::new(commit_observation),
        });
        CrashRuntime {
            state,
            previous_marker,
            preparation_failures: Vec::new(),
            recovery_degraded,
            _installing_thread_role: enter_thread_role(ThreadRole::Main),
        }
    }

    #[test]
    fn marker_schema_is_fixed_bounded_and_contains_no_sensitive_route() {
        let bytes = marker_bytes(1_754_173_619, ThreadRole::ScanWorker);
        let text = str::from_utf8(&bytes).expect("marker is ASCII");

        assert!(bytes.len() <= MAX_MARKER_BYTES);
        assert_eq!(text.lines().count(), 8);
        assert!(text.contains("thread_role=scan_worker\n"));
        assert!(text.contains("session_token=11111111111111111111111111111111\n"));
        assert!(validate_marker(&bytes, ValidationPolicy::Current).is_ok());
        for forbidden in [
            PRIVATE_PAYLOAD_SENTINEL,
            PRIVATE_ENV_SENTINEL,
            PRIVATE_THREAD_SENTINEL,
            r"C:\Users\private\scan-root",
            "panic_marker.rs",
            "payload",
            "location",
            "thread_name",
        ] {
            assert!(!text.contains(forbidden), "marker exposed {forbidden}");
        }
    }

    #[test]
    fn validator_rejects_unbounded_malformed_or_noncanonical_snapshots() {
        let valid =
            String::from_utf8(marker_bytes(42, ThreadRole::Unknown)).expect("marker is ASCII");
        let extra = format!("{valid}payload=secret\n");
        let raw_role = valid.replace("thread_role=unknown", "thread_role=private-thread");
        let arbitrary_version = valid
            .replace(&format!("app_version={}", env!("CARGO_PKG_VERSION")), "app_version=9.9.9");
        let uppercase_token = valid.replace(
            "session_token=11111111111111111111111111111111",
            "session_token=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        );

        for invalid in [
            extra.as_str(),
            raw_role.as_str(),
            arbitrary_version.as_str(),
            uppercase_token.as_str(),
        ] {
            assert!(MarkerSnapshot::from_bytes(invalid.as_bytes().to_vec()).is_err());
        }
        assert!(MarkerSnapshot::from_bytes(vec![b'x'; MAX_MARKER_BYTES + 1]).is_err());
    }

    #[test]
    fn role_guard_restores_only_the_static_allowlisted_role() {
        assert_eq!(THREAD_ROLE.with(Cell::get), ThreadRole::Unknown);
        {
            let _main = enter_thread_role(ThreadRole::Main);
            assert_eq!(THREAD_ROLE.with(Cell::get), ThreadRole::Main);
            {
                let _worker = enter_thread_role(ThreadRole::LayoutWorker);
                assert_eq!(THREAD_ROLE.with(Cell::get), ThreadRole::LayoutWorker);
            }
            assert_eq!(THREAD_ROLE.with(Cell::get), ThreadRole::Main);
        }
        assert_eq!(THREAD_ROLE.with(Cell::get), ThreadRole::Unknown);
    }

    #[test]
    fn prepared_snapshot_is_used_without_a_second_prepare_or_capability_read() {
        let expected = marker_bytes(73, ThreadRole::Ui);
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let store = FakeStore::ready(
            Arc::clone(&capability),
            PreparedMarkerView {
                target: MarkerSlotSnapshot::Present(snapshot(expected.clone())),
                sibling: MarkerSlotSnapshot::Missing,
            },
        );
        let private_path = Path::new(r"C:\private\sentinel-path");

        let preview = preview_marker_with_store(private_path, store.as_ref())
            .expect("preview prepared snapshot")
            .expect("prepared target exists");

        assert_eq!(preview.as_bytes(), expected);
        assert_eq!(store.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(capability.commit_calls.load(Ordering::Relaxed), 0);
        assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
        assert_eq!(capability.delete_calls.load(Ordering::Relaxed), 0);
        assert!(!preview.as_str().contains("private"));
    }

    #[test]
    fn runtime_previews_its_retained_previous_snapshot_without_repreparing() {
        let expected = marker_bytes(74, ThreadRole::Main);
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Snapshot(snapshot(expected.clone())),
            false,
            false,
            COMMIT_NOT_ATTEMPTED,
        );

        let preview = runtime
            .prepared_previous_marker()
            .expect("retained snapshot is valid")
            .expect("previous marker exists");

        assert_eq!(preview.as_bytes(), expected);
        assert_eq!(capability.commit_calls.load(Ordering::Relaxed), 0);
        assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
        assert_eq!(capability.delete_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn preparation_failure_aborts_before_hook_capability_use() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let failure = MarkerStoreFailure::new(MarkerStoreFailureKind::AccessDenied, Some(5));
        let store = FakeStore::failing(Arc::clone(&capability), failure);
        let error = preview_marker_with_store(Path::new("ignored"), store.as_ref())
            .expect_err("preparation failure is surfaced");

        assert_eq!(error.operation, CrashMarkerOperation::Preview);
        assert_eq!(error.kind, CrashMarkerErrorKind::AccessDenied);
        assert_eq!(error.os_code, Some(5));
        assert_eq!(capability.commit_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn initially_missing_target_that_appears_is_not_overwritten_when_commit_is_rejected() {
        let failure = MarkerStoreFailure::new(MarkerStoreFailureKind::OwnershipConflict, None);
        let capability = FakeCapability::new(MarkerCommitResult::not_committed(failure));
        let foreign =
            marker_bytes_with_token(80, ThreadRole::ShellWorker, SessionToken([0x44; 16]));
        capability.set_current(Some(foreign.clone()));
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Missing,
            false,
            false,
            COMMIT_NOT_ATTEMPTED,
        );

        record_panic_for_role(&runtime.state, ThreadRole::ScanWorker);
        assert_eq!(capability.current(), Some(foreign));
        assert_eq!(runtime.state.commit_observation.load(Ordering::Acquire), COMMIT_NOT_COMMITTED);
        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("no committed session marker is conservative");
        assert_eq!(outcome, FinishCleanOutcome::NoSessionMarkerCommitted);
        assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn target_identity_change_is_preserved_during_conditional_reconciliation() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let foreign =
            marker_bytes_with_token(82, ThreadRole::ShellWorker, SessionToken([0x55; 16]));
        capability.set_current(Some(foreign.clone()));
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Missing,
            false,
            true,
            COMMIT_COMMITTED,
        );

        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("identity mismatch is nonfatal");

        assert_eq!(outcome, FinishCleanOutcome::OwnershipLost);
        assert_eq!(capability.current(), Some(foreign));
    }

    #[test]
    fn foreign_sibling_is_preserved_and_blocks_delete_and_reconciliation() {
        let foreign =
            marker_bytes_with_token(83, ThreadRole::ShellWorker, SessionToken([0x66; 16]));
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        capability.set_current(Some(foreign.clone()));
        let warning = MarkerStoreFailure::new(MarkerStoreFailureKind::OwnershipConflict, None);
        let store = FakeStore::with_recovery(
            Arc::clone(&capability),
            PreparedMarkerView {
                target: MarkerSlotSnapshot::Missing,
                sibling: MarkerSlotSnapshot::Present(snapshot(foreign.clone())),
            },
            PreparationRecovery::KnownCopyPreserved(warning),
        );

        let error = delete_marker_with_store(Path::new("ignored"), store.as_ref())
            .expect_err("foreign sibling blocks deletion");
        assert_eq!(error.kind, CrashMarkerErrorKind::OwnershipConflict);
        assert_eq!(capability.delete_calls.load(Ordering::Relaxed), 0);

        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Missing,
            true,
            true,
            COMMIT_COMMITTED,
        );
        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("degraded preparation preserves known copy");
        assert_eq!(outcome, FinishCleanOutcome::PreservedPreparationDegraded);
        assert_eq!(capability.current(), Some(foreign));
        assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn partial_native_post_commit_outcomes_preserve_the_only_known_copy() {
        for os_code in [1176, 1177] {
            let failure =
                MarkerStoreFailure::new(MarkerStoreFailureKind::PreservedSibling, Some(os_code));
            let capability =
                FakeCapability::new(MarkerCommitResult::committed_but_unverified(failure));
            let runtime = test_runtime(
                Arc::clone(&capability),
                PreviousMarkerState::Missing,
                false,
                false,
                COMMIT_NOT_ATTEMPTED,
            );

            record_panic_for_role(&runtime.state, ThreadRole::LayoutWorker);
            let known_copy = capability.current().expect("post-commit copy is retained");
            assert_eq!(capability.commit_result.failure, Some(failure));
            assert!(validate_marker(&known_copy, ValidationPolicy::Current).is_ok());
            let outcome = runtime
                .finish_clean(ShutdownQuiescenceProof::for_test())
                .expect("uncertain commit is conservatively preserved");

            assert_eq!(outcome, FinishCleanOutcome::PreservedCommitUncertain);
            assert_eq!(capability.current(), Some(known_copy));
            assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn recovery_uncertainty_disables_cleanup_even_after_a_verified_commit() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let known_copy = marker_bytes_with_token(84, ThreadRole::Ui, TEST_TOKEN);
        capability.set_current(Some(known_copy.clone()));
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Missing,
            true,
            true,
            COMMIT_COMMITTED,
        );

        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("uncertain recovery preserves marker");

        assert_eq!(outcome, FinishCleanOutcome::PreservedPreparationDegraded);
        assert_eq!(capability.current(), Some(known_copy));
        assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dropping_without_a_quiescence_proof_cannot_mutate_marker_state() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let owned = marker_bytes_with_token(85, ThreadRole::Ui, TEST_TOKEN);
        capability.set_current(Some(owned.clone()));
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Missing,
            false,
            true,
            COMMIT_COMMITTED,
        );

        drop(runtime);

        assert_eq!(capability.current(), Some(owned));
        assert_eq!(capability.reconcile_calls.load(Ordering::Relaxed), 0);
        assert_eq!(capability.delete_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reconciliation_uncertainty_is_a_non_destructive_outcome() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let owned = marker_bytes_with_token(86, ThreadRole::Ui, TEST_TOKEN);
        capability.set_current(Some(owned.clone()));
        capability.set_reconcile_override(OwnedReconcileOutcome::PreservedUncertain);
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Missing,
            false,
            true,
            COMMIT_COMMITTED,
        );

        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("reconciliation uncertainty is nonfatal");

        assert_eq!(outcome, FinishCleanOutcome::PreservedReconciliationUncertain);
        assert_eq!(capability.current(), Some(owned));
    }

    #[test]
    fn verified_quiescence_restores_only_the_owned_session_marker() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        capability.set_current(Some(marker_bytes_with_token(87, ThreadRole::Ui, TEST_TOKEN)));
        let previous = marker_bytes_with_token(81, ThreadRole::Main, SessionToken([0x22; 16]));
        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Snapshot(snapshot(previous.clone())),
            false,
            true,
            COMMIT_COMMITTED,
        );

        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("restore previous marker");

        assert_eq!(outcome, FinishCleanOutcome::RestoredPrevious);
        assert_eq!(capability.current(), Some(previous));
    }

    #[test]
    fn explicit_delete_uses_only_the_prepared_capability() {
        let existing = marker_bytes(88, ThreadRole::Main);
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        capability.set_current(Some(existing.clone()));
        let store = FakeStore::ready(
            Arc::clone(&capability),
            PreparedMarkerView {
                target: MarkerSlotSnapshot::Present(snapshot(existing)),
                sibling: MarkerSlotSnapshot::Missing,
            },
        );

        assert!(
            delete_marker_with_store(Path::new("ignored"), store.as_ref())
                .expect("delete through capability")
        );
        assert_eq!(store.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(capability.delete_calls.load(Ordering::Relaxed), 1);
        assert_eq!(capability.current(), None);
    }

    #[test]
    fn previous_snapshot_failure_is_sanitized_and_forbids_cleanup() {
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let owned = marker_bytes_with_token(89, ThreadRole::Ui, TEST_TOKEN);
        capability.set_current(Some(owned.clone()));
        let failure = MarkerStoreFailure::new(MarkerStoreFailureKind::InvalidMarker, None);
        let preparation = prepare_session(PreparedMarkerSession::new(
            Arc::clone(&capability) as Arc<dyn PreparedMarkerCapability>,
            PreparedMarkerView {
                target: MarkerSlotSnapshot::Unavailable(failure),
                sibling: MarkerSlotSnapshot::Missing,
            },
            PreparationRecovery::Ready,
        ));
        assert_eq!(preparation.failures.len(), 1);
        assert_eq!(preparation.failures[0].operation, CrashMarkerOperation::ReadPreviousMarker);

        let runtime = test_runtime(
            Arc::clone(&capability),
            PreviousMarkerState::Unavailable,
            false,
            true,
            COMMIT_COMMITTED,
        );
        let outcome = runtime
            .finish_clean(ShutdownQuiescenceProof::for_test())
            .expect("unavailable predecessor is preserved");
        assert_eq!(outcome, FinishCleanOutcome::PreservedPreviousUnavailable);
        assert_eq!(capability.current(), Some(owned));
    }

    #[test]
    fn standard_store_preview_is_bounded_and_uses_the_prepared_view() {
        let directory = TestDirectory::new("std-bounded-preview");
        let paths = MarkerPaths::new(directory.path());
        fs::write(&paths.target, vec![b'x'; MAX_MARKER_BYTES + 1]).expect("write oversized marker");
        let error = preview_marker_with_store(directory.path(), &StdMarkerStore)
            .expect_err("oversized marker is rejected");
        assert_eq!(error.kind, CrashMarkerErrorKind::MarkerTooLarge);

        fs::write(&paths.target, b"payload=C:\\private\\secret\n").expect("write malformed marker");
        let error = preview_marker_with_store(directory.path(), &StdMarkerStore)
            .expect_err("malformed marker is rejected");
        assert_eq!(error.kind, CrashMarkerErrorKind::InvalidMarker);
        assert!(!error.to_string().contains("private"));
    }

    #[test]
    fn standard_store_reports_semantic_uncertainty_without_deleting_complete_sibling() {
        let directory = TestDirectory::new("std-preserved-sibling");
        let paths = MarkerPaths::new(directory.path());
        let sibling = marker_bytes_with_token(90, ThreadRole::Main, SessionToken([0x77; 16]));
        fs::write(&paths.sibling, &sibling).expect("write preserved sibling");

        let prepared = StdMarkerStore
            .prepare(&paths.target, &paths.sibling)
            .expect("prepare portable capability");
        assert!(matches!(
            prepared.recovery,
            PreparationRecovery::KnownCopyPreserved(MarkerStoreFailure {
                kind: MarkerStoreFailureKind::PreservedSibling,
                ..
            })
        ));
        assert_eq!(fs::read(&paths.sibling).expect("sibling remains"), sibling);
        let preview = preview_marker_with_store(directory.path(), &StdMarkerStore)
            .expect("preview preserved sibling")
            .expect("sibling exists");
        assert_eq!(preview.as_bytes(), sibling);
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn subprocess_child_records_redacted_marker() {
        let crashes = PathBuf::from(
            std::env::var_os(CHILD_CRASH_DIRECTORY_ENV)
                .expect("subprocess crash directory is provided"),
        );
        let private_environment = std::env::var(CHILD_PRIVATE_ENV)
            .expect("subprocess private environment sentinel is provided");
        let runtime = CrashRuntime::install_with_store(&crashes, Arc::new(StdMarkerStore))
            .expect("install child panic hook");

        let worker = std::thread::Builder::new()
            .name(PRIVATE_THREAD_SENTINEL.to_owned())
            .spawn(move || {
                with_thread_role(ThreadRole::ScanWorker, || {
                    panic!("{PRIVATE_PAYLOAD_SENTINEL}:{private_environment}");
                });
            })
            .expect("spawn named panic test thread");
        assert!(worker.join().is_err());
        assert!(runtime.session_panicked());
    }

    #[test]
    fn subprocess_hook_excludes_path_payload_thread_and_process_sentinels() {
        let directory = TestDirectory::new("subprocess-redaction");
        let status = run_subprocess_helper(
            "subprocess_child_records_redacted_marker",
            directory.path(),
            directory.path(),
        );
        assert!(status.success());

        let preview = preview_marker_with_store(directory.path(), &StdMarkerStore)
            .expect("subprocess marker is readable")
            .expect("subprocess marker exists");
        let text = preview.as_str();
        for forbidden in [
            PRIVATE_PAYLOAD_SENTINEL,
            PRIVATE_ENV_SENTINEL,
            PRIVATE_THREAD_SENTINEL,
            "panic_marker.rs",
            "--ignored",
            directory.path().to_string_lossy().as_ref(),
        ] {
            assert!(!text.contains(forbidden), "subprocess marker exposed {forbidden}");
        }
        assert!(text.contains("thread_role=scan_worker\n"));
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn subprocess_child_does_not_call_prior_hook() {
        let crashes = PathBuf::from(
            std::env::var_os(CHILD_CRASH_DIRECTORY_ENV)
                .expect("subprocess crash directory is provided"),
        );
        PRIOR_HOOK_CALLS.store(0, Ordering::Relaxed);
        std::panic::set_hook(Box::new(|_panic_info| {
            PRIOR_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        }));
        let runtime = CrashRuntime::install_with_store(&crashes, Arc::new(StdMarkerStore))
            .expect("replace prior hook");
        let worker = std::thread::spawn(|| panic!("{PRIVATE_PAYLOAD_SENTINEL}"));
        assert!(worker.join().is_err());
        assert!(runtime.session_panicked());
        assert_eq!(PRIOR_HOOK_CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn installed_hook_never_calls_the_prior_hook_in_this_build() {
        let directory = TestDirectory::new("subprocess-prior-hook");
        let status = run_subprocess_helper(
            "subprocess_child_does_not_call_prior_hook",
            directory.path(),
            directory.path(),
        );
        assert!(status.success());
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn subprocess_child_writer_failure_returns_without_deadlock() {
        let missing_crashes = PathBuf::from(
            std::env::var_os(CHILD_CRASH_DIRECTORY_ENV)
                .expect("subprocess missing crash directory is provided"),
        );
        let runtime = CrashRuntime::install_with_store(&missing_crashes, Arc::new(StdMarkerStore))
            .expect("install failure-path hook");
        let worker = std::thread::spawn(|| panic!("{PRIVATE_PAYLOAD_SENTINEL}"));
        assert!(worker.join().is_err());
        assert!(runtime.session_panicked());
    }

    #[test]
    fn subprocess_writer_failure_is_bounded_and_has_no_fallback() {
        let directory = TestDirectory::new("subprocess-failure");
        let missing_crashes = directory.path().join("missing-crashes");
        let status = run_subprocess_helper(
            "subprocess_child_writer_failure_returns_without_deadlock",
            &missing_crashes,
            directory.path(),
        );
        assert!(status.success());
        assert!(!missing_crashes.exists());
        assert!(!directory.path().join(MARKER_FILE_NAME).exists());
        assert!(!directory.path().join(TEMPORARY_FILE_NAMES[0]).exists());
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn subprocess_child_install_cas_precedes_store_prepare() {
        let crashes = PathBuf::from(
            std::env::var_os(CHILD_CRASH_DIRECTORY_ENV)
                .expect("subprocess crash directory is provided"),
        );
        let _runtime = CrashRuntime::install_with_store(&crashes, Arc::new(StdMarkerStore))
            .expect("install first hook");
        let capability = FakeCapability::new(MarkerCommitResult::committed());
        let probe = FakeStore::ready(capability, empty_view());
        let error =
            CrashRuntime::install_with_store(&crashes, Arc::clone(&probe) as Arc<dyn MarkerStore>)
                .expect_err("second installation is rejected");

        assert_eq!(error.kind, CrashMarkerErrorKind::HookAlreadyInstalled);
        assert_eq!(probe.prepare_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn installation_cas_precedes_every_store_operation() {
        let directory = TestDirectory::new("subprocess-cas-order");
        let status = run_subprocess_helper(
            "subprocess_child_install_cas_precedes_store_prepare",
            directory.path(),
            directory.path(),
        );
        assert!(status.success());
    }

    #[test]
    fn session_tokens_are_fixed_opaque_and_distinct() {
        let first = SessionToken::generate();
        let second = SessionToken::generate();
        assert_ne!(first, second);
        let bytes = marker_bytes_with_token(91, ThreadRole::Unknown, first);
        assert_eq!(
            validate_marker(&bytes, ValidationPolicy::Current)
                .expect("generated token is canonical")
                .owner,
            first
        );
    }

    #[cfg(windows)]
    mod native_bridge {
        use super::*;
        use crate::panic_marker::native::{WindowsMarkerStore, failure_from_native};
        use diskpie_platform::{
            DiagnosticFileError, DiagnosticFileErrorKind, DiagnosticFileOperation,
        };

        fn prepare_native(directory: &Path) -> PreparedMarkerSession {
            let paths = MarkerPaths::new(directory);
            WindowsMarkerStore
                .prepare(&paths.target, &paths.sibling)
                .expect("prepare the native marker session")
        }

        fn native_runtime(
            prepared: PreparedMarkerSession,
            previous_marker: PreviousMarkerState,
        ) -> CrashRuntime {
            runtime_with_capability(
                prepared.capability,
                previous_marker,
                false,
                false,
                COMMIT_NOT_ATTEMPTED,
            )
        }

        #[test]
        fn native_capability_reports_truthful_semantics() {
            let directory = TestDirectory::new("native-semantics");
            let prepared = prepare_native(directory.path());
            assert_eq!(prepared.recovery, PreparationRecovery::Ready);
            assert_eq!(prepared.initial, empty_view());
            assert_eq!(
                prepared.capability.replacement_semantics(),
                ReplacementSemantics::AtomicVisibility
            );
            assert_eq!(
                prepared.capability.ownership_reconciliation(),
                OwnershipReconciliation::AtomicFileIdentity
            );
        }

        #[test]
        fn native_commit_through_the_hook_path_restores_the_previous_marker() {
            let directory = TestDirectory::new("native-restore");
            let paths = MarkerPaths::new(directory.path());
            let previous = marker_bytes_with_token(81, ThreadRole::Main, SessionToken([0x22; 16]));
            fs::write(&paths.target, &previous).expect("write previous marker");

            let prepared = prepare_native(directory.path());
            assert_eq!(
                prepared.initial.target,
                MarkerSlotSnapshot::Present(snapshot(previous.clone()))
            );
            let runtime =
                native_runtime(prepared, PreviousMarkerState::Snapshot(snapshot(previous.clone())));

            record_panic_for_role(&runtime.state, ThreadRole::Ui);
            assert_eq!(runtime.state.commit_observation.load(Ordering::Acquire), COMMIT_COMMITTED);
            let committed = fs::read(&paths.target).expect("read committed marker");
            assert_eq!(committed, runtime.state.markers.for_role(ThreadRole::Ui));
            assert!(!paths.sibling.exists(), "no sibling survives a proven commit");

            let outcome = runtime
                .finish_clean(ShutdownQuiescenceProof::for_test())
                .expect("restore through the native session");
            assert_eq!(outcome, FinishCleanOutcome::RestoredPrevious);
            assert_eq!(fs::read(&paths.target).expect("read restored marker"), previous);
            assert!(!paths.sibling.exists());
        }

        #[test]
        fn native_commit_without_a_previous_marker_is_removed_after_quiescence() {
            let directory = TestDirectory::new("native-remove");
            let paths = MarkerPaths::new(directory.path());
            let prepared = prepare_native(directory.path());
            let runtime = native_runtime(prepared, PreviousMarkerState::Missing);

            record_panic_for_role(&runtime.state, ThreadRole::ScanWorker);
            record_panic_for_role(&runtime.state, ThreadRole::LayoutWorker);
            assert_eq!(runtime.state.commit_observation.load(Ordering::Acquire), COMMIT_COMMITTED);
            let latest = fs::read(&paths.target).expect("read latest marker");
            assert_eq!(latest, runtime.state.markers.for_role(ThreadRole::LayoutWorker));

            let outcome = runtime
                .finish_clean(ShutdownQuiescenceProof::for_test())
                .expect("remove through the native session");
            assert_eq!(outcome, FinishCleanOutcome::RemovedSessionMarker);
            assert!(!paths.target.exists());
            assert!(!paths.sibling.exists());
        }

        #[test]
        fn native_reconciliation_is_identity_conditional_not_token_conditional() {
            let directory = TestDirectory::new("native-identity");
            let paths = MarkerPaths::new(directory.path());
            let prepared = prepare_native(directory.path());
            let runtime = native_runtime(prepared, PreviousMarkerState::Missing);
            record_panic_for_role(&runtime.state, ThreadRole::Ui);
            let session_bytes = fs::read(&paths.target).expect("read session marker");

            // Same token, different object: the identity check must win.
            fs::remove_file(&paths.target).expect("remove session marker");
            fs::write(&paths.target, &session_bytes).expect("recreate with a new identity");
            let outcome = runtime
                .finish_clean(ShutdownQuiescenceProof::for_test())
                .expect("identity mismatch is nonfatal");
            assert_eq!(outcome, FinishCleanOutcome::OwnershipLost);
            assert_eq!(fs::read(&paths.target).expect("foreign object intact"), session_bytes);
            assert!(!paths.sibling.exists());
        }

        #[test]
        fn native_reconciliation_preserves_a_foreign_token() {
            let directory = TestDirectory::new("native-foreign-token");
            let paths = MarkerPaths::new(directory.path());
            let prepared = prepare_native(directory.path());
            let runtime = native_runtime(prepared, PreviousMarkerState::Missing);
            record_panic_for_role(&runtime.state, ThreadRole::Ui);

            let foreign =
                marker_bytes_with_token(82, ThreadRole::ShellWorker, SessionToken([0x55; 16]));
            fs::remove_file(&paths.target).expect("remove session marker");
            fs::write(&paths.target, &foreign).expect("foreign marker appears");
            let outcome = runtime
                .finish_clean(ShutdownQuiescenceProof::for_test())
                .expect("foreign marker is nonfatal");
            assert_eq!(outcome, FinishCleanOutcome::OwnershipLost);
            assert_eq!(fs::read(&paths.target).expect("foreign marker intact"), foreign);
        }

        #[test]
        fn native_store_reports_unavailable_slots_and_preserved_siblings() {
            let directory = TestDirectory::new("native-slots");
            let paths = MarkerPaths::new(directory.path());
            fs::write(&paths.target, b"payload=C:\\private\\secret\n").expect("write malformed");
            let foreign =
                marker_bytes_with_token(83, ThreadRole::ShellWorker, SessionToken([0x66; 16]));
            fs::write(&paths.sibling, &foreign).expect("write preserved sibling");

            let prepared = prepare_native(directory.path());
            assert!(matches!(
                prepared.initial.target,
                MarkerSlotSnapshot::Unavailable(MarkerStoreFailure {
                    kind: MarkerStoreFailureKind::InvalidMarker,
                    ..
                })
            ));
            assert_eq!(
                prepared.initial.sibling,
                MarkerSlotSnapshot::Present(snapshot(foreign.clone()))
            );
            assert!(matches!(prepared.recovery, PreparationRecovery::Uncertain(_)));

            let error =
                delete_marker(directory.path()).expect_err("uncertain preparation blocks delete");
            assert_eq!(error.operation, CrashMarkerOperation::Delete);
            assert!(!error.to_string().contains("private"));
            assert!(paths.target.exists());
            assert_eq!(fs::read(&paths.sibling).expect("sibling preserved"), foreign);

            fs::remove_file(&paths.target).expect("clear malformed target");
            let preview = preview_marker(directory.path())
                .expect("preview the preserved sibling")
                .expect("sibling is previewable");
            assert_eq!(preview.as_bytes(), foreign);
        }

        #[test]
        fn native_explicit_delete_uses_identity_matched_handles_only() {
            let directory = TestDirectory::new("native-delete");
            let paths = MarkerPaths::new(directory.path());
            let existing = marker_bytes(88, ThreadRole::Main);
            fs::write(&paths.target, &existing).expect("write marker");
            assert!(delete_marker(directory.path()).expect("delete through the native session"));
            assert!(!paths.target.exists());
            assert!(!delete_marker(directory.path()).expect("nothing left to delete"));

            fs::write(&paths.target, &existing).expect("write marker again");
            let prepared = prepare_native(directory.path());
            fs::remove_file(&paths.target).expect("remove inspected marker");
            fs::write(&paths.target, &existing).expect("swap the object under the name");
            let error = delete_prepared(prepared).expect_err("swapped object is preserved");
            assert_eq!(error.kind, CrashMarkerErrorKind::OwnershipConflict);
            assert_eq!(fs::read(&paths.target).expect("swapped marker intact"), existing);
        }

        #[test]
        fn native_failures_map_onto_the_store_vocabulary_with_codes() {
            let operation = DiagnosticFileOperation::ReplaceDestination;
            for (kind, expected) in [
                (DiagnosticFileErrorKind::AccessDenied, MarkerStoreFailureKind::AccessDenied),
                (DiagnosticFileErrorKind::NotFound, MarkerStoreFailureKind::NotFound),
                (DiagnosticFileErrorKind::InvalidDestination, MarkerStoreFailureKind::InvalidData),
                (DiagnosticFileErrorKind::ReparsePoint, MarkerStoreFailureKind::InvalidData),
                (
                    DiagnosticFileErrorKind::OwnedDestination,
                    MarkerStoreFailureKind::OwnershipConflict,
                ),
                (DiagnosticFileErrorKind::InvalidContent, MarkerStoreFailureKind::InvalidMarker),
                (DiagnosticFileErrorKind::TooLarge, MarkerStoreFailureKind::MarkerTooLarge),
                (DiagnosticFileErrorKind::AlreadyExists, MarkerStoreFailureKind::Busy),
                (
                    DiagnosticFileErrorKind::PartialReplacement,
                    MarkerStoreFailureKind::PreservedSibling,
                ),
                (DiagnosticFileErrorKind::Unsupported, MarkerStoreFailureKind::Unsupported),
                (DiagnosticFileErrorKind::Io, MarkerStoreFailureKind::Other),
            ] {
                let failure = failure_from_native(DiagnosticFileError {
                    operation,
                    kind,
                    os_code: Some(1176),
                });
                assert_eq!(failure, MarkerStoreFailure::new(expected, Some(1176)));
            }
        }

        #[test]
        #[ignore = "subprocess helper"]
        fn subprocess_child_records_native_redacted_marker() {
            let crashes = PathBuf::from(
                std::env::var_os(CHILD_CRASH_DIRECTORY_ENV)
                    .expect("subprocess crash directory is provided"),
            );
            let private_environment = std::env::var(CHILD_PRIVATE_ENV)
                .expect("subprocess private environment sentinel is provided");
            let username = std::env::var("USERNAME").unwrap_or_default();
            let runtime = CrashRuntime::install(&crashes).expect("install the native hook");
            assert_eq!(
                runtime.ownership_reconciliation(),
                OwnershipReconciliation::AtomicFileIdentity
            );

            let worker = std::thread::Builder::new()
                .name(PRIVATE_THREAD_SENTINEL.to_owned())
                .spawn(move || {
                    with_thread_role(ThreadRole::ScanWorker, || {
                        panic!("{PRIVATE_PAYLOAD_SENTINEL}:{private_environment}:{username}");
                    });
                })
                .expect("spawn named panic test thread");
            assert!(worker.join().is_err());
            assert!(runtime.session_panicked());
            // The runtime is dropped without a proof: the marker must survive.
        }

        #[test]
        fn subprocess_native_hook_leaves_a_redacted_marker_on_a_worker_panic() {
            let directory = TestDirectory::new("native-subprocess");
            let status = run_subprocess_helper(
                "subprocess_child_records_native_redacted_marker",
                directory.path(),
                directory.path(),
            );
            assert!(status.success());

            let preview = preview_marker(directory.path())
                .expect("subprocess marker is readable through the native store")
                .expect("subprocess marker exists");
            let text = preview.as_str();
            let username = std::env::var("USERNAME").unwrap_or_default();
            let mut forbidden = vec![
                PRIVATE_PAYLOAD_SENTINEL.to_owned(),
                PRIVATE_ENV_SENTINEL.to_owned(),
                PRIVATE_THREAD_SENTINEL.to_owned(),
                "panic_marker.rs".to_owned(),
                "--ignored".to_owned(),
                directory.path().to_string_lossy().into_owned(),
                std::env::temp_dir().to_string_lossy().into_owned(),
            ];
            if !username.is_empty() {
                forbidden.push(username);
            }
            for forbidden in forbidden {
                assert!(!text.contains(&forbidden), "subprocess marker exposed {forbidden}");
            }
            assert!(text.contains("thread_role=scan_worker\n"));
            assert!(validate_marker(preview.as_bytes(), ValidationPolicy::Current).is_ok());
            assert!(!directory.path().join(TEMPORARY_FILE_NAMES[0]).exists());
        }

        #[test]
        #[ignore = "subprocess helper"]
        fn subprocess_child_panics_on_the_installing_thread() {
            let crashes = PathBuf::from(
                std::env::var_os(CHILD_CRASH_DIRECTORY_ENV)
                    .expect("subprocess crash directory is provided"),
            );
            let _runtime = CrashRuntime::install(&crashes).expect("install the native hook");
            panic!("{PRIVATE_PAYLOAD_SENTINEL}");
        }

        /// The child panics on the libtest worker thread that installed the
        /// hook (its `thread_role=main`), not on the process main thread; the
        /// non-success exit comes from libtest's failure accounting, which
        /// only happens because the hook returned instead of swallowing the
        /// panic.
        #[test]
        fn subprocess_native_hook_does_not_swallow_the_panic() {
            let directory = TestDirectory::new("native-subprocess-main");
            let status = run_subprocess_helper(
                "subprocess_child_panics_on_the_installing_thread",
                directory.path(),
                directory.path(),
            );
            assert!(
                !status.success(),
                "the hook returns and libtest still reports the installing thread's panic"
            );

            let preview = preview_marker(directory.path())
                .expect("marker is readable")
                .expect("marker exists");
            assert!(preview.as_str().contains("thread_role=main\n"));
            assert!(!preview.as_str().contains(PRIVATE_PAYLOAD_SENTINEL));
        }
    }
}
