//! Dedicated Windows Shell single-threaded-apartment service.
//!
//! The service owns one message-pumping STA and accepts bounded, nonblocking
//! requests from the UI. COM interfaces, Shell items, native allocations, and
//! HWND wrappers are constructed and destroyed on that thread. Only owned,
//! plain-data requests and events cross the channel boundary.
//!
//! This first slice implements the folder picker. Future Shell actions can be
//! added to [`ShellRequest`] while retaining the apartment, queue, message-pump,
//! and shutdown machinery in this module.

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
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{
            Receiver, SyncSender, TryRecvError as MpscTryRecvError, TrySendError, channel,
            sync_channel,
        },
    },
    thread::{self, JoinHandle},
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
            Threading::{CreateEventW, SetEvent},
        },
        UI::{
            Shell::{
                FILEOPENDIALOGOPTIONS, FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
                FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
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

const WORKER_THREAD_NAME: &str = "diskpie-shell-sta";
const WAIT_HEALTH_INTERVAL_MS: u32 = 250;

const STATE_STARTING: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_STOPPING: u8 = 2;
const STATE_STOPPED: u8 = 3;

static SERVICE_INSTANCE_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Correlates a request with exactly one terminal dialog event.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DialogRequestId(u64);

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

/// Work accepted by the dedicated Shell STA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellRequest {
    /// Select one existing filesystem folder or drive root.
    PickFolder(FolderDialogRequest),
}

impl ShellRequest {
    const fn is_modal(self) -> bool {
        match self {
            Self::PickFolder(_) => true,
        }
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DialogEvent {
    Selected { request_id: DialogRequestId, path: PathBuf },
    Cancelled { request_id: DialogRequestId },
    Failed { request_id: DialogRequestId, error: DialogError },
}

impl DialogEvent {
    /// Returns the request ID shared by every terminal outcome.
    pub const fn request_id(&self) -> DialogRequestId {
        match self {
            Self::Selected { request_id, .. }
            | Self::Cancelled { request_id }
            | Self::Failed { request_id, .. } => *request_id,
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
    InvalidQueueCapacity { capacity: usize },
    AlreadyRunning,
    CreateWakeEvent { hresult: i32 },
    SpawnThread { os_code: Option<i32> },
    InitializeCom { hresult: i32 },
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

#[derive(Clone, Copy)]
struct QueuedRequest {
    id: DialogRequestId,
    request: ShellRequest,
}

#[derive(Debug, Eq, PartialEq)]
struct CompletedRequest {
    event: DialogEvent,
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

struct ServiceInstanceClaim;

impl ServiceInstanceClaim {
    fn acquire() -> Result<Self, ShellServiceStartError> {
        SERVICE_INSTANCE_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| ShellServiceStartError::AlreadyRunning)
    }
}

impl Drop for ServiceInstanceClaim {
    fn drop(&mut self) {
        SERVICE_INSTANCE_CLAIMED.store(false, Ordering::Release);
    }
}

impl Drop for WorkerStateGuard {
    fn drop(&mut self) {
        self.shared.lifecycle.store(STATE_STOPPED, Ordering::Release);
    }
}

/// Handle to the one process-owned, message-pumping Shell STA.
///
/// `submit` and `try_recv` never block. `shutdown` joins the worker and can
/// therefore wait for a currently visible native modal dialog to return; the
/// application's owner-window teardown should close that dialog during exit.
pub struct ShellService {
    request_tx: SyncSender<QueuedRequest>,
    event_rx: Receiver<CompletedRequest>,
    shared: Arc<SharedState>,
    next_request_id: AtomicU64,
    worker: Option<JoinHandle<()>>,
    instance_claim: Option<ServiceInstanceClaim>,
}

impl ShellService {
    /// Starts the service with [`ShellServiceConfig::default`].
    pub fn start() -> Result<Self, ShellServiceStartError> {
        Self::start_with_config(ShellServiceConfig::default())
    }

    /// Starts and handshakes with a dedicated STA worker.
    pub fn start_with_config(config: ShellServiceConfig) -> Result<Self, ShellServiceStartError> {
        let config = config.validate()?;
        let instance_claim = ServiceInstanceClaim::acquire()?;
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
            .spawn(move || worker_entry(request_rx, event_tx, startup_tx, worker_shared))
            .map_err(|error| ShellServiceStartError::SpawnThread {
                os_code: error.raw_os_error(),
            })?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                request_tx,
                event_rx,
                shared,
                next_request_id: AtomicU64::new(1),
                worker: Some(worker),
                instance_claim: Some(instance_claim),
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
        try_accept_request(&self.request_tx, &self.shared, QueuedRequest { id, request })?;

        // Signalling is an optimization backed by a short health timeout in
        // the message-aware wait. Once `try_send` succeeds the request remains
        // accepted even if an anomalous SetEvent call fails.
        let _ = self.shared.wake.signal();
        Ok(id)
    }

    /// Convenience wrapper for the currently implemented request kind.
    pub fn submit_folder(&self, owner: OwnerWindow) -> Result<DialogRequestId, SubmitError> {
        self.submit(ShellRequest::PickFolder(FolderDialogRequest { owner }))
    }

    /// Receives at most one terminal event without blocking the caller.
    ///
    /// A modal request remains outstanding until this method delivers its
    /// terminal event, bounding unread modal results as well as visible dialogs.
    pub fn try_recv(&self) -> Result<Option<DialogEvent>, TryReceiveError> {
        match self.event_rx.try_recv() {
            Ok(completed) => Ok(Some(deliver_completed_request(&self.shared, completed))),
            Err(MpscTryRecvError::Empty) => Ok(None),
            Err(MpscTryRecvError::Disconnected) => Err(TryReceiveError::ServiceStopped),
        }
    }

    /// Returns the service lifecycle without waiting.
    pub fn status(&self) -> ShellServiceStatus {
        self.shared.status()
    }

    /// Requests stop, wakes the message-aware wait, and joins the STA.
    ///
    /// The method is idempotent. Queued requests that have not started receive
    /// `DialogEvent::Failed` with `ServiceShutdown` before the worker exits.
    pub fn shutdown(&mut self) -> Result<(), ShellServiceShutdownError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), ShellServiceShutdownError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };

        {
            let _acceptance =
                self.shared.acceptance.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = self.shared.lifecycle.compare_exchange(
                STATE_RUNNING,
                STATE_STOPPING,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.shared.shutdown_requested.store(true, Ordering::Release);
        }
        let _ = self.shared.wake.signal();

        let joined = worker.join().map_err(|_| ShellServiceShutdownError::WorkerPanicked);
        self.instance_claim.take();
        joined
    }
}

impl Drop for ShellService {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
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
    let outcome = catch_unwind(AssertUnwindSafe(|| worker_loop(&request_rx, &event_tx, &shared)));
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
) -> DialogError {
    loop {
        if shared.shutdown_requested.load(Ordering::Acquire) {
            return DialogError::service_shutdown();
        }

        match request_rx.try_recv() {
            Ok(queued) => {
                if shared.shutdown_requested.load(Ordering::Acquire) {
                    fail_request(queued, event_tx, shared, DialogError::service_shutdown());
                    return DialogError::service_shutdown();
                }
                if let Err(error) = guard_request_execution(queued, event_tx, shared, || {
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
    let was_modal = queued.request.is_modal();
    let event = match queued.request {
        ShellRequest::PickFolder(request) => match show_folder_dialog(request.owner) {
            Ok(DialogOutcome::Selected(path)) => {
                DialogEvent::Selected { request_id: queued.id, path }
            }
            Ok(DialogOutcome::Cancelled) => DialogEvent::Cancelled { request_id: queued.id },
            Err(error) => DialogEvent::Failed { request_id: queued.id, error },
        },
    };

    send_completed_request(event_tx, shared, CompletedRequest { event, was_modal });
}

fn guard_request_execution<F>(
    queued: QueuedRequest,
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
    fail_request(queued, event_tx, shared, error);
    Err(error)
}

fn fail_request(
    queued: QueuedRequest,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    error: DialogError,
) {
    let completed = CompletedRequest {
        event: DialogEvent::Failed { request_id: queued.id, error },
        was_modal: queued.request.is_modal(),
    };
    send_completed_request(event_tx, shared, completed);
}

fn fail_queued_requests(
    request_rx: &Receiver<QueuedRequest>,
    event_tx: &std::sync::mpsc::Sender<CompletedRequest>,
    shared: &SharedState,
    error: DialogError,
) {
    while let Ok(queued) = request_rx.try_recv() {
        fail_request(queued, event_tx, shared, error);
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

fn deliver_completed_request(shared: &SharedState, completed: CompletedRequest) -> DialogEvent {
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

fn show_folder_dialog(owner: OwnerWindow) -> Result<DialogOutcome, DialogError> {
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

    let outcome = show_and_extract(&dialog, owner);
    // SAFETY: cleanup occurs on the owning STA before releasing `dialog` and is
    // attempted after every Show/result path once the client GUID was set.
    let cleanup = unsafe { dialog.ClearClientData() };

    merge_dialog_cleanup(outcome, cleanup.map_err(|error| error.code().0))
}

fn merge_dialog_cleanup(
    outcome: Result<DialogOutcome, DialogError>,
    cleanup: Result<(), i32>,
) -> Result<DialogOutcome, DialogError> {
    match (outcome, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(cleanup_hresult)) => {
            Err(DialogError::from_hresult(DialogErrorStage::ClearClientData, cleanup_hresult))
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_hresult)) => Err(error.with_cleanup_hresult(cleanup_hresult)),
    }
}

fn folder_dialog_options(existing: FILEOPENDIALOGOPTIONS) -> FILEOPENDIALOGOPTIONS {
    existing | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_DONTADDTORECENT
}

fn show_and_extract(
    dialog: &IFileOpenDialog,
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
    use std::{os::windows::ffi::OsStrExt, sync::atomic::AtomicBool};
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};

    fn owner() -> OwnerWindow {
        OwnerWindow::from_raw(1).expect("test HWND value is nonzero")
    }

    fn queued(id: u64) -> QueuedRequest {
        QueuedRequest {
            id: DialogRequestId(id),
            request: ShellRequest::PickFolder(FolderDialogRequest { owner: owner() }),
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
                event: DialogEvent::Cancelled { request_id: DialogRequestId(1) },
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
            DialogEvent::Cancelled { request_id: DialogRequestId(1) }
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
            DialogEvent::Failed { request_id: DialogRequestId(9), .. }
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

        let error = guard_request_execution(queued(11), &event_tx, &shared, || {
            panic!("injected Shell request panic");
        })
        .expect_err("the panic is converted to a typed terminal failure");

        assert_eq!(error, DialogError::worker_panicked());
        assert!(shared.modal_outstanding.load(Ordering::Acquire));
        let completed = event_rx.try_recv().expect("panic result is queued");
        assert_eq!(
            deliver_completed_request(&shared, completed),
            DialogEvent::Failed { request_id: DialogRequestId(11), error }
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
            DialogEvent::Failed {
                request_id: DialogRequestId(7),
                error: DialogError::service_shutdown(),
            }
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
            Err(DialogError::from_hresult(DialogErrorStage::ClearClientData, cleanup_hresult))
        );
    }

    #[test]
    fn owned_events_and_requests_are_send_without_com_pointers() {
        fn assert_send<T: Send>() {}
        assert_send::<ShellRequest>();
        assert_send::<DialogEvent>();
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
}
