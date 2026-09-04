//! Headless user-facing scan flow state.
//!
//! The shell renders exactly one [`ScanUi`] state per frame. The state is
//! derived from plain observations (dialog, resolver, launch, and runtime
//! facts) so the transitions can be tested without eframe, a Shell STA, or a
//! filesystem. Nothing in this module performs I/O or holds a platform handle.

use diskpie_app::{
    diagnostics::ErrorClass,
    runtime::{RuntimeError, RuntimeState},
    session::SessionPhase,
};
use diskpie_scan::{FsErrorKind, ScanFailure};

/// Coarse, user-presentable failure class. Paths and OS text never travel here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    /// Access to the selected object or its volume was denied.
    PermissionDenied,
    /// The selected path does not exist, is not ready, or is not a directory.
    PathUnavailable,
    /// Any other resolution, launch, or scan failure.
    Other,
}

impl FailureClass {
    /// Diagnostic class recorded for this failure. Never carries a path.
    #[must_use]
    pub const fn error_class(self) -> ErrorClass {
        match self {
            Self::PermissionDenied => ErrorClass::AccessDenied,
            Self::PathUnavailable => ErrorClass::NotFound,
            Self::Other => ErrorClass::Internal,
        }
    }

    #[must_use]
    pub const fn from_fs_error_kind(kind: FsErrorKind) -> Self {
        match kind {
            FsErrorKind::AccessDenied => Self::PermissionDenied,
            FsErrorKind::NotFound => Self::PathUnavailable,
            FsErrorKind::NotSupported
            | FsErrorKind::Interrupted
            | FsErrorKind::Io
            | FsErrorKind::ProviderFailure => Self::Other,
        }
    }

    #[must_use]
    pub const fn from_scan_failure(failure: &ScanFailure) -> Self {
        match failure {
            ScanFailure::Provider(error) => Self::from_fs_error_kind(error.kind),
            ScanFailure::EmptyRoots
            | ScanFailure::InvalidOptions { .. }
            | ScanFailure::ThreadSpawn { .. }
            | ScanFailure::WorkerPanicked { .. }
            | ScanFailure::WorkerDisconnected { .. }
            | ScanFailure::ResultChannelDisconnected
            | ScanFailure::EventChannelDisconnected
            | ScanFailure::CoordinatorPanicked
            | ScanFailure::IdExhausted
            | ScanFailure::CounterOverflow { .. }
            | ScanFailure::ProtocolViolation { .. } => Self::Other,
        }
    }

    #[must_use]
    pub const fn from_runtime_error(error: &RuntimeError) -> Self {
        match error {
            RuntimeError::Scan(failure) => Self::from_scan_failure(failure),
            _ => Self::Other,
        }
    }
}

/// The exact set of user-visible scan states the product direction requires.
///
/// Every variant is rendered with text, never by colour alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanUi {
    /// No results and no work in flight; the hero surface invites a choice.
    Empty,
    /// A native folder dialog is outstanding.
    Choosing,
    /// Path or volume resolution is running off the frame thread, or a
    /// launch is waiting for retirement capacity.
    Resolving,
    /// A scan is active and no snapshot has been committed yet.
    Scanning,
    /// Results are visible but incomplete: progressive results while the
    /// scan is still running (`settled == false`) or a terminal partial
    /// result (`settled == true`).
    Partial { settled: bool },
    /// Cancellation was requested and the provider has not returned yet.
    Cancelling,
    /// A complete scan is displayed.
    Complete,
    /// A cancelled scan left inspectable results.
    CancelledWithResults,
    /// Selection, resolution, launch, or the scan itself failed.
    Failed { class: FailureClass },
}

impl ScanUi {
    /// Whether cancellation is a meaningful request right now.
    #[must_use]
    pub const fn can_cancel(self) -> bool {
        matches!(self, Self::Scanning | Self::Partial { settled: false })
    }
}

/// Runtime facts the flow needs, already reduced to plain data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeObservation {
    Idle,
    Active,
    Cancelling,
    Settled(SettledPhase),
    ShuttingDown,
}

/// Terminal phase of the newest settled generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettledPhase {
    Complete,
    Partial,
    Cancelled,
    Failed(FailureClass),
}

impl RuntimeObservation {
    /// Reduces the runtime's observable state to the facts the flow uses.
    #[must_use]
    pub fn from_state(state: &RuntimeState) -> Self {
        match state {
            RuntimeState::Idle => Self::Idle,
            RuntimeState::Active { .. } => Self::Active,
            RuntimeState::Cancelling { .. } => Self::Cancelling,
            RuntimeState::Settled { phase, .. } => Self::Settled(settled_phase(phase)),
            RuntimeState::ShuttingDown { .. } | RuntimeState::ShutdownComplete => {
                Self::ShuttingDown
            }
        }
    }
}

fn settled_phase(phase: &SessionPhase) -> SettledPhase {
    match phase {
        SessionPhase::Failed(failure) => {
            SettledPhase::Failed(FailureClass::from_scan_failure(failure))
        }
        SessionPhase::Cancelled => SettledPhase::Cancelled,
        SessionPhase::Partial => SettledPhase::Partial,
        // A settled generation is terminal; the remaining variants cannot
        // describe one and are presented as complete rather than invented.
        SessionPhase::Complete | SessionPhase::AwaitingStart | SessionPhase::Running => {
            SettledPhase::Complete
        }
    }
}

/// Terminal outcome of a native folder dialog, without its payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogOutcome {
    Selected,
    Cancelled,
    Failed,
}

/// Outcome of handing a resolved launch to the runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchOutcome {
    /// The runtime accepted the roots.
    Accepted,
    /// Retirement backpressure: the same launch is retried on a later frame.
    Deferred,
    /// The launch was rejected for good.
    Rejected(FailureClass),
}

/// Accumulates plain observations and derives the single visible state.
#[derive(Clone, Debug)]
pub struct ScanFlow {
    dialog_outstanding: Option<u64>,
    resolves_outstanding: u32,
    launch_deferred: bool,
    failure: Option<FailureClass>,
    runtime: RuntimeObservation,
    has_results: bool,
}

impl Default for ScanFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanFlow {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dialog_outstanding: None,
            resolves_outstanding: 0,
            launch_deferred: false,
            failure: None,
            runtime: RuntimeObservation::Idle,
            has_results: false,
        }
    }

    /// Derives the visible state. Work in flight outranks a stale failure,
    /// and a failure outranks results that predate it so the user sees why
    /// the newer request did not replace them.
    #[must_use]
    pub const fn state(&self) -> ScanUi {
        if self.dialog_outstanding.is_some() {
            return ScanUi::Choosing;
        }
        if self.resolves_outstanding > 0 || self.launch_deferred {
            return ScanUi::Resolving;
        }
        if let Some(class) = self.failure {
            return ScanUi::Failed { class };
        }
        match self.runtime {
            RuntimeObservation::Idle | RuntimeObservation::ShuttingDown => ScanUi::Empty,
            RuntimeObservation::Active => {
                if self.has_results {
                    ScanUi::Partial { settled: false }
                } else {
                    ScanUi::Scanning
                }
            }
            RuntimeObservation::Cancelling => ScanUi::Cancelling,
            RuntimeObservation::Settled(phase) => match phase {
                SettledPhase::Failed(class) => ScanUi::Failed { class },
                SettledPhase::Complete if self.has_results => ScanUi::Complete,
                SettledPhase::Partial if self.has_results => ScanUi::Partial { settled: true },
                SettledPhase::Cancelled if self.has_results => ScanUi::CancelledWithResults,
                SettledPhase::Complete | SettledPhase::Partial | SettledPhase::Cancelled => {
                    ScanUi::Empty
                }
            },
        }
    }

    /// Whether the folder dialog is still open (or its result unread).
    #[must_use]
    pub const fn dialog_outstanding(&self) -> Option<u64> {
        self.dialog_outstanding
    }

    /// Whether any resolver job or deferred launch is outstanding.
    #[must_use]
    pub const fn background_work_outstanding(&self) -> bool {
        self.dialog_outstanding.is_some() || self.resolves_outstanding > 0 || self.launch_deferred
    }

    pub const fn dialog_submitted(&mut self, request_id: u64) {
        self.dialog_outstanding = Some(request_id);
        self.failure = None;
    }

    /// Records the terminal dialog event. An event for an unknown request is
    /// ignored so a stale STA result cannot close a newer dialog.
    pub fn dialog_finished(&mut self, request_id: u64, outcome: DialogOutcome) {
        if self.dialog_outstanding != Some(request_id) {
            return;
        }
        self.dialog_outstanding = None;
        if matches!(outcome, DialogOutcome::Failed) {
            self.failure = Some(FailureClass::Other);
        }
    }

    pub const fn resolve_started(&mut self) {
        self.resolves_outstanding = self.resolves_outstanding.saturating_add(1);
        self.failure = None;
    }

    pub const fn resolve_finished(&mut self, outcome: Result<(), FailureClass>) {
        self.resolves_outstanding = self.resolves_outstanding.saturating_sub(1);
        if let Err(class) = outcome {
            self.failure = Some(class);
        }
    }

    pub const fn launch_attempted(&mut self, outcome: LaunchOutcome) {
        match outcome {
            LaunchOutcome::Accepted => {
                self.launch_deferred = false;
                self.failure = None;
            }
            LaunchOutcome::Deferred => self.launch_deferred = true,
            LaunchOutcome::Rejected(class) => {
                self.launch_deferred = false;
                self.failure = Some(class);
            }
        }
    }

    /// Records the runtime facts of this frame. An active generation clears a
    /// stale resolution failure because the newer request evidently started.
    pub const fn observe_runtime(&mut self, runtime: RuntimeObservation, has_results: bool) {
        if matches!(runtime, RuntimeObservation::Active | RuntimeObservation::Cancelling) {
            self.failure = None;
        }
        self.runtime = runtime;
        self.has_results = has_results;
    }
}

/// Whether the logic phase must ask egui for another frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepaintRequest {
    /// Idle: no repaint until the next input event.
    None,
    /// Background services may deliver a result: poll again shortly.
    Soon,
    /// The runtime is producing work right now: repaint continuously.
    Now,
}

/// Pure repaint policy: idle frames must not request repaint.
#[must_use]
pub const fn repaint_policy(runtime_needs_repaint: bool, flow: &ScanFlow) -> RepaintRequest {
    if runtime_needs_repaint {
        RepaintRequest::Now
    } else if flow.background_work_outstanding() {
        RepaintRequest::Soon
    } else {
        RepaintRequest::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_scan::{FsError, FsOperation};

    #[test]
    fn choose_resolve_scan_partial_complete_is_the_happy_path() {
        let mut flow = ScanFlow::new();
        assert_eq!(flow.state(), ScanUi::Empty);

        flow.dialog_submitted(7);
        assert_eq!(flow.state(), ScanUi::Choosing);

        flow.dialog_finished(7, DialogOutcome::Selected);
        flow.resolve_started();
        assert_eq!(flow.state(), ScanUi::Resolving);

        flow.resolve_finished(Ok(()));
        flow.launch_attempted(LaunchOutcome::Accepted);
        flow.observe_runtime(RuntimeObservation::Active, false);
        assert_eq!(flow.state(), ScanUi::Scanning);

        flow.observe_runtime(RuntimeObservation::Active, true);
        assert_eq!(flow.state(), ScanUi::Partial { settled: false });
        assert!(flow.state().can_cancel());

        flow.observe_runtime(RuntimeObservation::Settled(SettledPhase::Complete), true);
        assert_eq!(flow.state(), ScanUi::Complete);
        assert!(!flow.state().can_cancel());
    }

    #[test]
    fn cancellation_passes_through_cancelling_into_cancelled_with_results() {
        let mut flow = ScanFlow::new();
        flow.observe_runtime(RuntimeObservation::Active, true);
        flow.observe_runtime(RuntimeObservation::Cancelling, true);
        assert_eq!(flow.state(), ScanUi::Cancelling);
        assert!(!flow.state().can_cancel());
        flow.observe_runtime(RuntimeObservation::Settled(SettledPhase::Cancelled), true);
        assert_eq!(flow.state(), ScanUi::CancelledWithResults);

        let mut early = ScanFlow::new();
        early.observe_runtime(RuntimeObservation::Settled(SettledPhase::Cancelled), false);
        assert_eq!(early.state(), ScanUi::Empty);
    }

    #[test]
    fn a_dismissed_dialog_returns_to_the_previous_results() {
        let mut flow = ScanFlow::new();
        flow.observe_runtime(RuntimeObservation::Settled(SettledPhase::Complete), true);
        flow.dialog_submitted(1);
        assert_eq!(flow.state(), ScanUi::Choosing);
        flow.dialog_finished(1, DialogOutcome::Cancelled);
        assert_eq!(flow.state(), ScanUi::Complete);
    }

    #[test]
    fn stale_dialog_events_do_not_close_a_newer_dialog() {
        let mut flow = ScanFlow::new();
        flow.dialog_submitted(2);
        flow.dialog_finished(1, DialogOutcome::Selected);
        assert_eq!(flow.state(), ScanUi::Choosing);
        assert_eq!(flow.dialog_outstanding(), Some(2));
    }

    #[test]
    fn resolution_failure_is_typed_and_cleared_by_the_next_request() {
        let mut flow = ScanFlow::new();
        flow.resolve_started();
        flow.resolve_finished(Err(FailureClass::PathUnavailable));
        assert_eq!(flow.state(), ScanUi::Failed { class: FailureClass::PathUnavailable });

        flow.dialog_submitted(3);
        assert_eq!(flow.state(), ScanUi::Choosing);
        flow.dialog_finished(3, DialogOutcome::Cancelled);
        assert_eq!(flow.state(), ScanUi::Empty);
    }

    #[test]
    fn a_failed_dialog_is_reported_as_a_failure() {
        let mut flow = ScanFlow::new();
        flow.dialog_submitted(4);
        flow.dialog_finished(4, DialogOutcome::Failed);
        assert_eq!(flow.state(), ScanUi::Failed { class: FailureClass::Other });
    }

    #[test]
    fn deferred_launch_shows_resolving_until_it_is_accepted_or_rejected() {
        let mut flow = ScanFlow::new();
        flow.launch_attempted(LaunchOutcome::Deferred);
        assert_eq!(flow.state(), ScanUi::Resolving);
        assert!(flow.background_work_outstanding());
        flow.launch_attempted(LaunchOutcome::Accepted);
        flow.observe_runtime(RuntimeObservation::Active, false);
        assert_eq!(flow.state(), ScanUi::Scanning);

        let mut rejected = ScanFlow::new();
        rejected.launch_attempted(LaunchOutcome::Deferred);
        rejected.launch_attempted(LaunchOutcome::Rejected(FailureClass::PermissionDenied));
        assert_eq!(rejected.state(), ScanUi::Failed { class: FailureClass::PermissionDenied });
    }

    #[test]
    fn scan_failures_map_to_the_typed_class_through_the_runtime_state() {
        let denied = ScanFailure::Provider(FsError::new(
            r"C:\denied",
            FsOperation::EnumerateDirectory,
            FsErrorKind::AccessDenied,
        ));
        let state = RuntimeState::Settled {
            generation: diskpie_scan::ScanGeneration::new(3),
            phase: SessionPhase::Failed(denied),
        };
        let mut flow = ScanFlow::new();
        flow.observe_runtime(RuntimeObservation::from_state(&state), false);
        assert_eq!(flow.state(), ScanUi::Failed { class: FailureClass::PermissionDenied });
        assert_eq!(FailureClass::PermissionDenied.error_class(), ErrorClass::AccessDenied);
        assert_eq!(
            FailureClass::from_runtime_error(&RuntimeError::ShuttingDown),
            FailureClass::Other
        );
    }

    #[test]
    fn runtime_observation_reduces_every_runtime_state() {
        let generation = diskpie_scan::ScanGeneration::new(1);
        assert_eq!(RuntimeObservation::from_state(&RuntimeState::Idle), RuntimeObservation::Idle);
        assert_eq!(
            RuntimeObservation::from_state(&RuntimeState::Active {
                generation,
                phase: SessionPhase::Running
            }),
            RuntimeObservation::Active
        );
        assert_eq!(
            RuntimeObservation::from_state(&RuntimeState::Cancelling { generation, pending: None }),
            RuntimeObservation::Cancelling
        );
        assert_eq!(
            RuntimeObservation::from_state(&RuntimeState::Settled {
                generation,
                phase: SessionPhase::Partial
            }),
            RuntimeObservation::Settled(SettledPhase::Partial)
        );
        assert_eq!(
            RuntimeObservation::from_state(&RuntimeState::ShuttingDown { generation: None }),
            RuntimeObservation::ShuttingDown
        );
        assert_eq!(
            RuntimeObservation::from_state(&RuntimeState::ShutdownComplete),
            RuntimeObservation::ShuttingDown
        );
    }

    #[test]
    fn idle_frames_do_not_request_repaint() {
        let idle = ScanFlow::new();
        assert_eq!(repaint_policy(false, &idle), RepaintRequest::None);

        let mut settled = ScanFlow::new();
        settled.observe_runtime(RuntimeObservation::Settled(SettledPhase::Complete), true);
        assert_eq!(repaint_policy(false, &settled), RepaintRequest::None);

        let mut choosing = ScanFlow::new();
        choosing.dialog_submitted(9);
        assert_eq!(repaint_policy(false, &choosing), RepaintRequest::Soon);

        assert_eq!(repaint_policy(true, &idle), RepaintRequest::Now);
    }
}
