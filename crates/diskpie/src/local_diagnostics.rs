//! Local, bounded, path-scrubbed diagnostic logging.

use std::{
    error::Error,
    fmt::{self, Write as _},
    io,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use diskpie_app::diagnostics::{
    DiagnosticCounters, DiagnosticEvent, DiagnosticLevel, RedactionPolicy, redact,
};
use diskpie_platform::{WindowsDailyLogHealth, WindowsDailyLogWriter};
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{
    Layer,
    filter::{LevelFilter, Targets},
    fmt::{
        FmtContext,
        format::{FormatEvent, FormatFields, Writer},
        writer::MakeWriter,
    },
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
};

/// Maximum number of complete event records queued for the writer thread.
pub const DIAGNOSTIC_QUEUE_CAPACITY: usize = 4_096;

/// Hard upper bound for one record submitted to the nonblocking queue.
pub const MAX_DIAGNOSTIC_EVENT_BYTES: usize = 4 * 1_024;

const APPLICATION_EVENT_TARGET: &str = "diskpie.event";
const MAX_RAW_EVENT_BYTES: usize = 3 * 1_024;
const RAW_TRUNCATION_MARKER: &str = " <truncated:raw>";
const EVENT_TRUNCATION_MARKER: &[u8] = b" <truncated:event>";
const INITIAL_RETENTION_TIMEOUT: Duration = Duration::from_millis(250);
const DIAGNOSTICS_FINISH_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_REAPER_CAPACITY: usize = 32;
const WORKER_REAPER_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Stable startup failure that never retains an underlying path or message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiagnosticsStartErrorKind {
    Appender,
    Worker,
    Subscriber,
}

/// Sanitized diagnostics startup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticsStartError {
    pub kind: DiagnosticsStartErrorKind,
}

impl fmt::Display for DiagnosticsStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "local diagnostics unavailable: {:?}", self.kind)
    }
}

impl Error for DiagnosticsStartError {}

/// Truthful result of the first bounded native retention attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialRetentionOutcome {
    /// The initial maintenance call returned without reporting an issue.
    Succeeded,
    /// The initial call returned and raised one or more path-free issues.
    Issues { issue_count: u64 },
    /// The worker exited before publishing an initial result.
    Unavailable,
    /// The bounded startup wait elapsed while native maintenance was still running.
    TimedOut,
}

/// Result of a bounded diagnostics shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticsFinishStatus {
    /// The worker drained, completed its final flush attempt, dropped its
    /// writer, and reported completion.
    ///
    /// The process-wide reaper still owns the finished thread handle and joins
    /// it asynchronously; callers never own or wait on that handle.
    Completed,
    /// The deadline elapsed; cancellation was requested and the bounded
    /// process-wide reaper remains responsible for completion and joining.
    HandedOff,
    /// The worker exited without reporting normal completion. The reaper still
    /// owns its thread handle and observes the eventual join result.
    WorkerFailed,
}

/// Bounded shutdown result and the counters observable at return time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticsFinishOutcome {
    pub status: DiagnosticsFinishStatus,
    pub counters: DiagnosticCounters,
    /// Present only when the owned worker reported completion inside the
    /// bounded finish; `HandedOff` and `WorkerFailed` never carry one.
    pub quiescence: Option<DiagnosticsQuiescenceReceipt>,
}

/// Receipt that the diagnostics worker drained, flushed, dropped its writer,
/// and reported completion inside [`LocalDiagnostics::finish`].
///
/// The private field keeps the receipt mintable only by `finish` from the
/// worker's own completion message, never from a bare
/// [`DiagnosticsFinishStatus`] value. The crash-marker shutdown proof
/// consumes it as the diagnostics worker family's join evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticsQuiescenceReceipt {
    _private: (),
}

/// Owns the asynchronous writer guard and bounded health counters.
pub struct LocalDiagnostics {
    guard: Option<DiagnosticsWorkerGuard>,
    dropped: Arc<AtomicU64>,
    write_failures: Arc<AtomicU64>,
    truncated_lines: Arc<AtomicU64>,
    retention_health: WindowsDailyLogHealth,
    initial_retention: InitialRetentionOutcome,
}

impl LocalDiagnostics {
    /// Initializes daily local logging without any terminal or network sink.
    ///
    /// The directory must already have been resolved and explicitly created
    /// through the project-path adapter. A worker-spawn panic is caught and
    /// translated to a plain, nonfatal startup error.
    pub fn initialize(
        directory: &Path,
        maximum_level: DiagnosticLevel,
    ) -> Result<Self, DiagnosticsStartError> {
        match panic::catch_unwind(AssertUnwindSafe(|| initialize_inner(directory, maximum_level))) {
            Ok(result) => result,
            Err(_panic) => Err(DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Worker }),
        }
    }

    /// Records one closed, path-free application event.
    pub fn record(&self, event: DiagnosticEvent) {
        match event.level() {
            DiagnosticLevel::Error => {
                tracing::error!(target: APPLICATION_EVENT_TARGET, event = %event);
            }
            DiagnosticLevel::Warn => {
                tracing::warn!(target: APPLICATION_EVENT_TARGET, event = %event);
            }
            DiagnosticLevel::Info => {
                tracing::info!(target: APPLICATION_EVENT_TARGET, event = %event);
            }
            DiagnosticLevel::Debug => {
                tracing::debug!(target: APPLICATION_EVENT_TARGET, event = %event);
            }
            DiagnosticLevel::Off => {}
        }
    }

    /// Returns a lock-free snapshot suitable for explicit diagnostic export.
    pub fn counters(&self) -> DiagnosticCounters {
        DiagnosticCounters {
            dropped_events: self.dropped.load(Ordering::Acquire),
            write_failures: self.write_failures.load(Ordering::Acquire),
            sink_truncated_lines: self.truncated_lines.load(Ordering::Acquire),
        }
    }

    /// Returns the synchronized result of the first bounded retention call.
    pub const fn initial_retention_outcome(&self) -> InitialRetentionOutcome {
        self.initial_retention
    }

    /// Returns all native retention issues observed so far without a path.
    pub fn retention_issue_count(&self) -> u64 {
        self.retention_health.retention_issue_count()
    }

    /// Attempts an orderly drain and flush for a fixed, bounded duration.
    ///
    /// The quiescence receipt is minted here and nowhere else, and only when
    /// the worker guard observed the worker's own completion message. A
    /// diagnostics value that never attached a guard reports completion
    /// without a receipt: there is no join evidence to certify.
    pub fn finish(mut self) -> DiagnosticsFinishOutcome {
        let GuardFinish { status, quiescence } = match self.guard.take() {
            Some(mut guard) => guard.finish(DIAGNOSTICS_FINISH_TIMEOUT),
            None => GuardFinish { status: DiagnosticsFinishStatus::Completed, quiescence: None },
        };
        DiagnosticsFinishOutcome { status, counters: self.counters(), quiescence }
    }
}

impl fmt::Debug for LocalDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalDiagnostics")
            .field("active", &self.guard.is_some())
            .field("counters", &self.counters())
            .field("initial_retention", &self.initial_retention)
            .field("retention_issue_count", &self.retention_issue_count())
            .finish()
    }
}

fn initialize_inner(
    directory: &Path,
    maximum_level: DiagnosticLevel,
) -> Result<LocalDiagnostics, DiagnosticsStartError> {
    let writer = WindowsDailyLogWriter::open(directory)
        .map_err(|_error| DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Appender })?;
    let retention_health = writer.health();
    let write_failures = Arc::new(AtomicU64::new(0));
    let spawned = spawn_diagnostics_worker(writer, Arc::clone(&write_failures))?;
    let initial_retention =
        await_initial_retention(&spawned.initial_retention, INITIAL_RETENTION_TIMEOUT);
    let queue = spawned.queue;
    let guard = spawned.guard;
    let dropped = Arc::clone(&queue.dropped);
    let truncated_lines = Arc::new(AtomicU64::new(0));
    let make_writer =
        EventMakeWriter { writer: queue, truncated_lines: Arc::clone(&truncated_lines) };
    let layer = tracing_subscriber::fmt::layer()
        .event_format(SafeEventFormatter)
        .with_ansi(false)
        .with_writer(make_writer)
        .with_filter(targets_for(maximum_level));
    tracing_subscriber::registry()
        .with(layer)
        .try_init()
        .map_err(|_error| DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Subscriber })?;

    Ok(LocalDiagnostics {
        guard: Some(guard),
        dropped,
        write_failures,
        truncated_lines,
        retention_health,
        initial_retention,
    })
}

fn targets_for(maximum_level: DiagnosticLevel) -> Targets {
    let application = level_filter(maximum_level);
    let upstream = match maximum_level {
        DiagnosticLevel::Off => LevelFilter::OFF,
        DiagnosticLevel::Error => LevelFilter::ERROR,
        DiagnosticLevel::Warn | DiagnosticLevel::Info | DiagnosticLevel::Debug => LevelFilter::WARN,
    };
    Targets::new().with_default(upstream).with_target(APPLICATION_EVENT_TARGET, application)
}

const fn level_filter(level: DiagnosticLevel) -> LevelFilter {
    match level {
        DiagnosticLevel::Off => LevelFilter::OFF,
        DiagnosticLevel::Error => LevelFilter::ERROR,
        DiagnosticLevel::Warn => LevelFilter::WARN,
        DiagnosticLevel::Info => LevelFilter::INFO,
        DiagnosticLevel::Debug => LevelFilter::DEBUG,
    }
}

trait WorkerWriter: io::Write + Send + 'static {
    fn maintenance_delay(&self) -> Duration;
    fn maintain_if_due(&mut self);

    fn retention_issue_count(&self) -> u64 {
        0
    }
}

impl WorkerWriter for WindowsDailyLogWriter {
    fn maintenance_delay(&self) -> Duration {
        WindowsDailyLogWriter::maintenance_delay(self)
    }

    fn maintain_if_due(&mut self) {
        WindowsDailyLogWriter::maintain_if_due(self);
    }

    fn retention_issue_count(&self) -> u64 {
        self.health().retention_issue_count()
    }
}

struct QueueOwner {
    sender: SyncSender<Box<[u8]>>,
}

#[derive(Clone)]
struct DiagnosticQueue {
    owner: Weak<QueueOwner>,
    dropped: Arc<AtomicU64>,
}

impl DiagnosticQueue {
    fn submit(&self, record: &[u8]) {
        let Some(owner) = self.owner.upgrade() else {
            saturating_increment(&self.dropped);
            return;
        };
        match owner.sender.try_send(record.into()) {
            Ok(()) => {}
            Err(TrySendError::Full(_record) | TrySendError::Disconnected(_record)) => {
                saturating_increment(&self.dropped);
            }
        }
    }

    #[cfg(test)]
    fn dropped_lines(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }
}

impl io::Write for DiagnosticQueue {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.submit(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct WorkerPermit {
    outstanding: Arc<AtomicUsize>,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        let previous = self.outstanding.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "diagnostics worker permit underflow");
    }
}

struct ReapRequest {
    worker: JoinHandle<()>,
    _permit: WorkerPermit,
    released: Arc<AtomicBool>,
}

struct DiagnosticsWorkerReaper {
    sender: SyncSender<ReapRequest>,
    outstanding: Arc<AtomicUsize>,
    registration_gate: std::sync::Mutex<()>,
    terminal: AtomicBool,
    _worker: JoinHandle<()>,
}

impl DiagnosticsWorkerReaper {
    fn start() -> Result<Self, DiagnosticsStartError> {
        let (sender, receiver) = mpsc::sync_channel(WORKER_REAPER_CAPACITY);
        let outstanding = Arc::new(AtomicUsize::new(0));
        let worker = thread::Builder::new()
            .name("diskpie-diagnostics-reaper".to_owned())
            .spawn(move || diagnostics_reaper_loop(receiver))
            .map_err(|_error| DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Worker })?;
        Ok(Self {
            sender,
            outstanding,
            registration_gate: std::sync::Mutex::new(()),
            terminal: AtomicBool::new(false),
            _worker: worker,
        })
    }

    fn try_reserve_locked(&self) -> Option<WorkerPermit> {
        if self.terminal.load(Ordering::Acquire) {
            return None;
        }
        let reserved = self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < WORKER_REAPER_CAPACITY).then_some(current + 1)
            })
            .is_ok();
        reserved.then(|| WorkerPermit { outstanding: Arc::clone(&self.outstanding) })
    }

    fn terminalize(&self) {
        self.terminal.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn disconnected_for_test() -> Self {
        let (sender, receiver) = mpsc::sync_channel(WORKER_REAPER_CAPACITY);
        drop(receiver);
        Self {
            sender,
            outstanding: Arc::new(AtomicUsize::new(0)),
            registration_gate: std::sync::Mutex::new(()),
            terminal: AtomicBool::new(false),
            _worker: thread::spawn(|| {}),
        }
    }

    #[cfg(test)]
    fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn is_terminal(&self) -> bool {
        self.terminal.load(Ordering::Acquire)
    }
}

static DIAGNOSTICS_WORKER_REAPER: OnceLock<Result<DiagnosticsWorkerReaper, DiagnosticsStartError>> =
    OnceLock::new();

fn diagnostics_worker_reaper() -> Result<&'static DiagnosticsWorkerReaper, DiagnosticsStartError> {
    match DIAGNOSTICS_WORKER_REAPER.get_or_init(DiagnosticsWorkerReaper::start) {
        Ok(reaper) => Ok(reaper),
        Err(error) => Err(*error),
    }
}

fn diagnostics_reaper_loop(receiver: Receiver<ReapRequest>) {
    let mut pending = Vec::with_capacity(WORKER_REAPER_CAPACITY);
    loop {
        while pending.len() < WORKER_REAPER_CAPACITY {
            match receiver.try_recv() {
                Ok(request) => pending.push(request),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        let mut index = 0;
        while index < pending.len() {
            if pending[index].released.load(Ordering::Acquire)
                && pending[index].worker.is_finished()
            {
                let request = pending.swap_remove(index);
                let _ignored = request.worker.join();
            } else {
                index += 1;
            }
        }

        if pending.len() == WORKER_REAPER_CAPACITY {
            thread::park_timeout(WORKER_REAPER_POLL_INTERVAL);
            continue;
        }
        match receiver.recv_timeout(WORKER_REAPER_POLL_INTERVAL) {
            Ok(request) => pending.push(request),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerExit {
    Completed,
}

struct DiagnosticsWorkerGuard {
    owner: Option<Arc<QueueOwner>>,
    cancel: Arc<AtomicBool>,
    completion: Receiver<WorkerExit>,
    released: Arc<AtomicBool>,
}

/// Result of one bounded guard finish; the receipt exists only when the
/// worker's completion message was received.
struct GuardFinish {
    status: DiagnosticsFinishStatus,
    quiescence: Option<DiagnosticsQuiescenceReceipt>,
}

impl DiagnosticsWorkerGuard {
    fn finish(&mut self, timeout: Duration) -> GuardFinish {
        self.owner.take();
        let finish = match self.completion.recv_timeout(timeout) {
            Ok(WorkerExit::Completed) => GuardFinish {
                status: DiagnosticsFinishStatus::Completed,
                quiescence: Some(DiagnosticsQuiescenceReceipt { _private: () }),
            },
            Err(RecvTimeoutError::Disconnected) => {
                GuardFinish { status: DiagnosticsFinishStatus::WorkerFailed, quiescence: None }
            }
            Err(RecvTimeoutError::Timeout) => {
                self.cancel.store(true, Ordering::Release);
                GuardFinish { status: DiagnosticsFinishStatus::HandedOff, quiescence: None }
            }
        };
        self.released.store(true, Ordering::Release);
        finish
    }
}

impl Drop for DiagnosticsWorkerGuard {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.owner.take();
        self.released.store(true, Ordering::Release);
    }
}

struct SpawnedDiagnosticsWorker {
    queue: DiagnosticQueue,
    guard: DiagnosticsWorkerGuard,
    initial_retention: Receiver<InitialRetentionOutcome>,
}

fn spawn_diagnostics_worker<W>(
    writer: W,
    failures: Arc<AtomicU64>,
) -> Result<SpawnedDiagnosticsWorker, DiagnosticsStartError>
where
    W: WorkerWriter,
{
    let reaper = diagnostics_worker_reaper()?;
    spawn_diagnostics_worker_with_reaper(reaper, writer, failures)
}

fn spawn_diagnostics_worker_with_reaper<W>(
    reaper: &DiagnosticsWorkerReaper,
    writer: W,
    failures: Arc<AtomicU64>,
) -> Result<SpawnedDiagnosticsWorker, DiagnosticsStartError>
where
    W: WorkerWriter,
{
    // Registration is serialized from reservation through publication. If the
    // receiver has died, exactly this attempted worker can lose its handle;
    // terminalization then rejects every later attempt before it can spawn.
    let _registration_guard = reaper
        .registration_gate
        .lock()
        .map_err(|_error| DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Worker })?;
    let permit = reaper
        .try_reserve_locked()
        .ok_or(DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Worker })?;
    let (sender, receiver) = mpsc::sync_channel(DIAGNOSTIC_QUEUE_CAPACITY);
    let owner = Arc::new(QueueOwner { sender });
    let queue =
        DiagnosticQueue { owner: Arc::downgrade(&owner), dropped: Arc::new(AtomicU64::new(0)) };
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (initial_sender, initial_retention) = mpsc::sync_channel(1);
    let (completion_sender, completion) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("diskpie-diagnostics".to_owned())
        .spawn(move || {
            diagnostics_worker_loop(writer, receiver, &worker_cancel, &failures, &initial_sender);
            let _ignored = completion_sender.try_send(WorkerExit::Completed);
        })
        .map_err(|_error| DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Worker })?;
    let released = Arc::new(AtomicBool::new(false));
    let registration = ReapRequest { worker, _permit: permit, released: Arc::clone(&released) };
    match reaper.sender.try_send(registration) {
        Ok(()) => {}
        Err(
            TrySendError::Full(_failed_registration)
            | TrySendError::Disconnected(_failed_registration),
        ) => {
            // Full is excluded by the permit/channel-capacity invariant. Treat
            // either failure as terminal so a dead receiver cannot cause an
            // unbounded sequence of newly spawned, detached workers.
            reaper.terminalize();
            return Err(DiagnosticsStartError { kind: DiagnosticsStartErrorKind::Worker });
        }
    }
    Ok(SpawnedDiagnosticsWorker {
        queue,
        guard: DiagnosticsWorkerGuard { owner: Some(owner), cancel, completion, released },
        initial_retention,
    })
}

fn await_initial_retention(
    receiver: &Receiver<InitialRetentionOutcome>,
    timeout: Duration,
) -> InitialRetentionOutcome {
    match receiver.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => InitialRetentionOutcome::TimedOut,
        Err(RecvTimeoutError::Disconnected) => InitialRetentionOutcome::Unavailable,
    }
}

fn diagnostics_worker_loop<W>(
    mut writer: W,
    receiver: Receiver<Box<[u8]>>,
    cancel: &AtomicBool,
    failures: &AtomicU64,
    initial_retention: &SyncSender<InitialRetentionOutcome>,
) where
    W: WorkerWriter,
{
    writer.maintain_if_due();
    let issue_count = writer.retention_issue_count();
    let initial_outcome = if issue_count == 0 {
        InitialRetentionOutcome::Succeeded
    } else {
        InitialRetentionOutcome::Issues { issue_count }
    };
    let _ignored = initial_retention.try_send(initial_outcome);

    let mut graceful_disconnect = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        match receiver.recv_timeout(writer.maintenance_delay()) {
            Ok(record) => {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                if writer.write_all(&record).is_err() {
                    saturating_increment(failures);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                graceful_disconnect = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if !cancel.load(Ordering::Acquire) {
                    writer.maintain_if_due();
                }
            }
        }
    }
    if graceful_disconnect && !cancel.load(Ordering::Acquire) && writer.flush().is_err() {
        saturating_increment(failures);
    }
    drop(writer);
}

#[derive(Clone)]
struct EventMakeWriter {
    writer: DiagnosticQueue,
    truncated_lines: Arc<AtomicU64>,
}

impl<'writer> MakeWriter<'writer> for EventMakeWriter {
    type Writer = EventLineWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        EventLineWriter::new(self.writer.clone(), Arc::clone(&self.truncated_lines))
    }
}

struct EventLineWriter {
    writer: DiagnosticQueue,
    bytes: [u8; MAX_DIAGNOSTIC_EVENT_BYTES],
    length: usize,
    capacity_truncated: bool,
    truncated_lines: Arc<AtomicU64>,
}

impl EventLineWriter {
    fn new(writer: DiagnosticQueue, truncated_lines: Arc<AtomicU64>) -> Self {
        Self {
            writer,
            bytes: [0; MAX_DIAGNOSTIC_EVENT_BYTES],
            length: 0,
            capacity_truncated: false,
            truncated_lines,
        }
    }

    fn payload_capacity() -> usize {
        MAX_DIAGNOSTIC_EVENT_BYTES - EVENT_TRUNCATION_MARKER.len() - 1
    }
}

impl io::Write for EventLineWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = Self::payload_capacity().saturating_sub(self.length);
        let copied = remaining.min(buffer.len());
        self.bytes[self.length..self.length + copied].copy_from_slice(&buffer[..copied]);
        self.length += copied;
        self.capacity_truncated |= copied != buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventLineWriter {
    fn drop(&mut self) {
        while std::str::from_utf8(&self.bytes[..self.length]).is_err() && self.length != 0 {
            self.length -= 1;
        }
        let raw_truncated =
            contains_bytes(&self.bytes[..self.length], RAW_TRUNCATION_MARKER.as_bytes());
        if self.capacity_truncated {
            let end = self.length + EVENT_TRUNCATION_MARKER.len();
            self.bytes[self.length..end].copy_from_slice(EVENT_TRUNCATION_MARKER);
            self.length = end;
        }
        if self.length == 0 || self.bytes[self.length - 1] != b'\n' {
            self.bytes[self.length] = b'\n';
            self.length += 1;
        }
        if self.capacity_truncated || raw_truncated {
            saturating_increment(&self.truncated_lines);
        }
        self.writer.submit(&self.bytes[..self.length]);
    }
}

struct SafeEventFormatter;

impl<S, N> FormatEvent<S, N> for SafeEventFormatter
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let mut raw = BoundedText::new();
        write!(&mut raw, "target={} ", metadata.target())?;
        if metadata.target() == APPLICATION_EVENT_TARGET {
            let mut visitor = ApplicationFieldVisitor { output: &mut raw, recorded: false };
            event.record(&mut visitor);
            if !visitor.recorded {
                raw.write_str("event=<omitted>")?;
            }
        } else {
            // Third-party `Debug`/`Display` implementations run on the event
            // producer. Do not invoke them or admit their free-form values to
            // the local file; a static target and level are sufficient to
            // identify the emitting component safely.
            raw.write_str("fields=<omitted>")?;
        }
        let raw = raw.finish();
        let redacted = redact(&raw, RedactionPolicy { include_paths: false });
        let timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_millis());
        writeln!(
            writer,
            "timestamp_unix_ms={timestamp} level={} {}",
            metadata.level(),
            redacted.as_str()
        )
    }
}

struct ApplicationFieldVisitor<'a> {
    output: &'a mut BoundedText,
    recorded: bool,
}

impl Visit for ApplicationFieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "event" && !self.recorded {
            self.recorded = true;
            let _ignored = write!(self.output, "event={value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event" && !self.recorded {
            self.recorded = true;
            let _ignored = write!(self.output, "event={value}");
        }
    }
}

struct BoundedText {
    value: String,
    truncated: bool,
}

impl BoundedText {
    fn new() -> Self {
        Self { value: String::with_capacity(512), truncated: false }
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.value.push_str(RAW_TRUNCATION_MARKER);
        }
        self.value
    }
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let payload_limit = MAX_RAW_EVENT_BYTES - RAW_TRUNCATION_MARKER.len();
        let remaining = payload_limit.saturating_sub(self.value.len());
        let mut copied = remaining.min(value.len());
        while copied != 0 && !value.is_char_boundary(copied) {
            copied -= 1;
        }
        self.value.push_str(&value[..copied]);
        self.truncated |= copied != value.len();
        Ok(())
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn saturating_increment(counter: &AtomicU64) {
    saturating_add(counter, 1);
}

fn saturating_add(counter: &AtomicU64, amount: u64) {
    let _ignored = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        Some(value.saturating_add(amount))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write as _,
        sync::{Condvar, Mutex},
        time::{Duration, Instant},
    };

    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CaptureWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for CaptureWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {}
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic flush failure"))
        }
    }

    impl WorkerWriter for FailingWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {}
    }

    struct BlockingWriter {
        state: Arc<(Mutex<BlockingState>, Condvar)>,
        blocked_once: bool,
    }

    #[derive(Default)]
    struct BlockingState {
        entered: bool,
        released: bool,
        dropped: bool,
    }

    impl io::Write for BlockingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if !self.blocked_once {
                self.blocked_once = true;
                let (lock, changed) = &*self.state;
                let mut state = lock.lock().expect("blocking state lock");
                state.entered = true;
                changed.notify_all();
                while !state.released {
                    state = changed.wait(state).expect("blocking state wait");
                }
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for BlockingWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {}
    }

    impl Drop for BlockingWriter {
        fn drop(&mut self) {
            let (lock, changed) = &*self.state;
            let mut state = lock.lock().expect("blocking state lock");
            state.dropped = true;
            changed.notify_all();
        }
    }

    struct IdleMaintenanceWriter {
        state: Arc<(Mutex<u64>, Condvar)>,
        next: Instant,
    }

    impl io::Write for IdleMaintenanceWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for IdleMaintenanceWriter {
        fn maintenance_delay(&self) -> Duration {
            self.next.saturating_duration_since(Instant::now())
        }

        fn maintain_if_due(&mut self) {
            let (lock, changed) = &*self.state;
            let mut count = lock.lock().expect("maintenance counter lock");
            *count += 1;
            changed.notify_all();
            self.next = Instant::now() + Duration::from_millis(20);
        }
    }

    struct BlockingFlushWriter {
        state: Arc<(Mutex<BlockingState>, Condvar)>,
    }

    impl io::Write for BlockingFlushWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let (lock, changed) = &*self.state;
            let mut state = lock.lock().expect("blocking flush state lock");
            state.entered = true;
            changed.notify_all();
            while !state.released {
                state = changed.wait(state).expect("blocking flush state wait");
            }
            Ok(())
        }
    }

    impl WorkerWriter for BlockingFlushWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {}
    }

    impl Drop for BlockingFlushWriter {
        fn drop(&mut self) {
            let (lock, changed) = &*self.state;
            let mut state = lock.lock().expect("blocking flush state lock");
            state.dropped = true;
            changed.notify_all();
        }
    }

    struct IssueMaintenanceWriter {
        issues: Arc<AtomicU64>,
        attempted: Arc<AtomicBool>,
    }

    impl io::Write for IssueMaintenanceWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for IssueMaintenanceWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {
            self.issues.store(3, Ordering::Release);
            self.attempted.store(true, Ordering::Release);
        }

        fn retention_issue_count(&self) -> u64 {
            self.issues.load(Ordering::Acquire)
        }
    }

    struct BlockingMaintenanceWriter {
        state: Arc<(Mutex<BlockingState>, Condvar)>,
    }

    impl io::Write for BlockingMaintenanceWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for BlockingMaintenanceWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {
            let (lock, changed) = &*self.state;
            let mut state = lock.lock().expect("blocking maintenance state lock");
            state.entered = true;
            changed.notify_all();
            while !state.released {
                state = changed.wait(state).expect("blocking maintenance state wait");
            }
        }
    }

    impl Drop for BlockingMaintenanceWriter {
        fn drop(&mut self) {
            let (lock, changed) = &*self.state;
            let mut state = lock.lock().expect("blocking maintenance state lock");
            state.dropped = true;
            changed.notify_all();
        }
    }

    struct PanicMaintenanceWriter;

    impl io::Write for PanicMaintenanceWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for PanicMaintenanceWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {
            panic!("synthetic initial maintenance failure");
        }
    }

    struct LifecycleWriter {
        dropped: Arc<AtomicUsize>,
    }

    impl io::Write for LifecycleWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WorkerWriter for LifecycleWriter {
        fn maintenance_delay(&self) -> Duration {
            Duration::from_secs(6 * 60 * 60)
        }

        fn maintain_if_due(&mut self) {}
    }

    impl Drop for LifecycleWriter {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn test_worker<W>(writer: W) -> (DiagnosticQueue, DiagnosticsWorkerGuard, Arc<AtomicU64>)
    where
        W: WorkerWriter,
    {
        let failures = Arc::new(AtomicU64::new(0));
        let spawned =
            spawn_diagnostics_worker(writer, Arc::clone(&failures)).expect("spawn test writer");
        assert_eq!(
            await_initial_retention(&spawned.initial_retention, Duration::from_secs(2)),
            InitialRetentionOutcome::Succeeded
        );
        (spawned.queue, spawned.guard, failures)
    }

    fn finish_test_worker(mut guard: DiagnosticsWorkerGuard) -> DiagnosticsFinishStatus {
        let GuardFinish { status, quiescence } = guard.finish(Duration::from_secs(2));
        assert_eq!(
            quiescence.is_some(),
            status == DiagnosticsFinishStatus::Completed,
            "a quiescence receipt is minted exactly for a completed finish"
        );
        status
    }

    #[test]
    fn event_queue_record_is_utf8_single_line_and_at_most_four_kibibytes() {
        let capture = CaptureWriter::default();
        let shared = Arc::clone(&capture.0);
        let (writer, guard, _failures) = test_worker(capture);
        let truncations = Arc::new(AtomicU64::new(0));
        {
            let mut line = EventLineWriter::new(writer, Arc::clone(&truncations));
            line.write_all("\u{1F642}".repeat(2_000).as_bytes()).expect("buffer accepts event");
        }
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);

        let bytes = shared.lock().expect("capture lock").clone();
        assert!(bytes.len() <= MAX_DIAGNOSTIC_EVENT_BYTES);
        assert!(std::str::from_utf8(&bytes).is_ok());
        assert!(bytes.ends_with(b"<truncated:event>\n"));
        assert_eq!(truncations.load(Ordering::Acquire), 1);
    }

    #[test]
    fn formatter_omits_all_upstream_values_before_disk() {
        let capture = CaptureWriter::default();
        let shared = Arc::clone(&capture.0);
        let (writer, guard, _failures) = test_worker(capture);
        let layer = tracing_subscriber::fmt::layer()
            .event_format(SafeEventFormatter)
            .with_ansi(false)
            .with_writer(EventMakeWriter { writer, truncated_lines: Arc::new(AtomicU64::new(0)) });
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                target: "third_party",
                message = "password=hunter2 C:\\Users\\private https://private.test/?token=yes\u{0000}"
            );
        });
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);

        let bytes = shared.lock().expect("capture lock").clone();
        let text = std::str::from_utf8(&bytes).expect("valid UTF-8 log");
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("Users"));
        assert!(!text.contains("private.test"));
        assert!(!text.contains('\0'));
        assert!(text.contains("target=third_party fields=<omitted>"));
    }

    #[test]
    fn formatter_never_invokes_an_upstream_debug_implementation() {
        struct PanickingDebug;

        impl fmt::Debug for PanickingDebug {
            fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("upstream Debug must not run in the diagnostics formatter")
            }
        }

        let capture = CaptureWriter::default();
        let shared = Arc::clone(&capture.0);
        let (writer, guard, _failures) = test_worker(capture);
        let layer = tracing_subscriber::fmt::layer()
            .event_format(SafeEventFormatter)
            .with_ansi(false)
            .with_writer(EventMakeWriter { writer, truncated_lines: Arc::new(AtomicU64::new(0)) });
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "third_party", dangerous = ?PanickingDebug);
        });
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);

        let bytes = shared.lock().expect("capture lock").clone();
        let text = std::str::from_utf8(&bytes).expect("valid UTF-8 log");
        assert!(text.contains("target=third_party fields=<omitted>"));
    }

    #[test]
    fn application_target_accepts_only_the_closed_event_field() {
        let capture = CaptureWriter::default();
        let shared = Arc::clone(&capture.0);
        let (writer, guard, _failures) = test_worker(capture);
        let layer = tracing_subscriber::fmt::layer()
            .event_format(SafeEventFormatter)
            .with_ansi(false)
            .with_writer(EventMakeWriter { writer, truncated_lines: Arc::new(AtomicU64::new(0)) });
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: APPLICATION_EVENT_TARGET,
                event = "code=app_started",
                ignored = "C:\\Users\\private\\must-not-appear"
            );
        });
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);

        let bytes = shared.lock().expect("capture lock").clone();
        let text = std::str::from_utf8(&bytes).expect("valid UTF-8 log");
        assert!(text.contains("event=code=app_started"));
        assert!(!text.contains("must-not-appear"));
        assert!(!text.contains("ignored="));
    }

    #[test]
    fn resilient_sink_counts_and_swallows_write_and_flush_failures() {
        let (mut writer, guard, failures) = test_worker(FailingWriter);
        assert_eq!(writer.write(b"event").expect("failure is isolated"), 5);
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);
        assert_eq!(failures.load(Ordering::Acquire), 2);
    }

    #[test]
    fn saturated_queue_drops_records_without_blocking_producers() {
        let state = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
        let sink = BlockingWriter { state: Arc::clone(&state), blocked_once: false };
        let (mut writer, guard, _failures) = test_worker(sink);
        writer.write_all(b"block-worker\n").expect("queue first record");

        {
            let (lock, changed) = &*state;
            let mut current = lock.lock().expect("blocking state lock");
            while !current.entered {
                let (next, timeout) = changed
                    .wait_timeout(current, Duration::from_secs(2))
                    .expect("blocking state wait");
                assert!(!timeout.timed_out(), "writer worker did not enter the test sink");
                current = next;
            }
        }

        let started = Instant::now();
        for _ in 0..DIAGNOSTIC_QUEUE_CAPACITY + 128 {
            writer.write_all(b"bounded-record\n").expect("lossy producer stays available");
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(writer.dropped_lines() > 0);

        {
            let (lock, changed) = &*state;
            let mut current = lock.lock().expect("blocking state lock");
            current.released = true;
            changed.notify_all();
        }
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);
    }

    #[test]
    fn saturated_queue_and_stalled_write_do_not_block_guard_drop() {
        let state = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
        let sink = BlockingWriter { state: Arc::clone(&state), blocked_once: false };
        let (mut writer, guard, _failures) = test_worker(sink);
        writer.write_all(b"block-worker\n").expect("queue blocking record");

        {
            let (lock, changed) = &*state;
            let mut current = lock.lock().expect("blocking state lock");
            while !current.entered {
                let (next, timeout) = changed
                    .wait_timeout(current, Duration::from_secs(2))
                    .expect("blocking state wait");
                assert!(!timeout.timed_out(), "writer did not enter stalled write");
                current = next;
            }
        }
        for _ in 0..DIAGNOSTIC_QUEUE_CAPACITY + 32 {
            writer.write_all(b"saturated\n").expect("producer remains nonblocking");
        }

        let started = Instant::now();
        drop(guard);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(writer.owner.upgrade().is_none(), "guard owned the only strong sender");

        let (lock, changed) = &*state;
        let mut current = lock.lock().expect("blocking state lock");
        current.released = true;
        changed.notify_all();
        while !current.dropped {
            let (next, timeout) =
                changed.wait_timeout(current, Duration::from_secs(2)).expect("blocking state wait");
            assert!(!timeout.timed_out(), "reaper did not reclaim released stalled worker");
            current = next;
        }
    }

    #[test]
    fn bounded_finish_hands_off_a_worker_stalled_in_flush() {
        let state = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
        let sink = BlockingFlushWriter { state: Arc::clone(&state) };
        let (_writer, mut guard, _failures) = test_worker(sink);
        guard.owner.take();

        {
            let (lock, changed) = &*state;
            let mut current = lock.lock().expect("blocking flush state lock");
            while !current.entered {
                let (next, timeout) = changed
                    .wait_timeout(current, Duration::from_secs(2))
                    .expect("blocking flush state wait");
                assert!(!timeout.timed_out(), "worker did not enter stalled flush");
                current = next;
            }
        }

        let started = Instant::now();
        let GuardFinish { status, quiescence } = guard.finish(Duration::from_millis(50));
        assert_eq!(status, DiagnosticsFinishStatus::HandedOff);
        assert!(quiescence.is_none(), "a handed-off worker never yields a receipt");
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(guard);

        let (lock, changed) = &*state;
        let mut current = lock.lock().expect("blocking flush state lock");
        current.released = true;
        changed.notify_all();
        while !current.dropped {
            let (next, timeout) = changed
                .wait_timeout(current, Duration::from_secs(2))
                .expect("blocking flush state wait");
            assert!(!timeout.timed_out(), "reaper did not reclaim flush worker");
            current = next;
        }
    }

    #[test]
    fn globally_retained_producer_is_weak_and_cannot_prevent_disconnect() {
        let capture = CaptureWriter::default();
        let (writer, mut guard, _failures) = test_worker(capture);
        let retained_by_subscriber = writer.clone();

        let GuardFinish { status, quiescence } = guard.finish(Duration::from_secs(2));
        assert_eq!(status, DiagnosticsFinishStatus::Completed);
        assert!(quiescence.is_some(), "a completed finish mints the quiescence receipt");
        assert!(retained_by_subscriber.owner.upgrade().is_none());
        let before = retained_by_subscriber.dropped_lines();
        retained_by_subscriber.submit(b"after-disconnect\n");
        assert_eq!(retained_by_subscriber.dropped_lines(), before + 1);
    }

    #[test]
    fn initial_retention_handshake_reports_success_issues_timeout_and_unavailable() {
        let failures = Arc::new(AtomicU64::new(0));
        let success = spawn_diagnostics_worker(CaptureWriter::default(), Arc::clone(&failures))
            .expect("spawn successful initial maintenance");
        assert_eq!(
            await_initial_retention(&success.initial_retention, Duration::from_secs(2)),
            InitialRetentionOutcome::Succeeded
        );
        assert_eq!(finish_test_worker(success.guard), DiagnosticsFinishStatus::Completed);

        let issues = Arc::new(AtomicU64::new(0));
        let attempted = Arc::new(AtomicBool::new(false));
        let issue_worker = IssueMaintenanceWriter {
            issues: Arc::clone(&issues),
            attempted: Arc::clone(&attempted),
        };
        let issue_spawned = spawn_diagnostics_worker(issue_worker, Arc::clone(&failures))
            .expect("spawn issue maintenance");
        assert_eq!(
            await_initial_retention(&issue_spawned.initial_retention, Duration::from_secs(2)),
            InitialRetentionOutcome::Issues { issue_count: 3 }
        );
        assert!(attempted.load(Ordering::Acquire), "result cannot precede the attempt");
        assert_eq!(finish_test_worker(issue_spawned.guard), DiagnosticsFinishStatus::Completed);

        let blocked_state = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
        let blocked = BlockingMaintenanceWriter { state: Arc::clone(&blocked_state) };
        let blocked_spawned = spawn_diagnostics_worker(blocked, Arc::clone(&failures))
            .expect("spawn blocked initial maintenance");
        {
            let (lock, changed) = &*blocked_state;
            let mut current = lock.lock().expect("blocked maintenance state lock");
            while !current.entered {
                let (next, timeout) = changed
                    .wait_timeout(current, Duration::from_secs(2))
                    .expect("blocked maintenance wait");
                assert!(!timeout.timed_out(), "initial maintenance did not start");
                current = next;
            }
        }
        assert_eq!(
            await_initial_retention(&blocked_spawned.initial_retention, Duration::from_millis(25)),
            InitialRetentionOutcome::TimedOut
        );
        let started = Instant::now();
        drop(blocked_spawned.guard);
        assert!(started.elapsed() < Duration::from_millis(500));
        {
            let (lock, changed) = &*blocked_state;
            let mut current = lock.lock().expect("blocked maintenance state lock");
            current.released = true;
            changed.notify_all();
            while !current.dropped {
                let (next, timeout) = changed
                    .wait_timeout(current, Duration::from_secs(2))
                    .expect("blocked maintenance wait");
                assert!(!timeout.timed_out(), "timed-out worker was not reclaimed");
                current = next;
            }
        }

        let unavailable = spawn_diagnostics_worker(PanicMaintenanceWriter, failures)
            .expect("spawn unavailable maintenance worker");
        assert_eq!(
            await_initial_retention(&unavailable.initial_retention, Duration::from_secs(2)),
            InitialRetentionOutcome::Unavailable
        );
        assert_eq!(finish_test_worker(unavailable.guard), DiagnosticsFinishStatus::WorkerFailed);
    }

    #[test]
    fn disconnected_reaper_terminalizes_before_any_later_worker_can_spawn() {
        let reaper = DiagnosticsWorkerReaper::disconnected_for_test();
        let failures = Arc::new(AtomicU64::new(0));
        let first_state = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
        let first = spawn_diagnostics_worker_with_reaper(
            &reaper,
            BlockingMaintenanceWriter { state: Arc::clone(&first_state) },
            Arc::clone(&failures),
        );
        assert_eq!(
            first.err().expect("disconnected reaper must reject registration").kind,
            DiagnosticsStartErrorKind::Worker
        );
        assert!(reaper.is_terminal());

        {
            let (lock, changed) = &*first_state;
            let mut current = lock.lock().expect("first orphan state lock");
            while !current.entered {
                let (next, timeout) = changed
                    .wait_timeout(current, Duration::from_secs(2))
                    .expect("first orphan state wait");
                assert!(!timeout.timed_out(), "first rejected worker did not start");
                current = next;
            }
        }

        let later_issues = Arc::new(AtomicU64::new(0));
        let later_attempted = Arc::new(AtomicBool::new(false));
        let later = spawn_diagnostics_worker_with_reaper(
            &reaper,
            IssueMaintenanceWriter {
                issues: Arc::clone(&later_issues),
                attempted: Arc::clone(&later_attempted),
            },
            failures,
        );
        assert_eq!(
            later.err().expect("terminal reaper must reject later startup").kind,
            DiagnosticsStartErrorKind::Worker
        );
        assert!(!later_attempted.load(Ordering::Acquire));
        assert_eq!(later_issues.load(Ordering::Acquire), 0);

        let (lock, changed) = &*first_state;
        let mut current = lock.lock().expect("first orphan state lock");
        current.released = true;
        changed.notify_all();
        while !current.dropped {
            let (next, timeout) = changed
                .wait_timeout(current, Duration::from_secs(2))
                .expect("first orphan cleanup wait");
            assert!(!timeout.timed_out(), "first rejected worker did not release resources");
            current = next;
        }
        assert_eq!(reaper.outstanding(), 0);
    }

    #[test]
    fn repeated_completed_lifecycles_leave_no_late_writer_resources() {
        const LIFECYCLES: usize = 12;
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..LIFECYCLES {
            let sink = LifecycleWriter { dropped: Arc::clone(&dropped) };
            let (_writer, guard, _failures) = test_worker(sink);
            assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);
        }
        assert_eq!(dropped.load(Ordering::Acquire), LIFECYCLES);
        assert!(
            diagnostics_worker_reaper().expect("test reaper").outstanding()
                <= WORKER_REAPER_CAPACITY
        );
    }

    #[test]
    fn idle_worker_wakes_for_periodic_native_maintenance() {
        let state = Arc::new((Mutex::new(0_u64), Condvar::new()));
        let sink = IdleMaintenanceWriter { state: Arc::clone(&state), next: Instant::now() };
        let (_writer, guard, _failures) = test_worker(sink);
        let (lock, changed) = &*state;
        let mut count = lock.lock().expect("maintenance counter lock");
        while *count < 2 {
            let (next, timeout) = changed
                .wait_timeout(count, Duration::from_secs(2))
                .expect("maintenance counter wait");
            assert!(!timeout.timed_out(), "idle diagnostics worker did not wake for maintenance");
            count = next;
        }
        drop(count);
        assert_eq!(finish_test_worker(guard), DiagnosticsFinishStatus::Completed);
    }

    #[test]
    fn bounded_raw_fields_preserve_utf8_and_report_truncation_once() {
        let mut raw = BoundedText::new();
        write!(&mut raw, "{}", "\u{6771}\u{4EAC}".repeat(2_000)).expect("bounded formatting");
        let finished = raw.finish();
        assert!(finished.len() <= MAX_RAW_EVENT_BYTES);
        assert!(finished.ends_with(RAW_TRUNCATION_MARKER));
        assert!(std::str::from_utf8(finished.as_bytes()).is_ok());
    }
}
