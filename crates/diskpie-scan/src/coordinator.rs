//! Fixed-worker, bounded-channel scan coordinator.

use crate::{
    fs::{DirectoryItem, FsEntry, ScanFs, ScanOmission, VisitControl},
    protocol::{
        CounterField, DirectoryCompletion, DirectoryState, ScanBatch, ScanCounters, ScanEvent,
        ScanFailure, ScanGeneration, ScanNodeId, ScanOptions, ScanReceiveError, ScanRequest,
        ScanTerminal, ScannedEntry, StartedRoot, StorageClass, TerminalState, ThreadRole, WorkerId,
    },
};
use diskpie_core::{EntryKind, VolumeKey};
use std::{
    collections::{HashMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Cloneable cooperative cancellation flag local to one scan generation.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Running scan and its bounded application-facing event receiver.
#[derive(Debug)]
pub struct ScanSession {
    generation: ScanGeneration,
    cancel: CancelToken,
    events: Receiver<ScanEvent>,
    coordinator: Option<JoinHandle<ScanTerminal>>,
}

impl ScanSession {
    #[must_use]
    pub const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Returns whether the coordinator has exited and [`Self::join`] cannot
    /// block on filesystem work.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.coordinator.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Receives only events for this session's generation.
    pub fn try_recv(&self) -> Result<ScanEvent, ScanReceiveError> {
        loop {
            match self.events.try_recv() {
                Ok(event) if self.generation.accepts(&event) => return Ok(event),
                Ok(_) => {}
                Err(TryRecvError::Empty) => return Err(ScanReceiveError::Empty),
                Err(TryRecvError::Disconnected) => return Err(ScanReceiveError::Disconnected),
            }
        }
    }

    /// Waits up to `timeout`, discarding any stale-generation event defensively.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ScanEvent, ScanReceiveError> {
        let started = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            match self.events.recv_timeout(remaining) {
                Ok(event) if self.generation.accepts(&event) => return Ok(event),
                Ok(_) if started.elapsed() < timeout => {}
                Ok(_) => return Err(ScanReceiveError::Timeout),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(ScanReceiveError::Timeout),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ScanReceiveError::Disconnected);
                }
            }
        }
    }

    /// Drains backpressure while joining every coordinator/worker thread.
    pub fn join(mut self) -> Result<ScanTerminal, ScanFailure> {
        self.join_inner()
    }

    fn join_inner(&mut self) -> Result<ScanTerminal, ScanFailure> {
        let Some(handle) = self.coordinator.take() else {
            return Err(ScanFailure::CoordinatorPanicked);
        };
        while !handle.is_finished() {
            match self.events.recv_timeout(Duration::from_millis(2)) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.join().map_err(|_| ScanFailure::CoordinatorPanicked)
    }
}

impl Drop for ScanSession {
    fn drop(&mut self) {
        if self.coordinator.is_none() {
            return;
        }
        if self.coordinator.as_ref().is_some_and(|handle| !handle.is_finished()) {
            self.cancel.cancel();
        }
        let _ = self.join_inner();
    }
}

/// Starts a scan with a fresh cancellation token.
pub fn start_scan(fs: Arc<dyn ScanFs>, request: ScanRequest) -> Result<ScanSession, ScanFailure> {
    start_scan_with_cancel(fs, request, CancelToken::new())
}

/// Starts a scan with a caller-supplied token, including an already-cancelled one.
pub fn start_scan_with_cancel(
    fs: Arc<dyn ScanFs>,
    request: ScanRequest,
    cancel: CancelToken,
) -> Result<ScanSession, ScanFailure> {
    request.options.validate()?;
    if request.roots.is_empty() {
        return Err(ScanFailure::EmptyRoots);
    }

    let generation = request.generation;
    let event_capacity = request.options.event_capacity;
    let (event_sender, events) = mpsc::sync_channel(event_capacity);
    let coordinator_cancel = cancel.clone();
    let coordinator = thread::Builder::new()
        .name(format!("diskpie-scan-coordinator-{}", generation.get()))
        .spawn(move || run_coordinator(fs, request, coordinator_cancel, event_sender))
        .map_err(|error| ScanFailure::ThreadSpawn {
            role: ThreadRole::Coordinator,
            detail: error.to_string(),
        })?;

    Ok(ScanSession { generation, cancel, events, coordinator: Some(coordinator) })
}

#[derive(Clone, Debug)]
struct WorkItem {
    generation: ScanGeneration,
    directory: ScanNodeId,
    path: PathBuf,
    volume: VolumeKey,
    storage_class: StorageClass,
}

/// Pending directories kept as one FIFO per volume with round-robin rotation.
///
/// Dispatch inspects only the head of each volume queue, so a volume whose
/// permit cap is saturated never forces a scan over every pending directory.
/// A large scan can hold hundreds of thousands of pending directories on one
/// volume, which made the previous global-queue search quadratic whenever a
/// rotational or removable cap was reached.
#[derive(Debug, Default)]
struct PendingQueue {
    queues: HashMap<VolumeKey, VecDeque<WorkItem>>,
    rotation: VecDeque<VolumeKey>,
    len: usize,
}

impl PendingQueue {
    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        self.queues.clear();
        self.rotation.clear();
        self.len = 0;
    }

    fn push_back(&mut self, work: WorkItem) {
        let queue = self.queues.entry(work.volume).or_default();
        if queue.is_empty() {
            self.rotation.push_back(work.volume);
        }
        queue.push_back(work);
        self.len += 1;
    }

    /// Returns an item to the head of its volume queue after a full worker
    /// channel rejected it, keeping that volume next in rotation.
    fn push_front(&mut self, work: WorkItem) {
        let queue = self.queues.entry(work.volume).or_default();
        if queue.is_empty() {
            self.rotation.push_front(work.volume);
        } else {
            self.rotation.retain(|volume| *volume != work.volume);
            self.rotation.push_front(work.volume);
        }
        queue.push_front(work);
        self.len += 1;
    }

    /// Pops the oldest directory of the first volume in rotation that
    /// `eligible` accepts and moves that volume to the back of the rotation so
    /// every volume with available permits gets a turn.
    fn pop_eligible(&mut self, mut eligible: impl FnMut(VolumeKey) -> bool) -> Option<WorkItem> {
        for _ in 0..self.rotation.len() {
            let volume = self.rotation.pop_front()?;
            if !eligible(volume) {
                self.rotation.push_back(volume);
                continue;
            }
            let Some(queue) = self.queues.get_mut(&volume) else {
                continue;
            };
            let work = queue.pop_front();
            if queue.is_empty() {
                self.queues.remove(&volume);
            } else {
                self.rotation.push_back(volume);
            }
            if let Some(work) = work {
                self.len -= 1;
                return Some(work);
            }
        }
        None
    }
}

#[derive(Debug)]
struct RawBatch {
    entries: Vec<FsEntry>,
    omissions: Vec<ScanOmission>,
    counters: ScanCounters,
}

#[derive(Debug)]
enum WorkerMessage {
    Batch {
        generation: ScanGeneration,
        worker: WorkerId,
        directory: ScanNodeId,
        batch: RawBatch,
    },
    Done {
        generation: ScanGeneration,
        worker: WorkerId,
        directory: ScanNodeId,
        counters: ScanCounters,
        state: DirectoryState,
    },
    Failed {
        generation: ScanGeneration,
        worker: WorkerId,
        directory: ScanNodeId,
        failure: ScanFailure,
    },
}

impl WorkerMessage {
    const fn generation(&self) -> ScanGeneration {
        match self {
            Self::Batch { generation, .. }
            | Self::Done { generation, .. }
            | Self::Failed { generation, .. } => *generation,
        }
    }
}

#[derive(Clone, Debug)]
struct BusyWork {
    work: WorkItem,
    received: ScanCounters,
}

#[derive(Debug)]
struct WorkerSlot {
    id: WorkerId,
    sender: Option<SyncSender<WorkItem>>,
    handle: Option<JoinHandle<()>>,
    busy: Option<BusyWork>,
}

#[derive(Debug)]
struct WorkerPool {
    slots: Vec<WorkerSlot>,
    cancel: CancelToken,
    park: Duration,
}

impl WorkerPool {
    fn spawn(
        fs: &Arc<dyn ScanFs>,
        count: usize,
        result_sender: &SyncSender<WorkerMessage>,
        cancel: &CancelToken,
        options: &ScanOptions,
    ) -> Result<Self, ScanFailure> {
        let mut pool = Self {
            slots: Vec::with_capacity(count),
            cancel: cancel.clone(),
            park: options.backpressure_park,
        };
        for index in 0..count {
            let raw_id = u16::try_from(index)
                .map_err(|_| ScanFailure::InvalidOptions { field: "worker_count" })?;
            let id = WorkerId::from_raw(raw_id);
            let (sender, receiver) = mpsc::sync_channel(1);
            let worker_fs = Arc::clone(fs);
            let worker_results = result_sender.clone();
            let worker_cancel = cancel.clone();
            let worker_options = options.clone();
            let handle = thread::Builder::new()
                .name(format!("diskpie-scan-worker-{raw_id}"))
                .spawn(move || {
                    worker_loop(
                        id,
                        worker_fs,
                        receiver,
                        worker_results,
                        worker_cancel,
                        worker_options,
                    );
                })
                .map_err(|error| ScanFailure::ThreadSpawn {
                    role: ThreadRole::Worker(id),
                    detail: error.to_string(),
                })?;
            pool.slots.push(WorkerSlot {
                id,
                sender: Some(sender),
                handle: Some(handle),
                busy: None,
            });
        }
        Ok(pool)
    }

    fn all_idle(&self) -> bool {
        self.slots.iter().all(|slot| slot.busy.is_none())
    }

    fn active_count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.busy.is_some()).count()
    }

    fn shutdown(&mut self, results: &Receiver<WorkerMessage>) {
        for slot in &mut self.slots {
            slot.sender.take();
        }
        while self
            .slots
            .iter()
            .filter_map(|slot| slot.handle.as_ref())
            .any(|handle| !handle.is_finished())
        {
            match results.recv_timeout(self.park) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for slot in &mut self.slots {
            if let Some(handle) = slot.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        if self.slots.iter().all(|slot| slot.handle.is_none()) {
            return;
        }
        self.cancel.cancel();
        for slot in &mut self.slots {
            slot.sender.take();
        }
        for slot in &mut self.slots {
            if let Some(handle) = slot.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

#[derive(Debug)]
struct CoordinatorState {
    generation: ScanGeneration,
    next_id: u64,
    pending: PendingQueue,
    active_by_volume: HashMap<VolumeKey, usize>,
    volume_caps: HashMap<VolumeKey, usize>,
    counters: ScanCounters,
    partial: bool,
    peak_active_workers: usize,
    peak_pending_directories: usize,
}

impl CoordinatorState {
    fn allocate_id(&mut self) -> Result<ScanNodeId, ScanFailure> {
        let id = ScanNodeId::from_raw(self.next_id);
        self.next_id = self.next_id.checked_add(1).ok_or(ScanFailure::IdExhausted)?;
        Ok(id)
    }

    fn note_peaks(&mut self, pool: &WorkerPool) {
        self.peak_active_workers = self.peak_active_workers.max(pool.active_count());
        self.peak_pending_directories = self.peak_pending_directories.max(self.pending.len());
    }
}

fn run_coordinator(
    fs: Arc<dyn ScanFs>,
    request: ScanRequest,
    cancel: CancelToken,
    events: SyncSender<ScanEvent>,
) -> ScanTerminal {
    let generation = request.generation;
    let mut state = CoordinatorState {
        generation,
        next_id: 0,
        pending: PendingQueue::default(),
        active_by_volume: HashMap::new(),
        volume_caps: HashMap::new(),
        counters: ScanCounters::default(),
        partial: false,
        peak_active_workers: 0,
        peak_pending_directories: 0,
    };

    let mut started_roots = Vec::with_capacity(request.roots.len());
    for root in &request.roots {
        let id = match state.allocate_id() {
            Ok(id) => id,
            Err(failure) => {
                return emit_early_terminal(&events, &cancel, &state, failure);
            }
        };
        if let Err(failure) = state.counters.observe_entry(EntryKind::Root) {
            return emit_early_terminal(&events, &cancel, &state, failure);
        }
        let cap = request.options.permit_caps.for_class(root.storage_class);
        state
            .volume_caps
            .entry(root.volume)
            .and_modify(|existing| *existing = (*existing).min(cap))
            .or_insert(cap);
        state.pending.push_back(WorkItem {
            generation,
            directory: id,
            path: root.path.clone(),
            volume: root.volume,
            storage_class: root.storage_class,
        });
        started_roots.push(StartedRoot {
            id,
            path: root.path.clone(),
            volume: root.volume,
            storage_class: root.storage_class,
        });
    }
    state.peak_pending_directories = state.pending.len();

    if let Err(failure) = send_event(
        &events,
        ScanEvent::Started { generation, roots: started_roots },
        &cancel,
        SendMode::Essential,
        request.options.backpressure_park,
    ) {
        return terminal_from_state(&state, TerminalState::Failed(failure));
    }

    if cancel.is_cancelled() {
        state.pending.clear();
        let terminal = terminal_from_state(&state, TerminalState::Cancelled);
        let _ = send_event(
            &events,
            ScanEvent::Terminal(terminal.clone()),
            &cancel,
            SendMode::Essential,
            request.options.backpressure_park,
        );
        return terminal;
    }

    let (result_sender, results) = mpsc::sync_channel(request.options.result_capacity);
    let mut pool = match WorkerPool::spawn(
        &fs,
        request.options.worker_count,
        &result_sender,
        &cancel,
        &request.options,
    ) {
        Ok(pool) => pool,
        Err(failure) => {
            cancel.cancel();
            let terminal = terminal_from_state(&state, TerminalState::Failed(failure));
            let _ = send_event(
                &events,
                ScanEvent::Terminal(terminal.clone()),
                &cancel,
                SendMode::Essential,
                request.options.backpressure_park,
            );
            return terminal;
        }
    };
    drop(result_sender);

    let terminal_state =
        coordinator_loop(&mut state, &mut pool, &results, &events, &cancel, &request.options);
    if matches!(terminal_state, TerminalState::Failed(_)) {
        cancel.cancel();
    }
    let terminal = terminal_from_state(&state, terminal_state);
    let _ = send_event(
        &events,
        ScanEvent::Terminal(terminal.clone()),
        &cancel,
        SendMode::Essential,
        request.options.backpressure_park,
    );
    pool.shutdown(&results);
    terminal
}

fn coordinator_loop(
    state: &mut CoordinatorState,
    pool: &mut WorkerPool,
    results: &Receiver<WorkerMessage>,
    events: &SyncSender<ScanEvent>,
    cancel: &CancelToken,
    options: &ScanOptions,
) -> TerminalState {
    let mut cancellation_observed = false;
    loop {
        if cancel.is_cancelled() && !cancellation_observed {
            cancellation_observed = true;
            state.pending.clear();
        }

        if !cancellation_observed && let Err(failure) = dispatch_available(state, pool) {
            return TerminalState::Failed(failure);
        }
        state.note_peaks(pool);

        if pool.all_idle() {
            if cancellation_observed {
                return TerminalState::Cancelled;
            }
            if state.pending.is_empty() {
                return if state.partial {
                    TerminalState::Partial
                } else {
                    TerminalState::Complete
                };
            }
        }

        match results.recv_timeout(options.coordinator_poll) {
            Ok(message) => {
                if message.generation() != state.generation {
                    continue;
                }
                if let Err(failure) =
                    process_worker_message(state, pool, events, cancel, options, message)
                {
                    return TerminalState::Failed(failure);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(failure) = detect_finished_worker(pool, cancellation_observed, state) {
                    if cancellation_observed {
                        continue;
                    }
                    return TerminalState::Failed(failure);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if cancellation_observed {
                    // Every worker sender is gone, so a worker that gave up
                    // its final `Done` because the result channel was full at
                    // cancellation time can never report its slot idle.
                    // Release those slots here instead of misreporting the
                    // cancelled scan as a channel failure.
                    let abandoned = pool
                        .slots
                        .iter()
                        .filter_map(|slot| {
                            slot.busy.as_ref().map(|busy| (slot.id, busy.work.directory))
                        })
                        .collect::<Vec<_>>();
                    for (worker, directory) in abandoned {
                        let _ = release_worker(state, pool, worker, directory);
                    }
                    return TerminalState::Cancelled;
                }
                if pool.all_idle() && state.pending.is_empty() {
                    return if state.partial {
                        TerminalState::Partial
                    } else {
                        TerminalState::Complete
                    };
                }
                return TerminalState::Failed(ScanFailure::ResultChannelDisconnected);
            }
        }
    }
}

fn dispatch_available(
    state: &mut CoordinatorState,
    pool: &mut WorkerPool,
) -> Result<(), ScanFailure> {
    for slot in &mut pool.slots {
        if slot.busy.is_some() {
            continue;
        }
        let CoordinatorState { pending, active_by_volume, volume_caps, .. } = &mut *state;
        let Some(work) = pending.pop_eligible(|volume| {
            let active = active_by_volume.get(&volume).copied().unwrap_or(0);
            let cap = volume_caps.get(&volume).copied().unwrap_or(1);
            active < cap
        }) else {
            continue;
        };
        let sender = slot.sender.as_ref().ok_or(ScanFailure::WorkerDisconnected {
            worker: slot.id,
            directory: Some(work.directory),
        })?;
        match sender.try_send(work.clone()) {
            Ok(()) => {
                let active = state.active_by_volume.entry(work.volume).or_insert(0);
                *active = active
                    .checked_add(1)
                    .ok_or(ScanFailure::CounterOverflow { field: CounterField::Directories })?;
                slot.busy = Some(BusyWork { work, received: ScanCounters::default() });
            }
            Err(TrySendError::Full(work)) => state.pending.push_front(work),
            Err(TrySendError::Disconnected(work)) => {
                return Err(ScanFailure::WorkerDisconnected {
                    worker: slot.id,
                    directory: Some(work.directory),
                });
            }
        }
    }
    Ok(())
}

fn process_worker_message(
    state: &mut CoordinatorState,
    pool: &mut WorkerPool,
    events: &SyncSender<ScanEvent>,
    cancel: &CancelToken,
    options: &ScanOptions,
    message: WorkerMessage,
) -> Result<(), ScanFailure> {
    match message {
        WorkerMessage::Batch { generation, worker, directory, batch } => process_batch(
            state, pool, events, cancel, options, generation, worker, directory, batch,
        ),
        WorkerMessage::Done { generation, worker, directory, counters, state: directory_state } => {
            process_done(
                state,
                pool,
                events,
                cancel,
                options,
                generation,
                worker,
                directory,
                counters,
                directory_state,
            )
        }
        WorkerMessage::Failed { worker, directory, failure, .. } => {
            release_worker(state, pool, worker, directory)?;
            Err(failure)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_batch(
    state: &mut CoordinatorState,
    pool: &mut WorkerPool,
    events: &SyncSender<ScanEvent>,
    cancel: &CancelToken,
    options: &ScanOptions,
    generation: ScanGeneration,
    worker: WorkerId,
    directory: ScanNodeId,
    batch: RawBatch,
) -> Result<(), ScanFailure> {
    let work = busy_work(pool, worker, directory)?.work.clone();
    let mut recomputed = ScanCounters::default();
    let mut scanned_entries = Vec::with_capacity(batch.entries.len());
    for entry in batch.entries {
        validate_fs_entry(&entry)?;
        recomputed.observe_entry(entry.kind)?;
        let id = state.allocate_id()?;
        let child_path = work.path.join(&entry.name);
        if !cancel.is_cancelled() && entry.kind.can_have_children() {
            state.pending.push_back(WorkItem {
                generation,
                directory: id,
                path: child_path,
                volume: work.volume,
                storage_class: work.storage_class,
            });
        }
        scanned_entries.push(ScannedEntry::from_fs(id, directory, entry));
    }
    for _ in &batch.omissions {
        recomputed.observe_omission()?;
    }
    recomputed.observe_batch()?;
    if recomputed != batch.counters {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!(
                "worker {worker} supplied inconsistent counters for directory {directory}"
            ),
        });
    }
    state.counters.checked_add(batch.counters)?;
    if !batch.omissions.is_empty() {
        state.partial = true;
    }
    busy_work_mut(pool, worker, directory)?.received.checked_add(batch.counters)?;
    state.peak_pending_directories = state.peak_pending_directories.max(state.pending.len());

    let _ = send_event(
        events,
        ScanEvent::Batch(ScanBatch {
            generation,
            parent: directory,
            entries: scanned_entries,
            omissions: batch.omissions,
            counters: batch.counters,
        }),
        cancel,
        SendMode::Cancelable,
        options.backpressure_park,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_done(
    state: &mut CoordinatorState,
    pool: &mut WorkerPool,
    events: &SyncSender<ScanEvent>,
    cancel: &CancelToken,
    options: &ScanOptions,
    generation: ScanGeneration,
    worker: WorkerId,
    directory: ScanNodeId,
    counters: ScanCounters,
    directory_state: DirectoryState,
) -> Result<(), ScanFailure> {
    let busy = release_worker(state, pool, worker, directory)?;
    if directory_state != DirectoryState::Cancelled && busy.received != counters {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!("worker {worker} completion counters differ for directory {directory}"),
        });
    }
    state.counters.observe_directory_completion()?;
    if directory_state == DirectoryState::Partial {
        state.partial = true;
    }
    let _ = send_event(
        events,
        ScanEvent::DirectoryComplete(DirectoryCompletion {
            generation,
            directory,
            counters,
            state: directory_state,
        }),
        cancel,
        SendMode::Cancelable,
        options.backpressure_park,
    )?;
    Ok(())
}

fn busy_work(
    pool: &WorkerPool,
    worker: WorkerId,
    directory: ScanNodeId,
) -> Result<&BusyWork, ScanFailure> {
    let slot = pool.slots.get(worker.index()).ok_or_else(|| ScanFailure::ProtocolViolation {
        detail: format!("unknown worker {worker}"),
    })?;
    let busy = slot.busy.as_ref().ok_or_else(|| ScanFailure::ProtocolViolation {
        detail: format!("idle worker {worker} reported directory {directory}"),
    })?;
    if busy.work.directory != directory {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!(
                "worker {worker} reported directory {directory} while assigned {}",
                busy.work.directory
            ),
        });
    }
    Ok(busy)
}

fn busy_work_mut(
    pool: &mut WorkerPool,
    worker: WorkerId,
    directory: ScanNodeId,
) -> Result<&mut BusyWork, ScanFailure> {
    let slot = pool.slots.get_mut(worker.index()).ok_or_else(|| {
        ScanFailure::ProtocolViolation { detail: format!("unknown worker {worker}") }
    })?;
    let busy = slot.busy.as_mut().ok_or_else(|| ScanFailure::ProtocolViolation {
        detail: format!("idle worker {worker} reported directory {directory}"),
    })?;
    if busy.work.directory != directory {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!(
                "worker {worker} reported directory {directory} while assigned {}",
                busy.work.directory
            ),
        });
    }
    Ok(busy)
}

fn release_worker(
    state: &mut CoordinatorState,
    pool: &mut WorkerPool,
    worker: WorkerId,
    directory: ScanNodeId,
) -> Result<BusyWork, ScanFailure> {
    let slot = pool.slots.get_mut(worker.index()).ok_or_else(|| {
        ScanFailure::ProtocolViolation { detail: format!("unknown worker {worker}") }
    })?;
    let busy = slot.busy.take().ok_or_else(|| ScanFailure::ProtocolViolation {
        detail: format!("idle worker {worker} completed directory {directory}"),
    })?;
    if busy.work.directory != directory {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!(
                "worker {worker} completed directory {directory} while assigned {}",
                busy.work.directory
            ),
        });
    }
    let active = state.active_by_volume.get_mut(&busy.work.volume).ok_or_else(|| {
        ScanFailure::ProtocolViolation {
            detail: format!("volume permit missing for directory {directory}"),
        }
    })?;
    *active = active.checked_sub(1).ok_or_else(|| ScanFailure::ProtocolViolation {
        detail: format!("volume permit underflow for directory {directory}"),
    })?;
    if *active == 0 {
        state.active_by_volume.remove(&busy.work.volume);
    }
    Ok(busy)
}

fn detect_finished_worker(
    pool: &mut WorkerPool,
    cancellation_observed: bool,
    state: &mut CoordinatorState,
) -> Option<ScanFailure> {
    for index in 0..pool.slots.len() {
        let finished = pool.slots[index].handle.as_ref().is_some_and(JoinHandle::is_finished);
        if !finished {
            continue;
        }
        let worker = pool.slots[index].id;
        let directory = pool.slots[index].busy.as_ref().map(|busy| busy.work.directory);
        if cancellation_observed {
            if let Some(directory) = directory {
                let _ = release_worker(state, pool, worker, directory);
            }
            continue;
        }
        return Some(ScanFailure::WorkerDisconnected { worker, directory });
    }
    None
}

fn validate_fs_entry(entry: &FsEntry) -> Result<(), ScanFailure> {
    if matches!(entry.kind, EntryKind::Root | EntryKind::SyntheticGroup) {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!("directory provider returned invalid child kind {:?}", entry.kind),
        });
    }
    let mut components = Path::new(&entry.name).components();
    let valid =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !valid {
        return Err(ScanFailure::ProtocolViolation {
            detail: format!("directory provider returned a non-child name {:?}", entry.name),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendMode {
    Cancelable,
    Essential,
}

fn send_event(
    sender: &SyncSender<ScanEvent>,
    mut event: ScanEvent,
    cancel: &CancelToken,
    mode: SendMode,
    park: Duration,
) -> Result<bool, ScanFailure> {
    loop {
        match sender.try_send(event) {
            Ok(()) => return Ok(true),
            Err(TrySendError::Full(returned)) => {
                event = returned;
                if mode == SendMode::Cancelable && cancel.is_cancelled() {
                    return Ok(false);
                }
                thread::park_timeout(park);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(ScanFailure::EventChannelDisconnected);
            }
        }
    }
}

fn terminal_from_state(state: &CoordinatorState, terminal_state: TerminalState) -> ScanTerminal {
    ScanTerminal {
        generation: state.generation,
        state: terminal_state,
        counters: state.counters,
        peak_active_workers: state.peak_active_workers,
        peak_pending_directories: state.peak_pending_directories,
    }
}

fn emit_early_terminal(
    events: &SyncSender<ScanEvent>,
    cancel: &CancelToken,
    state: &CoordinatorState,
    failure: ScanFailure,
) -> ScanTerminal {
    let terminal = terminal_from_state(state, TerminalState::Failed(failure));
    let _ = send_event(
        events,
        ScanEvent::Terminal(terminal.clone()),
        cancel,
        SendMode::Essential,
        Duration::from_millis(1),
    );
    terminal
}

fn worker_loop(
    worker: WorkerId,
    fs: Arc<dyn ScanFs>,
    work: Receiver<WorkItem>,
    results: SyncSender<WorkerMessage>,
    cancel: CancelToken,
    options: ScanOptions,
) {
    while let Ok(item) = work.recv() {
        let generation = item.generation;
        let directory = item.directory;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            process_directory(worker, fs.as_ref(), item, &results, &cancel, &options)
        }));
        match outcome {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                let _ = send_worker_message(
                    &results,
                    WorkerMessage::Failed {
                        generation,
                        worker,
                        directory,
                        failure: ScanFailure::WorkerPanicked { worker, directory: Some(directory) },
                    },
                    &cancel,
                    options.backpressure_park,
                );
                return;
            }
        }
    }
}

fn process_directory(
    worker: WorkerId,
    fs: &dyn ScanFs,
    work: WorkItem,
    results: &SyncSender<WorkerMessage>,
    cancel: &CancelToken,
    options: &ScanOptions,
) -> bool {
    if cancel.is_cancelled() {
        return send_worker_message(
            results,
            WorkerMessage::Done {
                generation: work.generation,
                worker,
                directory: work.directory,
                counters: ScanCounters::default(),
                state: DirectoryState::Cancelled,
            },
            cancel,
            options.backpressure_park,
        );
    }

    let mut accumulator = WorkerAccumulator::new();
    let mut stop_worker = false;
    let visit_result = fs.visit_directory(&work.path, &mut |item| {
        if cancel.is_cancelled() {
            return VisitControl::Stop;
        }
        if let Err(failure) = accumulator.push(item) {
            accumulator.failure = Some(failure);
            return VisitControl::Stop;
        }
        if (accumulator.buffered_items() >= options.batch_entries
            || accumulator.last_flush.elapsed() >= options.flush_interval)
            && !accumulator.flush(worker, &work, results, cancel, options.backpressure_park)
        {
            stop_worker = true;
            return VisitControl::Stop;
        }
        VisitControl::Continue
    });

    if let Some(failure) = accumulator.failure.take() {
        let _ = send_worker_message(
            results,
            WorkerMessage::Failed {
                generation: work.generation,
                worker,
                directory: work.directory,
                failure,
            },
            cancel,
            options.backpressure_park,
        );
        return false;
    }

    if let Err(error) = visit_result {
        if error.kind.is_fatal() {
            let _ = send_worker_message(
                results,
                WorkerMessage::Failed {
                    generation: work.generation,
                    worker,
                    directory: work.directory,
                    failure: ScanFailure::Provider(error),
                },
                cancel,
                options.backpressure_park,
            );
            return false;
        }
        if let Err(failure) = accumulator.push(DirectoryItem::Omission(error.into())) {
            let _ = send_worker_message(
                results,
                WorkerMessage::Failed {
                    generation: work.generation,
                    worker,
                    directory: work.directory,
                    failure,
                },
                cancel,
                options.backpressure_park,
            );
            return false;
        }
    }

    let cancelled = cancel.is_cancelled();
    if !accumulator.is_empty()
        && !accumulator.flush(worker, &work, results, cancel, options.backpressure_park)
    {
        stop_worker = true;
    }
    let state = if cancelled {
        DirectoryState::Cancelled
    } else if accumulator.total.omissions != 0 {
        DirectoryState::Partial
    } else {
        DirectoryState::Complete
    };
    if stop_worker && !cancelled {
        return false;
    }
    send_worker_message(
        results,
        WorkerMessage::Done {
            generation: work.generation,
            worker,
            directory: work.directory,
            counters: accumulator.total,
            state,
        },
        cancel,
        options.backpressure_park,
    )
}

#[derive(Debug)]
struct WorkerAccumulator {
    entries: Vec<FsEntry>,
    omissions: Vec<ScanOmission>,
    buffered: ScanCounters,
    total: ScanCounters,
    last_flush: Instant,
    failure: Option<ScanFailure>,
}

impl WorkerAccumulator {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            omissions: Vec::new(),
            buffered: ScanCounters::default(),
            total: ScanCounters::default(),
            last_flush: Instant::now(),
            failure: None,
        }
    }

    fn push(&mut self, item: DirectoryItem) -> Result<(), ScanFailure> {
        match item {
            DirectoryItem::Entry(entry) => {
                self.buffered.observe_entry(entry.kind)?;
                self.total.observe_entry(entry.kind)?;
                self.entries.push(entry);
            }
            DirectoryItem::Omission(omission) => {
                self.buffered.observe_omission()?;
                self.total.observe_omission()?;
                self.omissions.push(omission);
            }
        }
        Ok(())
    }

    fn buffered_items(&self) -> usize {
        self.entries.len() + self.omissions.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.omissions.is_empty()
    }

    fn flush(
        &mut self,
        worker: WorkerId,
        work: &WorkItem,
        results: &SyncSender<WorkerMessage>,
        cancel: &CancelToken,
        park: Duration,
    ) -> bool {
        if self.is_empty() {
            self.last_flush = Instant::now();
            return true;
        }
        if self.buffered.observe_batch().is_err() || self.total.observe_batch().is_err() {
            self.failure = Some(ScanFailure::CounterOverflow { field: CounterField::Batches });
            return false;
        }
        let batch = RawBatch {
            entries: std::mem::take(&mut self.entries),
            omissions: std::mem::take(&mut self.omissions),
            counters: std::mem::take(&mut self.buffered),
        };
        self.last_flush = Instant::now();
        send_worker_message(
            results,
            WorkerMessage::Batch {
                generation: work.generation,
                worker,
                directory: work.directory,
                batch,
            },
            cancel,
            park,
        )
    }
}

fn send_worker_message(
    sender: &SyncSender<WorkerMessage>,
    mut message: WorkerMessage,
    cancel: &CancelToken,
    park: Duration,
) -> bool {
    loop {
        match sender.try_send(message) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                message = returned;
                if cancel.is_cancelled() {
                    return false;
                }
                thread::park_timeout(park);
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

#[cfg(test)]
mod pending_queue_tests {
    use super::*;

    fn item(volume: u128, directory: u64) -> WorkItem {
        WorkItem {
            generation: ScanGeneration::new(1),
            directory: ScanNodeId::from_raw(directory),
            path: PathBuf::from(format!("v{volume}/d{directory}")),
            volume: VolumeKey::new(volume),
            storage_class: StorageClass::Unknown,
        }
    }

    fn drain(queue: &mut PendingQueue, eligible: impl Fn(VolumeKey) -> bool) -> Vec<(u128, u64)> {
        let mut popped = Vec::new();
        while let Some(work) = queue.pop_eligible(&eligible) {
            popped.push((work.volume.get(), work.directory.raw()));
        }
        popped
    }

    #[test]
    fn keeps_fifo_within_a_volume_and_rotates_across_volumes() {
        let mut queue = PendingQueue::default();
        for directory in 0..3 {
            queue.push_back(item(1, directory));
        }
        queue.push_back(item(2, 10));
        queue.push_back(item(2, 11));
        assert_eq!(queue.len(), 5);

        let order = drain(&mut queue, |_| true);
        assert_eq!(order, vec![(1, 0), (2, 10), (1, 1), (2, 11), (1, 2)]);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_saturated_volume_is_skipped_without_losing_its_work() {
        let mut queue = PendingQueue::default();
        for directory in 0..1_000 {
            queue.push_back(item(1, directory));
        }
        queue.push_back(item(2, 5_000));

        let saturated = VolumeKey::new(1);
        assert_eq!(drain(&mut queue, |volume| volume != saturated), vec![(2, 5_000)]);
        assert_eq!(queue.len(), 1_000, "the saturated volume keeps every pending directory");
        assert!(queue.pop_eligible(|volume| volume != saturated).is_none());

        let released = drain(&mut queue, |_| true);
        assert_eq!(released.len(), 1_000);
        assert!(released.windows(2).all(|pair| pair[0].1 < pair[1].1), "FIFO preserved");
    }

    #[test]
    fn a_rejected_item_returns_to_the_head_of_its_volume_and_rotation() {
        let mut queue = PendingQueue::default();
        queue.push_back(item(1, 0));
        queue.push_back(item(1, 1));
        queue.push_back(item(2, 2));

        let first = queue.pop_eligible(|_| true).expect("first item");
        assert_eq!(first.directory.raw(), 0);
        queue.push_front(first);

        assert_eq!(drain(&mut queue, |_| true), vec![(1, 0), (2, 2), (1, 1)]);
    }

    #[test]
    fn clear_empties_every_volume() {
        let mut queue = PendingQueue::default();
        queue.push_back(item(1, 0));
        queue.push_back(item(2, 1));
        queue.clear();
        assert!(queue.is_empty());
        assert!(queue.pop_eligible(|_| true).is_none());
    }
}
