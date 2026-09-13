//! Dedicated Windows Shell single-threaded-apartment service.
//!
//! The service owns one message-pumping STA and accepts bounded, nonblocking
//! requests from the UI. COM interfaces, Shell items, native allocations, and
//! HWND wrappers are constructed and destroyed on that thread. Only owned,
//! plain-data requests and events cross the channel boundary.
//!
//! [`ShellRequest`] covers the folder picker, open/reveal/Installed Apps
//! activation, the advisory Recycle Bin query, and the destructive recycle,
//! permanent-delete, and empty-bin actions. Destructive variants can only be
//! built from a consumed ADR 0008 capability. Every request has one terminal
//! [`ShellEvent`]; the native work itself lives in [`super::shell_actions`].
//! A queued request can be cancelled with [`ShellService::cancel`]; the same
//! flag is read cooperatively by the file-operation progress sink.
//!
//! Shutdown is bounded. [`ShellService::request_shutdown`] never waits,
//! [`ShellService::finish`] waits at most a caller-supplied deadline, and
//! `Drop` performs atomic/`Arc` work only. A worker that is still inside a
//! modal dialog after the deadline is transferred, together with the
//! process-wide singleton claim, to one precreated bounded reaper thread that
//! joins it before the claim is released. If that transfer is impossible the
//! service fails closed: the claim is never released and no further Shell STA
//! can start in this process.

use super::shell_actions::{
    DeleteJob, DispatchOutcome, DispatchStage, NativeShell, RecycleBinQueryOutcome,
    empty_recycle_bin, open_installed_apps, open_item, query_recycle_bin, reveal_item, run_delete,
};
use diskpie_app::actions::{
    Confirmed, DeleteMode, DeleteRequest, DestructiveOutcome, EmptyBinOutcome,
    EmptyRecycleBinRequest, FailureStage, OpenItem, PendingObligation, RecycleBinScope,
    RecycleRequest, RevealItem, StronglyConfirmed, TargetKind,
};
use diskpie_core::FileIdentity;
use std::{
    error::Error,
    ffi::{OsString, c_void},
    fmt,
    num::NonZeroIsize,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{
            Receiver, SyncSender, TryRecvError as MpscTryRecvError, TrySendError, channel,
            sync_channel,
        },
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            E_UNEXPECTED, ERROR_CANCELLED, HANDLE, HWND, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            },
            SystemInformation::GetSystemTime,
            Threading::{CreateEventW, SetEvent},
        },
        UI::{
            Shell::{
                FILEOPENDIALOGOPTIONS, FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM,
                FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
                FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{
                DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
                PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
            },
        },
    },
    core::{GUID, HRESULT, PCWSTR, PWSTR},
};

/// Default number of requests that may wait for the Shell STA.
pub const DEFAULT_REQUEST_CAPACITY: usize = 8;

/// Hard limit that prevents accidental construction of an excessively large queue.
pub const MAX_REQUEST_CAPACITY: usize = 256;

/// Stable Common Item Dialog client identity for DiskPie's folder picker.
///
/// Changing this value changes the client-state namespace understood by the
/// Windows dialog implementation. It is therefore persistent application data
/// even though DiskPie clears that data after every invocation.
const FOLDER_DIALOG_CLIENT_GUID: GUID = GUID::from_u128(0x4b48f36d_68bf_49be_9e08_c08ca5afbd60);
const EXPORT_DIALOG_CLIENT_GUID: GUID = GUID::from_u128(0x24f6a984_475e_4873_b6d7_1aaae53e6bb5);

const WORKER_THREAD_NAME: &str = "diskpie-shell-sta";
const REAPER_THREAD_NAME: &str = "diskpie-shell-reaper";
const WAIT_HEALTH_INTERVAL_MS: u32 = 250;

/// Maximum number of timed-out workers the process-wide reaper may hold.
///
/// Because the singleton claim travels with every queued worker and is only
/// released after that worker joined, at most one live Shell STA can be queued
/// at a time in practice. The small fixed capacity is a hard ceiling, not a
/// throughput target.
pub const SHELL_REAPER_CAPACITY: usize = 4;

/// Interval between join-readiness checks inside [`ShellService::finish`].
const FINISH_POLL_INTERVAL: Duration = Duration::from_millis(5);

const STATE_STARTING: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_STOPPING: u8 = 2;
const STATE_STOPPED: u8 = 3;

static PROCESS_REGISTRY: OnceLock<Arc<ShellInstanceRegistry>> = OnceLock::new();

fn process_registry() -> Arc<ShellInstanceRegistry> {
    Arc::clone(PROCESS_REGISTRY.get_or_init(|| Arc::new(ShellInstanceRegistry::new())))
}

/// Correlates a request with exactly one terminal [`ShellEvent`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DialogRequestId(u64);

/// Alias reflecting that every Shell request kind shares the identifier space.
pub type ShellRequestId = DialogRequestId;

impl DialogRequestId {
    /// Returns the process-local numeric request identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A borrowed owner-window value represented without a Windows pointer type.
///
/// The caller must keep the corresponding native window alive until the
/// terminal event for the request is received. The value is never dereferenced
/// by Rust; it is reconstructed as an `HWND` only on the Shell STA for `Show`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct OwnerWindow(NonZeroIsize);

impl OwnerWindow {
    /// Creates an owner from the integer value of a valid, non-null HWND.
    pub const fn from_raw(raw: isize) -> Option<Self> {
        match NonZeroIsize::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the integer HWND value without constructing a pointer wrapper.
    pub const fn as_raw(self) -> isize {
        self.0.get()
    }

    fn as_hwnd(self) -> HWND {
        HWND(self.as_raw() as *mut c_void)
    }
}

/// Parameters for a filesystem-only native folder picker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FolderDialogRequest {
    /// Window that owns the modal Common Item Dialog.
    pub owner: OwnerWindow,
}

/// Activate one real item with the fixed `open` verb.
#[derive(Eq, PartialEq)]
pub struct OpenRequest {
    owner: OwnerWindow,
    path: PathBuf,
}

impl fmt::Debug for OpenRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRequest")
            .field("owner", &self.owner)
            .field("path_present", &true)
            .finish()
    }
}

/// Reveal one real item in its parent Explorer window.
#[derive(Eq, PartialEq)]
pub struct RevealRequest {
    path: PathBuf,
}

impl fmt::Debug for RevealRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RevealRequest").field("path_present", &true).finish()
    }
}

/// Open the modern Installed Apps settings page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InstalledAppsRequest {
    pub owner: OwnerWindow,
}

/// Non-destructive advisory Recycle Bin estimate for one scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecycleBinQueryRequest {
    pub scope: RecycleBinScope,
}

/// Recycle or permanently delete one confirmed target.
///
/// Only [`ShellRequest::recycle`] and [`ShellRequest::delete_permanently`]
/// build this from a consumed capability; it carries exactly what the STA
/// needs and nothing the UI could forge.
#[derive(Eq, PartialEq)]
pub struct DeleteShellRequest {
    owner: OwnerWindow,
    path: PathBuf,
    kind: TargetKind,
    identity: Option<FileIdentity>,
    mode: DeleteMode,
}

impl DeleteShellRequest {
    #[must_use]
    pub const fn kind(&self) -> TargetKind {
        self.kind
    }

    #[must_use]
    pub const fn identity(&self) -> Option<FileIdentity> {
        self.identity
    }

    #[must_use]
    pub const fn mode(&self) -> DeleteMode {
        self.mode
    }
}

impl fmt::Debug for DeleteShellRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeleteShellRequest")
            .field("owner", &self.owner)
            .field("kind", &self.kind)
            .field("identity", &self.identity)
            .field("mode", &self.mode)
            .field("path_present", &true)
            .finish()
    }
}

/// Empty exactly the strongly confirmed Recycle Bin scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmptyBinShellRequest {
    owner: OwnerWindow,
    scope: RecycleBinScope,
}

impl EmptyBinShellRequest {
    #[must_use]
    pub const fn scope(&self) -> &RecycleBinScope {
        &self.scope
    }
}

/// Payload-free request identity used for terminal-event mapping.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShellRequestKind {
    PickFolder,
    SaveExport,
    Open,
    Reveal,
    InstalledApps,
    QueryRecycleBin,
    Recycle,
    DeletePermanently,
    EmptyRecycleBin,
}

/// Work accepted by the dedicated Shell STA.
///
/// The type is deliberately neither `Clone` nor `Copy`: a destructive request
/// exists exactly once, like the capability it was built from.
#[derive(Debug, Eq, PartialEq)]
pub enum ShellRequest {
    /// Select one existing filesystem folder or drive root.
    PickFolder(FolderDialogRequest),
    /// Choose a native .txt destination; this does not create or replace a file.
    SaveExport(FolderDialogRequest),
    Open(OpenRequest),
    Reveal(RevealRequest),
    InstalledApps(InstalledAppsRequest),
    QueryRecycleBin(RecycleBinQueryRequest),
    Recycle(DeleteShellRequest),
    DeletePermanently(DeleteShellRequest),
    EmptyRecycleBin(EmptyBinShellRequest),
}

impl ShellRequest {
    /// The caller must preflight the returned path and confirm its real
    /// transport and existing identity before committing an export.
    #[must_use]
    pub const fn save_export(owner: OwnerWindow) -> Self {
        Self::SaveExport(FolderDialogRequest { owner })
    }

    /// Builds an open request from a target validated for opening.
    #[must_use]
    pub fn open(owner: OwnerWindow, item: &OpenItem) -> Self {
        Self::Open(OpenRequest { owner, path: item.target().path().to_path_buf() })
    }

    /// Builds a reveal request from a target validated for revealing.
    #[must_use]
    pub fn reveal(item: &RevealItem) -> Self {
        Self::Reveal(RevealRequest { path: item.target().path().to_path_buf() })
    }

    /// Builds the fixed Installed Apps activation.
    #[must_use]
    pub const fn installed_apps(owner: OwnerWindow) -> Self {
        Self::InstalledApps(InstalledAppsRequest { owner })
    }

    /// Builds the non-destructive advisory Recycle Bin query.
    #[must_use]
    pub const fn query_recycle_bin(scope: RecycleBinScope) -> Self {
        Self::QueryRecycleBin(RecycleBinQueryRequest { scope })
    }

    /// Consumes a recycle capability; the obligation must be settled afterwards.
    #[must_use]
    pub fn recycle(
        owner: OwnerWindow,
        capability: Confirmed<RecycleRequest>,
    ) -> (Self, PendingObligation) {
        let (request, obligation) = capability.consume();
        let target = request.target();
        let request = DeleteShellRequest {
            owner,
            path: target.path().to_path_buf(),
            kind: target.kind(),
            identity: target.identity(),
            mode: DeleteMode::Recycle,
        };
        (Self::Recycle(request), obligation)
    }

    /// Consumes a strong deletion capability; the obligation must be settled afterwards.
    #[must_use]
    pub fn delete_permanently(
        owner: OwnerWindow,
        capability: StronglyConfirmed<DeleteRequest>,
    ) -> (Self, PendingObligation) {
        let (request, obligation) = capability.consume();
        let target = request.target();
        let request = DeleteShellRequest {
            owner,
            path: target.path().to_path_buf(),
            kind: target.kind(),
            identity: target.identity(),
            mode: DeleteMode::Permanent,
        };
        (Self::DeletePermanently(request), obligation)
    }

    /// Consumes a strong empty-bin capability; the obligation must be settled afterwards.
    #[must_use]
    pub fn empty_recycle_bin(
        owner: OwnerWindow,
        capability: StronglyConfirmed<EmptyRecycleBinRequest>,
    ) -> (Self, PendingObligation) {
        let (request, obligation) = capability.consume();
        let request = EmptyBinShellRequest { owner, scope: request.scope().clone() };
        (Self::EmptyRecycleBin(request), obligation)
    }

    /// Returns the payload-free request kind.
    #[must_use]
    pub const fn kind(&self) -> ShellRequestKind {
        match self {
            Self::PickFolder(_) => ShellRequestKind::PickFolder,
            Self::SaveExport(_) => ShellRequestKind::SaveExport,
            Self::Open(_) => ShellRequestKind::Open,
            Self::Reveal(_) => ShellRequestKind::Reveal,
            Self::InstalledApps(_) => ShellRequestKind::InstalledApps,
            Self::QueryRecycleBin(_) => ShellRequestKind::QueryRecycleBin,
            Self::Recycle(_) => ShellRequestKind::Recycle,
            Self::DeletePermanently(_) => ShellRequestKind::DeletePermanently,
            Self::EmptyRecycleBin(_) => ShellRequestKind::EmptyRecycleBin,
        }
    }

    /// Whether at most one such request may be outstanding at a time.
    ///
    /// The folder picker is a visible modal. Destructive operations are
    /// serialized the same way: one outstanding mutation, and its result must
    /// be delivered before the next one can be accepted.
    const fn is_modal(&self) -> bool {
        matches!(
            self.kind(),
            ShellRequestKind::PickFolder
                | ShellRequestKind::SaveExport
                | ShellRequestKind::Recycle
                | ShellRequestKind::DeletePermanently
                | ShellRequestKind::EmptyRecycleBin
        )
    }
}

/// Native operation at which a folder-dialog request failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogErrorStage {
    CreateDialog,
    GetOptions,
    SetOptions,
    SetClientGuid,
    Show,
    GetResult,
    GetDisplayName,
    ClearClientData,
    MessagePump,
    WorkerPanicked,
    ServiceShutdown,
}

/// Sanitized failure data returned to non-COM consumers.
///
/// No localized native message or selected path is retained. `hresult` is
/// absent only for synthetic worker-panic or service-shutdown outcomes. If the primary
/// operation and privacy cleanup both fail, `cleanup_hresult` preserves the
/// latter without hiding the former.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialogError {
    pub stage: DialogErrorStage,
    pub hresult: Option<i32>,
    pub cleanup_hresult: Option<i32>,
}

impl DialogError {
    const fn from_hresult(stage: DialogErrorStage, hresult: i32) -> Self {
        Self { stage, hresult: Some(hresult), cleanup_hresult: None }
    }

    const fn service_shutdown() -> Self {
        Self { stage: DialogErrorStage::ServiceShutdown, hresult: None, cleanup_hresult: None }
    }

    const fn worker_panicked() -> Self {
        Self { stage: DialogErrorStage::WorkerPanicked, hresult: None, cleanup_hresult: None }
    }

    fn with_cleanup_hresult(mut self, hresult: i32) -> Self {
        self.cleanup_hresult = Some(hresult);
        self
    }
}

impl fmt::Display for DialogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "folder dialog failed at {:?}", self.stage)?;
        if let Some(hresult) = self.hresult {
            write!(formatter, " (HRESULT 0x{:08X})", hresult as u32)?;
        }
        if let Some(hresult) = self.cleanup_hresult {
            write!(formatter, " (ClearClientData also failed: 0x{:08X})", hresult as u32)?;
        }
        Ok(())
    }
}

impl Error for DialogError {}

/// Terminal, owned result of a Shell request.
#[derive(Clone, Eq, PartialEq)]
pub enum DialogEvent {
    /// The user chose a folder. `cleanup_hresult` is set when the dialog's
    /// post-session privacy cleanup (`ClearClientData`) failed afterwards;
    /// the selection itself stands.
    Selected {
        request_id: DialogRequestId,
        path: PathBuf,
        cleanup_hresult: Option<i32>,
    },
    /// The user dismissed the dialog. `cleanup_hresult` has the same meaning
    /// as for `Selected`; a failed cleanup never turns a dismissal into a
    /// failure.
    Cancelled {
        request_id: DialogRequestId,
        cleanup_hresult: Option<i32>,
    },
    Failed {
        request_id: DialogRequestId,
        error: DialogError,
    },
}

impl fmt::Debug for DialogEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selected { request_id, cleanup_hresult, .. } => formatter
                .debug_struct("Selected")
                .field("request_id", request_id)
                .field("path_present", &true)
                .field("cleanup_failed", &cleanup_hresult.is_some())
                .finish(),
            Self::Cancelled { request_id, cleanup_hresult } => formatter
                .debug_struct("Cancelled")
                .field("request_id", request_id)
                .field("cleanup_failed", &cleanup_hresult.is_some())
                .finish(),
            Self::Failed { request_id, error } => formatter
                .debug_struct("Failed")
                .field("request_id", request_id)
                .field("error", error)
                .finish(),
        }
    }
}

impl DialogEvent {
    /// Returns the request ID shared by every terminal outcome.
    pub const fn request_id(&self) -> DialogRequestId {
        match self {
            Self::Selected { request_id, .. }
            | Self::Cancelled { request_id, .. }
            | Self::Failed { request_id, .. } => *request_id,
        }
    }
}

/// Terminal, owned result of any Shell request.
///
/// Open, reveal, and Installed Apps report `Dispatched`, never completion.
/// Destructive outcomes follow the ADR 0008 result model from
/// `diskpie_app::actions` and carry the per-item callback evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellEvent {
    Dialog(DialogEvent),
    SaveExport(DialogEvent),
    Open { request_id: DialogRequestId, outcome: DispatchOutcome },
    Reveal { request_id: DialogRequestId, outcome: DispatchOutcome },
    InstalledApps { request_id: DialogRequestId, outcome: DispatchOutcome },
    RecycleBinQuery { request_id: DialogRequestId, outcome: RecycleBinQueryOutcome },
    Recycle { request_id: DialogRequestId, outcome: DestructiveOutcome },
    DeletePermanently { request_id: DialogRequestId, outcome: DestructiveOutcome },
    EmptyRecycleBin { request_id: DialogRequestId, outcome: EmptyBinOutcome },
}

impl ShellEvent {
    /// Returns the request ID shared by every terminal outcome.
    pub const fn request_id(&self) -> DialogRequestId {
        match self {
            Self::Dialog(event) | Self::SaveExport(event) => event.request_id(),
            Self::Open { request_id, .. }
            | Self::Reveal { request_id, .. }
            | Self::InstalledApps { request_id, .. }
            | Self::RecycleBinQuery { request_id, .. }
            | Self::Recycle { request_id, .. }
            | Self::DeletePermanently { request_id, .. }
            | Self::EmptyRecycleBin { request_id, .. } => *request_id,
        }
    }
}

/// Why a nonblocking submission was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    QueueFull,
    ModalOutstanding,
    ServiceUnavailable,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::QueueFull => "the Shell request queue is full",
            Self::ModalOutstanding => "a modal Shell request is already outstanding",
            Self::ServiceUnavailable => "the Shell service is unavailable",
        };
        formatter.write_str(message)
    }
}

impl Error for SubmitError {}

/// Failure returned by the nonblocking event receiver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryReceiveError {
    ServiceStopped,
}

impl fmt::Display for TryReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the Shell service event channel is closed")
    }
}

impl Error for TryReceiveError {}

/// Fatal startup failure before a usable Shell STA exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellServiceStartError {
    InvalidQueueCapacity {
        capacity: usize,
    },
    /// The singleton claim is held: a Shell STA is running, a timed-out STA has
    /// not yet been joined by the reaper, or the lifecycle failed closed.
    AlreadyRunning,
    /// The process-wide reaper thread could not be created, so no bounded
    /// shutdown could be guaranteed for the new STA.
    ReaperUnavailable,
    CreateWakeEvent {
        hresult: i32,
    },
    SpawnThread {
        os_code: Option<i32>,
    },
    InitializeCom {
        hresult: i32,
    },
    WorkerExited,
}

impl fmt::Display for ShellServiceStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQueueCapacity { capacity } => write!(
                formatter,
                "Shell request capacity {capacity} is outside 1..={MAX_REQUEST_CAPACITY}"
            ),
            Self::AlreadyRunning => {
                formatter.write_str("a process-wide Shell STA is already running")
            }
            Self::ReaperUnavailable => {
                formatter.write_str("the process-wide Shell reaper thread is unavailable")
            }
            Self::CreateWakeEvent { hresult } => write!(
                formatter,
                "failed to create the Shell wake event (HRESULT 0x{:08X})",
                *hresult as u32
            ),
            Self::SpawnThread { os_code } => {
                write!(formatter, "failed to spawn the Shell STA thread")?;
                if let Some(code) = os_code {
                    write!(formatter, " (OS code {code})")?;
                }
                Ok(())
            }
            Self::InitializeCom { hresult } => write!(
                formatter,
                "failed to initialize the Shell STA (HRESULT 0x{:08X})",
                *hresult as u32
            ),
            Self::WorkerExited => formatter.write_str("the Shell STA exited during startup"),
        }
    }
}

impl Error for ShellServiceStartError {}

/// Failure while synchronously joining the service during shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellServiceShutdownError {
    WorkerPanicked,
}

impl fmt::Display for ShellServiceShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the Shell STA panicked during shutdown")
    }
}

impl Error for ShellServiceShutdownError {}

/// Typed outcome of the bounded [`ShellService::finish`] completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellServiceFinish {
    /// The worker exited within the deadline and was joined; the singleton
    /// claim is released and a new service may start.
    Exited { panicked: bool },
    /// The deadline elapsed. The worker's join handle and the singleton claim
    /// were transferred to the process-wide bounded reaper, which joins the
    /// worker before releasing the claim.
    TimedOutHandedToReaper,
    /// The deadline elapsed and the reaper could not accept the worker. The
    /// lifecycle failed closed: the claim stays held for the rest of the
    /// process and no further Shell STA can start.
    TimedOutReaperUnavailable,
}

/// Current externally observable lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellServiceStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
}

/// Construction parameters for [`ShellService`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellServiceConfig {
    pub request_capacity: usize,
}

impl Default for ShellServiceConfig {
    fn default() -> Self {
        Self { request_capacity: DEFAULT_REQUEST_CAPACITY }
    }
}

impl ShellServiceConfig {
    const fn validate(self) -> Result<Self, ShellServiceStartError> {
        if self.request_capacity == 0 || self.request_capacity > MAX_REQUEST_CAPACITY {
            return Err(ShellServiceStartError::InvalidQueueCapacity {
                capacity: self.request_capacity,
            });
        }
        Ok(self)
    }
}

struct QueuedRequest {
    id: DialogRequestId,
    request: ShellRequest,
    /// Request-local cancellation flag shared with [`ShellService::cancel`].
    cancel: Arc<AtomicBool>,
}

impl QueuedRequest {
    fn meta(&self) -> RequestMeta {
        RequestMeta { id: self.id, kind: self.request.kind(), was_modal: self.request.is_modal() }
    }
}

/// Payload-free request facts needed to emit a terminal event after the
/// request itself has been moved into its handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestMeta {
    id: DialogRequestId,
    kind: ShellRequestKind,
    was_modal: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct CompletedRequest {
    event: ShellEvent,
    // The UI releases this reservation only while delivering the event. That
    // keeps both visible dialogs and unread modal results bounded to one.
    was_modal: bool,
}

struct WakeEvent {
    handle: OwnedHandle,
}

impl WakeEvent {
    fn create() -> Result<Self, i32> {
        // SAFETY: null security attributes and name request a process-local,
        // unnamed auto-reset event. The returned handle is immediately owned by
        // this RAII wrapper.
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|error| error.code().0)?;
        // SAFETY: `CreateEventW` returned a new owned handle. `OwnedHandle`
        // becomes its sole owner and closes it exactly once.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok(Self { handle })
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    fn signal(&self) -> Result<(), i32> {
        // SAFETY: the handle remains owned by `self`; Windows event handles may
        // be signalled from any process thread.
        unsafe { SetEvent(self.handle()) }.map_err(|error| error.code().0)
    }
}

struct SharedState {
    lifecycle: AtomicU8,
    shutdown_requested: AtomicBool,
    modal_outstanding: AtomicBool,
    acceptance: Mutex<()>,
    wake: Arc<WakeEvent>,
}

impl SharedState {
    fn new(wake: Arc<WakeEvent>) -> Self {
        Self {
            lifecycle: AtomicU8::new(STATE_STARTING),
            shutdown_requested: AtomicBool::new(false),
            modal_outstanding: AtomicBool::new(false),
            acceptance: Mutex::new(()),
            wake,
        }
    }

    fn status(&self) -> ShellServiceStatus {
        lifecycle_from_raw(self.lifecycle.load(Ordering::Acquire))
    }
}

struct WorkerStateGuard {
    shared: Arc<SharedState>,
}

/// Process-wide singleton bookkeeping: the instance claim, the fail-closed
/// flag, and the precreated bounded reaper.
///
/// Production code uses exactly one registry ([`process_registry`]). Tests
/// construct private registries so injected stalled or panicking workers
/// cannot disturb each other or the real singleton.
struct ShellInstanceRegistry {
    claimed: AtomicBool,
    fail_closed: AtomicBool,
    reaper: OnceLock<Option<ShellReaper>>,
}

impl ShellInstanceRegistry {
    fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            fail_closed: AtomicBool::new(false),
            reaper: OnceLock::new(),
        }
    }

    /// Creates the reaper thread at most once. A creation failure is final.
    fn ensure_reaper(&self) -> Result<&ShellReaper, ShellServiceStartError> {
        self.reaper
            .get_or_init(ShellReaper::start)
            .as_ref()
            .ok_or(ShellServiceStartError::ReaperUnavailable)
    }

    /// Returns the precreated reaper without initializing anything.
    fn reaper(&self) -> Option<&ShellReaper> {
        self.reaper.get().and_then(Option::as_ref)
    }

    #[cfg(test)]
    fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    fn is_fail_closed(&self) -> bool {
        self.fail_closed.load(Ordering::Acquire)
    }
}

/// Exclusive right to run the one Shell STA of a registry.
///
/// The claim is released when it is dropped, except after the registry failed
/// closed: then the flag stays set forever because an unjoined STA may still
/// be alive somewhere in the process.
struct ServiceInstanceClaim {
    registry: Arc<ShellInstanceRegistry>,
}

impl ServiceInstanceClaim {
    fn acquire(registry: &Arc<ShellInstanceRegistry>) -> Result<Self, ShellServiceStartError> {
        if registry.is_fail_closed() {
            return Err(ShellServiceStartError::AlreadyRunning);
        }
        registry
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self { registry: Arc::clone(registry) })
            .map_err(|_| ShellServiceStartError::AlreadyRunning)
    }
}

impl Drop for ServiceInstanceClaim {
    fn drop(&mut self) {
        if !self.registry.is_fail_closed() {
            self.registry.claimed.store(false, Ordering::Release);
        }
    }
}

/// A timed-out worker and the claim that must outlive it.
struct ReapRequest {
    worker: JoinHandle<()>,
    claim: ServiceInstanceClaim,
}

impl ReapRequest {
    /// Joins the worker and only then releases the claim.
    fn reap(self) {
        let Self { worker, claim } = self;
        let _exit = worker.join();
        drop(claim);
    }
}

/// One process-wide thread that joins timed-out Shell workers.
///
/// The queue is a fixed-capacity `sync_channel`; submission never blocks and
/// never allocates a thread. Losing the receiver or filling the queue is
/// reported to the caller, which then fails closed.
struct ShellReaper {
    sender: SyncSender<ReapRequest>,
    _worker: Option<JoinHandle<()>>,
}

impl ShellReaper {
    fn start() -> Option<Self> {
        let (sender, receiver) = sync_channel(SHELL_REAPER_CAPACITY);
        let worker = thread::Builder::new()
            .name(REAPER_THREAD_NAME.to_owned())
            .spawn(move || shell_reaper_loop(&receiver))
            .ok()?;
        Some(Self { sender, _worker: Some(worker) })
    }

    /// Hands a worker over without waiting. The payload is returned intact
    /// when the queue is full or the reaper thread is gone.
    fn try_submit(&self, request: ReapRequest) -> Result<(), ReapRequest> {
        self.sender.try_send(request).map_err(|error| match error {
            TrySendError::Full(request) | TrySendError::Disconnected(request) => request,
        })
    }

    /// A reaper whose queue is already full and whose thread never drains it.
    ///
    /// The receiver is returned so the caller decides whether submissions
    /// observe `Full` (receiver alive) or `Disconnected` (receiver dropped).
    #[cfg(test)]
    fn saturated_for_test() -> (Self, Receiver<ReapRequest>) {
        let (sender, receiver) = sync_channel(SHELL_REAPER_CAPACITY);
        for _ in 0..SHELL_REAPER_CAPACITY {
            let registry = Arc::new(ShellInstanceRegistry::new());
            let claim = ServiceInstanceClaim::acquire(&registry).expect("fresh registry");
            let worker = thread::spawn(|| {});
            sender.try_send(ReapRequest { worker, claim }).expect("capacity is exact");
        }
        (Self { sender, _worker: None }, receiver)
    }
}

fn shell_reaper_loop(receiver: &Receiver<ReapRequest>) {
    while let Ok(request) = receiver.recv() {
        request.reap();
    }
}

/// How the STA worker reacts once shutdown is requested.
///
/// Production always uses `Native`. The test-only variants model a worker
/// that is stuck inside a modal dialog or that unwinds while stopping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerBehaviour {
    Native,
    #[cfg(test)]
    StallAfterShutdown(Duration),
    #[cfg(test)]
    PanicAfterShutdown,
}

impl WorkerBehaviour {
    fn observe_shutdown(self) {
        match self {
            Self::Native => {}
            #[cfg(test)]
            Self::StallAfterShutdown(duration) => thread::sleep(duration),
            #[cfg(test)]
            Self::PanicAfterShutdown => panic!("injected Shell STA shutdown panic"),
        }
    }
}

impl Drop for WorkerStateGuard {
    fn drop(&mut self) {
        self.shared.lifecycle.store(STATE_STOPPED, Ordering::Release);
    }
}

/// Handle to the one process-owned, message-pumping Shell STA.
///
/// `submit`, `try_recv`, and `request_shutdown` never block. The composition
/// root completes the service with the bounded `finish`, outside any UI
/// callback. `Drop` never joins: a worker that is still running is handed to
/// the process-wide bounded reaper together with the singleton claim. The
/// legacy `shutdown` still joins synchronously and can therefore wait for a
/// visible native modal dialog to return; prefer `finish`.
pub struct ShellService {
    request_tx: SyncSender<QueuedRequest>,
    event_rx: Receiver<CompletedRequest>,
    shared: Arc<SharedState>,
    registry: Arc<ShellInstanceRegistry>,
    next_request_id: AtomicU64,
    /// Cancellation flags of accepted requests whose terminal event has not
    /// been delivered by `try_recv` yet. Modal requests contribute at most one
    /// entry; non-modal requests (open, reveal, Installed Apps, bin query)
    /// accumulate with their undrained events until the caller polls, so the
    /// registry is bounded by the caller's polling cadence, not by the queue
    /// capacity. Each delivery removes its entry with a linear scan.
    cancellations: Mutex<Vec<(DialogRequestId, Arc<AtomicBool>)>>,
    worker: Option<JoinHandle<()>>,
    instance_claim: Option<ServiceInstanceClaim>,
    /// `Some(panicked)` once this handle joined its worker itself.
    joined: Option<bool>,
}

impl ShellService {
    /// Starts the service with [`ShellServiceConfig::default`].
    pub fn start() -> Result<Self, ShellServiceStartError> {
        Self::start_with_config(ShellServiceConfig::default())
    }

    /// Starts and handshakes with a dedicated STA worker.
    pub fn start_with_config(config: ShellServiceConfig) -> Result<Self, ShellServiceStartError> {
        Self::start_in(process_registry(), config, WorkerBehaviour::Native)
    }

    fn start_in(
        registry: Arc<ShellInstanceRegistry>,
        config: ShellServiceConfig,
        behaviour: WorkerBehaviour,
    ) -> Result<Self, ShellServiceStartError> {
        let config = config.validate()?;
        let instance_claim = ServiceInstanceClaim::acquire(&registry)?;
        // The reaper is precreated so a later timed-out shutdown never has to
        // create a thread and a creation failure surfaces here, not at drop.
        registry.ensure_reaper()?;
        let wake = Arc::new(
            WakeEvent::create()
                .map_err(|hresult| ShellServiceStartError::CreateWakeEvent { hresult })?,
        );
        let shared = Arc::new(SharedState::new(Arc::clone(&wake)));
        let (request_tx, request_rx) = sync_channel(config.request_capacity);
        let (event_tx, event_rx) = channel();
        let (startup_tx, startup_rx) = sync_channel(1);
        let worker_shared = Arc::clone(&shared);

        let worker = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || {
                worker_entry(request_rx, event_tx, startup_tx, worker_shared, behaviour);
            })
            .map_err(|error| ShellServiceStartError::SpawnThread {
                os_code: error.raw_os_error(),
            })?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                request_tx,
                event_rx,
                shared,
                registry,
                next_request_id: AtomicU64::new(1),
                cancellations: Mutex::new(Vec::new()),
                worker: Some(worker),
                instance_claim: Some(instance_claim),
                joined: None,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(ShellServiceStartError::WorkerExited)
            }
        }
    }

    /// Attempts to enqueue work without waiting for queue capacity.
    pub fn submit(&self, request: ShellRequest) -> Result<DialogRequestId, SubmitError> {
        let id = DialogRequestId(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let cancel = Arc::new(AtomicBool::new(false));
        try_accept_request(
            &self.request_tx,
            &self.shared,
            QueuedRequest { id, request, cancel: Arc::clone(&cancel) },
        )?;
        self.with_cancellations(|entries| entries.push((id, cancel)));

        // Signalling is an optimization backed by a short health timeout in
        // the message-aware wait. Once `try_send` succeeds the request remains
        // accepted even if an anomalous SetEvent call fails.
        let _ = self.shared.wake.signal();
        Ok(id)
    }

    /// Requests cancellation of an accepted request without waiting.
    ///
    /// A request that has not started yet is reliably skipped and reports
    /// `CancelledBeforeMutation` (or a `CancelledBeforeDispatch` failure).
    /// A file operation that is already running observes the flag in its next
    /// `PreDeleteItem` callback; interruption inside one recursive item or
    /// after `SHEmptyRecycleBinW` dispatch is not promised. Returns whether
    /// the request was still outstanding.
    pub fn cancel(&self, id: DialogRequestId) -> bool {
        self.with_cancellations(|entries| {
            entries.iter().find(|(entry, _)| *entry == id).is_some_and(|(_, flag)| {
                flag.store(true, Ordering::Release);
                true
            })
        })
    }

    fn with_cancellations<R>(
        &self,
        update: impl FnOnce(&mut Vec<(DialogRequestId, Arc<AtomicBool>)>) -> R,
    ) -> R {
        // The handle is `!Sync`, so this lock is never contended; it only
        // guards against a poisoned state after a panic in the caller.
        let mut entries =
            self.cancellations.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut entries)
    }

    /// Convenience wrapper for the currently implemented request kind.
    pub fn submit_folder(&self, owner: OwnerWindow) -> Result<DialogRequestId, SubmitError> {
        self.submit(ShellRequest::PickFolder(FolderDialogRequest { owner }))
    }

    /// Receives at most one terminal event without blocking the caller.
    ///
    /// A modal request remains outstanding until this method delivers its
    /// terminal event, bounding unread modal results as well as visible dialogs.
    pub fn try_recv(&self) -> Result<Option<ShellEvent>, TryReceiveError> {
        match self.event_rx.try_recv() {
            Ok(completed) => {
                let event = deliver_completed_request(&self.shared, completed);
                let id = event.request_id();
                self.with_cancellations(|entries| entries.retain(|(entry, _)| *entry != id));
                Ok(Some(event))
            }
            Err(MpscTryRecvError::Empty) => Ok(None),
            Err(MpscTryRecvError::Disconnected) => Err(TryReceiveError::ServiceStopped),
        }
    }

    /// Returns the service lifecycle without waiting.
    pub fn status(&self) -> ShellServiceStatus {
        self.shared.status()
    }

    /// Requests stop and wakes the message-aware wait without waiting.
    ///
    /// The method is idempotent and performs atomic work only, so it is safe
    /// inside an egui callback. Queued requests that have not started receive
    /// `DialogEvent::Failed` with `ServiceShutdown` before the worker exits. A
    /// worker inside a native modal dialog observes the request only after
    /// that dialog returns; the application's owner-window teardown should
    /// close it during exit.
    pub fn request_shutdown(&self) {
        // Acceptance closes through the atomic state alone. A submit that
        // already passed the Running check under the acceptance gate can still
        // enqueue; the worker drains under that same gate after observing the
        // request, so no accepted request is lost.
        let _ = self.shared.lifecycle.compare_exchange(
            STATE_RUNNING,
            STATE_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.shared.shutdown_requested.store(true, Ordering::Release);
        // Signalling is an optimization backed by the bounded health timeout
        // of the message-aware wait; the flag alone terminates the loop.
        let _ = self.shared.wake.signal();
    }

    /// Requests stop and waits at most `timeout` for the STA to exit.
    ///
    /// Call this from the composition root outside any UI callback. A worker
    /// that has not exited by the deadline is transferred, with the singleton
    /// claim, to the process-wide bounded reaper. When even that transfer is
    /// impossible the lifecycle fails closed and reports it.
    pub fn finish(mut self, timeout: Duration) -> ShellServiceFinish {
        self.request_shutdown();
        let Some(worker) = self.worker.take() else {
            return ShellServiceFinish::Exited { panicked: self.joined.unwrap_or(false) };
        };

        if wait_until_finished(&worker, timeout) {
            let panicked = worker.join().is_err();
            self.joined = Some(panicked);
            self.instance_claim.take();
            return ShellServiceFinish::Exited { panicked };
        }

        let Some(claim) = self.instance_claim.take() else {
            // Unreachable by construction: the claim is only taken together
            // with the worker. Fail closed rather than release ownership.
            self.registry.fail_closed.store(true, Ordering::Release);
            return ShellServiceFinish::TimedOutReaperUnavailable;
        };
        if self.hand_to_reaper(ReapRequest { worker, claim }) {
            ShellServiceFinish::TimedOutHandedToReaper
        } else {
            ShellServiceFinish::TimedOutReaperUnavailable
        }
    }

    /// Requests stop, wakes the message-aware wait, and joins the STA.
    ///
    /// The method is idempotent and repeats the recorded join outcome. It can
    /// block for as long as a visible modal dialog stays open; prefer
    /// [`ShellService::finish`].
    pub fn shutdown(&mut self) -> Result<(), ShellServiceShutdownError> {
        self.request_shutdown();
        let Some(worker) = self.worker.take() else {
            return match self.joined {
                Some(true) => Err(ShellServiceShutdownError::WorkerPanicked),
                Some(false) | None => Ok(()),
            };
        };

        let panicked = worker.join().is_err();
        self.joined = Some(panicked);
        self.instance_claim.take();
        if panicked { Err(ShellServiceShutdownError::WorkerPanicked) } else { Ok(()) }
    }

    /// Transfers a live worker and its claim to the bounded reaper.
    ///
    /// Returns `false` after failing closed: the claim is retained for the
    /// process lifetime and the detached worker is left to exit on its own.
    fn hand_to_reaper(&self, request: ReapRequest) -> bool {
        let rejected = match self.registry.reaper() {
            Some(reaper) => match reaper.try_submit(request) {
                Ok(()) => return true,
                Err(request) => request,
            },
            None => request,
        };
        // Order matters: the flag must be set before the claim drops so the
        // claim's `Drop` keeps the singleton marked as held.
        self.registry.fail_closed.store(true, Ordering::Release);
        drop(rejected);
        false
    }
}

impl Drop for ShellService {
    /// Atomic/`Arc` work only: never joins, never creates a thread.
    fn drop(&mut self) {
        self.request_shutdown();
        if let (Some(worker), Some(claim)) = (self.worker.take(), self.instance_claim.take()) {
            let _handed = self.hand_to_reaper(ReapRequest { worker, claim });
        }
    }
}

/// Polls join-readiness with bounded sleeps until `timeout` elapses.
fn wait_until_finished(worker: &JoinHandle<()>, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        if worker.is_finished() {
            return true;
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return false;
        }
        thread::sleep((timeout - elapsed).min(FINISH_POLL_INTERVAL));
    }
}

fn lifecycle_from_raw(raw: u8) -> ShellServiceStatus {
    match raw {
        STATE_STARTING => ShellServiceStatus::Starting,
        STATE_RUNNING => ShellServiceStatus::Running,
        STATE_STOPPING => ShellServiceStatus::Stopping,
        _ => ShellServiceStatus::Stopped,
    }
}

fn try_accept_request(
    sender: &SyncSender<QueuedRequest>,
    shared: &SharedState,
    queued: QueuedRequest,
) -> Result<(), SubmitError> {
    // `try_lock` preserves the public nonblocking contract. The worker takes
    // the same gate while it closes acceptance and performs its final drain,
    // so an accepted request cannot arrive after that drain observed empty.
    let _acceptance = shared.acceptance.try_lock().map_err(|_| SubmitError::ServiceUnavailable)?;
    if shared.status() != ShellServiceStatus::Running {
        return Err(SubmitError::ServiceUnavailable);
    }
    try_enqueue_request(sender, &shared.modal_outstanding, queued)
}

fn try_enqueue_request(
    sender: &SyncSender<QueuedRequest>,
    modal_outstanding: &AtomicBool,
    queued: QueuedRequest,
) -> Result<(), SubmitError> {
    let reserved_modal = queued.request.is_modal();
    if reserved_modal
        && modal_outstanding
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return Err(SubmitError::ModalOutstanding);
    }

    match sender.try_send(queued) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            if reserved_modal {
                modal_outstanding.store(false, Ordering::Release);
            }
            Err(SubmitError::QueueFull)
        }
        Err(TrySendError::Disconnected(_)) => {
            if reserved_modal {
                modal_outstanding.store(false, Ordering::Release);
            }
            Err(SubmitError::ServiceUnavailable)
        }
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, i32> {
        // SAFETY: this is the worker's first COM initialization. Both S_OK and
        // S_FALSE establish a call that must be balanced on this same thread.
        let hresult =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        classify_com_initialization(hresult.0).map(|()| Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: the guard is created and dropped inside `worker_entry`; it
        // balances the successful `CoInitializeEx` on the identical thread.
        unsafe { CoUninitialize() };
    }
}

fn classify_com_initialization(hresult: i32) -> Result<(), i32> {
    if HRESULT(hresult).is_ok() { Ok(()) } else { Err(hresult) }
}

fn worker_entry(
    request_rx: Receiver<QueuedRequest>,
    event_tx: std::sync::mpsc::Sender<CompletedRequest>,
    startup_tx: SyncSender<Result<(), ShellServiceStartError>>,
    shared: Arc<SharedState>,
    behaviour: WorkerBehaviour,
) {
    let _state_guard = WorkerStateGuard { shared: Arc::clone(&shared) };

    let apartment = match ComApartment::initialize() {
        Ok(apartment) => apartment,
        Err(hresult) => {
            let _ = startup_tx.send(Err(ShellServiceStartError::InitializeCom { hresult }));
            return;
        }
    };

    match pump_pending_messages() {
        Ok(PumpResult::Continue) => {}
        Ok(PumpResult::Quit) | Err(_) => {
            let _ = startup_tx.send(Err(ShellServiceStartError::WorkerExited));
            return;
        }
    }

    shared.lifecycle.store(STATE_RUNNING, Ordering::Release);
    if startup_tx.send(Ok(())).is_err() {
        return;
    }

    let _apartment = apartment;
    let outcome =
        catch_unwind(AssertUnwindSafe(|| worker_loop(&request_rx, &event_tx, &shared, behaviour)));
    match outcome {
        Ok(error) => stop_accepting_and_fail_queued(&request_rx, &event_tx, &shared, error),
        Err(payload) => {
            stop_accepting_and_fail_queued(
                &request_rx,
                &event_tx,
                &shared,
                DialogError::worker_panicked(),
            );
            resume_unwind(payload);
        }
    }
}

fn worker_loop(
    request_rx: &Receiver<QueuedRequest>,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    behaviour: WorkerBehaviour,
) -> DialogError {
    loop {
        if shared.shutdown_requested.load(Ordering::Acquire) {
            behaviour.observe_shutdown();
            return DialogError::service_shutdown();
        }

        match request_rx.try_recv() {
            Ok(queued) => {
                let meta = queued.meta();
                if shared.shutdown_requested.load(Ordering::Acquire) {
                    fail_request(meta, event_tx, shared, DialogError::service_shutdown());
                    behaviour.observe_shutdown();
                    return DialogError::service_shutdown();
                }
                if let Err(error) = guard_request_execution(meta, event_tx, shared, || {
                    process_request(queued, event_tx, shared);
                }) {
                    return error;
                }
            }
            Err(MpscTryRecvError::Empty) => {}
            Err(MpscTryRecvError::Disconnected) => return DialogError::service_shutdown(),
        }

        match pump_pending_messages() {
            Ok(PumpResult::Continue) => {}
            Ok(PumpResult::Quit) => {
                return DialogError::from_hresult(DialogErrorStage::MessagePump, E_UNEXPECTED.0);
            }
            Err(error) => return error,
        }

        let handle = shared.wake.handle();
        // SAFETY: `handle` is a live event for the full call. The worker pumps
        // every queued message after the message-aware wait reports input.
        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(std::slice::from_ref(&handle)),
                WAIT_HEALTH_INTERVAL_MS,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };

        if wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT {
            continue;
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            continue;
        }
        if wait == WAIT_FAILED {
            return DialogError::from_hresult(
                DialogErrorStage::MessagePump,
                HRESULT::from_thread().0,
            );
        }

        return DialogError::from_hresult(DialogErrorStage::MessagePump, E_UNEXPECTED.0);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PumpResult {
    Continue,
    Quit,
}

fn pump_pending_messages() -> Result<PumpResult, DialogError> {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is writable for the duration of the call. Passing a
        // null HWND retrieves all messages belonging to this STA thread.
        let available = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();
        if !available {
            return Ok(PumpResult::Continue);
        }
        if message.message == WM_QUIT {
            return Ok(PumpResult::Quit);
        }

        // SAFETY: `message` was initialized by `PeekMessageW`; both functions
        // consume it synchronously and retain no pointer.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn process_request(
    queued: QueuedRequest,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
) {
    let meta = queued.meta();
    let event = execute_request(queued);
    send_completed_request(event_tx, shared, CompletedRequest { event, was_modal: meta.was_modal });
}

/// Runs one request on the STA and produces its terminal event.
///
/// A request whose cancellation flag is already set never touches the Shell.
fn execute_request(queued: QueuedRequest) -> ShellEvent {
    let meta = queued.meta();
    let QueuedRequest { id, request, cancel } = queued;
    if cancel.load(Ordering::Acquire) {
        return cancelled_before_start_event(meta);
    }
    match request {
        ShellRequest::SaveExport(request) => {
            ShellEvent::SaveExport(match show_export_dialog(request.owner) {
                Ok((DialogOutcome::Selected(path), cleanup_hresult)) => {
                    DialogEvent::Selected { request_id: id, path, cleanup_hresult }
                }
                Ok((DialogOutcome::Cancelled, cleanup_hresult)) => {
                    DialogEvent::Cancelled { request_id: id, cleanup_hresult }
                }
                Err(error) => DialogEvent::Failed { request_id: id, error },
            })
        }
        ShellRequest::PickFolder(request) => {
            ShellEvent::Dialog(match show_folder_dialog(request.owner) {
                Ok((DialogOutcome::Selected(path), cleanup_hresult)) => {
                    DialogEvent::Selected { request_id: id, path, cleanup_hresult }
                }
                Ok((DialogOutcome::Cancelled, cleanup_hresult)) => {
                    DialogEvent::Cancelled { request_id: id, cleanup_hresult }
                }
                Err(error) => DialogEvent::Failed { request_id: id, error },
            })
        }
        ShellRequest::Open(request) => ShellEvent::Open {
            request_id: id,
            outcome: open_item(&mut NativeShell, request.owner, &request.path),
        },
        ShellRequest::Reveal(request) => ShellEvent::Reveal {
            request_id: id,
            outcome: reveal_item(&mut NativeShell, &request.path),
        },
        ShellRequest::InstalledApps(request) => ShellEvent::InstalledApps {
            request_id: id,
            outcome: open_installed_apps(&mut NativeShell, request.owner),
        },
        ShellRequest::QueryRecycleBin(request) => ShellEvent::RecycleBinQuery {
            request_id: id,
            outcome: match query_recycle_bin(&request.scope) {
                Ok(estimate) => RecycleBinQueryOutcome::Estimated(estimate),
                Err(hresult) => RecycleBinQueryOutcome::Failed { hresult: Some(hresult) },
            },
        },
        ShellRequest::Recycle(request) => ShellEvent::Recycle {
            request_id: id,
            outcome: run_delete_request(&request, DeleteMode::Recycle, &cancel),
        },
        ShellRequest::DeletePermanently(request) => ShellEvent::DeletePermanently {
            request_id: id,
            outcome: run_delete_request(&request, DeleteMode::Permanent, &cancel),
        },
        ShellRequest::EmptyRecycleBin(request) => ShellEvent::EmptyRecycleBin {
            request_id: id,
            outcome: empty_recycle_bin(request.owner, &request.scope, &cancel),
        },
    }
}

/// Runs a delete payload only when its mode matches the variant it arrived in.
///
/// Both destructive variants share the payload type, so a payload could be
/// re-wrapped in the other variant; the terminal event would then be labelled
/// with the wrong operation. Such a request is refused without running.
fn run_delete_request(
    request: &DeleteShellRequest,
    expected_mode: DeleteMode,
    cancel: &Arc<AtomicBool>,
) -> DestructiveOutcome {
    if request.mode != expected_mode {
        return DestructiveOutcome::Failed { stage: FailureStage::RequestMismatch, code: None };
    }
    run_delete(&DeleteJob {
        owner: request.owner,
        path: &request.path,
        kind: request.kind,
        identity: request.identity,
        mode: request.mode,
        cancel,
    })
}

/// Terminal event for a request cancelled before the STA started it.
fn cancelled_before_start_event(meta: RequestMeta) -> ShellEvent {
    let request_id = meta.id;
    let not_run = DispatchOutcome::not_run(DispatchStage::CancelledBeforeDispatch);
    match meta.kind {
        ShellRequestKind::SaveExport => {
            ShellEvent::SaveExport(DialogEvent::Cancelled { request_id, cleanup_hresult: None })
        }
        ShellRequestKind::PickFolder => {
            ShellEvent::Dialog(DialogEvent::Cancelled { request_id, cleanup_hresult: None })
        }
        ShellRequestKind::Open => ShellEvent::Open { request_id, outcome: not_run },
        ShellRequestKind::Reveal => ShellEvent::Reveal { request_id, outcome: not_run },
        ShellRequestKind::InstalledApps => {
            ShellEvent::InstalledApps { request_id, outcome: not_run }
        }
        ShellRequestKind::QueryRecycleBin => ShellEvent::RecycleBinQuery {
            request_id,
            outcome: RecycleBinQueryOutcome::NotRun(DispatchStage::CancelledBeforeDispatch),
        },
        ShellRequestKind::Recycle => {
            ShellEvent::Recycle { request_id, outcome: DestructiveOutcome::CancelledBeforeMutation }
        }
        ShellRequestKind::DeletePermanently => ShellEvent::DeletePermanently {
            request_id,
            outcome: DestructiveOutcome::CancelledBeforeMutation,
        },
        ShellRequestKind::EmptyRecycleBin => ShellEvent::EmptyRecycleBin {
            request_id,
            outcome: EmptyBinOutcome::CancelledBeforeMutation,
        },
    }
}

/// Terminal event for a request the STA could not run at all.
///
/// A worker panic during a destructive request is reported as
/// `UnknownMayHaveMutated`; every other failure to run is a cancellation
/// before mutation because the request never reached the Shell.
fn failed_request_event(meta: RequestMeta, error: DialogError) -> ShellEvent {
    let request_id = meta.id;
    let panicked = error.stage == DialogErrorStage::WorkerPanicked;
    let stage =
        if panicked { DispatchStage::WorkerPanicked } else { DispatchStage::ServiceShutdown };
    let not_run = DispatchOutcome::not_run(stage);
    let destructive = if panicked {
        DestructiveOutcome::UnknownMayHaveMutated {
            stage: FailureStage::WorkerPanicked,
            code: None,
        }
    } else {
        DestructiveOutcome::CancelledBeforeMutation
    };
    match meta.kind {
        ShellRequestKind::SaveExport => {
            ShellEvent::SaveExport(DialogEvent::Failed { request_id, error })
        }
        ShellRequestKind::PickFolder => {
            ShellEvent::Dialog(DialogEvent::Failed { request_id, error })
        }
        ShellRequestKind::Open => ShellEvent::Open { request_id, outcome: not_run },
        ShellRequestKind::Reveal => ShellEvent::Reveal { request_id, outcome: not_run },
        ShellRequestKind::InstalledApps => {
            ShellEvent::InstalledApps { request_id, outcome: not_run }
        }
        ShellRequestKind::QueryRecycleBin => ShellEvent::RecycleBinQuery {
            request_id,
            outcome: RecycleBinQueryOutcome::NotRun(stage),
        },
        ShellRequestKind::Recycle => ShellEvent::Recycle { request_id, outcome: destructive },
        ShellRequestKind::DeletePermanently => {
            ShellEvent::DeletePermanently { request_id, outcome: destructive }
        }
        ShellRequestKind::EmptyRecycleBin => ShellEvent::EmptyRecycleBin {
            request_id,
            outcome: if panicked {
                EmptyBinOutcome::UnknownMayHaveMutated {
                    stage: FailureStage::WorkerPanicked,
                    code: None,
                    before: None,
                    after: None,
                }
            } else {
                EmptyBinOutcome::CancelledBeforeMutation
            },
        },
    }
}

fn guard_request_execution<F>(
    meta: RequestMeta,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    execute: F,
) -> Result<(), DialogError>
where
    F: FnOnce(),
{
    if catch_unwind(AssertUnwindSafe(execute)).is_ok() {
        return Ok(());
    }
    let error = DialogError::worker_panicked();
    fail_request(meta, event_tx, shared, error);
    Err(error)
}

fn fail_request(
    meta: RequestMeta,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    error: DialogError,
) {
    let completed =
        CompletedRequest { event: failed_request_event(meta, error), was_modal: meta.was_modal };
    send_completed_request(event_tx, shared, completed);
}

fn fail_queued_requests(
    request_rx: &Receiver<QueuedRequest>,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    error: DialogError,
) {
    while let Ok(queued) = request_rx.try_recv() {
        fail_request(queued.meta(), event_tx, shared, error);
    }
}

fn stop_accepting_and_fail_queued(
    request_rx: &Receiver<QueuedRequest>,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    error: DialogError,
) {
    let _acceptance = shared.acceptance.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    shared.lifecycle.store(STATE_STOPPING, Ordering::Release);
    fail_queued_requests(request_rx, event_tx, shared, error);
}

fn send_completed_request(
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    completed: CompletedRequest,
) {
    let was_modal = completed.was_modal;
    if event_tx.send(completed).is_err() && was_modal {
        shared.modal_outstanding.store(false, Ordering::Release);
    }
}

fn deliver_completed_request(shared: &SharedState, completed: CompletedRequest) -> ShellEvent {
    if completed.was_modal {
        shared.modal_outstanding.store(false, Ordering::Release);
    }
    completed.event
}

#[derive(Debug, Eq, PartialEq)]
enum DialogOutcome {
    Selected(PathBuf),
    Cancelled,
}

fn show_folder_dialog(owner: OwnerWindow) -> Result<(DialogOutcome, Option<i32>), DialogError> {
    // SAFETY: COM is initialized as an STA on this thread. `dialog` never
    // leaves this function/thread and its projection releases it on return.
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| DialogError::from_hresult(DialogErrorStage::CreateDialog, error.code().0),
        )?;

    // SAFETY: `dialog` is a live interface owned by this STA.
    let existing_options = unsafe { dialog.GetOptions() }
        .map_err(|error| DialogError::from_hresult(DialogErrorStage::GetOptions, error.code().0))?;
    let options = folder_dialog_options(existing_options);
    // SAFETY: flags contain the dialog's existing options plus documented
    // FILEOPENDIALOGOPTIONS values, and the interface remains on its STA.
    unsafe { dialog.SetOptions(options) }
        .map_err(|error| DialogError::from_hresult(DialogErrorStage::SetOptions, error.code().0))?;
    // SAFETY: the GUID address is stable for the synchronous COM call and the
    // interface remains owned by this STA.
    unsafe { dialog.SetClientGuid(&FOLDER_DIALOG_CLIENT_GUID) }.map_err(|error| {
        DialogError::from_hresult(DialogErrorStage::SetClientGuid, error.code().0)
    })?;

    let cleanup = CleanupGuard::new(|| {
        // SAFETY: cleanup occurs on the owning STA before releasing `dialog`.
        // The RAII guard also attempts it while unwinding from result handling.
        unsafe { dialog.ClearClientData() }.map_err(|error| error.code().0)
    });
    cleanup.finish(show_and_extract(&dialog, owner))
}

fn show_export_dialog(owner: OwnerWindow) -> Result<(DialogOutcome, Option<i32>), DialogError> {
    // SAFETY: COM was initialized on this STA, and the interface never leaves it.
    let dialog: IFileSaveDialog =
        unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| DialogError::from_hresult(DialogErrorStage::CreateDialog, error.code().0),
        )?;
    // SAFETY: the interface is live and owned by this STA.
    let existing = unsafe { dialog.GetOptions() }
        .map_err(|error| DialogError::from_hresult(DialogErrorStage::GetOptions, error.code().0))?;
    // Handle-bound preflight will ask the user for a new name if it exists;
    // create-only exports must never offer an overwrite confirmation here.
    let options = (existing | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_DONTADDTORECENT)
        & !FOS_OVERWRITEPROMPT;
    // SAFETY: every pointer below is a live NUL-terminated local buffer for
    // synchronous COM calls. Only fixed product text and UTC digits are used.
    unsafe { dialog.SetOptions(options) }
        .map_err(|error| DialogError::from_hresult(DialogErrorStage::SetOptions, error.code().0))?;
    // SAFETY: the fixed GUID outlives the synchronous call.
    unsafe { dialog.SetClientGuid(&EXPORT_DIALOG_CLIENT_GUID) }.map_err(|error| {
        DialogError::from_hresult(DialogErrorStage::SetClientGuid, error.code().0)
    })?;
    let cleanup = CleanupGuard::new(|| {
        // SAFETY: the guard runs before releasing the interface on this STA.
        unsafe { dialog.ClearClientData() }.map_err(|error| error.code().0)
    });
    let result = (|| {
        // SAFETY: GetSystemTime returns a plain initialized SYSTEMTIME value.
        let utc = unsafe { GetSystemTime() };
        let name = format!(
            "DiskPie-diagnostics-{:04}-{:02}-{:02}-{:02}{:02}{:02}.txt",
            utc.wYear, utc.wMonth, utc.wDay, utc.wHour, utc.wMinute, utc.wSecond
        );
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let extension = [b't' as u16, b'x' as u16, b't' as u16, 0];
        // SAFETY: both buffers are NUL-terminated and live for these calls.
        unsafe {
            dialog.SetFileName(PCWSTR(name.as_ptr()))?;
            dialog.SetDefaultExtension(PCWSTR(extension.as_ptr()))?;
        }
        Ok::<(), windows::core::Error>(())
    })()
    .map_err(|error| DialogError::from_hresult(DialogErrorStage::SetOptions, error.code().0));
    cleanup.finish(result.and_then(|()| show_and_extract(&dialog, owner)))
}

struct CleanupGuard<F>
where
    F: FnMut() -> Result<(), i32>,
{
    cleanup: F,
    armed: bool,
}

impl<F> CleanupGuard<F>
where
    F: FnMut() -> Result<(), i32>,
{
    fn new(cleanup: F) -> Self {
        Self { cleanup, armed: true }
    }

    fn finish(
        mut self,
        outcome: Result<DialogOutcome, DialogError>,
    ) -> Result<(DialogOutcome, Option<i32>), DialogError> {
        let cleanup = (self.cleanup)();
        self.armed = false;
        merge_dialog_cleanup(outcome, cleanup)
    }
}

impl<F> Drop for CleanupGuard<F>
where
    F: FnMut() -> Result<(), i32>,
{
    fn drop(&mut self) {
        if self.armed {
            let _ = catch_unwind(AssertUnwindSafe(|| (self.cleanup)()));
            self.armed = false;
        }
    }
}

/// Combines the dialog outcome with the result of its privacy cleanup.
///
/// A completed dialog keeps its outcome: the user's selection or dismissal is
/// a fact the cleanup cannot undo, so a failed `ClearClientData` travels
/// beside it as `cleanup_hresult` for diagnostics instead of replacing it.
/// A failed dialog keeps its primary error and records the cleanup failure
/// on it.
fn merge_dialog_cleanup(
    outcome: Result<DialogOutcome, DialogError>,
    cleanup: Result<(), i32>,
) -> Result<(DialogOutcome, Option<i32>), DialogError> {
    match (outcome, cleanup) {
        (Ok(outcome), Ok(())) => Ok((outcome, None)),
        (Ok(outcome), Err(cleanup_hresult)) => Ok((outcome, Some(cleanup_hresult))),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_hresult)) => Err(error.with_cleanup_hresult(cleanup_hresult)),
    }
}

fn folder_dialog_options(existing: FILEOPENDIALOGOPTIONS) -> FILEOPENDIALOGOPTIONS {
    existing | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_DONTADDTORECENT
}

fn show_and_extract(
    dialog: &IFileDialog,
    owner: OwnerWindow,
) -> Result<DialogOutcome, DialogError> {
    // SAFETY: the HWND is a borrowed owner value supplied by the UI and is used
    // only for this synchronous call; the caller must keep it alive until the
    // terminal event. The dialog runs its native modal message loop on the STA.
    match unsafe { dialog.Show(Some(owner.as_hwnd())) } {
        Ok(()) => {}
        Err(error) if is_dialog_cancelled_hresult(error.code().0) => {
            return Ok(DialogOutcome::Cancelled);
        }
        Err(error) => {
            return Err(DialogError::from_hresult(DialogErrorStage::Show, error.code().0));
        }
    }

    // SAFETY: a successful Show permits retrieving the selected Shell item;
    // both COM interfaces stay and are released on this STA.
    let item = unsafe { dialog.GetResult() }
        .map_err(|error| DialogError::from_hresult(DialogErrorStage::GetResult, error.code().0))?;
    // SAFETY: `item` is live and SIGDN_FILESYSPATH requests an allocated,
    // NUL-terminated filesystem path. The pointer is wrapped immediately.
    let path = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|error| {
        DialogError::from_hresult(DialogErrorStage::GetDisplayName, error.code().0)
    })?;
    let path = CoTaskMemWide::new(path)?;
    Ok(DialogOutcome::Selected(PathBuf::from(path.to_os_string())))
}

fn is_dialog_cancelled_hresult(hresult: i32) -> bool {
    hresult == HRESULT::from_win32(ERROR_CANCELLED.0).0
}

struct CoTaskMemWide {
    value: PWSTR,
}

impl CoTaskMemWide {
    fn new(value: PWSTR) -> Result<Self, DialogError> {
        if value.is_null() {
            return Err(DialogError::from_hresult(
                DialogErrorStage::GetDisplayName,
                E_UNEXPECTED.0,
            ));
        }
        Ok(Self { value })
    }

    fn to_os_string(&self) -> OsString {
        // SAFETY: `IShellItem::GetDisplayName` returned this live,
        // NUL-terminated CoTaskMem allocation and the wrapper has not freed it.
        let units = unsafe { self.value.as_wide() };
        os_string_from_wide(units)
    }
}

fn os_string_from_wide(units: &[u16]) -> OsString {
    OsString::from_wide(units)
}

impl Drop for CoTaskMemWide {
    fn drop(&mut self) {
        // SAFETY: this is exactly the non-null allocation returned by
        // `GetDisplayName`; this wrapper is its sole owner.
        unsafe { CoTaskMemFree(Some(self.value.as_ptr().cast())) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, os::windows::ffi::OsStrExt, sync::atomic::AtomicBool};
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};

    fn owner() -> OwnerWindow {
        OwnerWindow::from_raw(1).expect("test HWND value is nonzero")
    }

    fn queued(id: u64) -> QueuedRequest {
        QueuedRequest {
            id: DialogRequestId(id),
            request: ShellRequest::PickFolder(FolderDialogRequest { owner: owner() }),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn configuration_rejects_unbounded_or_empty_capacity() {
        assert!(matches!(
            ShellServiceConfig { request_capacity: 0 }.validate(),
            Err(ShellServiceStartError::InvalidQueueCapacity { capacity: 0 })
        ));
        assert!(matches!(
            ShellServiceConfig { request_capacity: MAX_REQUEST_CAPACITY + 1 }.validate(),
            Err(ShellServiceStartError::InvalidQueueCapacity { .. })
        ));
        assert!(ShellServiceConfig::default().validate().is_ok());
    }

    #[test]
    fn queue_is_nonblocking_bounded_and_modal_is_serialized() {
        let (sender, receiver) = sync_channel(1);
        let modal = AtomicBool::new(false);

        assert_eq!(try_enqueue_request(&sender, &modal, queued(1)), Ok(()));
        assert_eq!(
            try_enqueue_request(&sender, &modal, queued(2)),
            Err(SubmitError::ModalOutstanding)
        );

        // Release the modal gate explicitly to isolate the queue-full branch.
        modal.store(false, Ordering::Release);
        assert_eq!(try_enqueue_request(&sender, &modal, queued(3)), Err(SubmitError::QueueFull));
        assert!(!modal.load(Ordering::Acquire));
        assert_eq!(receiver.try_recv().expect("first item remains queued").id.0, 1);
    }

    #[test]
    fn disconnected_queue_releases_modal_reservation() {
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        let modal = AtomicBool::new(false);

        assert_eq!(
            try_enqueue_request(&sender, &modal, queued(1)),
            Err(SubmitError::ServiceUnavailable)
        );
        assert!(!modal.load(Ordering::Acquire));
    }

    #[test]
    fn completed_modal_stays_outstanding_until_its_event_is_delivered() {
        let wake = Arc::new(WakeEvent::create().expect("test event can be created"));
        let shared = SharedState::new(wake);
        let (request_tx, _request_rx) = sync_channel(1);
        let (event_tx, event_rx) = channel();
        shared.modal_outstanding.store(true, Ordering::Release);

        send_completed_request(
            &event_tx,
            &shared,
            CompletedRequest {
                event: ShellEvent::Dialog(DialogEvent::Cancelled {
                    request_id: DialogRequestId(1),
                    cleanup_hresult: None,
                }),
                was_modal: true,
            },
        );

        assert_eq!(
            try_enqueue_request(&request_tx, &shared.modal_outstanding, queued(2)),
            Err(SubmitError::ModalOutstanding)
        );
        let completed = event_rx.try_recv().expect("completion is queued");
        assert_eq!(
            deliver_completed_request(&shared, completed),
            ShellEvent::Dialog(DialogEvent::Cancelled {
                request_id: DialogRequestId(1),
                cleanup_hresult: None
            })
        );
        assert!(!shared.modal_outstanding.load(Ordering::Acquire));
        assert_eq!(try_enqueue_request(&request_tx, &shared.modal_outstanding, queued(3)), Ok(()));
    }

    #[test]
    fn terminal_transition_drains_a_submit_already_inside_the_acceptance_gate() {
        let wake = Arc::new(WakeEvent::create().expect("test event can be created"));
        let shared = Arc::new(SharedState::new(wake));
        shared.lifecycle.store(STATE_RUNNING, Ordering::Release);
        shared.modal_outstanding.store(true, Ordering::Release);
        let (request_tx, request_rx) = sync_channel(1);
        let (event_tx, event_rx) = channel();

        let acceptance = shared.acceptance.lock().expect("test gate is healthy");
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            stop_accepting_and_fail_queued(
                &request_rx,
                &event_tx,
                &worker_shared,
                DialogError::from_hresult(DialogErrorStage::MessagePump, E_UNEXPECTED.0),
            );
        });

        // This send models a producer that passed the Running check while it
        // held the same gate. The terminal transition must wait, then drain it.
        request_tx.try_send(queued(9)).expect("accepted request fits");
        drop(acceptance);
        worker.join().expect("terminal transition completes");

        assert_eq!(shared.status(), ShellServiceStatus::Stopping);
        let completed = event_rx.try_recv().expect("accepted request receives a terminal event");
        assert!(matches!(
            deliver_completed_request(&shared, completed),
            ShellEvent::Dialog(DialogEvent::Failed { request_id: DialogRequestId(9), .. })
        ));
        assert_eq!(
            try_accept_request(&request_tx, &shared, queued(10)),
            Err(SubmitError::ServiceUnavailable)
        );
    }

    #[test]
    fn request_panic_produces_one_terminal_event_and_releases_modal_on_delivery() {
        let wake = Arc::new(WakeEvent::create().expect("test event can be created"));
        let shared = SharedState::new(wake);
        let (event_tx, event_rx) = channel();
        shared.modal_outstanding.store(true, Ordering::Release);

        let error = guard_request_execution(queued(11).meta(), &event_tx, &shared, || {
            panic!("injected Shell request panic");
        })
        .expect_err("the panic is converted to a typed terminal failure");

        assert_eq!(error, DialogError::worker_panicked());
        assert!(shared.modal_outstanding.load(Ordering::Acquire));
        let completed = event_rx.try_recv().expect("panic result is queued");
        assert_eq!(
            deliver_completed_request(&shared, completed),
            ShellEvent::Dialog(DialogEvent::Failed { request_id: DialogRequestId(11), error })
        );
        assert!(!shared.modal_outstanding.load(Ordering::Acquire));
        assert_eq!(event_rx.try_recv(), Err(MpscTryRecvError::Empty));
    }

    #[test]
    fn queued_shutdown_emits_plain_failed_event_and_releases_modal() {
        let wake = Arc::new(WakeEvent::create().expect("test event can be created"));
        let shared = SharedState::new(wake);
        let (request_tx, request_rx) = sync_channel(1);
        let (event_tx, event_rx) = channel();
        shared.modal_outstanding.store(true, Ordering::Release);
        request_tx.try_send(queued(7)).expect("test queue has capacity");

        fail_queued_requests(&request_rx, &event_tx, &shared, DialogError::service_shutdown());

        assert!(shared.modal_outstanding.load(Ordering::Acquire));
        let completed = event_rx.try_recv().expect("shutdown result remains queued");
        assert_eq!(
            deliver_completed_request(&shared, completed),
            ShellEvent::Dialog(DialogEvent::Failed {
                request_id: DialogRequestId(7),
                error: DialogError::service_shutdown(),
            })
        );
        assert!(!shared.modal_outstanding.load(Ordering::Acquire));
    }

    #[test]
    fn com_startup_classification_balances_all_success_codes() {
        assert_eq!(classify_com_initialization(S_OK.0), Ok(()));
        assert_eq!(classify_com_initialization(S_FALSE.0), Ok(()));
        assert_eq!(classify_com_initialization(RPC_E_CHANGED_MODE.0), Err(RPC_E_CHANGED_MODE.0));
    }

    #[test]
    fn options_preserve_existing_bits_and_add_exact_folder_contract() {
        let existing = FILEOPENDIALOGOPTIONS(0x4000_0000);
        let options = folder_dialog_options(existing);
        let required =
            FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_DONTADDTORECENT;

        assert!(options.contains(existing));
        assert!(options.contains(required));
        assert_eq!(options.0, existing.0 | required.0);
    }

    #[test]
    fn cancellation_is_only_the_documented_hresult() {
        let cancelled = HRESULT::from_win32(ERROR_CANCELLED.0).0;
        assert!(is_dialog_cancelled_hresult(cancelled));
        assert!(!is_dialog_cancelled_hresult(E_UNEXPECTED.0));
        assert!(!is_dialog_cancelled_hresult(S_OK.0));
    }

    #[test]
    fn cleanup_error_mapping_preserves_primary_hresult() {
        let primary = DialogError::from_hresult(DialogErrorStage::Show, E_UNEXPECTED.0);
        let cleanup_hresult = HRESULT::from_win32(5).0;
        assert_eq!(
            merge_dialog_cleanup(Err(primary), Err(cleanup_hresult)),
            Err(primary.with_cleanup_hresult(cleanup_hresult))
        );
        assert_eq!(
            merge_dialog_cleanup(Ok(DialogOutcome::Cancelled), Err(cleanup_hresult)),
            Ok((DialogOutcome::Cancelled, Some(cleanup_hresult))),
            "a dismissed dialog stays dismissed; the cleanup failure rides alongside"
        );
        assert_eq!(
            merge_dialog_cleanup(Ok(DialogOutcome::Cancelled), Ok(())),
            Ok((DialogOutcome::Cancelled, None))
        );
    }

    #[test]
    fn cleanup_guard_runs_once_on_finish_and_during_unwind() {
        let normal_calls = Cell::new(0_u8);
        let guard = CleanupGuard::new(|| {
            normal_calls.set(normal_calls.get() + 1);
            Ok(())
        });
        assert_eq!(
            guard.finish(Ok(DialogOutcome::Cancelled)),
            Ok((DialogOutcome::Cancelled, None))
        );
        assert_eq!(normal_calls.get(), 1);

        let unwind_calls = Cell::new(0_u8);
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _guard = CleanupGuard::new(|| {
                unwind_calls.set(unwind_calls.get() + 1);
                Ok(())
            });
            panic!("injected result-extraction panic");
        }));
        assert!(unwind.is_err());
        assert_eq!(unwind_calls.get(), 1);
    }

    #[test]
    fn selected_event_debug_output_redacts_the_native_path() {
        let event = DialogEvent::Selected {
            request_id: DialogRequestId(41),
            path: PathBuf::from(r"C:\private\customer-name"),
            cleanup_hresult: Some(HRESULT::from_win32(5).0),
        };

        let rendered = format!("{event:?}");
        assert!(rendered.contains("request_id"));
        assert!(rendered.contains("path_present: true"));
        assert!(rendered.contains("cleanup_failed: true"));
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("customer-name"));
    }

    #[test]
    fn owned_events_and_requests_are_send_without_com_pointers() {
        fn assert_send<T: Send>() {}
        assert_send::<ShellRequest>();
        assert_send::<DialogEvent>();
        assert_send::<ShellEvent>();
        assert_send::<DialogError>();
    }

    #[test]
    fn service_starts_pumps_and_shuts_down_without_showing_ui() {
        let mut service =
            ShellService::start_with_config(ShellServiceConfig { request_capacity: 1 })
                .expect("a fresh worker thread can initialize an STA");
        assert_eq!(service.status(), ShellServiceStatus::Running);
        assert_eq!(service.try_recv(), Ok(None));
        assert!(matches!(ShellService::start(), Err(ShellServiceStartError::AlreadyRunning)));

        service.shutdown().expect("worker joins cleanly");
        assert_eq!(service.status(), ShellServiceStatus::Stopped);
        assert_eq!(service.try_recv(), Err(TryReceiveError::ServiceStopped));
        service.shutdown().expect("shutdown is idempotent");
    }

    #[test]
    fn utf16_copy_helper_preserves_unpaired_surrogates() {
        let units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16];
        let value = os_string_from_wide(&units);
        assert_eq!(value.encode_wide().collect::<Vec<_>>(), units);
    }

    // Lifecycle tests below use private registries so injected stalled or
    // panicking workers never touch the real process singleton and can run in
    // parallel with each other.

    const STALL: Duration = Duration::from_millis(1_000);
    const RESTART_DEADLINE: Duration = Duration::from_secs(10);
    const FINISH_TIMEOUT: Duration = Duration::from_millis(100);

    fn private_registry() -> Arc<ShellInstanceRegistry> {
        Arc::new(ShellInstanceRegistry::new())
    }

    fn start_in(registry: &Arc<ShellInstanceRegistry>, behaviour: WorkerBehaviour) -> ShellService {
        ShellService::start_in(
            Arc::clone(registry),
            ShellServiceConfig { request_capacity: 1 },
            behaviour,
        )
        .expect("a fresh worker thread can initialize an STA")
    }

    fn start_error(
        registry: &Arc<ShellInstanceRegistry>,
    ) -> Result<ShellService, ShellServiceStartError> {
        ShellService::start_in(
            Arc::clone(registry),
            ShellServiceConfig { request_capacity: 1 },
            WorkerBehaviour::Native,
        )
    }

    /// Retries `start` until it succeeds and returns how long that took.
    fn wait_for_restart(registry: &Arc<ShellInstanceRegistry>) -> (ShellService, Duration) {
        let started = Instant::now();
        loop {
            match start_error(registry) {
                Ok(service) => return (service, started.elapsed()),
                Err(ShellServiceStartError::AlreadyRunning) => {}
                Err(error) => panic!("unexpected restart failure: {error}"),
            }
            assert!(
                started.elapsed() < RESTART_DEADLINE,
                "reaper did not release the claim of the stalled worker"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn request_shutdown_is_nonblocking_idempotent_and_closes_acceptance() {
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);

        let started = Instant::now();
        service.request_shutdown();
        service.request_shutdown();
        assert!(started.elapsed() < FINISH_TIMEOUT);
        assert!(matches!(
            service.status(),
            ShellServiceStatus::Stopping | ShellServiceStatus::Stopped
        ));
        assert_eq!(service.submit_folder(owner()), Err(SubmitError::ServiceUnavailable));

        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
        assert!(!registry.is_claimed());
        assert!(!registry.is_fail_closed());
    }

    #[test]
    fn finish_of_a_clean_worker_reports_exit_and_allows_restart() {
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);

        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
        assert!(!registry.is_claimed());

        let restarted = start_error(&registry).expect("claim was released by finish");
        assert_eq!(restarted.status(), ShellServiceStatus::Running);
        assert_eq!(
            restarted.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn finish_hands_a_stalled_worker_to_the_reaper_within_the_deadline() {
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::StallAfterShutdown(STALL));

        let requested = Instant::now();
        let outcome = service.finish(FINISH_TIMEOUT);
        let elapsed = requested.elapsed();
        assert_eq!(outcome, ShellServiceFinish::TimedOutHandedToReaper);
        assert!(elapsed < FINISH_TIMEOUT * 2, "finish took {elapsed:?}");

        // The stalled STA still owns the singleton claim.
        assert!(registry.is_claimed());
        assert!(!registry.is_fail_closed());
        assert!(matches!(start_error(&registry), Err(ShellServiceStartError::AlreadyRunning)));

        let (restarted, _) = wait_for_restart(&registry);
        assert!(
            requested.elapsed() >= STALL,
            "restart succeeded before the stalled worker could have exited"
        );
        assert_eq!(
            restarted.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn drop_of_a_stalled_service_returns_promptly_and_restart_waits_for_real_exit() {
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::StallAfterShutdown(STALL));

        let dropped = Instant::now();
        drop(service);
        let elapsed = dropped.elapsed();
        assert!(elapsed < FINISH_TIMEOUT * 2, "drop took {elapsed:?}");

        assert!(registry.is_claimed());
        assert!(matches!(start_error(&registry), Err(ShellServiceStartError::AlreadyRunning)));

        let (restarted, _) = wait_for_restart(&registry);
        assert!(dropped.elapsed() >= STALL);
        assert!(!registry.is_fail_closed());
        drop(restarted);
    }

    #[test]
    fn panicking_worker_reports_the_panic_and_releases_the_claim() {
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::PanicAfterShutdown);

        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: true }
        );
        assert!(!registry.is_claimed());
        assert!(!registry.is_fail_closed());

        let restarted = start_error(&registry).expect("panic releases the claim");
        assert_eq!(
            restarted.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn legacy_shutdown_repeats_a_recorded_worker_panic() {
        let registry = private_registry();
        let mut service = start_in(&registry, WorkerBehaviour::PanicAfterShutdown);

        assert_eq!(service.shutdown(), Err(ShellServiceShutdownError::WorkerPanicked));
        assert_eq!(service.shutdown(), Err(ShellServiceShutdownError::WorkerPanicked));
        assert!(!registry.is_claimed());
    }

    #[test]
    fn saturated_reaper_fails_closed_and_retains_the_claim() {
        let registry = private_registry();
        let (reaper, _receiver_kept_alive) = ShellReaper::saturated_for_test();
        assert!(registry.reaper.set(Some(reaper)).is_ok());
        let stall = Duration::from_millis(300);
        let service = start_in(&registry, WorkerBehaviour::StallAfterShutdown(stall));

        let requested = Instant::now();
        let outcome = service.finish(FINISH_TIMEOUT);
        assert!(requested.elapsed() < FINISH_TIMEOUT * 2);
        assert_eq!(outcome, ShellServiceFinish::TimedOutReaperUnavailable);

        assert!(registry.is_fail_closed());
        assert!(registry.is_claimed());
        assert!(matches!(start_error(&registry), Err(ShellServiceStartError::AlreadyRunning)));

        // Even after the detached worker has certainly exited, the lifecycle
        // stays closed: nobody joined that thread, so nobody may reuse the STA.
        thread::sleep(stall * 2);
        assert!(registry.is_claimed());
        assert!(matches!(start_error(&registry), Err(ShellServiceStartError::AlreadyRunning)));
    }

    #[test]
    fn disconnected_reaper_returns_the_payload_without_blocking() {
        let (reaper, receiver) = ShellReaper::saturated_for_test();
        drop(receiver);
        let registry = private_registry();
        let claim = ServiceInstanceClaim::acquire(&registry).expect("fresh registry");

        let started = Instant::now();
        let rejected = reaper
            .try_submit(ReapRequest { worker: thread::spawn(|| {}), claim })
            .expect_err("a dead reaper cannot accept ownership");
        assert!(started.elapsed() < FINISH_TIMEOUT);
        assert!(registry.is_claimed());
        drop(rejected);
        assert!(!registry.is_claimed(), "a healthy registry releases a dropped claim");
    }

    #[test]
    fn reaper_joins_the_worker_before_releasing_its_claim() {
        let reaper = ShellReaper::start().expect("test reaper thread starts");
        let registry = private_registry();
        let claim = ServiceInstanceClaim::acquire(&registry).expect("fresh registry");
        let hold = Duration::from_millis(200);
        let worker = thread::spawn(move || thread::sleep(hold));

        let submitted = Instant::now();
        reaper.try_submit(ReapRequest { worker, claim }).ok().expect("reaper has capacity");
        assert!(registry.is_claimed());

        while registry.is_claimed() {
            assert!(submitted.elapsed() < RESTART_DEADLINE, "reaper never released the claim");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(submitted.elapsed() >= hold, "claim released before the worker exited");
    }

    #[test]
    fn precreated_reaper_is_shared_by_every_start_in_a_registry() {
        let registry = private_registry();
        assert!(registry.reaper().is_none());
        let first = start_in(&registry, WorkerBehaviour::Native);
        let first_reaper: *const ShellReaper =
            registry.reaper().expect("start precreates the reaper");
        assert_eq!(
            first.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
        let second = start_in(&registry, WorkerBehaviour::Native);
        let second_reaper: *const ShellReaper = registry.reaper().expect("reaper persists");
        assert!(std::ptr::eq(first_reaper, second_reaper));
        drop(second);
    }

    // Shell action tests below. Everything that runs by default is
    // non-destructive: it validates identity, exercises cancellation, and
    // maps terminal events. Fixture-mutating tests are gated behind
    // `DISKPIE_DESTRUCTIVE_FIXTURES=1` and act only on files they created
    // inside their own temporary directory.

    use super::super::shell_actions::read_file_identity;
    use diskpie_app::actions::{
        ActionOutcome, ActionPurpose, ConfirmationFlow, DeleteRequest, DestructiveOutcome,
        EmptyBinOutcome, IdentityAssurance, ItemEffect, PostActionObligation, RecycleBinScope,
        StronglyConfirmed, TargetChangeReason, TargetValidator,
    };
    use diskpie_core::{
        EntryKind, GenerationId, NodeId, NodeSpec, OwnMetrics, ReparseKind, TreeBuilder,
        TreeSnapshot,
    };
    use std::{ffi::OsStr, fs, path::Path};

    const DESTRUCTIVE_FIXTURES_ENV: &str = "DISKPIE_DESTRUCTIVE_FIXTURES";
    const EVENT_DEADLINE: Duration = Duration::from_secs(20);
    static ACTION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ActionFixture(PathBuf);

    impl ActionFixture {
        fn new(label: &str) -> Self {
            let sequence = ACTION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diskpie-shell-service-{label}-{}-{sequence}", std::process::id()));
            std::fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ActionFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn destructive_fixtures_enabled(test: &str) -> bool {
        let enabled = std::env::var_os(DESTRUCTIVE_FIXTURES_ENV).is_some_and(|value| value == "1");
        if !enabled {
            println!(
                "skipping {test}: set {DESTRUCTIVE_FIXTURES_ENV}=1 to run fixture-mutating Shell tests"
            );
        }
        enabled
    }

    /// One real child under a scan root at `root`, exactly as the scanner would record it.
    fn child_snapshot(
        root: &Path,
        child: &OsStr,
        kind: EntryKind,
        identity: Option<diskpie_core::FileIdentity>,
    ) -> (TreeSnapshot, NodeId) {
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let root_node = builder.add_root(NodeSpec::root(root.as_os_str())).expect("root");
        let spec = NodeSpec::new(child, kind, OwnMetrics::ZERO_BY_POLICY);
        let spec = match identity {
            Some(identity) => spec.with_file_identity(identity),
            None => spec,
        };
        let child_node = builder.add_child(root_node, spec).expect("child");
        (builder.freeze().expect("snapshot"), child_node)
    }

    fn validated_target(
        snapshot: &TreeSnapshot,
        node: NodeId,
    ) -> diskpie_app::actions::FilesystemTarget {
        let executable = PathBuf::from(r"C:\zqx\bin\diskpie.exe");
        TargetValidator::new(snapshot, Some(&executable))
            .validate(node, GenerationId::new(1), false, ActionPurpose::Destructive)
            .expect("temporary fixture is a valid destructive target")
    }

    fn confirmed_recycle(
        snapshot: &TreeSnapshot,
        node: NodeId,
    ) -> diskpie_app::actions::Confirmed<diskpie_app::actions::RecycleRequest> {
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(validated_target(snapshot, node)).expect("begin");
        flow.accept_review().expect("accept");
        flow.take_recycle().expect("capability")
    }

    fn strongly_confirmed_delete(
        snapshot: &TreeSnapshot,
        node: NodeId,
    ) -> StronglyConfirmed<DeleteRequest> {
        let mut flow = ConfirmationFlow::new();
        flow.begin_delete(validated_target(snapshot, node)).expect("begin");
        flow.accept_review().expect("accept");
        flow.set_typed_word("DELETE").expect("type");
        flow.confirm_word().expect("confirm");
        flow.take_delete().expect("capability")
    }

    fn wait_for_event(service: &ShellService) -> ShellEvent {
        let started = Instant::now();
        loop {
            if let Some(event) = service.try_recv().expect("service alive") {
                return event;
            }
            assert!(started.elapsed() < EVENT_DEADLINE, "no terminal event within the deadline");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Runs by default in permanent mode on purpose: if late validation ever
    /// regressed, only this test's own temporary fixture could be destroyed,
    /// never an item added to the developer's real Recycle Bin.
    #[test]
    fn swapped_target_reports_target_changed_through_the_live_service_without_mutation() {
        let fixture = ActionFixture::new("swap");
        let file = fixture.path().join("confirmed.bin");
        fs::write(&file, b"original").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");
        let (snapshot, node) = child_snapshot(
            fixture.path(),
            OsStr::new("confirmed.bin"),
            EntryKind::File,
            Some(identity),
        );
        let capability = strongly_confirmed_delete(&snapshot, node);
        assert_eq!(capability.request().target().path(), file.as_path());

        // The confirmation is now stale: the path is reused by a different object.
        fs::remove_file(&file).expect("remove fixture");
        fs::write(&file, b"swapped-in").expect("recreate fixture");

        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);
        let (request, obligation) = ShellRequest::delete_permanently(owner(), capability);
        let id = service.submit(request).expect("accepted");
        assert_eq!(service.submit_folder(owner()), Err(SubmitError::ModalOutstanding));

        let event = wait_for_event(&service);
        assert_eq!(
            event,
            ShellEvent::DeletePermanently {
                request_id: id,
                outcome: DestructiveOutcome::TargetChanged {
                    reason: TargetChangeReason::IdentityMismatch
                },
            }
        );
        assert_eq!(fs::read(&file).expect("fixture intact"), b"swapped-in");
        assert!(!service.cancel(id), "a delivered request is no longer outstanding");

        let report =
            obligation.settle(ActionOutcome::Filesystem(DestructiveOutcome::TargetChanged {
                reason: TargetChangeReason::IdentityMismatch,
            }));
        assert!(matches!(
            report.obligation,
            PostActionObligation::RescanParent { parent, .. } if parent == NodeId::from_raw(0)
        ));
        assert!(!report.requires_halt());
        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn a_payload_rewrapped_in_the_other_destructive_variant_is_refused_unrun() {
        let fixture = ActionFixture::new("rewrap");
        let file = fixture.path().join("keep.bin");
        fs::write(&file, b"keep").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");
        let (snapshot, node) =
            child_snapshot(fixture.path(), OsStr::new("keep.bin"), EntryKind::File, Some(identity));

        let (request, _obligation) =
            ShellRequest::delete_permanently(owner(), strongly_confirmed_delete(&snapshot, node));
        let ShellRequest::DeletePermanently(payload) = request else {
            panic!("unexpected variant {request:?}");
        };
        assert_eq!(payload.mode(), DeleteMode::Permanent);
        let queued = QueuedRequest {
            id: DialogRequestId(7),
            request: ShellRequest::Recycle(payload),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(
            execute_request(queued),
            ShellEvent::Recycle {
                request_id: DialogRequestId(7),
                outcome: DestructiveOutcome::Failed {
                    stage: FailureStage::RequestMismatch,
                    code: None
                },
            }
        );

        let (request, _obligation) =
            ShellRequest::recycle(owner(), confirmed_recycle(&snapshot, node));
        let ShellRequest::Recycle(payload) = request else {
            panic!("unexpected variant {request:?}");
        };
        let queued = QueuedRequest {
            id: DialogRequestId(8),
            request: ShellRequest::DeletePermanently(payload),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(
            execute_request(queued),
            ShellEvent::DeletePermanently {
                request_id: DialogRequestId(8),
                outcome: DestructiveOutcome::Failed {
                    stage: FailureStage::RequestMismatch,
                    code: None
                },
            }
        );
        assert_eq!(fs::read(&file).expect("fixture intact"), b"keep");
    }

    #[test]
    fn precancelled_requests_never_reach_the_shell() {
        let fixture = ActionFixture::new("precancel");
        let file = fixture.path().join("keep.bin");
        fs::write(&file, b"keep").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");
        let (snapshot, node) =
            child_snapshot(fixture.path(), OsStr::new("keep.bin"), EntryKind::File, Some(identity));
        let (request, _obligation) =
            ShellRequest::delete_permanently(owner(), strongly_confirmed_delete(&snapshot, node));

        let queued = QueuedRequest {
            id: DialogRequestId(5),
            request,
            cancel: Arc::new(AtomicBool::new(true)),
        };
        assert_eq!(
            execute_request(queued),
            ShellEvent::DeletePermanently {
                request_id: DialogRequestId(5),
                outcome: DestructiveOutcome::CancelledBeforeMutation,
            }
        );
        assert_eq!(fs::read(&file).expect("fixture intact"), b"keep");

        let bin = ShellRequest::query_recycle_bin(RecycleBinScope::AllDrives);
        let queued = QueuedRequest {
            id: DialogRequestId(6),
            request: bin,
            cancel: Arc::new(AtomicBool::new(true)),
        };
        assert_eq!(
            execute_request(queued),
            ShellEvent::RecycleBinQuery {
                request_id: DialogRequestId(6),
                outcome: RecycleBinQueryOutcome::NotRun(DispatchStage::CancelledBeforeDispatch),
            }
        );
    }

    #[test]
    fn terminal_event_mapping_covers_every_request_kind() {
        let kinds = [
            ShellRequestKind::PickFolder,
            ShellRequestKind::SaveExport,
            ShellRequestKind::Open,
            ShellRequestKind::Reveal,
            ShellRequestKind::InstalledApps,
            ShellRequestKind::QueryRecycleBin,
            ShellRequestKind::Recycle,
            ShellRequestKind::DeletePermanently,
            ShellRequestKind::EmptyRecycleBin,
        ];
        for kind in kinds {
            let meta = RequestMeta { id: DialogRequestId(3), kind, was_modal: false };
            let cancelled = cancelled_before_start_event(meta);
            assert_eq!(cancelled.request_id(), DialogRequestId(3));
            let shutdown = failed_request_event(meta, DialogError::service_shutdown());
            let panicked = failed_request_event(meta, DialogError::worker_panicked());
            match kind {
                ShellRequestKind::SaveExport => {
                    assert!(matches!(
                        cancelled,
                        ShellEvent::SaveExport(DialogEvent::Cancelled { .. })
                    ));
                    assert!(matches!(shutdown, ShellEvent::SaveExport(DialogEvent::Failed { .. })));
                    assert!(matches!(panicked, ShellEvent::SaveExport(DialogEvent::Failed { .. })));
                    assert!(ShellRequest::save_export(owner()).is_modal());
                }
                ShellRequestKind::PickFolder => {
                    assert!(matches!(cancelled, ShellEvent::Dialog(DialogEvent::Cancelled { .. })));
                    assert!(matches!(shutdown, ShellEvent::Dialog(DialogEvent::Failed { .. })));
                }
                ShellRequestKind::Open
                | ShellRequestKind::Reveal
                | ShellRequestKind::InstalledApps => {
                    for (event, stage) in [
                        (cancelled, DispatchStage::CancelledBeforeDispatch),
                        (shutdown, DispatchStage::ServiceShutdown),
                        (panicked, DispatchStage::WorkerPanicked),
                    ] {
                        let outcome = match event {
                            ShellEvent::Open { outcome, .. }
                            | ShellEvent::Reveal { outcome, .. }
                            | ShellEvent::InstalledApps { outcome, .. } => outcome,
                            other => panic!("unexpected event {other:?}"),
                        };
                        assert_eq!(outcome, DispatchOutcome::Failed { stage, hresult: None });
                    }
                }
                ShellRequestKind::QueryRecycleBin => {
                    assert!(matches!(
                        shutdown,
                        ShellEvent::RecycleBinQuery {
                            outcome: RecycleBinQueryOutcome::NotRun(DispatchStage::ServiceShutdown),
                            ..
                        }
                    ));
                }
                ShellRequestKind::Recycle | ShellRequestKind::DeletePermanently => {
                    let outcome = |event: ShellEvent| match event {
                        ShellEvent::Recycle { outcome, .. }
                        | ShellEvent::DeletePermanently { outcome, .. } => outcome,
                        other => panic!("unexpected event {other:?}"),
                    };
                    assert_eq!(outcome(cancelled), DestructiveOutcome::CancelledBeforeMutation);
                    assert_eq!(outcome(shutdown), DestructiveOutcome::CancelledBeforeMutation);
                    assert_eq!(
                        outcome(panicked),
                        DestructiveOutcome::UnknownMayHaveMutated {
                            stage: diskpie_app::actions::FailureStage::WorkerPanicked,
                            code: None
                        }
                    );
                }
                ShellRequestKind::EmptyRecycleBin => {
                    assert!(matches!(
                        cancelled,
                        ShellEvent::EmptyRecycleBin {
                            outcome: EmptyBinOutcome::CancelledBeforeMutation,
                            ..
                        }
                    ));
                    assert!(matches!(
                        panicked,
                        ShellEvent::EmptyRecycleBin {
                            outcome: EmptyBinOutcome::UnknownMayHaveMutated { .. },
                            ..
                        }
                    ));
                }
            }
        }
    }

    #[test]
    fn destructive_requests_are_serialized_and_redact_paths() {
        let fixture = ActionFixture::new("redact");
        let file = fixture.path().join("secret-name.bin");
        fs::write(&file, b"x").expect("write fixture");
        let (snapshot, node) =
            child_snapshot(fixture.path(), OsStr::new("secret-name.bin"), EntryKind::File, None);
        let (request, _obligation) =
            ShellRequest::recycle(owner(), confirmed_recycle(&snapshot, node));
        assert!(request.is_modal());
        assert_eq!(request.kind(), ShellRequestKind::Recycle);
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("secret-name"), "{rendered}");
        assert!(rendered.contains("path_present: true"));
        drop(request);
        assert!(file.exists(), "building a request never touches the filesystem");
    }

    #[test]
    fn cancel_registry_tracks_outstanding_requests_only() {
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);
        assert!(!service.cancel(DialogRequestId(999)));
        let id = service
            .submit(ShellRequest::query_recycle_bin(RecycleBinScope::AllDrives))
            .expect("accepted");
        // Cancellation reaches the flag whether or not the STA already ran it.
        let outstanding = service.cancel(id);
        let event = wait_for_event(&service);
        assert!(outstanding);
        match event {
            ShellEvent::RecycleBinQuery { request_id, outcome } => {
                assert_eq!(request_id, id);
                assert!(matches!(
                    outcome,
                    RecycleBinQueryOutcome::Estimated(_)
                        | RecycleBinQueryOutcome::Failed { .. }
                        | RecycleBinQueryOutcome::NotRun(DispatchStage::CancelledBeforeDispatch)
                ));
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(!service.cancel(id), "delivery releases the cancellation entry");
        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn gated_recycle_of_a_temp_file_produces_recycle_bin_evidence() {
        if !destructive_fixtures_enabled(
            "gated_recycle_of_a_temp_file_produces_recycle_bin_evidence",
        ) {
            return;
        }
        let fixture = ActionFixture::new("recycle");
        let file = fixture.path().join("recycle-me.bin");
        fs::write(&file, b"recycle").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");
        let (snapshot, node) = child_snapshot(
            fixture.path(),
            OsStr::new("recycle-me.bin"),
            EntryKind::File,
            Some(identity),
        );
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);
        let (request, _obligation) =
            ShellRequest::recycle(owner(), confirmed_recycle(&snapshot, node));
        let id = service.submit(request).expect("accepted");

        let event = wait_for_event(&service);
        let ShellEvent::Recycle { request_id, outcome } = event else {
            panic!("unexpected event {event:?}");
        };
        assert_eq!(request_id, id);
        match outcome {
            DestructiveOutcome::Completed { items, assurance } => {
                assert_eq!(assurance, IdentityAssurance::Exact);
                assert_eq!(items.len(), 1);
                assert!(matches!(items[0].effect, ItemEffect::Recycled { .. }), "{items:?}");
                assert!(!file.exists(), "the fixture moved to the Recycle Bin");
            }
            other => panic!("recycle did not complete with Recycle Bin evidence: {other:?}"),
        }
        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    /// Open question recorded in ADR 0008: whether the copy engine reports a
    /// recycled directory as one `PostDeleteItem` with a Recycle Bin item, or
    /// per descendant. The classifier fails closed on a success callback with
    /// no new item, so this fixture is what proves or disproves that.
    #[test]
    fn gated_recycle_of_a_temp_directory_produces_recycle_bin_evidence() {
        if !destructive_fixtures_enabled(
            "gated_recycle_of_a_temp_directory_produces_recycle_bin_evidence",
        ) {
            return;
        }
        let fixture = ActionFixture::new("recycle-dir");
        let directory = fixture.path().join("recycle-me");
        fs::create_dir(&directory).expect("create directory fixture");
        fs::write(directory.join("child.bin"), b"child").expect("write child");
        fs::create_dir(directory.join("nested")).expect("create nested");
        fs::write(directory.join("nested").join("leaf.bin"), b"leaf").expect("write leaf");
        let (snapshot, node) =
            child_snapshot(fixture.path(), OsStr::new("recycle-me"), EntryKind::Directory, None);
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);
        let (request, _obligation) =
            ShellRequest::recycle(owner(), confirmed_recycle(&snapshot, node));
        let id = service.submit(request).expect("accepted");

        let event = wait_for_event(&service);
        let ShellEvent::Recycle { request_id, outcome } = event else {
            panic!("unexpected event {event:?}");
        };
        assert_eq!(request_id, id);
        println!("directory recycle evidence: {outcome:?}");
        match outcome {
            DestructiveOutcome::Completed { items, assurance } => {
                assert_eq!(assurance, IdentityAssurance::KindOnly);
                assert!(!items.is_empty());
                assert!(
                    items.iter().all(|item| matches!(item.effect, ItemEffect::Recycled { .. })),
                    "{items:?}"
                );
                assert!(!directory.exists(), "the directory moved to the Recycle Bin");
            }
            other => {
                panic!("directory recycle did not complete with Recycle Bin evidence: {other:?}")
            }
        }
        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn gated_permanent_delete_of_a_temp_file_completes_without_recycle_evidence() {
        if !destructive_fixtures_enabled(
            "gated_permanent_delete_of_a_temp_file_completes_without_recycle_evidence",
        ) {
            return;
        }
        let fixture = ActionFixture::new("delete");
        let file = fixture.path().join("delete-me.bin");
        fs::write(&file, b"delete").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");
        let (snapshot, node) = child_snapshot(
            fixture.path(),
            OsStr::new("delete-me.bin"),
            EntryKind::File,
            Some(identity),
        );
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);
        let (request, _obligation) =
            ShellRequest::delete_permanently(owner(), strongly_confirmed_delete(&snapshot, node));
        let id = service.submit(request).expect("accepted");

        let event = wait_for_event(&service);
        let ShellEvent::DeletePermanently { request_id, outcome } = event else {
            panic!("unexpected event {event:?}");
        };
        assert_eq!(request_id, id);
        // The copy engine reports a success HRESULT such as
        // COPYENGINE_S_DONT_PROCESS_CHILDREN (0x00270008), not necessarily S_OK.
        let DestructiveOutcome::Completed { items, assurance } = outcome else {
            panic!("permanent deletion did not complete: {outcome:?}");
        };
        assert_eq!(assurance, IdentityAssurance::Exact);
        assert_eq!(items.len(), 1);
        assert!(items[0].code >= 0, "{items:?}");
        assert_eq!(items[0].effect, ItemEffect::PermanentlyDeleted);
        assert!(!file.exists());
        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }

    #[test]
    fn gated_deleting_a_directory_link_leaves_the_external_sentinel_intact() {
        if !destructive_fixtures_enabled(
            "gated_deleting_a_directory_link_leaves_the_external_sentinel_intact",
        ) {
            return;
        }
        let outside = ActionFixture::new("sentinel");
        let sentinel = outside.path().join("sentinel.txt");
        fs::write(&sentinel, b"survive").expect("write sentinel");
        let fixture = ActionFixture::new("link");
        let link = fixture.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
            println!("skipping: directory symlink creation is unavailable ({})", error.kind());
            return;
        }
        let (snapshot, node) = child_snapshot(
            fixture.path(),
            OsStr::new("link"),
            EntryKind::ReparsePoint(ReparseKind::Directory),
            None,
        );
        let registry = private_registry();
        let service = start_in(&registry, WorkerBehaviour::Native);
        let (request, _obligation) =
            ShellRequest::delete_permanently(owner(), strongly_confirmed_delete(&snapshot, node));
        let id = service.submit(request).expect("accepted");

        let event = wait_for_event(&service);
        let ShellEvent::DeletePermanently { request_id, outcome } = event else {
            panic!("unexpected event {event:?}");
        };
        assert_eq!(request_id, id);
        assert!(
            matches!(
                outcome,
                DestructiveOutcome::Completed { assurance: IdentityAssurance::KindOnly, .. }
            ),
            "{outcome:?}"
        );
        assert_eq!(fs::read(&sentinel).expect("sentinel survives"), b"survive");
        assert!(fs::symlink_metadata(&link).is_err(), "the link object itself was removed");
        assert_eq!(
            service.finish(Duration::from_secs(5)),
            ShellServiceFinish::Exited { panicked: false }
        );
    }
}
