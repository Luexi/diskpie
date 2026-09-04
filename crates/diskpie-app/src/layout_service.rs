//! Latest-wins background scheduling for sunburst layout work.
//!
//! The UI publishes immutable snapshots and polls plain-data completions. A
//! bounded single-slot mailbox coalesces superseded requests, so progressive
//! scans cannot create an unbounded layout backlog or run layout on the render
//! thread.

use crate::navigation::HiddenBranchesPlan;
use diskpie_core::{
    GenerationId, TreeSnapshot,
    sunburst::{LayoutError, LayoutOptions, SizeBasis, SunburstLayout, compute_layout},
};
use std::{
    error::Error,
    fmt, io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, JoinHandle},
};

/// Monotonic identity for a submitted layout request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct LayoutRequestId(u64);

impl LayoutRequestId {
    /// Returns the process-local request sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn from_raw_for_test(value: u64) -> Self {
        Self(value)
    }
}

/// Failure produced by the worker for one accepted request.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutTaskError {
    /// The pure layout engine rejected the snapshot or parameters.
    Layout(LayoutError),
    /// A defensive panic boundary caught an unexpected worker failure.
    WorkerPanicked,
}

impl fmt::Display for LayoutTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(error) => error.fmt(formatter),
            Self::WorkerPanicked => formatter.write_str("the sunburst layout worker panicked"),
        }
    }
}

impl Error for LayoutTaskError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Layout(error) => Some(error),
            Self::WorkerPanicked => None,
        }
    }
}

impl From<LayoutError> for LayoutTaskError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

/// Latest accepted completion returned to the application thread.
#[derive(Debug)]
pub struct LayoutCompletion {
    pub id: LayoutRequestId,
    pub source_generation: GenerationId,
    pub root: diskpie_core::NodeId,
    pub size_basis: SizeBasis,
    pub result: Result<SunburstLayout, LayoutTaskError>,
    #[cfg(test)]
    retirement_probe: Option<RetirementProbe>,
}

/// Why a new request could not enter the one-slot mailbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutSubmitError {
    RequestIdExhausted,
    WorkerStopped,
    RetirementBackpressure,
}

impl fmt::Display for LayoutSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestIdExhausted => formatter.write_str("layout request IDs are exhausted"),
            Self::WorkerStopped => formatter.write_str("the layout worker has stopped"),
            Self::RetirementBackpressure => {
                formatter.write_str("layout retirement capacity is temporarily exhausted")
            }
        }
    }
}

impl Error for LayoutSubmitError {}

/// An unaccepted layout request returned with all caller-owned inputs intact.
pub struct LayoutSubmitRejected {
    error: LayoutSubmitError,
    snapshot: Arc<TreeSnapshot>,
    options: LayoutOptions,
    hidden: HiddenBranchesPlan,
}

impl fmt::Debug for LayoutSubmitRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LayoutSubmitRejected")
            .field("error", &self.error)
            .field("generation", &self.snapshot.generation())
            .field("options", &self.options)
            .field("hidden_len", &self.hidden.len())
            .finish()
    }
}

impl fmt::Display for LayoutSubmitRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Error for LayoutSubmitRejected {}

impl LayoutSubmitRejected {
    #[must_use]
    pub const fn error(&self) -> LayoutSubmitError {
        self.error
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (LayoutSubmitError, Arc<TreeSnapshot>, LayoutOptions, HiddenBranchesPlan) {
        (self.error, self.snapshot, self.options, self.hidden)
    }
}

#[cfg(test)]
#[derive(Debug)]
struct RetirementProbe(Arc<Mutex<Option<String>>>);

#[cfg(test)]
impl Drop for RetirementProbe {
    fn drop(&mut self) {
        *self.0.lock().expect("layout retirement probe lock") =
            thread::current().name().map(str::to_owned);
    }
}

#[derive(Debug)]
struct LayoutJob {
    id: LayoutRequestId,
    snapshot: Arc<TreeSnapshot>,
    options: LayoutOptions,
    hidden: HiddenBranchesPlan,
    #[cfg(test)]
    retirement_probe: Option<RetirementProbe>,
}

#[derive(Debug, Default)]
struct LayoutRetirement {
    pending: Vec<LayoutJob>,
    completed: Vec<LayoutCompletion>,
}

#[derive(Debug, Default)]
struct WorkerState {
    pending: Option<LayoutJob>,
    active: Option<LayoutRequestId>,
    completed: Option<LayoutCompletion>,
    retired: Option<LayoutRetirement>,
    latest: Option<LayoutRequestId>,
    shutdown: bool,
    #[cfg(test)]
    last_submitted_hidden_ptr: Option<usize>,
    #[cfg(test)]
    paused: bool,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<WorkerState>,
    wake: Condvar,
}

/// One background layout worker with a bounded, coalescing mailbox.
pub struct LayoutService {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    next_id: u64,
}

impl fmt::Debug for LayoutService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock_state(&self.shared);
        formatter
            .debug_struct("LayoutService")
            .field("pending", &state.pending.as_ref().map(|job| job.id))
            .field("active", &state.active)
            .field("completed", &state.completed.as_ref().map(|completion| completion.id))
            .field("latest", &state.latest)
            .field("shutdown", &state.shutdown)
            .finish()
    }
}

impl LayoutService {
    /// Starts the sole background worker. Startup performs no layout work.
    pub fn new() -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("diskpie-layout".to_owned())
            .spawn(move || worker_loop(&worker_shared))?;
        Ok(Self { shared, worker: Some(worker), next_id: 0 })
    }

    /// Replaces any queued or completed older request without waiting.
    ///
    /// Work already executing is allowed to finish, but its result is discarded
    /// if this request supersedes it before publication. Hidden branches stay
    /// immutably shared with navigation rather than copying an unbounded set.
    pub fn submit<H>(
        &mut self,
        snapshot: Arc<TreeSnapshot>,
        options: LayoutOptions,
        hidden: H,
    ) -> Result<LayoutRequestId, LayoutSubmitRejected>
    where
        H: Into<HiddenBranchesPlan>,
    {
        let hidden = hidden.into();
        if self.worker.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(LayoutSubmitRejected {
                error: LayoutSubmitError::WorkerStopped,
                snapshot,
                options,
                hidden,
            });
        }
        let Some(sequence) = self.next_id.checked_add(1) else {
            return Err(LayoutSubmitRejected {
                error: LayoutSubmitError::RequestIdExhausted,
                snapshot,
                options,
                hidden,
            });
        };
        let id = LayoutRequestId(sequence);

        let mut state = lock_state(&self.shared);
        if state.shutdown {
            return Err(LayoutSubmitRejected {
                error: LayoutSubmitError::WorkerStopped,
                snapshot,
                options,
                hidden,
            });
        }
        let displaces_work = state.pending.is_some() || state.completed.is_some();
        if displaces_work && state.retired.is_some() {
            return Err(LayoutSubmitRejected {
                error: LayoutSubmitError::RetirementBackpressure,
                snapshot,
                options,
                hidden,
            });
        }
        self.next_id = sequence;
        #[cfg(test)]
        {
            state.last_submitted_hidden_ptr = Some(hidden.identity());
        }
        if displaces_work {
            state.retired = Some(LayoutRetirement {
                pending: state.pending.take().into_iter().collect(),
                completed: state.completed.take().into_iter().collect(),
            });
        }
        state.pending = Some(LayoutJob {
            id,
            snapshot,
            options,
            hidden,
            #[cfg(test)]
            retirement_probe: None,
        });
        state.latest = Some(id);
        drop(state);
        self.shared.wake.notify_one();
        Ok(id)
    }

    /// Takes the latest completion if it is ready, never waiting for work.
    pub fn try_take(&self) -> Option<LayoutCompletion> {
        lock_state(&self.shared).completed.take()
    }

    /// Returns whether a queued or executing request remains outstanding.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        let state = lock_state(&self.shared);
        state.pending.is_some() || state.active.is_some()
    }

    /// Returns the newest accepted request identity.
    #[must_use]
    pub fn latest_request_id(&self) -> Option<LayoutRequestId> {
        lock_state(&self.shared).latest
    }
}

impl Drop for LayoutService {
    fn drop(&mut self) {
        {
            let mut state = lock_state(&self.shared);
            state.shutdown = true;
        }
        self.shared.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(shared: &Shared) {
    enum Action {
        Retire(LayoutRetirement),
        Compute(LayoutJob),
        Stop(LayoutRetirement),
    }

    loop {
        let action = {
            let mut state = lock_state(shared);
            while !state.shutdown
                && ((state.pending.is_none() && state.retired.is_none()) || worker_paused(&state))
            {
                state = shared.wake.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.shutdown {
                let mut retirement = state.retired.take().unwrap_or_default();
                retirement.pending.extend(state.pending.take());
                retirement.completed.extend(state.completed.take());
                Action::Stop(retirement)
            } else if let Some(retirement) = state.retired.take() {
                Action::Retire(retirement)
            } else {
                let job = state.pending.take().expect("a signalled layout worker has pending work");
                state.active = Some(job.id);
                Action::Compute(job)
            }
        };

        let job = match action {
            Action::Retire(retirement) => {
                drop(retirement);
                continue;
            }
            Action::Stop(retirement) => {
                drop(retirement);
                return;
            }
            Action::Compute(job) => job,
        };

        let source_generation = job.snapshot.generation();
        let root = job.options.root;
        let size_basis = job.options.size_basis;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let hidden = job.hidden.materialize();
            compute_layout(&job.snapshot, job.options, hidden.as_ref())
                .map_err(LayoutTaskError::from)
        }))
        .unwrap_or(Err(LayoutTaskError::WorkerPanicked));

        let mut state = lock_state(shared);
        if state.active == Some(job.id) {
            state.active = None;
        }
        if state.shutdown {
            return;
        }
        if state.latest == Some(job.id) {
            let replaced = state.completed.replace(LayoutCompletion {
                id: job.id,
                source_generation,
                root,
                size_basis,
                result,
                #[cfg(test)]
                retirement_probe: None,
            });
            drop(state);
            drop(replaced);
        }
    }
}

#[cfg(test)]
fn worker_paused(state: &WorkerState) -> bool {
    state.paused
}

#[cfg(not(test))]
fn worker_paused(_state: &WorkerState) -> bool {
    false
}

fn lock_state(shared: &Shared) -> MutexGuard<'_, WorkerState> {
    shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{
        MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder, sunburst::HiddenBranches,
        sunburst::SizeBasis,
    };
    use std::{thread, time::Duration};

    fn snapshot(generation: u64) -> Arc<TreeSnapshot> {
        let mut builder = TreeBuilder::new(GenerationId::new(generation));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        builder
            .add_child(
                root,
                NodeSpec::file(
                    "file",
                    OwnMetrics::new(
                        SizeMetric::known(3, MetricSource::PortableMetadata),
                        SizeMetric::known(4, MetricSource::FilesystemAllocation),
                    ),
                ),
            )
            .expect("file");
        Arc::new(builder.freeze().expect("snapshot"))
    }

    fn wait_for_completion(service: &LayoutService) -> LayoutCompletion {
        for _ in 0..2_000 {
            if let Some(completion) = service.try_take() {
                return completion;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("layout completion timed out");
    }

    #[test]
    fn computes_without_exposing_work_to_the_caller_thread() {
        let snapshot = snapshot(7);
        let mut service = LayoutService::new().expect("worker");
        let hidden = HiddenBranchesPlan::from(Arc::new(HiddenBranches::new()));
        let hidden_ptr = hidden.identity();
        let id = service
            .submit(
                Arc::clone(&snapshot),
                LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                hidden,
            )
            .expect("submit");
        assert_eq!(lock_state(&service.shared).last_submitted_hidden_ptr, Some(hidden_ptr));
        let completion = wait_for_completion(&service);

        assert_eq!(completion.id, id);
        assert_eq!(completion.source_generation, GenerationId::new(7));
        assert_eq!(completion.root, diskpie_core::NodeId::from_raw(0));
        assert_eq!(completion.result.expect("layout").sectors().len(), 2);
        assert!(!service.is_busy());
    }

    #[test]
    fn submit_retires_displaced_values_on_the_worker_and_returns_backpressure_owned() {
        let pending_observed = Arc::new(Mutex::new(None));
        let completed_observed = Arc::new(Mutex::new(None));
        let mut service = LayoutService::new().expect("worker");
        let old_snapshot = snapshot(40);
        {
            let mut state = lock_state(&service.shared);
            state.paused = true;
            state.pending = Some(LayoutJob {
                id: LayoutRequestId(40),
                snapshot: Arc::clone(&old_snapshot),
                options: LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                hidden: Arc::new(HiddenBranches::new()).into(),
                retirement_probe: Some(RetirementProbe(Arc::clone(&pending_observed))),
            });
            state.completed = Some(LayoutCompletion {
                id: LayoutRequestId(39),
                source_generation: GenerationId::new(39),
                root: diskpie_core::NodeId::from_raw(0),
                size_basis: SizeBasis::Logical,
                result: Err(LayoutTaskError::WorkerPanicked),
                retirement_probe: Some(RetirementProbe(Arc::clone(&completed_observed))),
            });
            state.latest = Some(LayoutRequestId(40));
        }

        let accepted = service
            .submit(
                snapshot(41),
                LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                Arc::new(HiddenBranches::new()),
            )
            .expect("one retirement slot accepts the replacement");
        {
            let state = lock_state(&service.shared);
            let retirement = state.retired.as_ref().expect("displaced values retained");
            assert!(retirement.pending[0].retirement_probe.is_some());
            assert!(retirement.completed[0].retirement_probe.is_some());
            assert_eq!(state.pending.as_ref().map(|job| job.id), Some(accepted));
        }

        let rejected_snapshot = snapshot(42);
        let rejected_hidden = HiddenBranchesPlan::from(Arc::new(HiddenBranches::new()));
        let hidden_identity = rejected_hidden.identity();
        let rejection = service
            .submit(
                Arc::clone(&rejected_snapshot),
                LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                rejected_hidden,
            )
            .expect_err("a second displacement is backpressured");
        assert_eq!(rejection.error(), LayoutSubmitError::RetirementBackpressure);
        let (error, returned_snapshot, _, returned_hidden) = rejection.into_parts();
        assert_eq!(error, LayoutSubmitError::RetirementBackpressure);
        assert!(Arc::ptr_eq(&returned_snapshot, &rejected_snapshot));
        assert_eq!(returned_hidden.identity(), hidden_identity);
        assert_eq!(
            lock_state(&service.shared).pending.as_ref().map(|job| job.id),
            Some(accepted),
            "rejection must leave the accepted pending request intact"
        );
        let mut returned = Vec::new();
        for generation in 43..75 {
            returned.push(
                service
                    .submit(
                        snapshot(generation),
                        LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                        Arc::new(HiddenBranches::new()),
                    )
                    .expect_err("retirement backpressure remains bounded"),
            );
        }
        assert!(
            returned.iter().all(|rejection| {
                rejection.error() == LayoutSubmitError::RetirementBackpressure
            })
        );
        assert_eq!(lock_state(&service.shared).pending.as_ref().map(|job| job.id), Some(accepted));

        {
            let mut state = lock_state(&service.shared);
            state.paused = false;
        }
        service.shared.wake.notify_all();
        drop(returned);
        for observed in [&pending_observed, &completed_observed] {
            for _ in 0..2_000 {
                if observed.lock().expect("probe lock").is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(observed.lock().expect("probe lock").as_deref(), Some("diskpie-layout"));
        }
    }

    #[test]
    fn latest_request_wins_even_if_an_older_result_was_ready() {
        let snapshot = snapshot(8);
        let mut service = LayoutService::new().expect("worker");
        let first = service
            .submit(
                Arc::clone(&snapshot),
                LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                Arc::new(HiddenBranches::new()),
            )
            .expect("first");
        while service.is_busy() {
            thread::yield_now();
        }

        let mut options = LayoutOptions::new(diskpie_core::NodeId::from_raw(0));
        options.size_basis = SizeBasis::Allocated;
        let second =
            service.submit(snapshot, options, Arc::new(HiddenBranches::new())).expect("second");
        let completion = wait_for_completion(&service);

        assert!(second > first);
        assert_eq!(completion.id, second);
        assert_eq!(completion.size_basis, SizeBasis::Allocated);
        assert_eq!(service.latest_request_id(), Some(second));
        assert!(service.try_take().is_none());
    }

    #[test]
    fn invalid_layout_is_reported_as_plain_data() {
        let snapshot = snapshot(9);
        let mut service = LayoutService::new().expect("worker");
        let mut options = LayoutOptions::new(diskpie_core::NodeId::from_raw(0));
        options.max_sectors = 0;
        service
            .submit(snapshot, options, Arc::new(HiddenBranches::new()))
            .expect("accepted request");
        let completion = wait_for_completion(&service);

        assert!(matches!(
            completion.result,
            Err(LayoutTaskError::Layout(LayoutError::EmptySectorBudget))
        ));
    }

    #[test]
    fn dropping_an_idle_service_joins_the_worker() {
        let service = LayoutService::new().expect("worker");
        drop(service);
    }
}
