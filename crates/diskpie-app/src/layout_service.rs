//! Latest-wins background scheduling for sunburst layout work.
//!
//! The UI publishes immutable snapshots and polls plain-data completions. A
//! bounded single-slot mailbox coalesces superseded requests, so progressive
//! scans cannot create an unbounded layout backlog or run layout on the render
//! thread.

use diskpie_core::{
    GenerationId, TreeSnapshot,
    sunburst::{
        HiddenBranches, LayoutError, LayoutOptions, SizeBasis, SunburstLayout, compute_layout,
    },
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
}

/// Why a new request could not enter the one-slot mailbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutSubmitError {
    RequestIdExhausted,
    WorkerStopped,
}

impl fmt::Display for LayoutSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestIdExhausted => formatter.write_str("layout request IDs are exhausted"),
            Self::WorkerStopped => formatter.write_str("the layout worker has stopped"),
        }
    }
}

impl Error for LayoutSubmitError {}

#[derive(Debug)]
struct LayoutJob {
    id: LayoutRequestId,
    snapshot: Arc<TreeSnapshot>,
    options: LayoutOptions,
    hidden: HiddenBranches,
}

#[derive(Debug, Default)]
struct WorkerState {
    pending: Option<LayoutJob>,
    active: Option<LayoutRequestId>,
    completed: Option<LayoutCompletion>,
    latest: Option<LayoutRequestId>,
    shutdown: bool,
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
    /// if this request supersedes it before publication.
    pub fn submit(
        &mut self,
        snapshot: Arc<TreeSnapshot>,
        options: LayoutOptions,
        hidden: HiddenBranches,
    ) -> Result<LayoutRequestId, LayoutSubmitError> {
        if self.worker.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(LayoutSubmitError::WorkerStopped);
        }
        let sequence = self.next_id.checked_add(1).ok_or(LayoutSubmitError::RequestIdExhausted)?;
        let id = LayoutRequestId(sequence);

        let mut state = lock_state(&self.shared);
        if state.shutdown {
            return Err(LayoutSubmitError::WorkerStopped);
        }
        self.next_id = sequence;
        state.pending = Some(LayoutJob { id, snapshot, options, hidden });
        state.completed = None;
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
            state.pending = None;
            state.completed = None;
        }
        self.shared.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(shared: &Shared) {
    loop {
        let job = {
            let mut state = lock_state(shared);
            while state.pending.is_none() && !state.shutdown {
                state = shared.wake.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.shutdown {
                return;
            }
            let job = state.pending.take().expect("a signalled layout worker has pending work");
            state.active = Some(job.id);
            job
        };

        let source_generation = job.snapshot.generation();
        let root = job.options.root;
        let size_basis = job.options.size_basis;
        let result = catch_unwind(AssertUnwindSafe(|| {
            compute_layout(&job.snapshot, job.options, &job.hidden).map_err(LayoutTaskError::from)
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
            state.completed =
                Some(LayoutCompletion { id: job.id, source_generation, root, size_basis, result });
        }
    }
}

fn lock_state(shared: &Shared) -> MutexGuard<'_, WorkerState> {
    shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{
        MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder, sunburst::SizeBasis,
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
        let id = service
            .submit(
                Arc::clone(&snapshot),
                LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                HiddenBranches::new(),
            )
            .expect("submit");
        let completion = wait_for_completion(&service);

        assert_eq!(completion.id, id);
        assert_eq!(completion.source_generation, GenerationId::new(7));
        assert_eq!(completion.root, diskpie_core::NodeId::from_raw(0));
        assert_eq!(completion.result.expect("layout").sectors().len(), 2);
        assert!(!service.is_busy());
    }

    #[test]
    fn latest_request_wins_even_if_an_older_result_was_ready() {
        let snapshot = snapshot(8);
        let mut service = LayoutService::new().expect("worker");
        let first = service
            .submit(
                Arc::clone(&snapshot),
                LayoutOptions::new(diskpie_core::NodeId::from_raw(0)),
                HiddenBranches::new(),
            )
            .expect("first");
        while service.is_busy() {
            thread::yield_now();
        }

        let mut options = LayoutOptions::new(diskpie_core::NodeId::from_raw(0));
        options.size_basis = SizeBasis::Allocated;
        let second = service.submit(snapshot, options, HiddenBranches::new()).expect("second");
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
        service.submit(snapshot, options, HiddenBranches::new()).expect("accepted request");
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
