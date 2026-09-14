//! Bounded, UI-independent orchestration for scans, immutable frames, and layout.
//!
//! Scan sessions, reducers, snapshot materialization, and presentation indexing live on a
//! supervisor thread. The UI thread only posts bounded commands, drains already-built values,
//! and submits immutable layout jobs. One active scan, a validated number of retiring scans,
//! and one coalesced launch bound all scan ownership.
//!
//! [`RuntimeController::drop`] never joins scan or layout workers. It transfers the layout
//! service and committed frame to the supervisor, requests cancellation, and detaches the
//! supervisor handle. A filesystem provider that never returns can therefore retain at most
//! `max_retiring_scans + 1` scan pipelines until process exit, but cannot block UI teardown.

#![forbid(unsafe_code)]

use crate::{
    branch_rescan::{BranchMergeError, BranchRescanIntent, merge_branch_snapshot_cancellable},
    layout_service::{LayoutRequestId, LayoutService, LayoutSubmitError, LayoutTaskError},
    navigation::{
        CommandOutcome, HiddenBranchesPlan, NavigationChange, NavigationCommand, NavigationError,
        NavigationState, RescanRequest,
    },
    presentation::{PresentationError, SnapshotPresentation},
    session::{
        DrainBudget, DrainReport, DrainStop, SessionError, SessionPhase, SessionProgress,
        SessionReducer, SnapshotDecision, SnapshotPolicy, SnapshotPublication,
    },
};
use diskpie_core::{
    EntryKind, GenerationId, NodeId, TreeSnapshot,
    sunburst::{LayoutOptions, SizeBasis, SunburstLayout},
};
use diskpie_scan::{
    CancelToken, ScanEvent, ScanFailure, ScanFs, ScanGeneration, ScanOptions, ScanReceiveError,
    ScanRequest, ScanRoot, ScanSession, start_scan_with_cancel,
};
use std::{
    collections::VecDeque,
    error::Error,
    fmt, io, mem,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        mpsc::{SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// A progressive scan publication may request layout at most twenty times per caller-clock second.
pub const MIN_PARTIAL_LAYOUT_INTERVAL: Duration = Duration::from_millis(50);
/// Defensive ceiling for simultaneously retiring scan sessions.
pub const MAX_RETIRING_SCANS: usize = 16;
const REAPER_CAPACITY: usize = 64;
const REAPER_TOTAL_CAPACITY: usize = REAPER_CAPACITY + 1;

/// Bounded scan, publication, and layout policy owned by the runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeConfig {
    pub scan_options: ScanOptions,
    pub drain_budget: DrainBudget,
    pub snapshot_policy: SnapshotPolicy,
    /// Template whose root and size basis are replaced from navigation state.
    pub layout_options: LayoutOptions,
    /// Maximum cancelled sessions collected concurrently beside the active session.
    pub max_retiring_scans: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            scan_options: ScanOptions::default(),
            drain_budget: DrainBudget::default(),
            snapshot_policy: SnapshotPolicy::default(),
            layout_options: LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
            max_retiring_scans: 2,
        }
    }
}

/// Terminal scan information retained after the reducer and session have been released.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanSummary {
    pub generation: ScanGeneration,
    pub phase: SessionPhase,
    pub progress: SessionProgress,
}

/// Observable lifecycle state containing no widget or platform objects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeState {
    Idle,
    Active {
        generation: ScanGeneration,
        phase: SessionPhase,
    },
    Cancelling {
        generation: ScanGeneration,
        pending: Option<ScanGeneration>,
    },
    Settled {
        generation: ScanGeneration,
        phase: SessionPhase,
    },
    ShuttingDown {
        generation: Option<ScanGeneration>,
    },
    /// All scan-execution ownership has been handed to background retirement.
    /// A filesystem provider may still be returning from an in-flight call.
    ShutdownComplete,
}

/// Whether a launch can begin immediately or entered the one-slot replacement queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchDisposition {
    Started,
    Queued,
    ReplacedPending,
}

/// Generation and queue outcome returned without waiting for scan work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaunchReceipt {
    pub generation: ScanGeneration,
    pub disposition: LaunchDisposition,
}

/// A scan launch rejected before the runtime consumed any caller-owned input.
///
/// Use [`Self::into_parts`] to retry the same provider, roots, and cancellation
/// token after transient retirement backpressure clears.
pub struct ScanLaunchRejected {
    error: RuntimeError,
    fs: Arc<dyn ScanFs>,
    roots: Vec<ScanRoot>,
    cancel: CancelToken,
}

impl fmt::Debug for ScanLaunchRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScanLaunchRejected")
            .field("error", &self.error)
            .field("root_count", &self.roots.len())
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ScanLaunchRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Error for ScanLaunchRejected {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.error.source()
    }
}

impl ScanLaunchRejected {
    #[must_use]
    pub const fn error(&self) -> &RuntimeError {
        &self.error
    }

    #[must_use]
    pub fn into_parts(self) -> (RuntimeError, Arc<dyn ScanFs>, Vec<ScanRoot>, CancelToken) {
        (self.error, self.fs, self.roots, self.cancel)
    }
}

/// Rejected branch launch retaining the exact intent and all caller resources.
pub struct BranchScanLaunchRejected {
    error: RuntimeError,
    inputs: Box<BranchLaunchInputs>,
}

struct BranchLaunchInputs {
    intent: BranchRescanIntent,
    fs: Arc<dyn ScanFs>,
    root: ScanRoot,
    cancel: CancelToken,
}

impl BranchScanLaunchRejected {
    fn new(
        error: RuntimeError,
        intent: BranchRescanIntent,
        fs: Arc<dyn ScanFs>,
        root: ScanRoot,
        cancel: CancelToken,
    ) -> Self {
        Self { error, inputs: Box::new(BranchLaunchInputs { intent, fs, root, cancel }) }
    }

    #[must_use]
    pub const fn error(&self) -> &RuntimeError {
        &self.error
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (RuntimeError, BranchRescanIntent, Arc<dyn ScanFs>, ScanRoot, CancelToken) {
        let BranchLaunchInputs { intent, fs, root, cancel } = *self.inputs;
        (self.error, intent, fs, root, cancel)
    }
}

impl fmt::Debug for BranchScanLaunchRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BranchScanLaunchRejected")
            .field("error", &self.error)
            .field("intent", &self.inputs.intent)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for BranchScanLaunchRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Error for BranchScanLaunchRejected {}

/// Plain-data summary of one nonblocking runtime poll.
#[derive(Clone, Debug)]
pub struct TickReport {
    pub drain: Option<DrainReport>,
    pub scan_started: Option<ScanGeneration>,
    pub scan_finished: Option<ScanGeneration>,
    /// True only when a snapshot, presentation, navigation state, and layout commit atomically.
    pub snapshot_published: bool,
    pub layout_submitted: Option<LayoutRequestId>,
    pub layout_completed: bool,
    pub discarded_layout_completion: bool,
    pub needs_repaint: bool,
    pub state: RuntimeState,
}

impl TickReport {
    fn new(state: RuntimeState) -> Self {
        Self {
            drain: None,
            scan_started: None,
            scan_finished: None,
            snapshot_published: false,
            layout_submitted: None,
            layout_completed: false,
            discarded_layout_completion: false,
            needs_repaint: false,
            state,
        }
    }
}

/// Result of one pure navigation command plus any requested layout work.
#[derive(Clone, Debug)]
pub struct NavigationReport {
    pub outcome: CommandOutcome,
    pub layout_submitted: Option<LayoutRequestId>,
    pub needs_repaint: bool,
}

impl NavigationReport {
    /// Returns scanner work without starting any I/O.
    #[must_use]
    pub fn rescan_request(&self) -> Option<&RescanRequest> {
        match &self.outcome {
            CommandOutcome::RescanRequested(request) => Some(request),
            CommandOutcome::Applied(_) | CommandOutcome::Unchanged => None,
        }
    }
}

/// Typed orchestration failure. The latest asynchronous failure remains inspectable.
#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeError {
    GenerationExhausted,
    ShuttingDown,
    SupervisorStopped,
    RetirementBackpressure,
    RetirementWorkerStopped,
    NoNavigation,
    FrameTransitionPending,
    ScanNotSettled,
    StaleBranchIntent,
    BranchMerge(BranchMergeError),
    InvalidConfig { field: &'static str },
    InvalidRoot { index: usize },
    ClockWentBackwards { previous: Duration, current: Duration },
    LayoutWorkerStart { kind: io::ErrorKind, detail: String },
    SupervisorWorkerStart { kind: io::ErrorKind, detail: String },
    Scan(ScanFailure),
    Session(SessionError),
    Navigation(NavigationError),
    Presentation(PresentationError),
    LayoutSubmit(LayoutSubmitError),
    LayoutTask(LayoutTaskError),
    LayoutCompletionMismatch,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenerationExhausted => formatter.write_str("scan generations are exhausted"),
            Self::ShuttingDown => formatter.write_str("runtime shutdown has been requested"),
            Self::SupervisorStopped => formatter.write_str("runtime supervisor has stopped"),
            Self::RetirementBackpressure => {
                formatter.write_str("retirement capacity is temporarily exhausted")
            }
            Self::RetirementWorkerStopped => {
                formatter.write_str("the value retirement worker has stopped")
            }
            Self::NoNavigation => formatter.write_str("no committed snapshot is navigable yet"),
            Self::FrameTransitionPending => {
                formatter.write_str("a newer generation is waiting for an atomic frame commit")
            }
            Self::ScanNotSettled => {
                formatter.write_str("branch rescan requires a settled revision")
            }
            Self::StaleBranchIntent => {
                formatter.write_str("branch intent no longer matches the committed revision")
            }
            Self::BranchMerge(error) => write!(formatter, "branch replacement failed: {error}"),
            Self::InvalidConfig { field } => {
                write!(formatter, "invalid runtime config field {field}")
            }
            Self::InvalidRoot { index } => write!(formatter, "scan root {index} has an empty path"),
            Self::ClockWentBackwards { previous, current } => {
                write!(formatter, "caller clock moved backwards from {previous:?} to {current:?}")
            }
            Self::LayoutWorkerStart { kind, detail } => {
                write!(formatter, "could not start layout worker ({kind:?}): {detail}")
            }
            Self::SupervisorWorkerStart { kind, detail } => {
                write!(formatter, "could not start runtime supervisor ({kind:?}): {detail}")
            }
            Self::Scan(error) => write!(formatter, "scan failed: {error}"),
            Self::Session(error) => write!(formatter, "scan reduction failed: {error}"),
            Self::Navigation(error) => write!(formatter, "navigation failed: {error}"),
            Self::Presentation(error) => write!(formatter, "presentation failed: {error}"),
            Self::LayoutSubmit(error) => write!(formatter, "layout submission failed: {error}"),
            Self::LayoutTask(error) => write!(formatter, "layout computation failed: {error}"),
            Self::LayoutCompletionMismatch => {
                formatter.write_str("layout completion does not match its staged frame")
            }
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Navigation(error) => Some(error),
            Self::Presentation(error) => Some(error),
            Self::LayoutSubmit(error) => Some(error),
            Self::LayoutTask(error) => Some(error),
            Self::BranchMerge(error) => Some(error),
            Self::GenerationExhausted
            | Self::ShuttingDown
            | Self::SupervisorStopped
            | Self::RetirementBackpressure
            | Self::RetirementWorkerStopped
            | Self::NoNavigation
            | Self::FrameTransitionPending
            | Self::ScanNotSettled
            | Self::StaleBranchIntent
            | Self::InvalidConfig { .. }
            | Self::InvalidRoot { .. }
            | Self::ClockWentBackwards { .. }
            | Self::LayoutWorkerStart { .. }
            | Self::SupervisorWorkerStart { .. }
            | Self::LayoutCompletionMismatch => None,
        }
    }
}

impl From<ScanFailure> for RuntimeError {
    fn from(error: ScanFailure) -> Self {
        Self::Scan(error)
    }
}

impl From<SessionError> for RuntimeError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<NavigationError> for RuntimeError {
    fn from(error: NavigationError) -> Self {
        Self::Navigation(error)
    }
}

impl From<PresentationError> for RuntimeError {
    fn from(error: PresentationError) -> Self {
        Self::Presentation(error)
    }
}

impl From<LayoutSubmitError> for RuntimeError {
    fn from(error: LayoutSubmitError) -> Self {
        Self::LayoutSubmit(error)
    }
}

impl From<LayoutTaskError> for RuntimeError {
    fn from(error: LayoutTaskError) -> Self {
        Self::LayoutTask(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DisplayedRevision {
    generation: GenerationId,
    revision: u64,
    terminal: bool,
}

struct PreparedPublication {
    revision: DisplayedRevision,
    snapshot: Arc<TreeSnapshot>,
    navigation: Arc<NavigationState>,
    hidden: HiddenBranchesPlan,
    navigation_epoch: u64,
    presentation: Arc<SnapshotPresentation>,
}

#[derive(Clone)]
struct NavigationSeed {
    epoch: u64,
    navigation: Arc<NavigationState>,
}

struct PendingLaunch {
    generation: ScanGeneration,
    fs: Arc<dyn ScanFs>,
    roots: Vec<ScanRoot>,
    cancel: CancelToken,
    branch: Option<BranchRescanIntent>,
    initial_omissions: u64,
}

struct PendingLaunchRejected {
    error: RuntimeError,
    launch: Box<PendingLaunch>,
}

#[derive(Clone)]
struct LiveStatus {
    generation: ScanGeneration,
    phase: SessionPhase,
    progress: SessionProgress,
    cancel_requested: bool,
    cancel: CancelToken,
}

#[derive(Clone)]
struct ReservedLaunchStatus {
    generation: ScanGeneration,
    cancel: CancelToken,
}

#[derive(Clone, Default)]
struct SupervisorStatus {
    active: Option<LiveStatus>,
    reserved: Option<ReservedLaunchStatus>,
    pending: Option<ScanGeneration>,
    retiring: usize,
    newest_retiring: Option<ScanGeneration>,
    settled: Option<ScanSummary>,
    quiescent: bool,
    shutdown_ack: bool,
    stopped: bool,
}

#[derive(Default)]
struct SupervisorOutput {
    publication: Option<PreparedPublication>,
    error: Option<RuntimeError>,
    drain: Option<DrainReport>,
    scan_started: Option<ScanGeneration>,
    scan_finished: Option<ScanGeneration>,
}

#[derive(Default)]
struct SupervisorInbox {
    launch: Option<PendingLaunch>,
    navigation: Option<NavigationSeed>,
    remap: Option<PreparedPublication>,
    pulse: Option<Duration>,
    cancel_generation: Option<ScanGeneration>,
    shutdown: bool,
    detach: bool,
    reap: Option<ReservedRuntimeReap>,
    #[cfg(test)]
    panic_next_step: bool,
}

#[derive(Default)]
struct SupervisorSharedState {
    inbox: SupervisorInbox,
    status: SupervisorStatus,
    output: SupervisorOutput,
    partial_throttle: Option<(GenerationId, Duration)>,
    control_epoch: u64,
    #[cfg(test)]
    supervisor_waiting: bool,
    #[cfg(test)]
    branch_publication_hook: Option<Arc<dyn Fn(BranchPublicationStage) + Send + Sync>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BranchPublicationStage {
    Merge,
    Presentation,
    Publish,
}

#[cfg(test)]
fn branch_publication_checkpoint(shared: &SupervisorShared, stage: BranchPublicationStage) {
    let hook = lock_supervisor(shared).branch_publication_hook.clone();
    if let Some(hook) = hook {
        hook(stage);
    }
}

impl SupervisorSharedState {
    fn has_output(&self) -> bool {
        self.output.publication.is_some()
            || self.output.error.is_some()
            || self.output.drain.is_some()
            || self.output.scan_started.is_some()
            || self.output.scan_finished.is_some()
            || self.inbox.navigation.is_some()
            || self.inbox.remap.is_some()
    }

    /// Inspect this only while holding the shared-state lock. Launch ownership
    /// moves from the inbox to `reserved` under that same lock, so combining
    /// separately sampled status/inbox/output can incorrectly observe a gap.
    fn branch_scan_readiness(&self) -> Result<(), RuntimeError> {
        if self.status.stopped {
            return Err(RuntimeError::SupervisorStopped);
        }
        if self.inbox.shutdown {
            return Err(RuntimeError::ShuttingDown);
        }
        if self.status.active.is_some()
            || self.status.reserved.is_some()
            || self.status.pending.is_some()
            || self.inbox.launch.is_some()
            || self.has_output()
        {
            return Err(RuntimeError::ScanNotSettled);
        }
        Ok(())
    }
}

#[derive(Default)]
struct SupervisorShared {
    state: Mutex<SupervisorSharedState>,
    wake: Condvar,
}

struct SupervisorHandle {
    shared: Arc<SupervisorShared>,
    /// Dropping a `JoinHandle` detaches; joins are deliberately owned by the supervisor itself.
    worker: Option<JoinHandle<()>>,
    reaper: BoundedReaper,
    teardown_permit: Option<ReaperPermit>,
    _session_reaper: SessionReaper,
    _reaper_worker: Option<JoinHandle<()>>,
    _session_reaper_worker: Option<JoinHandle<()>>,
    max_retiring_scans: usize,
}

impl SupervisorHandle {
    fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        let worker_start_error = |error: io::Error| RuntimeError::SupervisorWorkerStart {
            kind: error.kind(),
            detail: error.to_string(),
        };
        let (reaper, reaper_worker) = BoundedReaper::new().map_err(worker_start_error)?;
        let teardown_permit =
            reaper.try_reserve().map_err(|_| RuntimeError::SupervisorWorkerStart {
                kind: io::ErrorKind::Other,
                detail: "value reaper did not reserve teardown capacity".to_owned(),
            })?;
        let (session_reaper, session_reaper_worker) =
            SessionReaper::new().map_err(worker_start_error)?;
        let shared = Arc::new(SupervisorShared::default());
        let worker_shared = Arc::clone(&shared);
        let worker_reaper = reaper.clone();
        let worker_session_reaper = session_reaper.clone();
        let max_retiring_scans = config.max_retiring_scans;
        let worker = thread::Builder::new()
            .name("diskpie-runtime-supervisor".to_owned())
            .spawn(move || {
                SupervisorCore::new(worker_shared, config, worker_reaper, worker_session_reaper)
                    .run();
            })
            .map_err(|error| RuntimeError::SupervisorWorkerStart {
                kind: error.kind(),
                detail: error.to_string(),
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
            reaper,
            teardown_permit: Some(teardown_permit),
            _session_reaper: session_reaper,
            _reaper_worker: Some(reaper_worker),
            _session_reaper_worker: Some(session_reaper_worker),
            max_retiring_scans,
        })
    }

    fn enqueue_launch(
        &self,
        launch: PendingLaunch,
    ) -> Result<LaunchDisposition, PendingLaunchRejected> {
        let permit = match self.reaper.try_reserve() {
            Ok(permit) => permit,
            Err(ReaperReserveError::Full) => {
                return Err(PendingLaunchRejected {
                    error: RuntimeError::RetirementBackpressure,
                    launch: Box::new(launch),
                });
            }
            Err(ReaperReserveError::Stopped) => {
                self.mark_reaper_stopped();
                return Err(PendingLaunchRejected {
                    error: RuntimeError::RetirementWorkerStopped,
                    launch: Box::new(launch),
                });
            }
        };
        let mut shared = lock_supervisor(&self.shared);
        if shared.inbox.shutdown {
            return Err(PendingLaunchRejected {
                error: RuntimeError::ShuttingDown,
                launch: Box::new(launch),
            });
        }
        if shared.status.stopped {
            return Err(PendingLaunchRejected {
                error: RuntimeError::SupervisorStopped,
                launch: Box::new(launch),
            });
        }

        let replaced = shared.inbox.launch.replace(launch);
        let replaced_pending = replaced.is_some();
        if let Some(replaced) = &replaced {
            replaced.cancel.cancel();
        }
        let cancelled_generation = if let Some(active) = &mut shared.status.active {
            active.cancel.cancel();
            active.cancel_requested = true;
            Some(active.generation)
        } else {
            None
        };
        if let Some(generation) = cancelled_generation {
            shared.inbox.cancel_generation = Some(generation);
        }
        if let Some(reserved) = &shared.status.reserved {
            reserved.cancel.cancel();
        }
        let can_start = shared.status.reserved.is_none()
            && (shared.status.active.is_none() || shared.status.retiring < self.max_retiring_scans);
        mark_supervisor_control(&mut shared);
        drop(shared);
        if let Some(replaced) = replaced {
            permit.submit(ReapJob::new(replaced));
        }
        self.shared.wake.notify_one();
        Ok(if replaced_pending {
            LaunchDisposition::ReplacedPending
        } else if can_start {
            LaunchDisposition::Started
        } else {
            LaunchDisposition::Queued
        })
    }

    fn mark_reaper_stopped(&self) {
        let mut shared = lock_supervisor(&self.shared);
        shared.inbox.shutdown = true;
        shared.status.stopped = true;
        shared.status.shutdown_ack = true;
        mark_supervisor_control(&mut shared);
        drop(shared);
        self.shared.wake.notify_one();
    }

    fn pulse(&self, now: Duration) {
        let mut shared = lock_supervisor(&self.shared);
        if !shared.inbox.detach {
            shared.inbox.pulse = Some(now);
            mark_supervisor_control(&mut shared);
        }
        drop(shared);
        self.shared.wake.notify_one();
    }

    fn update_navigation(&self, navigation: NavigationSeed, retired: &mut ReservedReapBatch) {
        let replaced = {
            let mut shared = lock_supervisor(&self.shared);
            if shared.inbox.shutdown || shared.status.stopped {
                drop(shared);
                retired.push(navigation);
                return;
            }
            let replaced = shared.inbox.navigation.replace(navigation);
            mark_supervisor_control(&mut shared);
            replaced
        };
        if let Some(replaced) = replaced {
            retired.push(replaced);
        }
        self.shared.wake.notify_one();
    }

    fn request_remap(&self, publication: PreparedPublication, retired: &mut ReservedReapBatch) {
        let replaced = {
            let mut shared = lock_supervisor(&self.shared);
            if shared.inbox.shutdown || shared.status.stopped {
                drop(shared);
                retired.push(publication);
                return;
            }
            let replaced = shared.inbox.remap.replace(publication);
            mark_supervisor_control(&mut shared);
            replaced
        };
        if let Some(replaced) = replaced {
            retired.push(replaced);
        }
        self.shared.wake.notify_one();
    }

    fn cancel_active(&self) -> bool {
        let mut shared = lock_supervisor(&self.shared);
        let has_active = shared.status.active.is_some()
            || shared.status.reserved.is_some()
            || shared.inbox.launch.is_some();
        if has_active && !shared.inbox.shutdown {
            let active_generation = if let Some(active) = &mut shared.status.active {
                active.cancel.cancel();
                active.cancel_requested = true;
                Some(active.generation)
            } else {
                None
            };
            let reserved_generation = if let Some(reserved) = &shared.status.reserved {
                reserved.cancel.cancel();
                Some(reserved.generation)
            } else {
                None
            };
            if let Some(launch) = &shared.inbox.launch {
                launch.cancel.cancel();
            }
            shared.inbox.cancel_generation = reserved_generation
                .or(active_generation)
                .or_else(|| shared.inbox.launch.as_ref().map(|launch| launch.generation));
            mark_supervisor_control(&mut shared);
        }
        drop(shared);
        if has_active {
            self.shared.wake.notify_one();
        }
        has_active
    }

    fn request_shutdown(&self) {
        let mut shared = lock_supervisor(&self.shared);
        shared.inbox.shutdown = true;
        mark_supervisor_control(&mut shared);
        if let Some(active) = &mut shared.status.active {
            active.cancel.cancel();
            active.cancel_requested = true;
        }
        if let Some(reserved) = &shared.status.reserved {
            reserved.cancel.cancel();
        }
        if let Some(pending) = &shared.inbox.launch {
            pending.cancel.cancel();
        }
        drop(shared);
        self.shared.wake.notify_one();
    }

    fn detach(&mut self, mut reap: RuntimeReapPayload) {
        let permit = self
            .teardown_permit
            .take()
            .expect("runtime teardown retains its reserved reaper permit");
        let mut shared = lock_supervisor(&self.shared);
        shared.inbox.shutdown = true;
        shared.inbox.detach = true;
        mark_supervisor_control(&mut shared);
        if let Some(active) = &mut shared.status.active {
            active.cancel.cancel();
            active.cancel_requested = true;
        }
        if let Some(reserved) = &shared.status.reserved {
            reserved.cancel.cancel();
        }
        let pending = shared.inbox.launch.take();
        if let Some(pending) = &pending {
            pending.cancel.cancel();
        }
        debug_assert!(reap.pending.is_none());
        reap.pending = pending;
        let reap = ReservedRuntimeReap { permit, payload: reap };
        let stopped = shared.status.stopped;
        let direct_reap = if stopped {
            Some(reap)
        } else {
            debug_assert!(shared.inbox.reap.is_none());
            shared.inbox.reap = Some(reap);
            None
        };
        drop(shared);
        if let Some(reap) = direct_reap {
            reap.submit();
        } else {
            self.shared.wake.notify_one();
        }
        // Detach without joining. The supervisor owns all potentially blocking destruction.
        let _ = self.worker.take();
    }

    fn status(&self) -> SupervisorStatus {
        lock_supervisor(&self.shared).status.clone()
    }

    fn take_output(&self, now: Duration) -> SupervisorOutput {
        let mut shared = lock_supervisor(&self.shared);
        let output = mem::take(&mut shared.output);
        if let Some(publication) = &output.publication
            && !publication.revision.terminal
        {
            record_partial_throttle(&mut shared, publication.revision.generation, now);
        }
        output
    }

    fn has_output(&self) -> bool {
        lock_supervisor(&self.shared).has_output()
    }

    #[cfg(test)]
    fn inject_step_panic(&self) {
        let mut shared = lock_supervisor(&self.shared);
        shared.inbox.panic_next_step = true;
        mark_supervisor_control(&mut shared);
        drop(shared);
        self.shared.wake.notify_one();
    }
}

struct WorkerScan {
    generation: ScanGeneration,
    /// Remains live after joining the coordinator and throughout publication preparation.
    cancel: CancelToken,
    session: Option<ScanSession>,
    reducer: Option<SessionReducer>,
    terminal_summary: Option<ScanSummary>,
    observed_cancelled: Option<ScanSummary>,
    cancel_requested: bool,
    superseded: bool,
    reduction_error: Option<SessionError>,
    discard_events: bool,
    terminal_event_seen: bool,
    branch: Option<BranchRescanIntent>,
    branch_publication: Option<SnapshotPublication>,
    #[cfg(test)]
    retirement_probe: Option<WorkerScanRetirementProbe>,
}

#[cfg(test)]
struct WorkerScanRetirementProbe(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl Drop for WorkerScanRetirementProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl WorkerScan {
    fn new(session: ScanSession) -> Self {
        let generation = session.generation();
        Self {
            generation,
            cancel: session.cancel_token(),
            session: Some(session),
            reducer: Some(SessionReducer::new(generation)),
            terminal_summary: None,
            observed_cancelled: None,
            cancel_requested: false,
            superseded: false,
            reduction_error: None,
            discard_events: false,
            terminal_event_seen: false,
            branch: None,
            branch_publication: None,
            #[cfg(test)]
            retirement_probe: None,
        }
    }

    fn cancel(&mut self, superseded: bool) {
        self.cancel.cancel();
        self.cancel_requested = true;
        self.superseded |= superseded;
    }

    fn is_finished(&self) -> bool {
        self.session.as_ref().is_none_or(ScanSession::is_finished)
    }

    fn phase(&self) -> SessionPhase {
        self.terminal_summary
            .as_ref()
            .map(|summary| summary.phase.clone())
            .or_else(|| self.observed_cancelled.as_ref().map(|summary| summary.phase.clone()))
            .or_else(|| self.reducer.as_ref().map(|reducer| reducer.phase().clone()))
            .expect("a live scan has reducer state or a terminal summary")
    }

    fn progress(&self) -> SessionProgress {
        self.terminal_summary
            .as_ref()
            .map(|summary| summary.progress)
            .or_else(|| self.observed_cancelled.as_ref().map(|summary| summary.progress))
            .or_else(|| self.reducer.as_ref().map(SessionReducer::progress))
            .expect("a live scan has reducer state or a terminal summary")
    }
}

struct DriveResult {
    drain: Option<DrainReport>,
    publication: Option<PreparedPublication>,
    error: Option<RuntimeError>,
    ready_to_finalize: bool,
}

struct FinalizedScan {
    summary: ScanSummary,
    publication: Option<PreparedPublication>,
    error: Option<RuntimeError>,
}

#[derive(Default)]
struct FreezeResult {
    publication: Option<PreparedPublication>,
    error: Option<RuntimeError>,
}

struct SupervisorControls {
    epoch: u64,
    navigation: Option<NavigationSeed>,
    remap: Option<PreparedPublication>,
    pulse: Option<Duration>,
    cancel_generation: Option<ScanGeneration>,
    shutdown: bool,
    detach: bool,
}

struct SupervisorCore {
    shared: Arc<SupervisorShared>,
    reaper: BoundedReaper,
    session_reaper: SessionReaper,
    config: RuntimeConfig,
    active: Option<WorkerScan>,
    retiring: Vec<WorkerScan>,
    navigation: Option<NavigationSeed>,
    settled: Option<ScanSummary>,
    now: Duration,
    observed_control_epoch: u64,
    shutting_down: bool,
    detached: bool,
}

impl SupervisorCore {
    fn new(
        shared: Arc<SupervisorShared>,
        config: RuntimeConfig,
        reaper: BoundedReaper,
        session_reaper: SessionReaper,
    ) -> Self {
        Self {
            shared,
            reaper,
            session_reaper,
            retiring: Vec::with_capacity(config.max_retiring_scans),
            config,
            active: None,
            navigation: None,
            settled: None,
            now: Duration::ZERO,
            observed_control_epoch: 0,
            shutting_down: false,
            detached: false,
        }
    }

    fn run(mut self) {
        loop {
            match catch_unwind(AssertUnwindSafe(|| self.step())) {
                Ok(true) => {}
                Ok(false) => return,
                Err(_) => {
                    self.stop_after_panic();
                    return;
                }
            }
        }
    }

    fn step(&mut self) -> bool {
        #[cfg(test)]
        {
            let should_panic = {
                let mut shared = lock_supervisor(&self.shared);
                mem::take(&mut shared.inbox.panic_next_step)
            };
            assert!(!should_panic, "injected supervisor step panic");
        }

        let controls = self.take_controls();
        self.observed_control_epoch = controls.epoch;
        if let Some(navigation) = controls.navigation {
            if self.navigation.is_some() {
                let Some(mut retired) = self.reserve_supervisor_retirement() else {
                    return false;
                };
                if let Some(replaced) = self.navigation.replace(navigation) {
                    retired.push(replaced);
                }
            } else {
                self.navigation = Some(navigation);
            }
        }
        if let Some(now) = controls.pulse {
            self.now = self.now.max(now);
        }
        self.detached |= controls.detach;
        if controls.shutdown {
            self.begin_shutdown();
        }
        if let Some(generation) = controls.cancel_generation
            && self.active.as_ref().map(|scan| scan.generation) == Some(generation)
        {
            self.cancel_active_observed(false);
        }
        if let Some(publication) = controls.remap {
            self.remap_publication(publication);
        }

        self.promote_pending();
        self.drive_active();
        self.drive_retiring();
        self.retire_cancelled_active();
        self.promote_pending();
        self.publish_status();

        if self.detached && self.is_quiescent() {
            let Some(mut retired) = self.reserve_supervisor_retirement() else {
                return false;
            };
            let (reap, publication, navigation, remap) = {
                let mut shared = lock_supervisor(&self.shared);
                shared.status.stopped = true;
                (
                    shared.inbox.reap.take(),
                    shared.output.publication.take(),
                    shared.inbox.navigation.take(),
                    shared.inbox.remap.take(),
                )
            };
            if let Some(publication) = publication {
                retired.push(publication);
            }
            if let Some(navigation) = navigation {
                retired.push(navigation);
            }
            if let Some(remap) = remap {
                retired.push(remap);
            }
            if let Some(navigation) = self.navigation.take() {
                retired.push(navigation);
            }
            if let Some(reap) = reap {
                reap.submit();
            }
            return false;
        }

        self.wait_for_work();
        true
    }

    fn stop_after_panic(&mut self) {
        let Some(mut retired) = self.reserve_supervisor_retirement() else {
            return;
        };
        if let Some(active) = &mut self.active {
            active.cancel(true);
        }
        for scan in &mut self.retiring {
            scan.cancel(true);
        }
        let mut scans = mem::take(&mut self.retiring);
        if let Some(active) = self.active.take() {
            scans.push(active);
        }
        if !scans.is_empty()
            && let Err(scans) = self.session_reaper.submit(scans)
        {
            retired.push(scans);
        }
        if let Some(navigation) = self.navigation.take() {
            retired.push(navigation);
        }

        let (pending, inbox_navigation, remap, publication, reap) = {
            let mut shared = lock_supervisor(&self.shared);
            let pending = shared.inbox.launch.take();
            let inbox_navigation = shared.inbox.navigation.take();
            let remap = shared.inbox.remap.take();
            let publication = shared.output.publication.take();
            let reap = shared.inbox.reap.take();
            shared.status.active = None;
            shared.status.reserved = None;
            shared.status.pending = None;
            shared.status.retiring = 0;
            shared.status.quiescent = true;
            shared.status.shutdown_ack = true;
            shared.status.stopped = true;
            shared.output.error = Some(RuntimeError::SupervisorStopped);
            (pending, inbox_navigation, remap, publication, reap)
        };
        if let Some(pending) = pending {
            pending.cancel.cancel();
            retired.push(pending);
        }
        if let Some(navigation) = inbox_navigation {
            retired.push(navigation);
        }
        if let Some(publication) = remap {
            retired.push(publication);
        }
        if let Some(publication) = publication {
            retired.push(publication);
        }
        if let Some(reap) = reap {
            reap.submit();
        }
    }

    fn take_controls(&self) -> SupervisorControls {
        let mut shared = lock_supervisor(&self.shared);
        SupervisorControls {
            epoch: shared.control_epoch,
            navigation: shared.inbox.navigation.take(),
            remap: shared.inbox.remap.take(),
            pulse: shared.inbox.pulse.take(),
            cancel_generation: shared.inbox.cancel_generation.take(),
            shutdown: shared.inbox.shutdown,
            detach: shared.inbox.detach,
        }
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        let Some(mut retired) = self.reserve_supervisor_retirement() else {
            self.shutting_down = true;
            return;
        };
        self.shutting_down = true;
        let pending = lock_supervisor(&self.shared).inbox.launch.take();
        if let Some(pending) = pending {
            pending.cancel.cancel();
            retired.push(pending);
        }
        let mut scans = mem::take(&mut self.retiring);
        if let Some(active) = self.active.take() {
            scans.push(active);
        }
        for scan in &mut scans {
            scan.cancel(true);
        }
        if !scans.is_empty()
            && let Err(scans) = self.session_reaper.submit(scans)
        {
            retired.push(scans);
        }
        let mut shared = lock_supervisor(&self.shared);
        shared.status.active = None;
        shared.status.reserved = None;
        shared.status.pending = None;
        shared.status.retiring = 0;
        shared.status.quiescent = true;
        shared.status.shutdown_ack = true;
    }

    fn remap_publication(&self, mut publication: PreparedPublication) {
        let Some(mut retired) = self.reserve_supervisor_retirement() else {
            return;
        };
        let Some(seed) = &self.navigation else {
            drop(retired);
            self.store_publication(publication);
            return;
        };
        if seed.navigation.generation() > publication.revision.generation {
            retired.push(publication);
            return;
        }
        let mut navigation = seed.navigation.as_ref().clone();
        if let Err(error) = navigation.replace_snapshot(Arc::clone(&publication.snapshot)) {
            self.store_error(RuntimeError::Navigation(error));
            retired.push(publication);
            return;
        }
        publication.hidden = navigation.hidden_branches_plan();
        publication.navigation = Arc::new(navigation);
        publication.navigation_epoch = seed.epoch;
        drop(retired);
        self.store_publication(publication);
    }

    fn cancel_active_observed(&mut self, superseded: bool) {
        let Some(active) = &mut self.active else {
            return;
        };
        active.cancel(superseded);
        let frozen =
            freeze_observed_cancelled(active, self.now, &self.config, self.navigation.as_ref());
        if let Some(publication) = frozen.publication {
            self.store_publication(publication);
        }
        if let Some(error) = frozen.error {
            self.store_error(error);
        }
        if self.retiring.len() < self.config.max_retiring_scans {
            let active = self.active.take().expect("cancelled active scan exists");
            self.retiring.push(active);
        }
    }

    fn retire_cancelled_active(&mut self) {
        if self.retiring.len() >= self.config.max_retiring_scans
            || !self
                .active
                .as_ref()
                .is_some_and(|scan| scan.cancel_requested && scan.observed_cancelled.is_some())
        {
            return;
        }
        let active = self.active.take().expect("cancelled active scan exists");
        self.retiring.push(active);
    }

    fn start_scan(&mut self, launch: PendingLaunch) -> Option<WorkerScan> {
        let generation = launch.generation;
        let request = ScanRequest::new(generation, launch.roots)
            .with_options(self.config.scan_options.clone());
        match start_scan_with_cancel(launch.fs, request, launch.cancel) {
            Ok(session) => {
                self.store_started(generation);
                let mut scan = WorkerScan::new(session);
                scan.branch = launch.branch;
                scan.reducer = Some(
                    SessionReducer::new(generation)
                        .with_initial_omissions(launch.initial_omissions),
                );
                Some(scan)
            }
            Err(error) => {
                self.store_error(RuntimeError::Scan(error.clone()));
                self.store_settled(ScanSummary {
                    generation,
                    phase: SessionPhase::Failed(error),
                    progress: SessionProgress::default(),
                });
                None
            }
        }
    }

    fn promote_pending(&mut self) {
        if self.shutting_down {
            return;
        }
        if self.active.is_some() && self.retiring.len() >= self.config.max_retiring_scans {
            return;
        }

        // The shared inbox is the sole pending slot. Recheck it while reserving the
        // launch so a newer enqueue cannot be overwritten by an older core-local copy.
        let launch = {
            let mut shared = lock_supervisor(&self.shared);
            if shared.inbox.shutdown || shared.status.reserved.is_some() {
                None
            } else {
                let launch = shared.inbox.launch.take();
                if let Some(launch) = &launch {
                    shared.status.reserved = Some(ReservedLaunchStatus {
                        generation: launch.generation,
                        cancel: launch.cancel.clone(),
                    });
                }
                launch
            }
        };
        let Some(launch) = launch else {
            return;
        };
        let generation = launch.generation;
        let Some(candidate) = self.start_scan(launch) else {
            let mut shared = lock_supervisor(&self.shared);
            if shared.status.reserved.as_ref().map(|reserved| reserved.generation)
                == Some(generation)
            {
                shared.status.reserved = None;
            }
            return;
        };
        let previous = self.active.replace(candidate);
        let previous_index = previous.map(|mut previous| {
            previous.cancel(true);
            let index = self.retiring.len();
            self.retiring.push(previous);
            index
        });
        {
            let mut shared = lock_supervisor(&self.shared);
            if shared.status.reserved.as_ref().map(|reserved| reserved.generation)
                == Some(generation)
            {
                shared.status.reserved = None;
            }
            if let Some(candidate) = &self.active {
                shared.status.active = Some(LiveStatus {
                    generation: candidate.generation,
                    phase: candidate.phase(),
                    progress: candidate.progress(),
                    cancel_requested: candidate.cancel_requested,
                    cancel: candidate
                        .session
                        .as_ref()
                        .map(ScanSession::cancel_token)
                        .unwrap_or_default(),
                });
            }
        }
        if let Some(previous_index) = previous_index {
            let previous = &mut self.retiring[previous_index];
            let frozen = freeze_observed_cancelled(
                previous,
                self.now,
                &self.config,
                self.navigation.as_ref(),
            );
            if let Some(publication) = frozen.publication {
                self.store_publication(publication);
            }
            if let Some(error) = frozen.error {
                self.store_error(error);
            }
        }
    }

    fn drive_active(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        let result = drive_scan(
            active,
            self.now,
            &self.config,
            !active.superseded,
            self.navigation.as_ref(),
            &self.shared,
        );
        if let Some(drain) = result.drain {
            lock_supervisor(&self.shared).output.drain = Some(drain);
        }
        if let Some(publication) = result.publication {
            self.store_publication(publication);
        }
        if let Some(error) = result.error {
            self.store_error(error);
        }
        if result.ready_to_finalize {
            let scan = self.active.take().expect("active scan is ready");
            self.finish_scan(scan);
        }
    }

    fn drive_retiring(&mut self) {
        let mut index = 0;
        while index < self.retiring.len() {
            let result = {
                let scan = &mut self.retiring[index];
                drive_scan(
                    scan,
                    self.now,
                    &self.config,
                    false,
                    self.navigation.as_ref(),
                    &self.shared,
                )
            };
            if let Some(publication) = result.publication {
                self.store_publication(publication);
            }
            if let Some(error) = result.error {
                self.store_error(error);
            }
            if result.ready_to_finalize {
                let scan = self.retiring.swap_remove(index);
                self.finish_scan(scan);
            } else {
                index += 1;
            }
        }
    }

    fn finish_scan(&mut self, scan: WorkerScan) {
        let generation = scan.generation;
        let is_branch = scan.branch.is_some();
        let cancel = scan.cancel.clone();
        let mut finalized = finalize_scan(scan, self.now, self.navigation.as_ref(), &self.shared);
        #[cfg(test)]
        if is_branch {
            branch_publication_checkpoint(&self.shared, BranchPublicationStage::Publish);
        }
        let Some(mut retired) = self.reserve_supervisor_retirement() else {
            return;
        };
        let shared_owner = Arc::clone(&self.shared);
        let discarded = {
            // This is the branch publication linearization point. Cancellation
            // takes the same lock. Clear the live generation here so a later
            // cancel cannot claim it was accepted for an already published scan.
            // All tree construction and disposal remains outside the guard.
            let mut shared = lock_supervisor(&shared_owner);
            let cancelled = cancel.is_cancelled()
                || shared.inbox.shutdown
                || shared.inbox.cancel_generation == Some(generation)
                || shared.status.active.as_ref().is_some_and(|active| {
                    active.generation == generation && active.cancel_requested
                });
            let discarded = if is_branch && cancelled {
                if !matches!(finalized.summary.phase, SessionPhase::Failed(_)) {
                    finalized.summary.phase = SessionPhase::Cancelled;
                }
                finalized.publication.take()
            } else if let Some(publication) = finalized.publication.take() {
                if shared
                    .output
                    .publication
                    .as_ref()
                    .is_some_and(|current| current.revision >= publication.revision)
                {
                    Some(publication)
                } else {
                    shared.output.publication.replace(publication)
                }
            } else {
                None
            };
            if let Some(error) = finalized.error {
                shared.output.error = Some(error);
            }
            self.store_settled(finalized.summary);
            shared.status.settled = self.settled.clone();
            if shared.status.active.as_ref().map(|active| active.generation) == Some(generation) {
                shared.status.active = None;
            }
            if shared.inbox.cancel_generation == Some(generation) {
                shared.inbox.cancel_generation = None;
            }
            shared.output.scan_finished = Some(generation);
            discarded
        };
        if let Some(publication) = discarded {
            retired.push(publication);
        }
    }

    fn store_started(&self, generation: ScanGeneration) {
        lock_supervisor(&self.shared).output.scan_started = Some(generation);
    }

    fn store_settled(&mut self, summary: ScanSummary) {
        if self.settled.as_ref().is_none_or(|current| summary.generation >= current.generation) {
            self.settled = Some(summary);
        }
    }

    fn store_error(&self, error: RuntimeError) {
        lock_supervisor(&self.shared).output.error = Some(error);
    }

    fn store_publication(&self, publication: PreparedPublication) {
        let Some(mut retired) = self.reserve_supervisor_retirement() else {
            return;
        };
        let discarded = {
            let mut shared = lock_supervisor(&self.shared);
            if shared
                .output
                .publication
                .as_ref()
                .is_some_and(|current| current.revision >= publication.revision)
            {
                publication
            } else {
                match shared.output.publication.replace(publication) {
                    Some(replaced) => replaced,
                    None => return,
                }
            }
        };
        retired.push(discarded);
    }

    fn reserve_supervisor_retirement(&self) -> Option<ReservedReapBatch> {
        match self.reaper.reserve() {
            Ok(permit) => Some(ReservedReapBatch::new(permit)),
            Err(ReaperReserveError::Full) => unreachable!("blocking reservation cannot be full"),
            Err(ReaperReserveError::Stopped) => {
                let mut shared = lock_supervisor(&self.shared);
                shared.inbox.shutdown = true;
                shared.status.stopped = true;
                shared.status.shutdown_ack = true;
                shared.output.error = Some(RuntimeError::RetirementWorkerStopped);
                mark_supervisor_control(&mut shared);
                drop(shared);
                self.shared.wake.notify_all();
                None
            }
        }
    }

    fn publish_status(&self) {
        let mut active = self.active.as_ref().map(|scan| LiveStatus {
            generation: scan.generation,
            phase: scan.phase(),
            progress: scan.progress(),
            cancel_requested: scan.cancel_requested,
            cancel: scan.session.as_ref().map(ScanSession::cancel_token).unwrap_or_default(),
        });
        let newest_retiring = self.retiring.iter().map(|scan| scan.generation).max();
        let mut shared = lock_supervisor(&self.shared);
        if let (Some(next), Some(current)) = (&mut active, &shared.status.active)
            && next.generation == current.generation
        {
            next.cancel_requested |= current.cancel_requested;
        }
        let status = SupervisorStatus {
            active,
            reserved: shared.status.reserved.clone(),
            pending: shared.inbox.launch.as_ref().map(|pending| pending.generation),
            retiring: self.retiring.len(),
            newest_retiring,
            settled: self.settled.clone(),
            quiescent: self.is_quiescent()
                && shared.inbox.launch.is_none()
                && shared.status.reserved.is_none(),
            shutdown_ack: shared.status.shutdown_ack,
            stopped: false,
        };
        shared.status = status;
    }

    fn is_quiescent(&self) -> bool {
        self.active.is_none() && self.retiring.is_empty()
    }

    fn wait_for_work(&self) {
        let mut shared = lock_supervisor(&self.shared);
        let has_control = shared.control_epoch != self.observed_control_epoch
            || shared.inbox.launch.is_some()
            || shared.inbox.navigation.is_some()
            || shared.inbox.remap.is_some()
            || shared.inbox.pulse.is_some()
            || shared.inbox.cancel_generation.is_some()
            || (shared.inbox.shutdown && !self.shutting_down)
            || (shared.inbox.detach && !self.detached);
        if has_control {
            return;
        }
        let wait = if self.is_quiescent() {
            Duration::from_secs(60)
        } else {
            self.config.scan_options.coordinator_poll
        };
        set_supervisor_waiting(&mut shared, true);
        let (mut shared, _timeout) = self
            .shared
            .wake
            .wait_timeout(shared, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_supervisor_waiting(&mut shared, false);
    }
}

fn drive_scan(
    scan: &mut WorkerScan,
    now: Duration,
    config: &RuntimeConfig,
    allow_progress: bool,
    navigation: Option<&NavigationSeed>,
    shared: &SupervisorShared,
) -> DriveResult {
    if scan.terminal_summary.is_some() {
        return DriveResult {
            drain: None,
            publication: None,
            error: None,
            ready_to_finalize: scan.is_finished(),
        };
    }

    let mut drain = None;
    let mut error = None;
    let mut publication = None;

    if scan.cancel_requested && scan.observed_cancelled.is_none() && scan.reduction_error.is_none()
    {
        let frozen = freeze_observed_cancelled(scan, now, config, navigation);
        publication = frozen.publication;
        error = frozen.error;
    }

    if scan.discard_events {
        if let Some(session) = &scan.session {
            let (report, terminal_seen) = drain_discarded_events(session, config.drain_budget);
            scan.terminal_event_seen |= terminal_seen;
            drain = Some(report);
        }
    } else if scan.reduction_error.is_none()
        && let Some(session) = &scan.session
    {
        let started = Instant::now();
        let reducer = scan.reducer.as_mut().expect("nonterminal scan owns a reducer");
        match reducer.drain_session(session, config.drain_budget, || started.elapsed()) {
            Ok(report) => drain = Some(report),
            Err(reduction_error) => {
                session.cancel();
                scan.cancel_requested = true;
                scan.reduction_error = Some(reduction_error.clone());
                scan.discard_events = true;
                error = Some(RuntimeError::Session(reduction_error));
            }
        }
    }

    let terminal = scan.reducer.as_ref().is_some_and(|reducer| reducer.phase().is_terminal());
    if publication.is_none()
        && scan.reduction_error.is_none()
        && scan.observed_cancelled.is_none()
        && publication_generation_is_current(scan.generation, navigation)
        && (allow_progress || terminal)
        && (scan.branch.is_none()
            || (terminal
                && !scan.cancel_requested
                && !scan.superseded
                && matches!(scan.phase(), SessionPhase::Complete | SessionPhase::Partial)))
        && (!scan.superseded || terminal)
    {
        let reducer = scan.reducer.as_mut().expect("nonterminal scan owns a reducer");
        let decision = reducer.snapshot_decision(now, config.snapshot_policy);
        match decision {
            Ok(decision) if publication_allowed(shared, scan.generation, now, decision) => {
                match reducer.publish_snapshot(now) {
                    Ok(snapshot_publication) => {
                        if !terminal {
                            let mut shared = lock_supervisor(shared);
                            record_partial_throttle(
                                &mut shared,
                                GenerationId::from(scan.generation),
                                now,
                            );
                        }
                        if scan.branch.is_some() {
                            // Commit only after the coordinator's successful join.
                            scan.branch_publication = Some(snapshot_publication);
                        } else {
                            match prepare_publication_with_navigation(
                                snapshot_publication,
                                terminal,
                                navigation,
                            ) {
                                Ok(prepared) => publication = Some(prepared),
                                Err(preparation_error) => error = Some(preparation_error),
                            }
                        }
                    }
                    Err(reduction_error) => {
                        error = Some(RuntimeError::Session(reduction_error.clone()));
                        scan.reduction_error = Some(reduction_error);
                        scan.discard_events = true;
                        if let Some(session) = &scan.session {
                            session.cancel();
                        }
                        scan.cancel_requested = true;
                    }
                }
            }
            Ok(_) => {}
            Err(reduction_error) => {
                error = Some(RuntimeError::Session(reduction_error.clone()));
                scan.reduction_error = Some(reduction_error);
                scan.discard_events = true;
                if let Some(session) = &scan.session {
                    session.cancel();
                }
                scan.cancel_requested = true;
            }
        }
    }

    if terminal {
        let reducer = scan.reducer.take().expect("terminal scan owns a reducer");
        let phase = if scan.observed_cancelled.is_some() {
            SessionPhase::Cancelled
        } else {
            reducer.phase().clone()
        };
        scan.terminal_summary =
            Some(ScanSummary { generation: scan.generation, phase, progress: reducer.progress() });
        // Terminal model state is now immutable plain data; release reducer arenas immediately.
        drop(reducer);
    }

    let ready_to_finalize = scan.is_finished()
        && (scan.terminal_summary.is_some()
            || scan.reduction_error.is_some()
            || scan.terminal_event_seen);
    DriveResult { drain, publication, error, ready_to_finalize }
}

fn publication_allowed(
    shared: &SupervisorShared,
    generation: ScanGeneration,
    now: Duration,
    decision: SnapshotDecision,
) -> bool {
    match decision {
        SnapshotDecision::PublishInitial | SnapshotDecision::PublishTerminal => true,
        SnapshotDecision::PublishEventThreshold | SnapshotDecision::PublishTimeThreshold => {
            let generation = GenerationId::from(generation);
            let mut shared = lock_supervisor(shared);
            if shared.output.publication.as_ref().is_some_and(|publication| {
                !publication.revision.terminal && publication.revision.generation == generation
            }) {
                return false;
            }
            let allowed = shared.partial_throttle.is_none_or(|(current, last)| {
                current != generation || now.saturating_sub(last) >= MIN_PARTIAL_LAYOUT_INTERVAL
            });
            if allowed {
                record_partial_throttle(&mut shared, generation, now);
            }
            allowed
        }
        SnapshotDecision::NoChanges | SnapshotDecision::Deferred => false,
    }
}

fn record_partial_throttle(
    shared: &mut SupervisorSharedState,
    generation: GenerationId,
    now: Duration,
) {
    match &mut shared.partial_throttle {
        Some((current, last)) if *current == generation => *last = (*last).max(now),
        Some((current, _)) if *current > generation => {}
        slot => *slot = Some((generation, now)),
    }
}

fn publication_generation_is_current(
    generation: ScanGeneration,
    navigation: Option<&NavigationSeed>,
) -> bool {
    navigation.is_none_or(|seed| seed.navigation.generation() <= GenerationId::from(generation))
}

fn freeze_observed_cancelled(
    scan: &mut WorkerScan,
    now: Duration,
    config: &RuntimeConfig,
    navigation: Option<&NavigationSeed>,
) -> FreezeResult {
    if scan.observed_cancelled.is_some()
        || scan.terminal_summary.is_some()
        || scan.reducer.as_ref().is_some_and(|reducer| reducer.phase().is_terminal())
    {
        return FreezeResult::default();
    }
    if scan.branch.is_some() {
        scan.observed_cancelled = Some(ScanSummary {
            generation: scan.generation,
            phase: SessionPhase::Cancelled,
            progress: scan.progress(),
        });
        return FreezeResult::default();
    }
    if let Some(session) = &scan.session {
        session.cancel();
        if scan.reduction_error.is_none() && !scan.discard_events {
            let started = Instant::now();
            let reducer = scan.reducer.as_mut().expect("live scan owns a reducer");
            if let Err(reduction_error) =
                reducer.drain_session(session, config.drain_budget, || started.elapsed())
            {
                scan.reduction_error = Some(reduction_error.clone());
                scan.discard_events = true;
                return FreezeResult {
                    publication: None,
                    error: Some(RuntimeError::Session(reduction_error)),
                };
            }
        }
    }
    let reducer = scan.reducer.as_mut().expect("live scan owns a reducer");
    if matches!(reducer.phase(), SessionPhase::AwaitingStart) {
        return FreezeResult::default();
    }
    let progress = reducer.progress();
    if !publication_generation_is_current(scan.generation, navigation) {
        scan.observed_cancelled = Some(ScanSummary {
            generation: scan.generation,
            phase: SessionPhase::Cancelled,
            progress,
        });
        return FreezeResult::default();
    }
    let publication = match reducer.publish_cancelled_snapshot(now) {
        Ok(publication) => publication,
        Err(reduction_error) => {
            scan.reduction_error = Some(reduction_error.clone());
            scan.discard_events = true;
            return FreezeResult {
                publication: None,
                error: Some(RuntimeError::Session(reduction_error)),
            };
        }
    };
    scan.observed_cancelled =
        Some(ScanSummary { generation: scan.generation, phase: SessionPhase::Cancelled, progress });
    match prepare_publication_with_navigation(publication, true, navigation) {
        Ok(publication) => FreezeResult { publication: Some(publication), error: None },
        Err(error) => FreezeResult { publication: None, error: Some(error) },
    }
}

fn drain_discarded_events(session: &ScanSession, budget: DrainBudget) -> (DrainReport, bool) {
    let started = Instant::now();
    let mut report = DrainReport {
        consumed: 0,
        applied: 0,
        stale: 0,
        elapsed: Duration::ZERO,
        stop: DrainStop::SourceEmpty,
    };
    let mut terminal_seen = false;
    loop {
        if report.consumed >= budget.max_events {
            report.stop = DrainStop::CountLimit;
            break;
        }
        report.elapsed = started.elapsed();
        if report.elapsed >= budget.max_duration {
            report.stop = DrainStop::TimeLimit;
            break;
        }
        match session.try_recv() {
            Ok(ScanEvent::Terminal(_)) => {
                report.consumed += 1;
                terminal_seen = true;
                report.stop = DrainStop::Terminal;
                break;
            }
            Ok(_) => report.consumed += 1,
            Err(ScanReceiveError::Empty | ScanReceiveError::Timeout) => {
                report.stop = DrainStop::SourceEmpty;
                break;
            }
            Err(ScanReceiveError::Disconnected) => {
                report.stop = DrainStop::Terminal;
                break;
            }
        }
    }
    (report, terminal_seen)
}

fn finalize_scan(
    mut scan: WorkerScan,
    now: Duration,
    navigation: Option<&NavigationSeed>,
    shared: &SupervisorShared,
) -> FinalizedScan {
    debug_assert!(scan.is_finished());
    let session = scan.session.take().expect("a live scan owns a session");
    let joined = session.join();
    let mut error = scan.reduction_error.clone().map(RuntimeError::Session);

    if let Some(mut summary) = scan.terminal_summary.take() {
        if scan.branch.is_some()
            && (scan.cancel_requested || scan.superseded || scan.cancel.is_cancelled())
            && !matches!(summary.phase, SessionPhase::Failed(_))
        {
            summary.phase = SessionPhase::Cancelled;
        }
        return match joined {
            Ok(_) => {
                let publication =
                    if !scan.cancel_requested && !scan.superseded && !scan.cancel.is_cancelled() {
                        match (scan.branch.as_ref(), scan.branch_publication.take()) {
                            (Some(branch), Some(publication)) => {
                                match prepare_branch_publication(
                                    branch,
                                    publication,
                                    navigation,
                                    &scan.cancel,
                                    shared,
                                ) {
                                    Ok(publication) => Some(publication),
                                    Err(RuntimeError::BranchMerge(BranchMergeError::Cancelled)) => {
                                        summary.phase = SessionPhase::Cancelled;
                                        None
                                    }
                                    Err(failure) => {
                                        summary.phase =
                                            SessionPhase::Failed(ScanFailure::ProtocolViolation {
                                                detail: "branch snapshot merge failed".into(),
                                            });
                                        error = Some(failure);
                                        None
                                    }
                                }
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                FinalizedScan { summary, publication, error }
            }
            Err(scan_error) => FinalizedScan {
                summary: ScanSummary {
                    generation: scan.generation,
                    phase: SessionPhase::Failed(scan_error.clone()),
                    progress: summary.progress,
                },
                publication: None,
                error: Some(RuntimeError::Scan(scan_error)),
            },
        };
    }
    let mut reducer = scan.reducer.take().expect("unfinished reduction owns a reducer");

    match joined {
        Ok(terminal) if !reducer.phase().is_terminal() => {
            if let Err(reduction_error) = reducer.apply_event(ScanEvent::Terminal(terminal)) {
                error = Some(RuntimeError::Session(reduction_error));
            }
        }
        Ok(_) => {}
        Err(scan_error) => {
            error = Some(RuntimeError::Scan(scan_error.clone()));
            return FinalizedScan {
                summary: ScanSummary {
                    generation: scan.generation,
                    phase: SessionPhase::Failed(scan_error),
                    progress: reducer.progress(),
                },
                publication: None,
                error,
            };
        }
    }

    let publication = if scan.observed_cancelled.is_none()
        && reducer.phase().is_terminal()
        && publication_generation_is_current(scan.generation, navigation)
    {
        match reducer.maybe_publish_snapshot(now, SnapshotPolicy::IMMEDIATE) {
            Ok(Some(publication)) => {
                let prepared = match scan.branch.as_ref() {
                    Some(branch)
                        if !scan.cancel_requested
                            && !scan.superseded
                            && matches!(
                                reducer.phase(),
                                SessionPhase::Complete | SessionPhase::Partial
                            ) =>
                    {
                        prepare_branch_publication(
                            branch,
                            publication,
                            navigation,
                            &scan.cancel,
                            shared,
                        )
                    }
                    Some(_) => {
                        return FinalizedScan {
                            summary: ScanSummary {
                                generation: scan.generation,
                                phase: reducer.phase().clone(),
                                progress: reducer.progress(),
                            },
                            publication: None,
                            error,
                        };
                    }
                    None => prepare_publication_with_navigation(publication, true, navigation),
                };
                match prepared {
                    Ok(prepared) => Some(prepared),
                    Err(RuntimeError::BranchMerge(BranchMergeError::Cancelled)) => None,
                    Err(preparation_error) => {
                        error = Some(preparation_error);
                        None
                    }
                }
            }
            Ok(None) => None,
            Err(reduction_error) => {
                error = Some(RuntimeError::Session(reduction_error));
                None
            }
        }
    } else {
        None
    };
    let phase = if (scan.cancel_requested || scan.cancel.is_cancelled())
        && !matches!(reducer.phase(), SessionPhase::Failed(_))
    {
        SessionPhase::Cancelled
    } else if scan.branch.is_some()
        && error.is_some()
        && matches!(reducer.phase(), SessionPhase::Complete | SessionPhase::Partial)
    {
        SessionPhase::Failed(ScanFailure::ProtocolViolation {
            detail: "branch snapshot merge failed".into(),
        })
    } else {
        reducer.phase().clone()
    };
    let summary = ScanSummary { generation: scan.generation, phase, progress: reducer.progress() };
    FinalizedScan { summary, publication, error }
}

fn prepare_branch_publication(
    branch: &BranchRescanIntent,
    publication: SnapshotPublication,
    navigation: Option<&NavigationSeed>,
    cancel: &CancelToken,
    _shared: &SupervisorShared,
) -> Result<PreparedPublication, RuntimeError> {
    #[cfg(test)]
    branch_publication_checkpoint(_shared, BranchPublicationStage::Merge);
    let snapshot = merge_branch_snapshot_cancellable(
        &branch.snapshot,
        branch.target,
        &publication.snapshot,
        || cancel.is_cancelled(),
    )
    .map_err(RuntimeError::BranchMerge)?;
    #[cfg(test)]
    branch_publication_checkpoint(_shared, BranchPublicationStage::Presentation);
    prepare_publication_with_navigation_cancellable(
        SnapshotPublication { snapshot: Arc::new(snapshot), ..publication },
        true,
        navigation,
        || cancel.is_cancelled(),
    )
    .and_then(|prepared| prepared.ok_or(RuntimeError::BranchMerge(BranchMergeError::Cancelled)))
}

#[cfg(test)]
fn prepare_publication(
    publication: SnapshotPublication,
    terminal: bool,
) -> Result<PreparedPublication, RuntimeError> {
    prepare_publication_with_navigation(publication, terminal, None)
}

fn prepare_publication_with_navigation(
    publication: SnapshotPublication,
    terminal: bool,
    seed: Option<&NavigationSeed>,
) -> Result<PreparedPublication, RuntimeError> {
    Ok(prepare_publication_with_navigation_cancellable(publication, terminal, seed, || false)?
        .expect("an uncancelled publication produces a frame"))
}

fn prepare_publication_with_navigation_cancellable(
    publication: SnapshotPublication,
    terminal: bool,
    seed: Option<&NavigationSeed>,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<PreparedPublication>, RuntimeError> {
    let Some(presentation) =
        SnapshotPresentation::new_cancellable(&publication.snapshot, &mut cancelled)?
    else {
        return Ok(None);
    };
    let presentation = Arc::new(presentation);
    let (navigation, navigation_epoch) = match seed {
        Some(seed) if seed.navigation.generation() <= publication.snapshot.generation() => {
            let mut navigation = seed.navigation.as_ref().clone();
            navigation.replace_snapshot(Arc::clone(&publication.snapshot))?;
            (Arc::new(navigation), seed.epoch)
        }
        Some(_) | None => (Arc::new(NavigationState::new(Arc::clone(&publication.snapshot))?), 0),
    };
    if cancelled() {
        return Ok(None);
    }
    let hidden = navigation.hidden_branches_plan();
    Ok(Some(PreparedPublication {
        revision: DisplayedRevision {
            generation: publication.snapshot.generation(),
            revision: publication.revision,
            terminal,
        },
        snapshot: publication.snapshot,
        navigation,
        hidden,
        navigation_epoch,
        presentation,
    }))
}

fn lock_supervisor(shared: &SupervisorShared) -> MutexGuard<'_, SupervisorSharedState> {
    shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mark_supervisor_control(shared: &mut SupervisorSharedState) {
    shared.control_epoch = shared.control_epoch.wrapping_add(1);
}

fn set_supervisor_waiting(shared: &mut SupervisorSharedState, waiting: bool) {
    #[cfg(test)]
    {
        shared.supervisor_waiting = waiting;
    }
    #[cfg(not(test))]
    {
        let _ = (shared, waiting);
    }
}

#[cfg(test)]
struct FrameRetirementProbe(Arc<Mutex<Option<String>>>);

#[cfg(test)]
impl Drop for FrameRetirementProbe {
    fn drop(&mut self) {
        *self.0.lock().expect("frame retirement probe lock") =
            thread::current().name().map(str::to_owned);
    }
}

struct FrameData {
    revision: DisplayedRevision,
    snapshot: Arc<TreeSnapshot>,
    navigation: Arc<NavigationState>,
    presentation: Arc<SnapshotPresentation>,
    #[cfg(test)]
    retirement_probe: Option<FrameRetirementProbe>,
}

struct CommittedFrame {
    data: FrameData,
    layout: SunburstLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedLayout {
    id: LayoutRequestId,
    revision: DisplayedRevision,
    root: diskpie_core::NodeId,
    size_basis: SizeBasis,
}

struct StagedFrame {
    data: FrameData,
    expected: ExpectedLayout,
    snapshot_changed: bool,
}

struct QueuedFrame {
    data: FrameData,
    hidden: HiddenBranchesPlan,
    snapshot_changed: bool,
}

struct RuntimeReapPayload {
    pending: Option<PendingLaunch>,
    layout_service: Option<LayoutService>,
    committed: Option<CommittedFrame>,
    staged: Option<StagedFrame>,
    queued: Option<QueuedFrame>,
}

impl RuntimeReapPayload {
    fn dispose(self) {
        let Self { pending, layout_service, committed, staged, queued } = self;
        drop(pending);
        drop(queued);
        drop(staged);
        drop(committed);
        drop(layout_service);
    }
}

struct ReservedRuntimeReap {
    permit: ReaperPermit,
    payload: RuntimeReapPayload,
}

impl ReservedRuntimeReap {
    fn submit(self) {
        let Self { permit, payload } = self;
        permit.submit(ReapJob::runtime(payload));
    }
}

enum ReapJob {
    Value(Box<dyn FnOnce() + Send>),
    Runtime(Box<RuntimeReapPayload>),
}

impl ReapJob {
    fn new<T>(value: T) -> Self
    where
        T: Send + 'static,
    {
        Self::Value(Box::new(move || drop(value)))
    }

    fn runtime(value: RuntimeReapPayload) -> Self {
        Self::Runtime(Box::new(value))
    }

    fn batch(jobs: Vec<Self>) -> Self {
        Self::Value(Box::new(move || {
            for job in jobs {
                job.dispose();
            }
        }))
    }

    fn dispose(self) {
        match self {
            Self::Value(dropper) => dropper(),
            Self::Runtime(payload) => (*payload).dispose(),
        }
    }
}

struct ReservedReapBatch {
    permit: Option<ReaperPermit>,
    jobs: Vec<ReapJob>,
}

impl ReservedReapBatch {
    fn new(permit: ReaperPermit) -> Self {
        Self { permit: Some(permit), jobs: Vec::new() }
    }

    fn push<T>(&mut self, value: T)
    where
        T: Send + 'static,
    {
        self.jobs.push(ReapJob::new(value));
    }
}

impl Drop for ReservedReapBatch {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        if self.jobs.is_empty() {
            drop(permit);
        } else {
            permit.submit(ReapJob::batch(mem::take(&mut self.jobs)));
        }
    }
}

struct ReaperState {
    queue: VecDeque<ReapJob>,
    available: usize,
    reserved: usize,
    active: usize,
    handles: usize,
    stopped: bool,
}

impl Default for ReaperState {
    fn default() -> Self {
        Self {
            queue: VecDeque::with_capacity(REAPER_TOTAL_CAPACITY),
            available: REAPER_TOTAL_CAPACITY,
            reserved: 0,
            active: 0,
            handles: 1,
            stopped: false,
        }
    }
}

#[derive(Default)]
struct ReaperShared {
    state: Mutex<ReaperState>,
    work: Condvar,
    capacity: Condvar,
}

struct BoundedReaper {
    shared: Arc<ReaperShared>,
}

impl Clone for BoundedReaper {
    fn clone(&self) -> Self {
        let mut state = lock_reaper(&self.shared);
        debug_assert!(!state.stopped);
        state.handles += 1;
        drop(state);
        Self { shared: Arc::clone(&self.shared) }
    }
}

impl Drop for BoundedReaper {
    fn drop(&mut self) {
        let mut state = lock_reaper(&self.shared);
        state.handles = state.handles.saturating_sub(1);
        drop(state);
        self.shared.work.notify_one();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReaperReserveError {
    Full,
    Stopped,
}

#[cfg(test)]
struct ReapRejected {
    reason: ReaperReserveError,
    job: ReapJob,
}

struct ReaperPermit {
    shared: Arc<ReaperShared>,
    live: bool,
}

impl ReaperPermit {
    fn submit(mut self, job: ReapJob) {
        let mut state = lock_reaper(&self.shared);
        debug_assert!(!state.stopped, "a live permit keeps the reaper worker alive");
        state.reserved = state.reserved.saturating_sub(1);
        state.queue.push_back(job);
        self.live = false;
        drop(state);
        self.shared.work.notify_one();
    }
}

impl Drop for ReaperPermit {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut state = lock_reaper(&self.shared);
        state.reserved = state.reserved.saturating_sub(1);
        state.available += 1;
        self.live = false;
        drop(state);
        self.shared.capacity.notify_one();
        self.shared.work.notify_one();
    }
}

impl BoundedReaper {
    fn new() -> io::Result<(Self, JoinHandle<()>)> {
        let shared = Arc::new(ReaperShared::default());
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("diskpie-value-reaper".to_owned())
            .spawn(move || reaper_loop(&worker_shared))?;
        Ok((Self { shared }, worker))
    }

    fn try_reserve(&self) -> Result<ReaperPermit, ReaperReserveError> {
        let mut state = lock_reaper(&self.shared);
        if state.stopped {
            return Err(ReaperReserveError::Stopped);
        }
        if state.available == 0 {
            return Err(ReaperReserveError::Full);
        }
        state.available -= 1;
        state.reserved += 1;
        drop(state);
        Ok(ReaperPermit { shared: Arc::clone(&self.shared), live: true })
    }

    fn reserve(&self) -> Result<ReaperPermit, ReaperReserveError> {
        let mut state = lock_reaper(&self.shared);
        while state.available == 0 && !state.stopped {
            state =
                self.shared.capacity.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if state.stopped {
            return Err(ReaperReserveError::Stopped);
        }
        state.available -= 1;
        state.reserved += 1;
        drop(state);
        Ok(ReaperPermit { shared: Arc::clone(&self.shared), live: true })
    }

    #[cfg(test)]
    fn try_submit(&self, job: ReapJob) -> Result<(), ReapRejected> {
        match self.try_reserve() {
            Ok(permit) => {
                permit.submit(job);
                Ok(())
            }
            Err(reason) => Err(ReapRejected { reason, job }),
        }
    }
}

fn reaper_loop(shared: &ReaperShared) {
    loop {
        let job = {
            let mut state = lock_reaper(shared);
            while state.queue.is_empty() && (state.handles != 0 || state.reserved != 0) {
                state = shared.work.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.queue.is_empty() {
                state.stopped = true;
                drop(state);
                shared.capacity.notify_all();
                return;
            }
            let job = state.queue.pop_front().expect("nonempty reaper queue");
            state.active += 1;
            debug_assert_eq!(state.active, 1);
            drop(state);
            job
        };
        let _ = catch_unwind(AssertUnwindSafe(|| job.dispose()));
        let mut state = lock_reaper(shared);
        debug_assert_eq!(state.active, 1);
        state.active -= 1;
        state.available += 1;
        drop(state);
        shared.capacity.notify_one();
    }
}

fn lock_reaper(shared: &ReaperShared) -> MutexGuard<'_, ReaperState> {
    shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
struct SessionReaper {
    sender: SyncSender<Vec<WorkerScan>>,
}

impl SessionReaper {
    fn new() -> io::Result<(Self, JoinHandle<()>)> {
        let (sender, receiver) = sync_channel::<Vec<WorkerScan>>(1);
        let worker =
            thread::Builder::new().name("diskpie-session-reaper".to_owned()).spawn(move || {
                while let Ok(scans) = receiver.recv() {
                    let _ = catch_unwind(AssertUnwindSafe(|| drop(scans)));
                }
            })?;
        Ok((Self { sender }, worker))
    }

    fn submit(&self, scans: Vec<WorkerScan>) -> Result<(), Vec<WorkerScan>> {
        self.sender.send(scans).map_err(|error| error.0)
    }
}

/// UI-independent owner of bounded scan supervision and atomic immutable frames.
pub struct RuntimeController {
    config: RuntimeConfig,
    supervisor: SupervisorHandle,
    layout_service: Option<LayoutService>,
    committed: Option<CommittedFrame>,
    staged: Option<StagedFrame>,
    queued: Option<QueuedFrame>,
    last_generation: u64,
    navigation_epoch: u64,
    last_clock: Option<Duration>,
    last_error: Option<RuntimeError>,
    shutting_down: bool,
}

impl fmt::Debug for RuntimeController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeController")
            .field("state", &self.state())
            .field("last_generation", &self.last_generation)
            .field("displayed_revision", &self.displayed_revision())
            .field("has_frame", &self.committed.is_some())
            .field("has_staged_frame", &self.staged.is_some())
            .field("has_queued_frame", &self.queued.is_some())
            .field("last_error", &self.last_error)
            .finish_non_exhaustive()
    }
}

impl RuntimeController {
    /// Validates all bounded policy before starting the supervisor or layout worker.
    pub fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        validate_runtime_config(&config)?;
        let layout_service = LayoutService::new().map_err(|error| {
            RuntimeError::LayoutWorkerStart { kind: error.kind(), detail: error.to_string() }
        })?;
        let supervisor = match SupervisorHandle::new(config.clone()) {
            Ok(supervisor) => supervisor,
            Err(error) => {
                drop(layout_service);
                return Err(error);
            }
        };
        Ok(Self {
            config,
            supervisor,
            layout_service: Some(layout_service),
            committed: None,
            staged: None,
            queued: None,
            last_generation: 0,
            navigation_epoch: 0,
            last_clock: None,
            last_error: None,
            shutting_down: false,
        })
    }

    /// Validates roots before consuming a generation or cancelling existing work.
    pub fn request_scan(
        &mut self,
        fs: Arc<dyn ScanFs>,
        roots: Vec<ScanRoot>,
        cancel: CancelToken,
    ) -> Result<LaunchReceipt, ScanLaunchRejected> {
        self.request_scan_with_omissions(fs, roots, cancel, 0)
    }

    /// Launches valid roots while retaining a count of roots the resolver omitted.
    /// Omitted-root details belong to the resolver; scan protocol counters remain exact.
    pub fn request_scan_with_omissions(
        &mut self,
        fs: Arc<dyn ScanFs>,
        roots: Vec<ScanRoot>,
        cancel: CancelToken,
        initial_omissions: u64,
    ) -> Result<LaunchReceipt, ScanLaunchRejected> {
        if self.shutting_down {
            return Err(ScanLaunchRejected {
                error: RuntimeError::ShuttingDown,
                fs,
                roots,
                cancel,
            });
        }
        if let Err(error) = validate_roots(&roots) {
            return Err(ScanLaunchRejected { error, fs, roots, cancel });
        }
        let Some(sequence) = self.last_generation.checked_add(1) else {
            return Err(ScanLaunchRejected {
                error: RuntimeError::GenerationExhausted,
                fs,
                roots,
                cancel,
            });
        };
        let generation = ScanGeneration::new(sequence);
        let disposition = match self.supervisor.enqueue_launch(PendingLaunch {
            generation,
            fs,
            roots,
            cancel,
            branch: None,
            initial_omissions,
        }) {
            Ok(disposition) => disposition,
            Err(PendingLaunchRejected { error, launch }) => {
                let launch = *launch;
                return Err(ScanLaunchRejected {
                    error,
                    fs: launch.fs,
                    roots: launch.roots,
                    cancel: launch.cancel,
                });
            }
        };
        self.last_generation = sequence;
        Ok(LaunchReceipt { generation, disposition })
    }

    /// Captures a branch target only after a terminal revision has committed.
    pub fn branch_rescan_intent(&self, target: NodeId) -> Result<BranchRescanIntent, RuntimeError> {
        self.ensure_branch_settled()?;
        let frame = self.committed.as_ref().ok_or(RuntimeError::NoNavigation)?;
        if !frame
            .data
            .snapshot
            .node(target)
            .is_some_and(|record| matches!(record.kind(), EntryKind::Root | EntryKind::Directory))
        {
            return Err(RuntimeError::BranchMerge(BranchMergeError::InvalidTarget));
        }
        Ok(BranchRescanIntent {
            snapshot: Arc::clone(&frame.data.snapshot),
            revision: frame.data.revision.revision,
            target,
        })
    }

    /// Launches one native branch, retaining the displayed tree until a complete
    /// or recoverably partial replacement and its layout can commit together.
    pub fn request_branch_scan(
        &mut self,
        intent: BranchRescanIntent,
        fs: Arc<dyn ScanFs>,
        root: ScanRoot,
        cancel: CancelToken,
    ) -> Result<LaunchReceipt, BranchScanLaunchRejected> {
        let validation = self.ensure_branch_settled().and_then(|()| {
            let frame = self.committed.as_ref().ok_or(RuntimeError::NoNavigation)?;
            if !Arc::ptr_eq(&intent.snapshot, &frame.data.snapshot)
                || intent.revision != frame.data.revision.revision
            {
                return Err(RuntimeError::StaleBranchIntent);
            }
            if intent.native_path().ok().as_ref() != Some(&root.path) {
                return Err(RuntimeError::StaleBranchIntent);
            }
            validate_roots(std::slice::from_ref(&root))
        });
        if let Err(error) = validation {
            return Err(BranchScanLaunchRejected::new(error, intent, fs, root, cancel));
        }
        let Some(sequence) = self.last_generation.checked_add(1) else {
            return Err(BranchScanLaunchRejected::new(
                RuntimeError::GenerationExhausted,
                intent,
                fs,
                root,
                cancel,
            ));
        };
        let generation = ScanGeneration::new(sequence);
        match self.supervisor.enqueue_launch(PendingLaunch {
            generation,
            fs,
            roots: vec![root],
            cancel,
            branch: Some(intent),
            initial_omissions: 0,
        }) {
            Ok(disposition) => {
                self.last_generation = sequence;
                Ok(LaunchReceipt { generation, disposition })
            }
            Err(PendingLaunchRejected { error, launch }) => {
                let PendingLaunch { fs, mut roots, cancel, branch, .. } = *launch;
                Err(BranchScanLaunchRejected::new(
                    error,
                    branch.expect("branch launch"),
                    fs,
                    roots.pop().expect("one branch root"),
                    cancel,
                ))
            }
        }
    }

    fn ensure_branch_settled(&self) -> Result<(), RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::ShuttingDown);
        }
        let shared = lock_supervisor(&self.supervisor.shared);
        shared.branch_scan_readiness()?;
        if self.staged.is_some()
            || self.queued.is_some()
            || !self.committed.as_ref().is_some_and(|frame| frame.data.revision.terminal)
        {
            return Err(RuntimeError::ScanNotSettled);
        }
        Ok(())
    }

    /// Requests cancellation without waiting for a filesystem provider.
    pub fn cancel_active(&mut self) -> bool {
        if self.shutting_down {
            return false;
        }
        self.supervisor.cancel_active()
    }

    /// Cancels every scan and rejects future scan or navigation commands.
    pub fn request_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.supervisor.request_shutdown();
    }

    /// Posts the caller clock and drains only values already built by background workers.
    pub fn tick<C>(&mut self, mut clock: C) -> Result<TickReport, RuntimeError>
    where
        C: FnMut() -> Duration,
    {
        let result = self.tick_inner(clock());
        if let Err(error) = &result {
            self.last_error = Some(error.clone());
        }
        result
    }

    /// Executes a generation-sealed navigation command without scanner I/O.
    pub fn execute_navigation(
        &mut self,
        command: NavigationCommand,
    ) -> Result<NavigationReport, RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::ShuttingDown);
        }
        let result = self
            .reserve_ui_retirement()
            .and_then(|mut retired| self.execute_navigation_inner(command, &mut retired));
        if let Err(error) = &result {
            self.last_error = Some(error.clone());
        }
        result
    }

    /// Creates a command for the committed frame. Commands are rejected after shutdown.
    pub fn navigation_command(
        &self,
        action: crate::navigation::NavigationAction,
    ) -> Result<NavigationCommand, RuntimeError> {
        if self.shutting_down {
            return Err(RuntimeError::ShuttingDown);
        }
        self.navigation()
            .map(|navigation| navigation.command(action))
            .ok_or(RuntimeError::NoNavigation)
    }

    #[must_use]
    pub fn state(&self) -> RuntimeState {
        let status = self.supervisor.status();
        if self.shutting_down {
            if self.is_shutdown_complete_from(&status) {
                return RuntimeState::ShutdownComplete;
            }
            return RuntimeState::ShuttingDown {
                generation: status
                    .active
                    .as_ref()
                    .map(|active| active.generation)
                    .or_else(|| status.reserved.as_ref().map(|reserved| reserved.generation))
                    .or(status.newest_retiring),
            };
        }
        if let Some(active) = status.active {
            if active.cancel_requested {
                RuntimeState::Cancelling { generation: active.generation, pending: status.pending }
            } else {
                RuntimeState::Active { generation: active.generation, phase: active.phase }
            }
        } else if let Some(reserved) = status.reserved {
            RuntimeState::Active {
                generation: reserved.generation,
                phase: SessionPhase::AwaitingStart,
            }
        } else if let Some(summary) = status.settled.as_ref().filter(|summary| {
            status.newest_retiring.is_none_or(|retiring| summary.generation > retiring)
        }) {
            RuntimeState::Settled { generation: summary.generation, phase: summary.phase.clone() }
        } else if status.retiring != 0 {
            RuntimeState::Cancelling {
                generation: status.newest_retiring.expect("retiring scan has a generation"),
                pending: status.pending,
            }
        } else if let Some(summary) = status.settled {
            RuntimeState::Settled { generation: summary.generation, phase: summary.phase }
        } else {
            RuntimeState::Idle
        }
    }

    #[must_use]
    pub fn phase(&self) -> Option<SessionPhase> {
        let status = self.supervisor.status();
        status
            .active
            .map(|active| active.phase)
            .or_else(|| status.reserved.map(|_| SessionPhase::AwaitingStart))
            .or_else(|| status.settled.map(|summary| summary.phase))
    }

    #[must_use]
    pub fn progress(&self) -> Option<SessionProgress> {
        let status = self.supervisor.status();
        status
            .active
            .map(|active| active.progress)
            .or_else(|| status.settled.map(|summary| summary.progress))
    }

    #[must_use]
    pub fn settled_summary(&self) -> Option<ScanSummary> {
        self.supervisor.status().settled
    }

    #[must_use]
    pub fn active_generation(&self) -> Option<ScanGeneration> {
        self.supervisor.status().active.map(|active| active.generation)
    }

    #[must_use]
    pub fn pending_generation(&self) -> Option<ScanGeneration> {
        let shared = lock_supervisor(&self.supervisor.shared);
        shared.inbox.launch.as_ref().map(|launch| launch.generation).or(shared.status.pending)
    }

    #[must_use]
    pub fn retiring_scans(&self) -> usize {
        self.supervisor.status().retiring
    }

    #[must_use]
    pub fn cancel_token(&self) -> Option<CancelToken> {
        let status = self.supervisor.status();
        status
            .active
            .map(|active| active.cancel)
            .or_else(|| status.reserved.map(|reserved| reserved.cancel))
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<&TreeSnapshot> {
        self.committed.as_ref().map(|frame| frame.data.snapshot.as_ref())
    }

    #[must_use]
    pub fn snapshot_handle(&self) -> Option<Arc<TreeSnapshot>> {
        self.committed.as_ref().map(|frame| Arc::clone(&frame.data.snapshot))
    }

    #[must_use]
    pub fn displayed_revision(&self) -> Option<u64> {
        self.committed.as_ref().map(|frame| frame.data.revision.revision)
    }

    #[must_use]
    pub fn navigation(&self) -> Option<&NavigationState> {
        self.committed.as_ref().map(|frame| frame.data.navigation.as_ref())
    }

    #[must_use]
    pub fn presentation(&self) -> Option<&SnapshotPresentation> {
        self.committed.as_ref().map(|frame| frame.data.presentation.as_ref())
    }

    #[must_use]
    pub fn layout(&self) -> Option<&SunburstLayout> {
        self.committed.as_ref().map(|frame| &frame.layout)
    }

    #[must_use]
    pub fn layout_request_id(&self) -> Option<LayoutRequestId> {
        self.staged.as_ref().map(|staged| staged.expected.id)
    }

    #[must_use]
    pub const fn last_error(&self) -> Option<&RuntimeError> {
        self.last_error.as_ref()
    }

    pub fn clear_error(&mut self) {
        self.last_error = None;
    }

    #[must_use]
    /// Returns true once scan-execution ownership has been handed to background retirement.
    /// This does not promise that a blocked filesystem provider has returned.
    pub fn is_shutdown_complete(&self) -> bool {
        self.is_shutdown_complete_from(&self.supervisor.status())
    }

    #[must_use]
    pub fn needs_repaint(&self) -> bool {
        let status = self.supervisor.status();
        if self.is_shutdown_complete_from(&status) {
            return false;
        }
        if self.shutting_down && !status.shutdown_ack {
            return true;
        }
        status.active.is_some()
            || status.reserved.is_some()
            || status.retiring != 0
            || status.pending.is_some()
            || self.pending_generation().is_some()
            || self.supervisor.has_output()
            || self.layout_service.as_ref().is_some_and(LayoutService::is_busy)
            || self.staged.is_some()
            || self.queued.is_some()
    }

    fn tick_inner(&mut self, now: Duration) -> Result<TickReport, RuntimeError> {
        self.observe_clock(now)?;
        self.supervisor.pulse(now);
        let mut retired = self.reserve_ui_retirement()?;
        let mut report = TickReport::new(self.state());
        let mut output = self.supervisor.take_output(now);
        report.drain = output.drain;
        report.scan_started = output.scan_started;
        report.scan_finished = output.scan_finished;

        if let Some(publication) = output.publication.take() {
            if self.shutting_down {
                retired.push(publication);
            } else {
                self.accept_prepared(publication, now, &mut report, &mut retired)?;
            }
        }
        if !self.shutting_down {
            self.take_layout_completion(&mut report, &mut retired)?;
        }

        report.needs_repaint = self.needs_repaint()
            || report.snapshot_published
            || report.layout_completed
            || report.scan_started.is_some()
            || report.scan_finished.is_some();
        report.state = self.state();
        if let Some(error) = output.error {
            return Err(error);
        }
        Ok(report)
    }

    fn execute_navigation_inner(
        &mut self,
        command: NavigationCommand,
        retired: &mut ReservedReapBatch,
    ) -> Result<NavigationReport, RuntimeError> {
        #[derive(Clone, Copy, Eq, PartialEq)]
        enum Source {
            Queued,
            Staged,
            Committed,
        }

        let committed = self.committed.as_ref().ok_or(RuntimeError::NoNavigation)?;
        let (source, data, snapshot_changed) = if let Some(queued) = &self.queued {
            (Source::Queued, &queued.data, queued.snapshot_changed)
        } else if let Some(staged) = &self.staged {
            (Source::Staged, &staged.data, staged.snapshot_changed)
        } else {
            (Source::Committed, &committed.data, false)
        };
        if data.revision.generation != command.generation() {
            return Err(RuntimeError::FrameTransitionPending);
        }
        let source_revision = data.revision;
        let source_snapshot = Arc::clone(&data.snapshot);
        let source_presentation = Arc::clone(&data.presentation);
        let mut navigation = data.navigation.as_ref().clone();
        let outcome = navigation.execute(command)?;
        let navigation = Arc::new(navigation);
        if matches!(outcome, CommandOutcome::Applied(_)) {
            self.navigation_epoch =
                self.navigation_epoch.checked_add(1).ok_or(RuntimeError::GenerationExhausted)?;
        }
        let needs_layout = matches!(
            outcome,
            CommandOutcome::Applied(
                NavigationChange::ViewRoot
                    | NavigationChange::SelectionAndViewRoot
                    | NavigationChange::HiddenBranches
                    | NavigationChange::SizeBasis
            )
        );
        let layout_submitted = if needs_layout {
            let data = FrameData {
                revision: source_revision,
                snapshot: source_snapshot,
                navigation,
                presentation: source_presentation,
                #[cfg(test)]
                retirement_probe: None,
            };
            self.stage_frame(data, snapshot_changed, retired)?
        } else {
            if matches!(outcome, CommandOutcome::Applied(NavigationChange::Selection)) {
                let replaced = match source {
                    Source::Queued => mem::replace(
                        &mut self.queued.as_mut().expect("queued source exists").data.navigation,
                        Arc::clone(&navigation),
                    ),
                    Source::Staged => mem::replace(
                        &mut self.staged.as_mut().expect("staged source exists").data.navigation,
                        Arc::clone(&navigation),
                    ),
                    Source::Committed => mem::replace(
                        &mut self
                            .committed
                            .as_mut()
                            .expect("committed source exists")
                            .data
                            .navigation,
                        Arc::clone(&navigation),
                    ),
                };
                retired.push(replaced);
                for (kind, frame) in [
                    (Source::Queued, self.queued.as_mut().map(|queued| &mut queued.data)),
                    (Source::Staged, self.staged.as_mut().map(|staged| &mut staged.data)),
                    (Source::Committed, self.committed.as_mut().map(|frame| &mut frame.data)),
                ] {
                    let Some(frame) = frame else {
                        continue;
                    };
                    if kind == source || frame.revision.generation != command.generation() {
                        continue;
                    }
                    let state = Arc::make_mut(&mut frame.navigation);
                    let selection = state.command(command.action());
                    let _ = state.execute(selection);
                }
                self.supervisor.update_navigation(
                    NavigationSeed { epoch: self.navigation_epoch, navigation },
                    retired,
                );
            }
            None
        };
        let needs_repaint = !matches!(outcome, CommandOutcome::Unchanged);
        Ok(NavigationReport { outcome, layout_submitted, needs_repaint })
    }

    fn accept_prepared(
        &mut self,
        publication: PreparedPublication,
        _now: Duration,
        report: &mut TickReport,
        retired: &mut ReservedReapBatch,
    ) -> Result<(), RuntimeError> {
        if publication.revision.generation < GenerationId::new(self.last_generation) {
            retired.push(publication);
            return Ok(());
        }
        if let Some(committed) = &self.committed {
            if publication.revision.generation < committed.data.revision.generation {
                retired.push(publication);
                return Ok(());
            }
            if publication.revision <= committed.data.revision {
                retired.push(publication);
                return Ok(());
            }
        }
        if self.staged.as_ref().is_some_and(|staged| publication.revision <= staged.data.revision) {
            retired.push(publication);
            return Ok(());
        }
        if self.queued.as_ref().is_some_and(|queued| publication.revision <= queued.data.revision) {
            retired.push(publication);
            return Ok(());
        }
        if publication.navigation_epoch < self.navigation_epoch {
            self.supervisor.request_remap(publication, retired);
            return Ok(());
        }

        report.layout_submitted = self.stage_publication(publication, retired)?;
        Ok(())
    }

    fn stage_publication(
        &mut self,
        publication: PreparedPublication,
        retired: &mut ReservedReapBatch,
    ) -> Result<Option<LayoutRequestId>, RuntimeError> {
        self.navigation_epoch = self.navigation_epoch.max(publication.navigation_epoch);
        let data = FrameData {
            revision: publication.revision,
            snapshot: publication.snapshot,
            navigation: publication.navigation,
            presentation: publication.presentation,
            #[cfg(test)]
            retirement_probe: None,
        };
        self.submit_or_queue(
            QueuedFrame { data, hidden: publication.hidden, snapshot_changed: true },
            retired,
        )
    }

    fn stage_frame(
        &mut self,
        data: FrameData,
        snapshot_changed: bool,
        retired: &mut ReservedReapBatch,
    ) -> Result<Option<LayoutRequestId>, RuntimeError> {
        let hidden = data.navigation.hidden_branches_plan();
        self.submit_or_queue(QueuedFrame { data, hidden, snapshot_changed }, retired)
    }

    fn submit_or_queue(
        &mut self,
        frame: QueuedFrame,
        retired: &mut ReservedReapBatch,
    ) -> Result<Option<LayoutRequestId>, RuntimeError> {
        if let Some(completion) = self.layout_service.as_ref().and_then(LayoutService::try_take) {
            if let Some(staged) = self.staged.take() {
                retired.push(staged);
            }
            retired.push(completion);
        }
        if self.staged.is_some() {
            self.queue_latest(frame, retired);
            return Ok(None);
        }
        if let Some(replaced) = self.queued.take() {
            retired.push(replaced);
        }
        self.submit_frame(frame, retired).map(Some)
    }

    fn queue_latest(&mut self, frame: QueuedFrame, retired: &mut ReservedReapBatch) {
        if let Some(replaced) = self.queued.replace(frame) {
            retired.push(replaced);
        }
    }

    fn submit_frame(
        &mut self,
        frame: QueuedFrame,
        retired: &mut ReservedReapBatch,
    ) -> Result<LayoutRequestId, RuntimeError> {
        debug_assert!(self.staged.is_none());
        let QueuedFrame { data, hidden, snapshot_changed } = frame;
        let mut options = self.config.layout_options;
        options.root = data.navigation.view_root();
        options.size_basis = data.navigation.size_basis();
        let service = self.layout_service.as_mut().ok_or(RuntimeError::SupervisorStopped)?;
        let id = match service.submit(Arc::clone(&data.snapshot), options, hidden) {
            Ok(id) => id,
            Err(rejection) => {
                let error = rejection.error();
                retired.push(rejection);
                retired.push(data);
                return Err(RuntimeError::LayoutSubmit(error));
            }
        };
        self.staged = Some(StagedFrame {
            expected: ExpectedLayout {
                id,
                revision: data.revision,
                root: options.root,
                size_basis: options.size_basis,
            },
            data,
            snapshot_changed,
        });
        let navigation =
            self.staged.as_ref().expect("newly staged frame exists").data.navigation.clone();
        self.supervisor.update_navigation(
            NavigationSeed { epoch: self.navigation_epoch, navigation },
            retired,
        );
        Ok(id)
    }

    fn take_layout_completion(
        &mut self,
        report: &mut TickReport,
        retired: &mut ReservedReapBatch,
    ) -> Result<(), RuntimeError> {
        let completion = self.layout_service.as_ref().and_then(LayoutService::try_take);
        let Some(completion) = completion else {
            return self.submit_queued_if_idle(report, retired);
        };
        let Some(staged) = self.staged.take() else {
            retired.push(completion);
            report.discarded_layout_completion = true;
            return Ok(());
        };
        if completion.id != staged.expected.id {
            self.staged = Some(staged);
            retired.push(completion);
            report.discarded_layout_completion = true;
            return Ok(());
        }
        if completion.source_generation != staged.expected.revision.generation
            || completion.root != staged.expected.root
            || completion.size_basis != staged.expected.size_basis
        {
            retired.push(staged);
            retired.push(completion);
            self.restore_committed_navigation(retired);
            return Err(RuntimeError::LayoutCompletionMismatch);
        }
        if let Some(queued) = self.queued.take() {
            retired.push(staged);
            retired.push(completion);
            report.discarded_layout_completion = true;
            report.layout_submitted = Some(self.submit_frame(queued, retired)?);
            return Ok(());
        }
        if staged.snapshot_changed
            && staged.data.revision.generation < GenerationId::new(self.last_generation)
        {
            retired.push(staged);
            retired.push(completion);
            self.restore_committed_navigation(retired);
            report.discarded_layout_completion = true;
            return Ok(());
        }
        let layout = match completion.result {
            Ok(layout) => layout,
            Err(error) => {
                retired.push(staged);
                self.restore_committed_navigation(retired);
                return Err(RuntimeError::LayoutTask(error));
            }
        };
        report.snapshot_published = staged.snapshot_changed;
        report.layout_completed = true;
        report.needs_repaint = true;
        let replaced = self.committed.replace(CommittedFrame { data: staged.data, layout });
        if let Some(replaced) = replaced {
            retired.push(replaced);
        }
        Ok(())
    }

    fn submit_queued_if_idle(
        &mut self,
        report: &mut TickReport,
        retired: &mut ReservedReapBatch,
    ) -> Result<(), RuntimeError> {
        if self.staged.is_some() || self.layout_service.as_ref().is_some_and(LayoutService::is_busy)
        {
            return Ok(());
        }
        if let Some(orphaned) = self.layout_service.as_ref().and_then(LayoutService::try_take) {
            retired.push(orphaned);
        }
        if let Some(queued) = self.queued.take() {
            report.layout_submitted = Some(self.submit_frame(queued, retired)?);
        }
        Ok(())
    }

    fn restore_committed_navigation(&mut self, retired: &mut ReservedReapBatch) {
        let Some(committed) = &self.committed else {
            return;
        };
        self.navigation_epoch = self.navigation_epoch.saturating_add(1);
        self.supervisor.update_navigation(
            NavigationSeed {
                epoch: self.navigation_epoch,
                navigation: committed.data.navigation.clone(),
            },
            retired,
        );
    }

    fn reserve_ui_retirement(&mut self) -> Result<ReservedReapBatch, RuntimeError> {
        match self.supervisor.reaper.try_reserve() {
            Ok(permit) => Ok(ReservedReapBatch::new(permit)),
            Err(ReaperReserveError::Full) => Err(RuntimeError::RetirementBackpressure),
            Err(ReaperReserveError::Stopped) => {
                self.shutting_down = true;
                self.supervisor.mark_reaper_stopped();
                Err(RuntimeError::RetirementWorkerStopped)
            }
        }
    }

    fn observe_clock(&mut self, now: Duration) -> Result<(), RuntimeError> {
        if let Some(previous) = self.last_clock
            && now < previous
        {
            return Err(RuntimeError::ClockWentBackwards { previous, current: now });
        }
        self.last_clock = Some(now);
        Ok(())
    }

    fn is_shutdown_complete_from(&self, status: &SupervisorStatus) -> bool {
        self.shutting_down
            && status.shutdown_ack
            && status.quiescent
            && !self.supervisor.has_output()
            && self.layout_service.as_ref().is_none_or(|service| !service.is_busy())
    }
}

impl Drop for RuntimeController {
    fn drop(&mut self) {
        self.shutting_down = true;
        let reap = RuntimeReapPayload {
            pending: None,
            layout_service: self.layout_service.take(),
            committed: self.committed.take(),
            staged: self.staged.take(),
            queued: self.queued.take(),
        };
        self.supervisor.detach(reap);
    }
}

fn validate_runtime_config(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    validate_scan_options(&config.scan_options)?;
    if config.drain_budget.max_events == 0 {
        return Err(RuntimeError::InvalidConfig { field: "drain_budget.max_events" });
    }
    if config.drain_budget.max_duration.is_zero() {
        return Err(RuntimeError::InvalidConfig { field: "drain_budget.max_duration" });
    }
    if config.snapshot_policy.min_events == 0 {
        return Err(RuntimeError::InvalidConfig { field: "snapshot_policy.min_events" });
    }
    if config.snapshot_policy.max_interval < config.snapshot_policy.min_interval {
        return Err(RuntimeError::InvalidConfig { field: "snapshot_policy.max_interval" });
    }
    if !(1..=MAX_RETIRING_SCANS).contains(&config.max_retiring_scans) {
        return Err(RuntimeError::InvalidConfig { field: "max_retiring_scans" });
    }
    let layout = config.layout_options;
    if !layout.start_angle.is_finite() {
        return Err(RuntimeError::InvalidConfig { field: "layout_options.start_angle" });
    }
    if !layout.center_radius.is_finite()
        || layout.center_radius <= 0.0
        || layout.center_radius >= 1.0
    {
        return Err(RuntimeError::InvalidConfig { field: "layout_options.center_radius" });
    }
    if !layout.minimum_sweep.is_finite()
        || layout.minimum_sweep < 0.0
        || layout.minimum_sweep > std::f64::consts::TAU
    {
        return Err(RuntimeError::InvalidConfig { field: "layout_options.minimum_sweep" });
    }
    if layout.max_sectors == 0 {
        return Err(RuntimeError::InvalidConfig { field: "layout_options.max_sectors" });
    }
    Ok(())
}

fn validate_scan_options(options: &ScanOptions) -> Result<(), RuntimeError> {
    let positive = [
        ("worker_count", options.worker_count),
        ("batch_entries", options.batch_entries),
        ("result_capacity", options.result_capacity),
        ("event_capacity", options.event_capacity),
        ("permit_caps.local_low_seek_penalty", options.permit_caps.local_low_seek_penalty),
        ("permit_caps.rotational", options.permit_caps.rotational),
        ("permit_caps.removable", options.permit_caps.removable),
        ("permit_caps.network", options.permit_caps.network),
        ("permit_caps.unknown", options.permit_caps.unknown),
    ];
    for (field, value) in positive {
        if value == 0 {
            return Err(RuntimeError::Scan(ScanFailure::InvalidOptions { field }));
        }
    }
    if options.worker_count > usize::from(u16::MAX) + 1 {
        return Err(RuntimeError::Scan(ScanFailure::InvalidOptions { field: "worker_count" }));
    }
    for (field, value) in [
        ("flush_interval", options.flush_interval),
        ("backpressure_park", options.backpressure_park),
        ("coordinator_poll", options.coordinator_poll),
    ] {
        if value.is_zero() {
            return Err(RuntimeError::Scan(ScanFailure::InvalidOptions { field }));
        }
    }
    Ok(())
}

fn validate_roots(roots: &[ScanRoot]) -> Result<(), RuntimeError> {
    if roots.is_empty() {
        return Err(RuntimeError::Scan(ScanFailure::EmptyRoots));
    }
    if let Some(index) = roots.iter().position(|root| root.path.as_os_str().is_empty()) {
        return Err(RuntimeError::InvalidRoot { index });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::NavigationAction;
    use diskpie_core::{
        EntryKind, FileIdentity, MetricSource, NodeId, NodeSpec, OwnMetrics, ScanState, SizeMetric,
        TreeBuilder, VolumeKey,
        sunburst::{HiddenBranches, compute_layout},
    };
    use diskpie_scan::{
        DirectoryItem, FsEntry, FsError, FsErrorKind, FsOperation, PermitCaps, ScanNodeId,
        StartedRoot, StorageClass, VisitControl,
    };
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    #[derive(Clone, Debug)]
    enum FakePlan {
        Items { items: Vec<DirectoryItem>, delay: Duration },
        Error(FsError),
        Gated { gate: Arc<Gate>, items: Vec<DirectoryItem> },
    }

    #[derive(Debug, Default)]
    struct GateState {
        entered: bool,
        released: bool,
    }

    #[derive(Debug, Default)]
    struct Gate {
        state: Mutex<GateState>,
        wake: Condvar,
    }

    impl Gate {
        fn enter_and_wait(&self) {
            let mut state = self.state.lock().expect("gate lock");
            state.entered = true;
            self.wake.notify_all();
            while !state.released {
                state = self.wake.wait(state).expect("gate wait");
            }
        }

        fn wait_until_entered(&self) {
            wait_until(|| self.state.lock().expect("gate lock").entered);
        }

        fn release(&self) {
            let mut state = self.state.lock().expect("gate lock");
            state.released = true;
            self.wake.notify_all();
        }
    }

    struct ReleaseGates(Vec<Arc<Gate>>);

    impl Drop for ReleaseGates {
        fn drop(&mut self) {
            for gate in &self.0 {
                gate.release();
            }
        }
    }

    struct DropThreadProbe {
        observed: Arc<Mutex<Option<String>>>,
        _large: Vec<u8>,
    }

    impl Drop for DropThreadProbe {
        fn drop(&mut self) {
            *self.observed.lock().expect("drop probe lock") =
                thread::current().name().map(str::to_owned);
        }
    }

    struct BlockingDrop(Arc<Gate>);

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            self.0.enter_and_wait();
        }
    }

    struct CountedDrop(Arc<AtomicUsize>);

    impl Drop for CountedDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct DropFs(Arc<AtomicUsize>);

    impl Drop for DropFs {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ScanFs for DropFs {
        fn visit_directory(
            &self,
            _directory: &Path,
            _visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
        ) -> Result<(), FsError> {
            Ok(())
        }
    }

    fn submit_to_reaper(reaper: &BoundedReaper, job: ReapJob) {
        reaper.reserve().expect("reaper permit").submit(job);
    }

    fn assert_reaper_bound(state: &ReaperState) {
        let occupied = state.active + state.queue.len() + state.reserved;
        assert!(occupied <= REAPER_TOTAL_CAPACITY);
        assert_eq!(state.available + occupied, REAPER_TOTAL_CAPACITY);
    }

    fn saturate_reaper(reaper: &BoundedReaper, gate: &Arc<Gate>) -> ReaperPermit {
        let teardown_permit = reaper.try_reserve().expect("reserved teardown capacity");
        submit_to_reaper(reaper, ReapJob::new(BlockingDrop(Arc::clone(gate))));
        gate.wait_until_entered();
        for _ in 0..(REAPER_CAPACITY - 1) {
            submit_to_reaper(reaper, ReapJob::new(()));
        }
        let state = lock_reaper(&reaper.shared);
        assert_eq!(state.active, 1);
        assert_eq!(state.queue.len(), REAPER_CAPACITY - 1);
        assert_eq!(state.reserved, 1);
        assert_eq!(state.available, 0);
        assert_reaper_bound(&state);
        drop(state);
        teardown_permit
    }

    fn saturate_runtime_reaper(reaper: &BoundedReaper, gate: &Arc<Gate>) {
        submit_to_reaper(reaper, ReapJob::new(BlockingDrop(Arc::clone(gate))));
        gate.wait_until_entered();
        for _ in 0..(REAPER_CAPACITY - 1) {
            submit_to_reaper(reaper, ReapJob::new(()));
        }
        let state = lock_reaper(&reaper.shared);
        assert_eq!(state.active, 1);
        assert_eq!(state.queue.len(), REAPER_CAPACITY - 1);
        assert_eq!(state.reserved, 1);
        assert_eq!(state.available, 0);
        assert_reaper_bound(&state);
    }

    #[derive(Debug, Default)]
    struct FakeFs {
        plans: Mutex<HashMap<PathBuf, FakePlan>>,
        visits: Mutex<Vec<PathBuf>>,
        visited_items: AtomicUsize,
    }

    impl FakeFs {
        fn set(&self, path: impl Into<PathBuf>, plan: FakePlan) {
            self.plans.lock().expect("fake filesystem lock").insert(path.into(), plan);
        }

        fn visits(&self) -> usize {
            self.visits.lock().expect("visit lock").len()
        }

        fn visited(&self, path: &Path) -> bool {
            self.visits.lock().expect("visit lock").iter().any(|visited| visited == path)
        }
    }

    impl ScanFs for FakeFs {
        fn visit_directory(
            &self,
            directory: &Path,
            visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
        ) -> Result<(), FsError> {
            self.visits.lock().expect("visit lock").push(directory.to_owned());
            let plan = self
                .plans
                .lock()
                .expect("fake filesystem lock")
                .get(directory)
                .cloned()
                .unwrap_or_else(|| {
                    FakePlan::Error(FsError::new(
                        directory,
                        FsOperation::EnumerateDirectory,
                        FsErrorKind::NotFound,
                    ))
                });
            let (items, delay) = match plan {
                FakePlan::Items { items, delay } => (items, delay),
                FakePlan::Error(error) => return Err(error),
                FakePlan::Gated { gate, items } => {
                    gate.enter_and_wait();
                    (items, Duration::ZERO)
                }
            };
            for item in items {
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                self.visited_items.fetch_add(1, Ordering::SeqCst);
                if visitor(item) == VisitControl::Stop {
                    break;
                }
            }
            Ok(())
        }
    }

    fn items(items: Vec<DirectoryItem>) -> FakePlan {
        FakePlan::Items { items, delay: Duration::ZERO }
    }

    fn delayed_items(items: Vec<DirectoryItem>, delay: Duration) -> FakePlan {
        FakePlan::Items { items, delay }
    }

    fn metrics(bytes: u64) -> OwnMetrics {
        OwnMetrics::new(
            SizeMetric::known(bytes, MetricSource::PortableMetadata),
            SizeMetric::known(bytes, MetricSource::FilesystemAllocation),
        )
    }

    fn file(name: impl Into<std::ffi::OsString>, bytes: u64) -> DirectoryItem {
        DirectoryItem::Entry(FsEntry::new(name, EntryKind::File, metrics(bytes)))
    }

    fn directory(name: impl Into<std::ffi::OsString>) -> DirectoryItem {
        DirectoryItem::Entry(FsEntry::new(name, EntryKind::Directory, OwnMetrics::ZERO_BY_POLICY))
    }

    fn root(path: impl Into<PathBuf>, volume: u128) -> ScanRoot {
        ScanRoot::new(path, VolumeKey::new(volume), StorageClass::LocalLowSeekPenalty)
    }

    fn config(max_events: usize, max_retiring_scans: usize) -> RuntimeConfig {
        RuntimeConfig {
            scan_options: ScanOptions {
                worker_count: 1,
                batch_entries: 1,
                result_capacity: 4,
                event_capacity: 64,
                flush_interval: Duration::from_millis(1),
                backpressure_park: Duration::from_millis(1),
                coordinator_poll: Duration::from_millis(1),
                permit_caps: PermitCaps {
                    local_low_seek_penalty: 1,
                    rotational: 1,
                    removable: 1,
                    network: 1,
                    unknown: 1,
                },
            },
            drain_budget: DrainBudget { max_events, max_duration: Duration::from_millis(5) },
            // Publish every change so the runtime's own partial-layout
            // throttle is what the progressive-frame tests observe.
            snapshot_policy: SnapshotPolicy::IMMEDIATE,
            layout_options: LayoutOptions::new(NodeId::from_raw(0)),
            max_retiring_scans,
        }
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn tick(
        runtime: &mut RuntimeController,
        clock: &mut Duration,
    ) -> Result<TickReport, RuntimeError> {
        *clock += Duration::from_millis(10);
        runtime.tick(|| *clock)
    }

    fn accept_for_test(
        runtime: &mut RuntimeController,
        publication: PreparedPublication,
        now: Duration,
        report: &mut TickReport,
    ) -> Result<(), RuntimeError> {
        let mut retired = runtime.reserve_ui_retirement()?;
        runtime.accept_prepared(publication, now, report, &mut retired)
    }

    fn pump_until(
        runtime: &mut RuntimeController,
        clock: &mut Duration,
        mut ready: impl FnMut(&RuntimeController) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            tick(runtime, clock).expect("runtime tick");
            if ready(runtime) {
                return;
            }
            assert!(Instant::now() < deadline, "runtime pump timed out: {runtime:?}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn pump_complete(runtime: &mut RuntimeController, clock: &mut Duration) {
        pump_until(runtime, clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.state() == ScanState::Complete)
                && runtime.layout().is_some()
                && matches!(runtime.state(), RuntimeState::Settled { .. })
        });
    }

    fn node_named(snapshot: &TreeSnapshot, name: &str) -> NodeId {
        snapshot
            .nodes()
            .iter()
            .position(|node| node.name() == name)
            .and_then(|index| u32::try_from(index).ok())
            .map(NodeId::from_raw)
            .unwrap_or_else(|| panic!("node {name:?} was not found"))
    }

    fn branch_fixture() -> (Arc<FakeFs>, RuntimeController, Duration, NodeId) {
        let fake = Arc::new(FakeFs::default());
        fake.set("root", items(vec![directory("branch"), file("outside", 9)]));
        fake.set(PathBuf::from("root").join("branch"), items(vec![file("before", 3)]));
        let mut runtime = RuntimeController::new(config(32, 2)).unwrap();
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("root", 1)],
                CancelToken::new(),
            )
            .unwrap();
        let mut clock = Duration::ZERO;
        pump_until(&mut runtime, &mut clock, |runtime| runtime.ensure_branch_settled().is_ok());
        let branch = node_named(runtime.snapshot().unwrap(), "branch");
        (fake, runtime, clock, branch)
    }

    #[test]
    fn accepted_cancel_during_branch_preparation_never_publishes_replacement() {
        for stage in [
            BranchPublicationStage::Merge,
            BranchPublicationStage::Presentation,
            BranchPublicationStage::Publish,
        ] {
            let (fake, mut runtime, mut clock, branch) = branch_fixture();
            let baseline = runtime.snapshot_handle().unwrap();
            let intent = runtime.branch_rescan_intent(branch).unwrap();
            let path = intent.native_path().unwrap();
            fake.set(&path, items(vec![file("after", 25)]));
            let gate = Arc::new(Gate::default());
            let _release = ReleaseGates(vec![Arc::clone(&gate)]);
            let worker_gate = Arc::clone(&gate);
            lock_supervisor(&runtime.supervisor.shared).branch_publication_hook =
                Some(Arc::new(move |at| {
                    if at == stage {
                        worker_gate.enter_and_wait();
                    }
                }));
            let token = CancelToken::new();
            runtime.request_branch_scan(intent, fake, root(path, 1), token.clone()).unwrap();
            pump_until(&mut runtime, &mut clock, |_| gate.state.lock().unwrap().entered);
            assert!(runtime.cancel_active(), "cancel must be admitted before {stage:?}");
            assert!(token.is_cancelled());
            gate.release();
            pump_until(&mut runtime, &mut clock, |runtime| runtime.ensure_branch_settled().is_ok());
            assert!(
                Arc::ptr_eq(&baseline, &runtime.snapshot_handle().unwrap()),
                "cancelled at {stage:?}"
            );
            assert_eq!(runtime.phase(), Some(SessionPhase::Cancelled));
            assert!(runtime.last_error().is_none());
            assert!(!runtime.cancel_active(), "a finished generation cannot accept cancellation");
        }
    }

    #[test]
    fn branch_scan_preserves_baseline_until_atomic_replacement_and_rejects_stale_intents() {
        let (fake, mut runtime, mut clock, branch) = branch_fixture();
        let intent = runtime.branch_rescan_intent(branch).unwrap();
        let stale = intent.clone();
        let baseline = runtime.snapshot_handle().unwrap();
        let path = intent.native_path().unwrap();
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set(
            &path,
            FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("after", 25)] },
        );
        runtime
            .request_branch_scan(
                intent,
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                root(&path, 1),
                CancelToken::new(),
            )
            .unwrap();
        gate.wait_until_entered();
        for _ in 0..8 {
            tick(&mut runtime, &mut clock).unwrap();
        }
        assert!(Arc::ptr_eq(&runtime.snapshot_handle().unwrap(), &baseline));
        runtime
            .execute_navigation(
                runtime.navigation_command(NavigationAction::Activate(branch)).unwrap(),
            )
            .unwrap();
        gate.release();
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.generation() == GenerationId::new(2))
                && runtime.ensure_branch_settled().is_ok()
        });
        assert_eq!(
            runtime.navigation().unwrap().view_root(),
            node_named(runtime.snapshot().unwrap(), "branch")
        );
        assert!(runtime.snapshot().unwrap().nodes().iter().any(|record| record.name() == "after"));
        assert!(
            runtime.snapshot().unwrap().nodes().iter().any(|record| record.name() == "outside")
        );
        assert!(
            !runtime.snapshot().unwrap().nodes().iter().any(|record| record.name() == "before")
        );
        let token = CancelToken::new();
        let rejection = runtime
            .request_branch_scan(
                stale,
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                root(&path, 1),
                token.clone(),
            )
            .unwrap_err();
        assert_eq!(rejection.error(), &RuntimeError::StaleBranchIntent);
        let (_, intent, returned_fs, returned_root, returned_cancel) = rejection.into_parts();
        assert!(Arc::ptr_eq(intent.snapshot(), &baseline));
        assert_eq!(returned_root.path, path);
        assert!(Arc::ptr_eq(&returned_fs, &(fake as Arc<dyn ScanFs>)));
        assert!(!returned_cancel.is_cancelled());
        assert!(!token.is_cancelled());
        assert_eq!(runtime.last_generation, 2);
    }

    #[test]
    fn cancelled_branch_keeps_baseline_and_a_later_generation_wins() {
        let (fake, mut runtime, mut clock, branch) = branch_fixture();
        let baseline = runtime.snapshot_handle().unwrap();
        let intent = runtime.branch_rescan_intent(branch).unwrap();
        let path = intent.native_path().unwrap();
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set(&path, FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("late", 99)] });
        runtime
            .request_branch_scan(
                intent,
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                root(path, 1),
                CancelToken::new(),
            )
            .unwrap();
        gate.wait_until_entered();
        assert!(runtime.cancel_active());
        pump_until(&mut runtime, &mut clock, |runtime| runtime.retiring_scans() > 0);
        assert!(Arc::ptr_eq(&runtime.snapshot_handle().unwrap(), &baseline));
        fake.set("other", items(vec![file("winner", 10)]));
        runtime.request_scan(fake, vec![root("other", 2)], CancelToken::new()).unwrap();
        gate.release();
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.generation() == GenerationId::new(3))
                && runtime.ensure_branch_settled().is_ok()
        });
        assert!(runtime.snapshot().unwrap().nodes().iter().any(|record| record.name() == "winner"));
        assert!(!runtime.snapshot().unwrap().nodes().iter().any(|record| record.name() == "late"));
    }

    #[test]
    fn branch_fatal_failure_keeps_baseline_and_recoverable_failure_commits_partial() {
        let (fake, mut runtime, mut clock, branch) = branch_fixture();
        let baseline = runtime.snapshot_handle().unwrap();
        let intent = runtime.branch_rescan_intent(branch).unwrap();
        let path = intent.native_path().unwrap();
        fake.set(
            &path,
            FakePlan::Error(FsError::new(
                &path,
                FsOperation::EnumerateDirectory,
                FsErrorKind::ProviderFailure,
            )),
        );
        let receipt = runtime
            .request_branch_scan(
                intent,
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                root(&path, 1),
                CancelToken::new(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !runtime
            .settled_summary()
            .is_some_and(|summary| summary.generation == receipt.generation)
            || runtime.ensure_branch_settled().is_err()
        {
            let _ = tick(&mut runtime, &mut clock);
            assert!(Instant::now() < deadline, "branch failure did not settle: {runtime:?}");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(runtime.phase(), Some(SessionPhase::Failed(_))));
        assert!(Arc::ptr_eq(&runtime.snapshot_handle().unwrap(), &baseline));
        let intent = runtime.branch_rescan_intent(branch).unwrap();
        fake.set(
            &path,
            FakePlan::Error(FsError::new(
                &path,
                FsOperation::EnumerateDirectory,
                FsErrorKind::AccessDenied,
            )),
        );
        runtime.request_branch_scan(intent, fake, root(path, 1), CancelToken::new()).unwrap();
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.generation() == GenerationId::new(3))
                && runtime.ensure_branch_settled().is_ok()
        });
        assert_eq!(runtime.snapshot().unwrap().state(), ScanState::Partial);
        assert!(
            runtime.snapshot().unwrap().nodes().iter().any(|record| record.name() == "outside")
        );
    }

    #[test]
    fn branch_readiness_observes_inbox_to_reserved_handoff_atomically() {
        let supervisor = SupervisorShared::default();
        let generation = ScanGeneration::new(2);
        let prior_status = {
            let mut shared = lock_supervisor(&supervisor);
            shared.inbox.launch = Some(PendingLaunch {
                generation,
                fs: Arc::new(FakeFs::default()),
                roots: vec![root("pending-branch", 1)],
                cancel: CancelToken::new(),
                branch: None,
                initial_omissions: 0,
            });
            assert_eq!(shared.branch_scan_readiness(), Err(RuntimeError::ScanNotSettled));
            shared.status.clone()
        };

        // This is the exact ownership transfer performed by promote_pending.
        // Force it between the old implementation's status and inbox samples,
        // without relying on thread scheduling to hit the short launch window.
        let launch = {
            let mut shared = lock_supervisor(&supervisor);
            let launch = shared.inbox.launch.take().unwrap();
            shared.status.reserved = Some(ReservedLaunchStatus {
                generation: launch.generation,
                cancel: launch.cancel.clone(),
            });
            launch
        };
        {
            let shared = lock_supervisor(&supervisor);
            assert!(prior_status.active.is_none() && prior_status.reserved.is_none());
            assert!(shared.inbox.launch.is_none() && shared.status.pending.is_none());
            assert!(!shared.has_output(), "no Started event exists during reservation");
            // The mixed observations above all look idle; the single locked
            // observation must still reject the owned generation in reserved.
            assert_eq!(shared.branch_scan_readiness(), Err(RuntimeError::ScanNotSettled));
        }
        let mut shared = lock_supervisor(&supervisor);
        shared.status.reserved = None;
        shared.output.scan_finished = Some(generation);
        assert_eq!(shared.branch_scan_readiness(), Err(RuntimeError::ScanNotSettled));
        assert_eq!(shared.output.scan_finished.take(), Some(generation));
        assert_eq!(shared.branch_scan_readiness(), Ok(()));
        drop(shared);
        assert_eq!(launch.generation, generation, "reservation retains the original request");
    }

    #[test]
    fn bounded_reaper_runs_large_destruction_on_its_worker() {
        let observed = Arc::new(Mutex::new(None));
        let (reaper, worker) = BoundedReaper::new().expect("reaper");
        submit_to_reaper(
            &reaper,
            ReapJob::new(DropThreadProbe {
                observed: Arc::clone(&observed),
                _large: vec![0; 1024 * 1024],
            }),
        );
        wait_until(|| observed.lock().expect("drop probe lock").is_some());
        assert_eq!(
            observed.lock().expect("drop probe lock").as_deref(),
            Some("diskpie-value-reaper")
        );
        drop(reaper);
        worker.join().expect("reaper exits after sender closes");
    }

    #[test]
    fn saturated_reaper_returns_ownership_and_destroys_it_after_capacity_recovers() {
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        let dropped = Arc::new(AtomicUsize::new(0));
        let (reaper, worker) = BoundedReaper::new().expect("reaper");
        let teardown_permit = saturate_reaper(&reaper, &gate);

        let started = Instant::now();
        let mut rejected = Vec::new();
        for _ in 0..32 {
            rejected.push(
                reaper
                    .try_submit(ReapJob::new(CountedDrop(Arc::clone(&dropped))))
                    .expect_err("ordinary capacity is full"),
            );
        }
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "saturated admission blocked the caller"
        );
        assert!(rejected.iter().all(|rejection| { rejection.reason == ReaperReserveError::Full }));
        assert_eq!(dropped.load(Ordering::SeqCst), 0, "rejected ownership was destroyed");
        {
            let state = lock_reaper(&reaper.shared);
            assert_eq!(state.active, 1, "the stalled disposal lost its capacity slot");
            assert_eq!(state.queue.len(), REAPER_CAPACITY - 1);
            assert_eq!(state.reserved, 1, "the teardown permit was consumed");
            assert_eq!(state.available, 0);
            assert_reaper_bound(&state);
        }

        gate.release();
        for rejection in rejected {
            submit_to_reaper(&reaper, rejection.job);
        }
        drop(teardown_permit);
        drop(reaper);
        worker.join().expect("reaper drains after release");
        assert_eq!(dropped.load(Ordering::SeqCst), 32, "returned ownership leaked");
    }

    #[test]
    fn disconnected_session_reaper_returns_scan_ownership_without_leaking() {
        let generation = ScanGeneration::new(66);
        let provider_drops = Arc::new(AtomicUsize::new(0));
        let session = start_scan_with_cancel(
            Arc::new(DropFs(provider_drops)) as Arc<dyn ScanFs>,
            ScanRequest::new(generation, vec![root("finished", 66)]),
            CancelToken::new(),
        )
        .expect("scan starts");
        wait_until(|| session.is_finished());
        let scan_drops = Arc::new(AtomicUsize::new(0));
        let mut scan = WorkerScan::new(session);
        scan.retirement_probe = Some(WorkerScanRetirementProbe(Arc::clone(&scan_drops)));
        let (sender, receiver) = sync_channel::<Vec<WorkerScan>>(1);
        drop(receiver);
        let reaper = SessionReaper { sender };

        let returned = reaper.submit(vec![scan]).expect_err("receiver is disconnected");
        assert_eq!(returned.len(), 1);
        assert_eq!(scan_drops.load(Ordering::SeqCst), 0);
        drop(returned);
        assert_eq!(scan_drops.load(Ordering::SeqCst), 1, "returned scan ownership leaked");
    }

    #[test]
    fn saturated_scan_admission_returns_the_request_and_preserves_pending() {
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        wait_until(|| lock_supervisor(&runtime.supervisor.shared).supervisor_waiting);
        let pending_fs = Arc::new(FakeFs::default());
        let pending_generation = ScanGeneration::new(77);
        let supervisor_shared = Arc::clone(&runtime.supervisor.shared);
        let mut held_shared = lock_supervisor(&supervisor_shared);
        held_shared.inbox.launch = Some(PendingLaunch {
            branch: None,
            initial_omissions: 0,
            generation: pending_generation,
            fs: Arc::clone(&pending_fs) as Arc<dyn ScanFs>,
            roots: vec![root("pending", 77)],
            cancel: CancelToken::new(),
        });
        let mut builder = TreeBuilder::new(GenerationId::new(78));
        builder.add_root(NodeSpec::root("publication")).expect("root");
        held_shared.output.publication = Some(
            prepare_publication(
                SnapshotPublication {
                    revision: 1,
                    snapshot: Arc::new(builder.freeze().expect("snapshot")),
                    selected: None,
                },
                false,
            )
            .expect("publication"),
        );
        saturate_runtime_reaper(&runtime.supervisor.reaper, &gate);
        assert_eq!(lock_reaper(&runtime.supervisor.reaper.shared).queue.len(), REAPER_CAPACITY - 1);

        let rejected_fs = Arc::new(FakeFs::default());
        let rejected_token = CancelToken::new();
        let started = Instant::now();
        let rejection = runtime
            .request_scan(
                Arc::clone(&rejected_fs) as Arc<dyn ScanFs>,
                vec![root("rejected", 78)],
                rejected_token.clone(),
            )
            .expect_err("saturated retirement rejects launch");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(rejection.error(), &RuntimeError::RetirementBackpressure);
        assert_eq!(runtime.last_generation, 0, "a rejected generation was consumed");
        let (error, returned_fs, returned_roots, returned_token) = rejection.into_parts();
        assert_eq!(error, RuntimeError::RetirementBackpressure);
        let expected_fs: Arc<dyn ScanFs> = rejected_fs;
        assert!(Arc::ptr_eq(&returned_fs, &expected_fs));
        assert_eq!(returned_roots.len(), 1);
        returned_token.cancel();
        assert!(rejected_token.is_cancelled(), "the exact cancellation ownership was returned");

        let returned_provider_drops = Arc::new(AtomicUsize::new(0));
        let mut returned_providers = Vec::new();
        for index in 0..32 {
            let rejection = runtime
                .request_scan(
                    Arc::new(DropFs(Arc::clone(&returned_provider_drops))) as Arc<dyn ScanFs>,
                    vec![root(format!("rejected-{index}"), 200 + index)],
                    CancelToken::new(),
                )
                .expect_err("all saturated launches are returned");
            assert_eq!(rejection.error(), &RuntimeError::RetirementBackpressure);
            let (_, provider, _, _) = rejection.into_parts();
            returned_providers.push(provider);
        }
        assert_eq!(
            held_shared.inbox.launch.as_ref().map(|launch| launch.generation),
            Some(pending_generation)
        );
        assert_eq!(runtime.last_generation, 0);
        assert_eq!(returned_provider_drops.load(Ordering::SeqCst), 0);
        for _ in 0..32 {
            assert!(matches!(
                runtime.execute_navigation(NavigationCommand::new(
                    GenerationId::new(78),
                    NavigationAction::RestoreAllBranches,
                )),
                Err(RuntimeError::RetirementBackpressure)
            ));
        }
        drop(held_shared);
        for tick in 1..=32 {
            assert!(matches!(
                runtime.tick(|| Duration::from_millis(tick)),
                Err(RuntimeError::RetirementBackpressure)
            ));
        }
        assert_eq!(
            lock_supervisor(&runtime.supervisor.shared)
                .output
                .publication
                .as_ref()
                .expect("publication remains bounded in its single slot")
                .revision
                .revision,
            1
        );

        gate.release();
        drop(returned_providers);
        assert_eq!(returned_provider_drops.load(Ordering::SeqCst), 32);
        drop(runtime);
    }

    #[test]
    fn terminal_publication_replaces_partial_even_when_the_reaper_is_saturated() {
        fn publication(revision: u64, terminal: bool) -> PreparedPublication {
            let mut builder = TreeBuilder::new(GenerationId::new(91));
            builder.add_root(NodeSpec::root("root")).expect("root");
            prepare_publication(
                SnapshotPublication {
                    revision,
                    snapshot: Arc::new(builder.freeze().expect("snapshot")),
                    selected: None,
                },
                terminal,
            )
            .expect("prepared publication")
        }

        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        let (reaper, reaper_worker) = BoundedReaper::new().expect("reaper");
        let (session_reaper, session_worker) = SessionReaper::new().expect("session reaper");
        let shared = Arc::new(SupervisorShared::default());
        let core =
            SupervisorCore::new(Arc::clone(&shared), config(8, 2), reaper.clone(), session_reaper);
        core.store_publication(publication(1, false));
        let _teardown = saturate_reaper(&reaper, &gate);

        let (done, completed) = std::sync::mpsc::sync_channel(1);
        let store = thread::spawn(move || {
            core.store_publication(publication(2, true));
            done.send(()).expect("completion receiver");
        });
        assert!(
            completed.recv_timeout(Duration::from_millis(100)).is_err(),
            "the supervisor must preserve ownership while retirement is saturated"
        );
        assert_eq!(
            lock_supervisor(&shared)
                .output
                .publication
                .as_ref()
                .expect("partial publication remains owned")
                .revision
                .revision,
            1
        );
        gate.release();
        completed
            .recv_timeout(Duration::from_secs(2))
            .expect("terminal replacement resumes when retirement capacity returns");
        let retained = lock_supervisor(&shared).output.publication.take().expect("publication");
        assert_eq!(retained.revision.revision, 2);
        assert!(retained.revision.terminal);
        drop(retained);

        store.join().expect("store thread");
        drop(_teardown);
        drop(shared);
        drop(reaper);
        reaper_worker.join().expect("reaper drains");
        session_worker.join().expect("session reaper exits");
    }

    #[test]
    fn unobserved_cancel_and_shutdown_epochs_prevent_lost_wakes() {
        let (reaper, reaper_worker) = BoundedReaper::new().expect("reaper");
        let (session_reaper, session_worker) = SessionReaper::new().expect("session reaper");
        let shared = Arc::new(SupervisorShared::default());
        let mut core =
            SupervisorCore::new(Arc::clone(&shared), config(8, 2), reaper.clone(), session_reaper);

        {
            let mut state = lock_supervisor(&shared);
            state.inbox.cancel_generation = Some(ScanGeneration::new(1));
            mark_supervisor_control(&mut state);
        }
        let started = Instant::now();
        core.wait_for_work();
        assert!(started.elapsed() < Duration::from_millis(250), "lost cancel wake");
        let controls = core.take_controls();
        core.observed_control_epoch = controls.epoch;
        assert_eq!(controls.cancel_generation, Some(ScanGeneration::new(1)));

        {
            let mut state = lock_supervisor(&shared);
            state.inbox.shutdown = true;
            mark_supervisor_control(&mut state);
        }
        let started = Instant::now();
        core.wait_for_work();
        assert!(started.elapsed() < Duration::from_millis(250), "lost shutdown wake");

        drop(core);
        drop(shared);
        drop(reaper);
        reaper_worker.join().expect("reaper exits");
        session_worker.join().expect("session reaper exits");
    }

    #[test]
    fn progressive_frames_are_prebuilt_throttled_and_terminal_is_not_starved() {
        let fake = Arc::new(FakeFs::default());
        fake.set(
            "root",
            delayed_items(
                (0..40).map(|index| file(format!("f-{index}"), 1)).collect(),
                Duration::from_millis(3),
            ),
        );
        let mut runtime = RuntimeController::new(config(2, 2)).expect("runtime");
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("root", 1)],
                CancelToken::new(),
            )
            .expect("scan");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut clock = Duration::ZERO;
        let mut partial_layout_times = Vec::new();
        let mut saw_progressive_commit = false;
        loop {
            let report = tick(&mut runtime, &mut clock).expect("tick");
            if let Some(drain) = report.drain {
                assert!(drain.consumed <= 2);
            }
            // A submitted frame is still staged unless its layout already
            // completed within this same tick, in which case the committed
            // snapshot is the frame that was just submitted.
            let submitted_state = report.layout_submitted.and_then(|_| {
                runtime
                    .staged
                    .as_ref()
                    .map(|staged| staged.data.snapshot.state())
                    .or_else(|| runtime.snapshot().map(TreeSnapshot::state))
            });
            if submitted_state == Some(ScanState::Partial) {
                partial_layout_times.push(clock);
            }
            if report.snapshot_published
                && runtime.snapshot().is_some_and(|snapshot| snapshot.state() == ScanState::Partial)
            {
                saw_progressive_commit = true;
                assert_eq!(
                    runtime.presentation().expect("presentation").generation(),
                    runtime.snapshot().expect("snapshot").generation()
                );
            }
            if runtime.snapshot().is_some_and(|snapshot| snapshot.state() == ScanState::Complete)
                && matches!(runtime.state(), RuntimeState::Settled { .. })
            {
                break;
            }
            assert!(Instant::now() < deadline, "terminal frame was starved: {runtime:?}");
            thread::sleep(Duration::from_millis(1));
        }

        assert!(saw_progressive_commit);
        assert!(partial_layout_times.len() >= 2);
        assert!(
            partial_layout_times
                .windows(2)
                .all(|pair| { pair[1].saturating_sub(pair[0]) >= MIN_PARTIAL_LAYOUT_INTERVAL }),
            "partial layout submissions were not throttled: {partial_layout_times:?}"
        );
        assert_eq!(runtime.snapshot().expect("terminal snapshot").len(), 41);
    }

    #[test]
    fn retiring_cap_allows_immediate_replacement_then_coalesces() {
        let fake = Arc::new(FakeFs::default());
        let gates = (0..3).map(|_| Arc::new(Gate::default())).collect::<Vec<_>>();
        let _release = ReleaseGates(gates.clone());
        for (index, gate) in gates.iter().enumerate() {
            fake.set(
                format!("blocked-{index}"),
                FakePlan::Gated { gate: Arc::clone(gate), items: vec![file("late", 1)] },
            );
        }
        fake.set("queued-3", items(vec![file("obsolete", 1)]));
        fake.set("latest-4", items(vec![file("latest", 2)]));

        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        let token1 = CancelToken::new();
        let token2 = CancelToken::new();
        let token3 = CancelToken::new();
        let token4 = CancelToken::new();
        let token5 = CancelToken::new();
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("blocked-0", 2)],
                token1.clone(),
            )
            .expect("generation 1");
        gates[0].wait_until_entered();

        let second = runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("blocked-1", 2)],
                token2.clone(),
            )
            .expect("generation 2");
        assert_eq!(second.disposition, LaunchDisposition::Started);
        gates[1].wait_until_entered();
        wait_until(|| runtime.retiring_scans() == 1);

        let third = runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("blocked-2", 2)],
                token3.clone(),
            )
            .expect("generation 3");
        assert_eq!(third.disposition, LaunchDisposition::Started);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gates[2].state.lock().expect("gate lock").entered {
            assert!(
                Instant::now() < deadline,
                "third scan did not start: runtime={runtime:?}, pending={:?}, stopped={}",
                runtime.pending_generation(),
                runtime.supervisor.status().stopped
            );
            thread::sleep(Duration::from_millis(1));
        }
        wait_until(|| runtime.retiring_scans() == 2);

        let fourth = runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("queued-3", 2)],
                token4.clone(),
            )
            .expect("generation 4");
        assert_eq!(fourth.disposition, LaunchDisposition::Queued);
        wait_until(|| runtime.pending_generation() == Some(ScanGeneration::new(4)));
        let fifth = runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("latest-4", 2)],
                token5.clone(),
            )
            .expect("generation 5");
        assert_eq!(fifth.disposition, LaunchDisposition::ReplacedPending);
        wait_until(|| token4.is_cancelled() && token3.is_cancelled());
        assert!(!fake.visited(Path::new("queued-3")));
        assert!(runtime.retiring_scans() <= 2);

        gates[0].release();
        wait_until(|| fake.visited(Path::new("latest-4")));
        assert!(runtime.retiring_scans() <= 2);
        gates[1].release();
        gates[2].release();

        let mut clock = Duration::ZERO;
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| {
                snapshot.generation() == GenerationId::new(5)
                    && snapshot.state() == ScanState::Complete
            }) && runtime.retiring_scans() == 0
        });
        assert!(token1.is_cancelled());
        assert!(token2.is_cancelled());
        assert!(token3.is_cancelled());
        assert!(token4.is_cancelled());
        assert!(!token5.is_cancelled());
        assert_eq!(runtime.retiring_scans(), 0);
        assert_eq!(fake.visits(), 4, "coalesced generation never starts a provider");
    }

    #[test]
    fn cancellation_publishes_observed_data_with_the_real_terminal_counters() {
        let fake = Arc::new(FakeFs::default());
        fake.set(
            "root",
            delayed_items(
                (0..100).map(|index| file(format!("f-{index}"), 1)).collect(),
                Duration::from_millis(2),
            ),
        );
        let token = CancelToken::new();
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime.request_scan(fake, vec![root("root", 3)], token.clone()).expect("scan");
        wait_until(|| runtime.cancel_token().is_some() && runtime.progress().is_some());
        wait_until(|| runtime.supervisor.status().active.is_some());
        wait_until(|| {
            runtime.supervisor.pulse(Duration::from_millis(1));
            runtime.progress().is_some_and(|progress| progress.observed().entries >= 2)
        });
        assert!(runtime.cancel_active());
        wait_until(|| token.is_cancelled());

        let mut clock = Duration::from_millis(1);
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.state() == ScanState::Cancelled)
                && matches!(
                    runtime.state(),
                    RuntimeState::Settled { phase: SessionPhase::Cancelled, .. }
                )
        });
        let summary = runtime.settled_summary().expect("lightweight summary");
        let terminal = summary.progress.terminal_reported().expect("real terminal counters");
        let observed = summary.progress.observed();
        assert!(terminal.entries >= observed.entries);
        assert!(runtime.snapshot().expect("cancelled snapshot").len() as u64 <= observed.entries);
        assert_eq!(runtime.retiring_scans(), 0);
        assert!(runtime.active_generation().is_none());
    }

    #[test]
    fn cancellation_freezes_observed_state_before_a_provider_returns() {
        let fake = Arc::new(FakeFs::default());
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set(
            "blocked",
            FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("late", 1)] },
        );
        let token = CancelToken::new();
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime.request_scan(fake, vec![root("blocked", 31)], token.clone()).expect("scan");
        gate.wait_until_entered();
        wait_until(|| runtime.supervisor.status().active.is_some());

        let started = Instant::now();
        assert!(runtime.cancel_active());
        assert!(token.is_cancelled(), "the authoritative token is cancelled synchronously");
        let mut clock = Duration::ZERO;
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.state() == ScanState::Cancelled)
                && runtime.active_generation().is_none()
                && runtime.retiring_scans() == 1
        });
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!gate.state.lock().expect("gate lock").released);

        gate.release();
        pump_until(&mut runtime, &mut clock, |runtime| runtime.retiring_scans() == 0);
        let summary = runtime.settled_summary().expect("settled cancellation");
        assert_eq!(summary.phase, SessionPhase::Cancelled);
        assert!(summary.progress.terminal_reported().is_some());
    }

    #[test]
    fn reducer_fault_keeps_budgetedly_draining_until_the_session_can_finish() {
        let fake = Arc::new(FakeFs::default());
        fake.set("fault", items((0..200).map(|index| file(format!("f-{index}"), 1)).collect()));
        let runtime_config = config(2, 2);
        let generation = ScanGeneration::new(41);
        let request = ScanRequest::new(generation, vec![root("fault", 41)])
            .with_options(runtime_config.scan_options.clone());
        let session = start_scan_with_cancel(fake as Arc<dyn ScanFs>, request, CancelToken::new())
            .expect("session");
        let mut scan = WorkerScan::new(session);
        scan.reducer
            .as_mut()
            .expect("reducer")
            .apply_event(ScanEvent::Started {
                generation,
                roots: vec![StartedRoot {
                    id: ScanNodeId::from_raw(0),
                    path: PathBuf::from("fault"),
                    volume: VolumeKey::new(41),
                    storage_class: StorageClass::LocalLowSeekPenalty,
                }],
            })
            .expect("seed duplicate Started fault");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut now = Duration::ZERO;
        let mut saw_fault = false;
        let shared = SupervisorShared::default();
        loop {
            now += Duration::from_millis(10);
            let result = drive_scan(&mut scan, now, &runtime_config, true, None, &shared);
            saw_fault |=
                matches!(result.error, Some(RuntimeError::Session(SessionError::DuplicateStarted)));
            assert!(result.drain.is_none_or(|drain| drain.consumed <= 2));
            if result.ready_to_finalize {
                break;
            }
            assert!(Instant::now() < deadline, "faulted reducer stopped draining");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(saw_fault);
        assert!(scan.discard_events);
        assert!(scan.terminal_event_seen || scan.is_finished());
        let finalized = finalize_scan(scan, now, None, &shared);
        assert_eq!(finalized.summary.generation, generation);
    }

    #[test]
    fn failed_candidate_layout_keeps_the_last_committed_frame() {
        let fake = Arc::new(FakeFs::default());
        fake.set("first", items(vec![file("old", 1)]));
        fake.set("second", items(vec![file("new", 2)]));
        let mut runtime = RuntimeController::new(config(64, 2)).expect("runtime");
        let mut clock = Duration::ZERO;
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("first", 4)],
                CancelToken::new(),
            )
            .expect("first scan");
        pump_complete(&mut runtime, &mut clock);
        let old_snapshot = runtime.snapshot_handle().expect("old snapshot");
        let old_layout = runtime.layout().expect("old layout").clone();

        runtime.config.layout_options.max_sectors = 0;
        runtime
            .request_scan(fake, vec![root("second", 4)], CancelToken::new())
            .expect("second scan");
        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match tick(&mut runtime, &mut clock) {
                Ok(_) => {
                    assert_eq!(
                        runtime.snapshot().expect("old frame retained").generation(),
                        old_snapshot.generation()
                    );
                    assert_eq!(runtime.layout(), Some(&old_layout));
                }
                Err(error @ RuntimeError::LayoutTask(_)) => break error,
                Err(error) => panic!("unexpected runtime error: {error}"),
            }
            assert!(Instant::now() < deadline, "invalid layout never completed");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(error, RuntimeError::LayoutTask(_)));
        assert_eq!(runtime.snapshot().expect("old snapshot").generation(), GenerationId::new(1));
        assert_eq!(runtime.layout(), Some(&old_layout));
    }

    #[test]
    fn ready_layout_and_staged_frame_are_retired_off_ui_before_navigation_resubmits() {
        let fake = Arc::new(FakeFs::default());
        fake.set("root", items(vec![directory("branch"), file("peer", 2)]));
        fake.set(PathBuf::from("root").join("branch"), items(vec![file("leaf", 3)]));
        let mut runtime = RuntimeController::new(config(64, 2)).expect("runtime");
        let mut clock = Duration::ZERO;
        runtime.request_scan(fake, vec![root("root", 92)], CancelToken::new()).expect("scan");
        pump_complete(&mut runtime, &mut clock);

        let branch = node_named(runtime.snapshot().expect("snapshot"), "branch");
        let zoom = runtime
            .execute_navigation(
                runtime.navigation_command(NavigationAction::Zoom(branch)).expect("zoom command"),
            )
            .expect("zoom");
        assert!(zoom.layout_submitted.is_some());
        let observed = Arc::new(Mutex::new(None));
        runtime.staged.as_mut().expect("staged zoom").data.retirement_probe =
            Some(FrameRetirementProbe(Arc::clone(&observed)));
        wait_until(|| runtime.layout_service.as_ref().is_some_and(|service| !service.is_busy()));

        let replacement = runtime
            .execute_navigation(
                runtime
                    .navigation_command(NavigationAction::SetSizeBasis(SizeBasis::Allocated))
                    .expect("metric command"),
            )
            .expect("replacement navigation");
        assert!(replacement.layout_submitted.is_some());
        assert!(runtime.queued.is_none());
        wait_until(|| observed.lock().expect("retirement probe").is_some());
        assert_eq!(
            observed.lock().expect("retirement probe").as_deref(),
            Some("diskpie-value-reaper")
        );
    }

    #[test]
    fn coalesced_layout_replacement_retires_the_displaced_frame_off_ui() {
        fn queued(revision: u64, probe: Option<FrameRetirementProbe>) -> QueuedFrame {
            let mut builder = TreeBuilder::new(GenerationId::new(93));
            builder.add_root(NodeSpec::root("root")).expect("root");
            let snapshot = Arc::new(builder.freeze().expect("snapshot"));
            let navigation =
                Arc::new(NavigationState::new(Arc::clone(&snapshot)).expect("queued navigation"));
            let hidden = navigation.hidden_branches_plan();
            QueuedFrame {
                data: FrameData {
                    revision: DisplayedRevision {
                        generation: snapshot.generation(),
                        revision,
                        terminal: false,
                    },
                    presentation: Arc::new(
                        SnapshotPresentation::new(&snapshot).expect("presentation"),
                    ),
                    snapshot,
                    navigation,
                    retirement_probe: probe,
                },
                hidden,
                snapshot_changed: true,
            }
        }

        let observed = Arc::new(Mutex::new(None));
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        let mut retired = runtime.reserve_ui_retirement().expect("retirement permit");
        runtime.queue_latest(
            queued(1, Some(FrameRetirementProbe(Arc::clone(&observed)))),
            &mut retired,
        );
        runtime.queue_latest(queued(2, None), &mut retired);
        drop(retired);

        wait_until(|| observed.lock().expect("retirement probe").is_some());
        assert_eq!(
            observed.lock().expect("retirement probe").as_deref(),
            Some("diskpie-value-reaper")
        );
        assert_eq!(runtime.queued.as_ref().expect("latest queued").data.revision.revision, 2);
    }

    #[test]
    fn navigation_geometry_commits_atomically_and_rescan_performs_no_io() {
        let fake = Arc::new(FakeFs::default());
        fake.set("root", items(vec![directory("branch"), file("peer", 2)]));
        fake.set(PathBuf::from("root").join("branch"), items(vec![file("leaf", 3)]));
        let mut runtime = RuntimeController::new(config(64, 2)).expect("runtime");
        let mut clock = Duration::ZERO;
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("root", 5)],
                CancelToken::new(),
            )
            .expect("scan");
        pump_complete(&mut runtime, &mut clock);

        let branch = node_named(runtime.snapshot().expect("snapshot"), "branch");
        let old_root = runtime.navigation().expect("navigation").view_root();
        let old_layout_root = runtime.layout().expect("layout").root();
        let visits = fake.visits();
        let rescan = runtime
            .execute_navigation(
                runtime.navigation_command(NavigationAction::RescanAll).expect("rescan command"),
            )
            .expect("rescan");
        assert!(rescan.rescan_request().is_some());
        assert_eq!(rescan.layout_submitted, None);
        assert_eq!(fake.visits(), visits);

        let zoom = runtime
            .execute_navigation(
                runtime.navigation_command(NavigationAction::Zoom(branch)).expect("zoom command"),
            )
            .expect("zoom");
        assert!(zoom.layout_submitted.is_some());
        assert_eq!(runtime.navigation().expect("old navigation").view_root(), old_root);
        assert_eq!(runtime.layout().expect("old layout").root(), old_layout_root);

        let staged_revision = runtime.staged.as_ref().expect("zoom staged").data.revision;
        let size = runtime
            .execute_navigation(
                runtime
                    .navigation_command(NavigationAction::SetSizeBasis(SizeBasis::Allocated))
                    .expect("size command"),
            )
            .expect("composed size change");
        let latest = runtime
            .queued
            .as_ref()
            .map(|queued| &queued.data)
            .or_else(|| runtime.staged.as_ref().map(|staged| &staged.data))
            .expect("composed frame remains queued or staged");
        assert_eq!(latest.revision, staged_revision);
        assert_eq!(latest.navigation.view_root(), branch);
        assert_eq!(latest.navigation.size_basis(), SizeBasis::Allocated);
        if let Some(queued) = &runtime.queued {
            assert_eq!(
                queued.hidden.identity(),
                queued.data.navigation.hidden_branches_plan().identity()
            );
        } else {
            assert!(size.layout_submitted.is_some());
        }

        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.navigation().is_some_and(|navigation| navigation.view_root() == branch)
                && runtime.layout().is_some_and(|layout| {
                    layout.root() == branch && layout.size_basis() == SizeBasis::Allocated
                })
        });

        let terminal_snapshot = runtime.snapshot_handle().expect("terminal snapshot");
        let terminal_revision = runtime.displayed_revision().expect("revision") + 1;
        let seed = NavigationSeed {
            epoch: runtime.navigation_epoch,
            navigation: Arc::new(runtime.navigation().expect("navigation").clone()),
        };
        let terminal = prepare_publication_with_navigation(
            SnapshotPublication {
                revision: terminal_revision,
                snapshot: Arc::clone(&terminal_snapshot),
                selected: None,
            },
            true,
            Some(&seed),
        )
        .expect("terminal publication");
        let mut report = TickReport::new(runtime.state());
        accept_for_test(&mut runtime, terminal, clock, &mut report).expect("terminal stage");
        runtime
            .execute_navigation(
                runtime
                    .navigation_command(NavigationAction::SetSizeBasis(SizeBasis::Logical))
                    .expect("terminal navigation command"),
            )
            .expect("terminal navigation composition");
        let (latest, snapshot_changed) = runtime
            .queued
            .as_ref()
            .map(|queued| (&queued.data, queued.snapshot_changed))
            .or_else(|| {
                runtime.staged.as_ref().map(|staged| (&staged.data, staged.snapshot_changed))
            })
            .expect("terminal composition remains queued or staged");
        assert_eq!(latest.revision.revision, terminal_revision);
        assert!(latest.revision.terminal);
        assert!(snapshot_changed);
        assert!(Arc::ptr_eq(&latest.snapshot, &terminal_snapshot));
        assert_eq!(latest.navigation.size_basis(), SizeBasis::Logical);
    }

    #[test]
    fn replacement_preserves_identity_and_stale_publications_do_not_mutate_the_frame() {
        fn snapshot(generation: u64, name: &str, identity: FileIdentity) -> Arc<TreeSnapshot> {
            let mut builder = TreeBuilder::new(GenerationId::new(generation));
            let root = builder.add_root(NodeSpec::root("root")).expect("root");
            builder
                .add_child(root, NodeSpec::directory(name).with_file_identity(identity))
                .expect("child");
            Arc::new(builder.freeze().expect("snapshot"))
        }

        let identity = FileIdentity::new(VolumeKey::new(8), 88);
        let mut runtime = RuntimeController::new(config(64, 2)).expect("runtime");
        let mut clock = Duration::ZERO;
        let first = prepare_publication(
            SnapshotPublication {
                revision: 1,
                snapshot: snapshot(8, "before", identity),
                selected: None,
            },
            true,
        )
        .expect("prepared frame");
        let mut report = TickReport::new(runtime.state());
        accept_for_test(&mut runtime, first, clock, &mut report).expect("first frame");
        pump_until(&mut runtime, &mut clock, |runtime| runtime.snapshot().is_some());
        let before = node_named(runtime.snapshot().expect("snapshot"), "before");
        runtime
            .execute_navigation(
                runtime
                    .navigation_command(NavigationAction::Activate(before))
                    .expect("activate command"),
            )
            .expect("activate");
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.navigation().is_some_and(|navigation| navigation.view_root() == before)
        });

        let replacement = prepare_publication(
            SnapshotPublication {
                revision: 1,
                snapshot: snapshot(9, "after", identity),
                selected: None,
            },
            true,
        )
        .expect("replacement");
        let mut report = TickReport::new(runtime.state());
        accept_for_test(&mut runtime, replacement, clock, &mut report).expect("stage replacement");
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| snapshot.generation() == GenerationId::new(9))
        });
        let after = node_named(runtime.snapshot().expect("replacement"), "after");
        assert_eq!(runtime.navigation().expect("navigation").view_root(), after);
        assert_eq!(runtime.navigation().expect("navigation").selected(), Some(after));

        let stale = prepare_publication(
            SnapshotPublication {
                revision: 2,
                snapshot: snapshot(7, "stale", identity),
                selected: None,
            },
            true,
        )
        .expect("stale bundle");
        let mut report = TickReport::new(runtime.state());
        assert_eq!(accept_for_test(&mut runtime, stale, clock, &mut report), Ok(()));
        assert_eq!(runtime.snapshot().expect("unchanged").generation(), GenerationId::new(9));
    }

    #[test]
    fn same_generation_preparation_keeps_append_only_navigation_ids() {
        fn progressive(extra: bool) -> Arc<TreeSnapshot> {
            let mut builder = TreeBuilder::new(GenerationId::new(71));
            let root = builder.add_root(NodeSpec::root("root")).expect("root");
            builder.add_child(root, NodeSpec::directory("branch")).expect("branch");
            if extra {
                builder.add_child(root, NodeSpec::file("later", metrics(1))).expect("later");
            }
            Arc::new(builder.freeze().expect("snapshot"))
        }

        let first = progressive(false);
        let branch = node_named(&first, "branch");
        let mut navigation = NavigationState::new(first).expect("navigation");
        navigation
            .execute(navigation.command(NavigationAction::Activate(branch)))
            .expect("activate branch");
        let seed = NavigationSeed { epoch: 7, navigation: Arc::new(navigation) };
        let prepared = prepare_publication_with_navigation(
            SnapshotPublication { revision: 2, snapshot: progressive(true), selected: None },
            false,
            Some(&seed),
        )
        .expect("prepared progressive frame");

        assert_eq!(prepared.navigation_epoch, 7);
        assert_eq!(prepared.navigation.view_root(), branch);
        assert_eq!(prepared.navigation.selected(), Some(branch));
    }

    #[test]
    fn releasing_generation_one_after_generation_two_commits_is_not_an_error() {
        let fake = Arc::new(FakeFs::default());
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set("old", FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("old", 1)] });
        fake.set("new", items(vec![file("new", 2)]));
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("old", 61)],
                CancelToken::new(),
            )
            .expect("generation one");
        gate.wait_until_entered();
        runtime
            .request_scan(fake, vec![root("new", 62)], CancelToken::new())
            .expect("generation two");
        let mut clock = Duration::ZERO;
        pump_until(&mut runtime, &mut clock, |runtime| {
            runtime.snapshot().is_some_and(|snapshot| {
                snapshot.generation() == GenerationId::new(2)
                    && snapshot.state() == ScanState::Complete
            })
        });

        gate.release();
        pump_until(&mut runtime, &mut clock, |runtime| runtime.retiring_scans() == 0);
        assert_eq!(
            runtime.snapshot().expect("new frame retained").generation(),
            GenerationId::new(2)
        );
        assert!(runtime.last_error().is_none());
    }

    #[test]
    fn validation_precedes_generation_consumption_and_cancellation() {
        let mut invalid = config(8, 2);
        invalid.drain_budget.max_events = 0;
        assert!(matches!(
            RuntimeController::new(invalid),
            Err(RuntimeError::InvalidConfig { field: "drain_budget.max_events" })
        ));
        let mut invalid = config(8, 0);
        assert!(matches!(
            RuntimeController::new(invalid.clone()),
            Err(RuntimeError::InvalidConfig { field: "max_retiring_scans" })
        ));
        invalid.max_retiring_scans = MAX_RETIRING_SCANS + 1;
        assert!(matches!(
            RuntimeController::new(invalid),
            Err(RuntimeError::InvalidConfig { field: "max_retiring_scans" })
        ));

        let fake = Arc::new(FakeFs::default());
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set("root", FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("late", 1)] });
        let token = CancelToken::new();
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime
            .request_scan(
                Arc::clone(&fake) as Arc<dyn ScanFs>,
                vec![root("root", 9)],
                token.clone(),
            )
            .expect("valid scan");
        gate.wait_until_entered();

        let empty = runtime
            .request_scan(Arc::clone(&fake) as Arc<dyn ScanFs>, Vec::new(), CancelToken::new())
            .expect_err("empty roots rejected");
        assert_eq!(empty.error(), &RuntimeError::Scan(ScanFailure::EmptyRoots));
        let invalid = runtime
            .request_scan(fake, vec![root(PathBuf::new(), 9)], CancelToken::new())
            .expect_err("empty path rejected");
        assert_eq!(invalid.error(), &RuntimeError::InvalidRoot { index: 0 });
        assert_eq!(runtime.last_generation, 1);
        assert!(!token.is_cancelled());
        assert_eq!(runtime.active_generation(), Some(ScanGeneration::new(1)));
        gate.release();
    }

    #[test]
    fn rejected_scan_debug_exposes_counts_but_never_root_paths() {
        let sentinel = "C:\\private-sentinel-root\\customer";
        let rejection = ScanLaunchRejected {
            error: RuntimeError::RetirementBackpressure,
            fs: Arc::new(FakeFs::default()) as Arc<dyn ScanFs>,
            roots: vec![root(sentinel, 901)],
            cancel: CancelToken::new(),
        };

        let debug = format!("{rejection:?}");

        assert!(debug.contains("root_count: 1"));
        assert!(debug.contains("cancelled: false"));
        assert!(!debug.contains(sentinel));
        assert!(!debug.contains("private-sentinel-root"));
    }

    #[test]
    fn shutdown_rejects_navigation_and_repaint_matches_completion() {
        let fake = Arc::new(FakeFs::default());
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set("root", FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("late", 1)] });
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime.request_scan(fake, vec![root("root", 10)], CancelToken::new()).expect("scan");
        gate.wait_until_entered();
        let command =
            NavigationCommand::new(GenerationId::new(1), NavigationAction::ClearSelection);

        runtime.request_shutdown();
        let mut unacknowledged = runtime.supervisor.status();
        unacknowledged.quiescent = true;
        unacknowledged.shutdown_ack = false;
        assert!(!runtime.is_shutdown_complete_from(&unacknowledged));
        assert_eq!(
            runtime.navigation_command(NavigationAction::RescanAll),
            Err(RuntimeError::ShuttingDown)
        );
        assert!(matches!(runtime.execute_navigation(command), Err(RuntimeError::ShuttingDown)));
        assert!(!runtime.is_shutdown_complete());
        assert!(runtime.needs_repaint());

        let mut clock = Duration::ZERO;
        pump_until(&mut runtime, &mut clock, RuntimeController::is_shutdown_complete);
        assert!(
            !gate.state.lock().expect("gate lock").released,
            "shutdown completion must not wait for the filesystem provider to return"
        );
        assert!(runtime.supervisor.status().shutdown_ack);
        assert_eq!(runtime.state(), RuntimeState::ShutdownComplete);
        assert!(!runtime.needs_repaint());
        gate.release();
    }

    #[test]
    fn drop_detaches_a_blocked_provider_without_waiting_for_it() {
        let fake = Arc::new(FakeFs::default());
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set("root", FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("late", 1)] });
        let token = CancelToken::new();
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime.request_scan(fake, vec![root("root", 11)], token.clone()).expect("scan");
        gate.wait_until_entered();

        let started = Instant::now();
        drop(runtime);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(250), "Drop blocked for {elapsed:?}");
        wait_until(|| token.is_cancelled());
        gate.release();
    }

    #[test]
    fn saturated_runtime_drop_uses_reserved_payload_and_reaper_exits_without_leaks() {
        fn teardown_frame_data(
            revision: DisplayedRevision,
            snapshot: &Arc<TreeSnapshot>,
            navigation: &Arc<NavigationState>,
            presentation: &Arc<SnapshotPresentation>,
            observed: &Arc<Mutex<Option<String>>>,
        ) -> FrameData {
            FrameData {
                revision,
                snapshot: Arc::clone(snapshot),
                navigation: Arc::clone(navigation),
                presentation: Arc::clone(presentation),
                retirement_probe: Some(FrameRetirementProbe(Arc::clone(observed))),
            }
        }

        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        wait_until(|| lock_supervisor(&runtime.supervisor.shared).supervisor_waiting);
        let reaper_shared = Arc::clone(&runtime.supervisor.reaper.shared);

        let mut builder = TreeBuilder::new(GenerationId::new(120));
        builder.add_root(NodeSpec::root("root")).expect("root");
        let snapshot = Arc::new(builder.freeze().expect("snapshot"));
        let navigation =
            Arc::new(NavigationState::new(Arc::clone(&snapshot)).expect("teardown navigation"));
        let presentation =
            Arc::new(SnapshotPresentation::new(&snapshot).expect("teardown presentation"));
        let revision =
            DisplayedRevision { generation: snapshot.generation(), revision: 1, terminal: true };
        let observations =
            [Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None))];
        let options = LayoutOptions::new(NodeId::from_raw(0));
        let layout = compute_layout(&snapshot, options, &HiddenBranches::new()).expect("layout");
        runtime.committed = Some(CommittedFrame {
            data: teardown_frame_data(
                revision,
                &snapshot,
                &navigation,
                &presentation,
                &observations[0],
            ),
            layout,
        });
        runtime.staged = Some(StagedFrame {
            data: teardown_frame_data(
                revision,
                &snapshot,
                &navigation,
                &presentation,
                &observations[1],
            ),
            expected: ExpectedLayout {
                id: LayoutRequestId::from_raw_for_test(500),
                revision,
                root: options.root,
                size_basis: options.size_basis,
            },
            snapshot_changed: true,
        });
        runtime.queued = Some(QueuedFrame {
            data: teardown_frame_data(
                revision,
                &snapshot,
                &navigation,
                &presentation,
                &observations[2],
            ),
            hidden: navigation.hidden_branches_plan(),
            snapshot_changed: true,
        });
        drop(navigation);
        drop(presentation);
        drop(snapshot);

        let pending_drops = Arc::new(AtomicUsize::new(0));
        {
            let mut shared = lock_supervisor(&runtime.supervisor.shared);
            shared.inbox.launch = Some(PendingLaunch {
                branch: None,
                initial_omissions: 0,
                generation: ScanGeneration::new(121),
                fs: Arc::new(DropFs(Arc::clone(&pending_drops))) as Arc<dyn ScanFs>,
                roots: vec![root("pending", 121)],
                cancel: CancelToken::new(),
            });
        }
        saturate_runtime_reaper(&runtime.supervisor.reaper, &gate);

        let started = Instant::now();
        drop(runtime);
        assert!(started.elapsed() < Duration::from_millis(250), "runtime drop blocked");
        assert_eq!(pending_drops.load(Ordering::SeqCst), 0, "teardown ran on the caller");
        for observed in &observations {
            assert!(observed.lock().expect("probe lock").is_none());
        }

        gate.release();
        wait_until(|| pending_drops.load(Ordering::SeqCst) == 1);
        for observed in &observations {
            wait_until(|| observed.lock().expect("probe lock").is_some());
            assert_eq!(
                observed.lock().expect("probe lock").as_deref(),
                Some("diskpie-value-reaper")
            );
        }
        wait_until(|| lock_reaper(&reaper_shared).stopped);
        let state = lock_reaper(&reaper_shared);
        assert!(state.queue.is_empty());
        assert_eq!(state.reserved, 0);
        assert_eq!(state.active, 0);
        assert_eq!(state.available, REAPER_TOTAL_CAPACITY);
        assert_reaper_bound(&state);
    }

    #[test]
    fn supervisor_step_panic_stops_and_transfers_blocked_sessions_without_joining() {
        let fake = Arc::new(FakeFs::default());
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGates(vec![Arc::clone(&gate)]);
        fake.set(
            "panic",
            FakePlan::Gated { gate: Arc::clone(&gate), items: vec![file("late", 1)] },
        );
        let token = CancelToken::new();
        let mut runtime = RuntimeController::new(config(8, 2)).expect("runtime");
        runtime.request_scan(fake, vec![root("panic", 51)], token.clone()).expect("scan");
        gate.wait_until_entered();
        wait_until(|| runtime.supervisor.status().active.is_some());

        runtime.supervisor.inject_step_panic();
        wait_until(|| runtime.supervisor.status().stopped);
        assert!(token.is_cancelled());
        let mut clock = Duration::ZERO;
        assert!(matches!(tick(&mut runtime, &mut clock), Err(RuntimeError::SupervisorStopped)));
        runtime.request_shutdown();
        assert!(runtime.is_shutdown_complete());
        assert_eq!(runtime.state(), RuntimeState::ShutdownComplete);

        let started = Instant::now();
        drop(runtime);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "panic teardown joined a blocked provider"
        );
        gate.release();
    }
}
