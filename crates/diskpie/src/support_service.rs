//! Bounded root-owned support work. No egui, COM objects, or filesystem work
//! crosses into callbacks. The precreated reaper owns every join handle from
//! startup; dropping either client or worker requests stop without waiting.

#![cfg(windows)]
#![forbid(unsafe_code)]

use std::{
    fmt, io,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use diskpie_app::{
    actions::{ActionPurpose, FilesystemTarget, TargetRejection, TargetValidator},
    diagnostics::{
        AggregateMetrics, AllowlistedSettings, DiagnosticCounters, DiagnosticExport,
        ExportArtifact, ExportMetadata, ExportRequest, LogSource, build_export,
    },
    explorer_integration::{
        self, ExplorerIntegrationStatus, IntegrationError, IntegrationRegistry, IntegrationReport,
        IntegrationRequest, StagingToken,
    },
};
use diskpie_core::{GenerationId, NodeId, TreeSnapshot};
use diskpie_platform::windows::{
    diagnostic_files::{
        DiagnosticFileError, ExportDestinationKind, ExportWriteOutcome, PreparedDiagnosticExport,
        collect_diagnostic_logs, prepare_diagnostic_export,
    },
    explorer_integration::WindowsIntegrationRegistry,
    volumes::{WindowsDriveType, discover_volumes, resolve_scan_root},
};

/// Caps queued AND undrained jobs, so results can never block shutdown.
pub const SUPPORT_JOB_CAPACITY: usize = 4;
const WORKER_POLL: Duration = Duration::from_millis(20);
const JOIN_PENDING: u8 = 0;
const JOIN_SUCCEEDED: u8 = 1;
const JOIN_FAILED: u8 = 2;
static WORKER_LIVE: AtomicBool = AtomicBool::new(false);
static REAPER: OnceLock<Result<SyncSender<ReapJob>, io::ErrorKind>> = OnceLock::new();
static QUARANTINED_WORKER: OnceLock<ReapJob> = OnceLock::new();

/// Exact private paths supplied once by the composition root. They are never
/// derived from UI strings and intentionally excluded from Debug output.
#[derive(Default)]
pub struct SupportConfig {
    pub executable: Option<PathBuf>,
    pub logs_directory: Option<PathBuf>,
    pub owned_files: Vec<PathBuf>,
}

impl fmt::Debug for SupportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupportConfig")
            .field("executable_known", &self.executable.is_some())
            .field("logs_available", &self.logs_directory.is_some())
            .field("owned_files", &self.owned_files.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SupportJobId(u64);

impl SupportJobId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExportToken(u64);

/// Each work item is consumed once. Prepared artifacts remain immutable.
pub enum SupportJob {
    PrepareTarget {
        snapshot: Arc<TreeSnapshot>,
        generation: GenerationId,
        node: NodeId,
        hidden: bool,
        purpose: ActionPurpose,
    },
    InspectIntegration,
    ApplyIntegration(IntegrationRequest),
    BuildExport {
        metadata: ExportMetadata,
        settings: AllowlistedSettings,
        aggregates: AggregateMetrics,
        counters: DiagnosticCounters,
        include_paths: bool,
    },
    PrepareExport {
        destination: PathBuf,
        artifact: ExportArtifact,
    },
    CommitExport {
        token: ExportToken,
    },
    DiscardExport {
        token: ExportToken,
    },
}

impl fmt::Debug for SupportJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::PrepareTarget { .. } => "PrepareTarget",
            Self::InspectIntegration => "InspectIntegration",
            Self::ApplyIntegration(_) => "ApplyIntegration",
            Self::BuildExport { .. } => "BuildExport",
            Self::PrepareExport { .. } => "PrepareExport",
            Self::CommitExport { .. } => "CommitExport",
            Self::DiscardExport { .. } => "DiscardExport",
        };
        f.write_str(name)
    }
}

#[derive(Debug)]
pub enum SupportError {
    Target(TargetRejection),
    Integration(IntegrationError),
    Diagnostic(DiagnosticFileError),
    ExecutableUnavailable,
    InvalidExportToken,
    ExportAlreadyPrepared,
    WorkerPanicked,
}

impl fmt::Display for SupportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(error) => write!(f, "{error}"),
            Self::Integration(error) => write!(f, "{error}"),
            Self::Diagnostic(error) => write!(f, "{error}"),
            Self::ExecutableUnavailable => f.write_str("application executable unavailable"),
            Self::InvalidExportToken => f.write_str("export preparation is no longer valid"),
            Self::ExportAlreadyPrepared => f.write_str("another export destination is prepared"),
            Self::WorkerPanicked => f.write_str("support worker failed"),
        }
    }
}

pub enum SupportOutcome {
    Target {
        target: FilesystemTarget,
        recycle_supported: bool,
    },
    Integration {
        /// A fresh inspection is attempted even after an unsuccessful mutation.
        status: Result<ExplorerIntegrationStatus, IntegrationError>,
        /// Retains successful, failed, and partially applied operation evidence
        /// independently of whether the following inspection succeeds.
        report: Option<Result<IntegrationReport, IntegrationError>>,
    },
    Export(DiagnosticExport),
    ExportPrepared {
        token: ExportToken,
        destination: PathBuf,
        kind: ExportDestinationKind,
        replaces_existing: bool,
    },
    ExportWritten(ExportWriteOutcome),
    Discarded,
    Failed(SupportError),
    Abandoned,
}

impl fmt::Debug for SupportOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target { target, recycle_supported } => f
                .debug_struct("Target")
                .field("target", target)
                .field("recycle_supported", recycle_supported)
                .finish(),
            Self::Integration { status, report } => f
                .debug_struct("Integration")
                .field("status", status)
                .field("report", report)
                .finish(),
            Self::Export(export) => f.debug_tuple("Export").field(export).finish(),
            Self::ExportPrepared { token, kind, replaces_existing, .. } => f
                .debug_struct("ExportPrepared")
                .field("token", token)
                .field("kind", kind)
                .field("replaces_existing", replaces_existing)
                .finish(),
            Self::ExportWritten(outcome) => f.debug_tuple("ExportWritten").field(outcome).finish(),
            Self::Discarded => f.write_str("Discarded"),
            Self::Failed(error) => f.debug_tuple("Failed").field(error).finish(),
            Self::Abandoned => f.write_str("Abandoned"),
        }
    }
}

#[derive(Debug)]
pub struct SupportResult {
    pub id: SupportJobId,
    pub outcome: SupportOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitSupportError {
    QueueFull,
    WorkerStopped,
}

struct QueuedJob {
    id: SupportJobId,
    job: SupportJob,
}

pub struct SupportClient {
    jobs: SyncSender<QueuedJob>,
    results: Receiver<SupportResult>,
    stop: Arc<AtomicBool>,
    status: Arc<AtomicU8>,
    next_id: u64,
    outstanding: usize,
}

impl SupportClient {
    pub fn submit(&mut self, job: SupportJob) -> Result<SupportJobId, SubmitSupportError> {
        if self.stop.load(Ordering::Acquire) || self.status.load(Ordering::Acquire) != JOIN_PENDING
        {
            return Err(SubmitSupportError::WorkerStopped);
        }
        if self.outstanding >= SUPPORT_JOB_CAPACITY {
            return Err(SubmitSupportError::QueueFull);
        }
        let id = SupportJobId(self.next_id);
        match self.jobs.try_send(QueuedJob { id, job }) {
            Ok(()) => {
                self.next_id = self.next_id.checked_add(1).unwrap_or_else(|| {
                    self.stop.store(true, Ordering::Release);
                    u64::MAX
                });
                self.outstanding += 1;
                Ok(id)
            }
            Err(TrySendError::Full(_)) => Err(SubmitSupportError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(SubmitSupportError::WorkerStopped),
        }
    }

    pub fn try_recv(&mut self) -> Option<SupportResult> {
        match self.results.try_recv() {
            Ok(result) => {
                self.outstanding = self.outstanding.saturating_sub(1);
                Some(result)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    #[must_use]
    pub const fn outstanding(&self) -> usize {
        self.outstanding
    }

    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.status.load(Ordering::Acquire) != JOIN_PENDING
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for SupportClient {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportFinish {
    Joined,
    HandedOff,
    WorkerFailed,
}

/// The precreated process reaper owns the JoinHandle; this is the root's
/// completion observer. Joined means the actual join, not just an idle queue.
pub struct SupportWorker {
    stop: Arc<AtomicBool>,
    status: Arc<AtomicU8>,
}

impl SupportWorker {
    pub fn finish(self, timeout: Duration) -> SupportFinish {
        self.stop.store(true, Ordering::Release);
        let start = Instant::now();
        loop {
            match self.status.load(Ordering::Acquire) {
                JOIN_SUCCEEDED => return SupportFinish::Joined,
                JOIN_FAILED => return SupportFinish::WorkerFailed,
                _ if start.elapsed() >= timeout => return SupportFinish::HandedOff,
                _ => thread::sleep(Duration::from_millis(2)),
            }
        }
    }
}

impl Drop for SupportWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct ReapJob {
    handle: JoinHandle<()>,
    status: Arc<AtomicU8>,
}

/// A failed reaper handoff retains its one handle for the process lifetime.
/// Such a startup failure must never count as a clean shutdown receipt.
pub fn has_quarantined_worker() -> bool {
    QUARANTINED_WORKER.get().is_some()
}

fn reaper() -> io::Result<SyncSender<ReapJob>> {
    match REAPER.get_or_init(|| {
        let (tx, rx) = sync_channel::<ReapJob>(1);
        thread::Builder::new()
            .name("diskpie-support-reaper".to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let succeeded = job.handle.join().is_ok();
                    WORKER_LIVE.store(false, Ordering::Release);
                    job.status.store(
                        if succeeded { JOIN_SUCCEEDED } else { JOIN_FAILED },
                        Ordering::Release,
                    );
                }
            })
            .map(|_| tx)
            .map_err(|error| error.kind())
    }) {
        Ok(sender) => Ok(sender.clone()),
        Err(kind) => Err(io::Error::new(*kind, "support reaper unavailable")),
    }
}

pub fn start(config: SupportConfig) -> io::Result<(SupportClient, SupportWorker)> {
    start_runner(move |job, prepared| run_job(&config, job, prepared))
}

struct PreparedExport {
    token: ExportToken,
    destination: PreparedDiagnosticExport,
    artifact: ExportArtifact,
}

fn start_runner(
    mut run: impl FnMut(SupportJob, &mut Option<PreparedExport>) -> SupportOutcome + Send + 'static,
) -> io::Result<(SupportClient, SupportWorker)> {
    let reaper = reaper()?;
    if WORKER_LIVE.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "support worker already active"));
    }
    let (jobs, job_rx) = sync_channel::<QueuedJob>(SUPPORT_JOB_CAPACITY);
    let (result_tx, results) = sync_channel::<SupportResult>(SUPPORT_JOB_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(AtomicU8::new(JOIN_PENDING));
    let worker_stop = Arc::clone(&stop);
    let handle = match thread::Builder::new().name("diskpie-support".to_owned()).spawn(move || {
        let mut prepared = None;
        loop {
            let queued = match job_rx.recv_timeout(WORKER_POLL) {
                Ok(queued) => queued,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if !worker_stop.load(Ordering::Acquire) =>
                {
                    continue;
                }
                Err(_) => break,
            };
            let outcome = if worker_stop.load(Ordering::Acquire) {
                SupportOutcome::Abandoned
            } else {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(queued.job, &mut prepared)
                })) {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        worker_stop.store(true, Ordering::Release);
                        prepared = None;
                        SupportOutcome::Failed(SupportError::WorkerPanicked)
                    }
                }
            };
            // Admission counts undrained results, so this cannot fill while
            // the client remains alive. Never wait for a stopped renderer.
            if result_tx.try_send(SupportResult { id: queued.id, outcome }).is_err() {
                break;
            }
        }
    }) {
        Ok(handle) => handle,
        Err(error) => {
            WORKER_LIVE.store(false, Ordering::Release);
            return Err(error);
        }
    };
    if let Err(error) = reaper.try_send(ReapJob { handle, status: Arc::clone(&status) }) {
        stop.store(true, Ordering::Release);
        // Fail closed in a bounded, inspectable process-lifetime owner. The
        // singleton claim remains held, so no second worker can reach this
        // quarantine and no future start may multiply orphaned workers.
        let job = match error {
            TrySendError::Full(job) | TrySendError::Disconnected(job) => job,
        };
        assert!(QUARANTINED_WORKER.set(job).is_ok(), "support quarantine already occupied");
        return Err(io::Error::other("support reaper rejected worker ownership"));
    }
    Ok((
        SupportClient {
            jobs,
            results,
            stop: Arc::clone(&stop),
            status: Arc::clone(&status),
            next_id: 1,
            outstanding: 0,
        },
        SupportWorker { stop, status },
    ))
}

fn run_job(
    config: &SupportConfig,
    job: SupportJob,
    prepared: &mut Option<PreparedExport>,
) -> SupportOutcome {
    match try_run_job(config, job, prepared) {
        Ok(outcome) => outcome,
        Err(error) => SupportOutcome::Failed(error),
    }
}

fn try_run_job(
    config: &SupportConfig,
    job: SupportJob,
    prepared: &mut Option<PreparedExport>,
) -> Result<SupportOutcome, SupportError> {
    match job {
        SupportJob::PrepareTarget { snapshot, generation, node, hidden, purpose } => {
            let target = TargetValidator::new(&snapshot, config.executable.as_deref())
                .validate(node, generation, hidden, purpose)
                .map_err(SupportError::Target)?;
            let recycle_supported = purpose == ActionPurpose::Destructive
                && resolve_scan_root(target.parent_path().unwrap_or(target.path()))
                    .ok()
                    .filter(|resolved| resolved.drive_type == WindowsDriveType::Fixed)
                    .is_some_and(|resolved| {
                        discover_volumes().volumes.iter().any(|volume| {
                            volume.volume_key == resolved.scan_root.volume
                                && volume.drive_type == WindowsDriveType::Fixed
                                && volume
                                    .filesystem
                                    .as_deref()
                                    .and_then(std::ffi::OsStr::to_str)
                                    .is_some_and(|filesystem| {
                                        filesystem.eq_ignore_ascii_case("NTFS")
                                    })
                        })
                    });
            Ok(SupportOutcome::Target { target, recycle_supported })
        }
        SupportJob::InspectIntegration | SupportJob::ApplyIntegration(_) => {
            let executable =
                config.executable.as_deref().ok_or(SupportError::ExecutableUnavailable)?;
            let mut registry =
                WindowsIntegrationRegistry::open_current_user_classes().map_err(|error| {
                    SupportError::Integration(IntegrationError::Registry {
                        error,
                        rollback: explorer_integration::RollbackReport::NotNeeded,
                    })
                })?;
            let request = match job {
                SupportJob::ApplyIntegration(request) => Some(request),
                _ => None,
            };
            Ok(run_integration_job(&mut registry, executable, request, &StagingToken::generate()))
        }
        SupportJob::BuildExport { metadata, settings, aggregates, counters, include_paths } => {
            let snapshot =
                config.logs_directory.as_deref().map(collect_diagnostic_logs).transpose();
            let (storage, notice) = match snapshot {
                Ok(Some(snapshot)) => (
                    snapshot.sources,
                    format!(
                        "Collection omitted {} older source files and {} prefix bytes; retained newest complete records.\n",
                        snapshot.omitted_files, snapshot.omitted_bytes
                    ),
                ),
                Ok(None) | Err(_) => (
                    vec![None],
                    "Diagnostic log storage was unavailable during collection.\n".to_owned(),
                ),
            };
            let mut sources: Vec<_> = storage
                .iter()
                .map(|source| match source {
                    Some(bytes) => LogSource::available(bytes),
                    None => LogSource::unavailable(),
                })
                .collect();
            sources.push(LogSource::available(notice.as_bytes()));
            Ok(SupportOutcome::Export(build_export(&ExportRequest {
                metadata,
                settings,
                aggregates,
                counters,
                log_sources: &sources,
                include_paths,
            })))
        }
        SupportJob::PrepareExport { destination, artifact } => {
            if prepared.is_some() {
                return Err(SupportError::ExportAlreadyPrepared);
            }
            let owned: Vec<_> = config.owned_files.iter().map(PathBuf::as_path).collect();
            let mut capability = prepare_diagnostic_export(&destination, &owned)
                .map_err(SupportError::Diagnostic)?;
            if let Some(logs) = &config.logs_directory {
                capability.protect_log_directory(logs).map_err(SupportError::Diagnostic)?;
            }
            static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let token = ExportToken(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
            let result = SupportOutcome::ExportPrepared {
                token,
                destination,
                kind: capability.destination_kind(),
                replaces_existing: capability.replaces_existing(),
            };
            *prepared = Some(PreparedExport { token, destination: capability, artifact });
            Ok(result)
        }
        SupportJob::CommitExport { token } => {
            if prepared.as_ref().is_none_or(|current| current.token != token) {
                return Err(SupportError::InvalidExportToken);
            }
            let current = prepared.take().expect("token was checked");
            current
                .destination
                .commit(current.artifact.bytes())
                .map(SupportOutcome::ExportWritten)
                .map_err(SupportError::Diagnostic)
        }
        SupportJob::DiscardExport { token } => {
            if prepared.as_ref().is_none_or(|current| current.token != token) {
                return Err(SupportError::InvalidExportToken);
            }
            *prepared = None;
            Ok(SupportOutcome::Discarded)
        }
    }
}

fn run_integration_job(
    registry: &mut dyn IntegrationRegistry,
    executable: &std::path::Path,
    request: Option<IntegrationRequest>,
    token: &StagingToken,
) -> SupportOutcome {
    let report =
        request.map(|request| explorer_integration::apply(registry, request, executable, token));
    let status = explorer_integration::inspect(registry, executable);
    SupportOutcome::Integration { status, report }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_refreshes_after_partial_failure_and_retains_both_outcomes() {
        use explorer_integration::{
            FakeRegistry, KeyState, RegistryErrorKind, RegistryOperation, VerbTarget,
        };
        let executable = std::path::Path::new(r"C:\DiskPie\diskpie.exe");
        let token = StagingToken::new(42, 1);
        let mut registry = FakeRegistry::new();
        explorer_integration::apply(&mut registry, IntegrationRequest::Install, executable, &token)
            .unwrap();
        registry.clear_operations();
        let mut probe = registry.clone();
        explorer_integration::apply(&mut probe, IntegrationRequest::Remove, executable, &token)
            .unwrap();
        let second_delete = probe
            .operations()
            .iter()
            .enumerate()
            .filter(|(_, operation)| **operation == RegistryOperation::Delete)
            .nth(1)
            .unwrap()
            .0;
        registry.fail_operation(second_delete, RegistryErrorKind::AccessDenied);
        let SupportOutcome::Integration { status: Ok(status), report: Some(Err(error)) } =
            run_integration_job(
                &mut registry,
                executable,
                Some(IntegrationRequest::Remove),
                &token,
            )
        else {
            panic!("operation failure must retain a fresh inspection");
        };
        assert!(error.may_have_mutated());
        assert_eq!(status.key_state(VerbTarget::Directory), &KeyState::NotInstalled);
        assert_eq!(status.key_state(VerbTarget::Drive), &KeyState::OwnedCurrent);

        let mut registry = FakeRegistry::new();
        let mut probe = registry.clone();
        explorer_integration::apply(&mut probe, IntegrationRequest::Install, executable, &token)
            .unwrap();
        registry.fail_operation(probe.operations().len(), RegistryErrorKind::AccessDenied);
        assert!(matches!(
            run_integration_job(
                &mut registry,
                executable,
                Some(IntegrationRequest::Install),
                &token
            ),
            SupportOutcome::Integration {
                status: Err(_),
                report: Some(Ok(IntegrationReport {
                    outcome: explorer_integration::IntegrationOutcome::Installed,
                    ..
                }))
            }
        ));
    }

    #[test]
    fn support_worker_bounds_results_and_joins_after_client_drop() {
        let gate = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::clone(&gate);
        let (mut client, worker) = start_runner(move |_, _| {
            while !worker_gate.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            SupportOutcome::Discarded
        })
        .expect("start bounded support worker");
        for _ in 0..SUPPORT_JOB_CAPACITY {
            client.submit(SupportJob::InspectIntegration).unwrap();
        }
        assert_eq!(
            client.submit(SupportJob::InspectIntegration),
            Err(SubmitSupportError::QueueFull)
        );
        assert_eq!(client.outstanding(), SUPPORT_JOB_CAPACITY);
        gate.store(true, Ordering::Release);
        drop(client);
        assert_eq!(worker.finish(Duration::from_secs(2)), SupportFinish::Joined);

        let gate = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::clone(&gate);
        let worker_entered = Arc::clone(&entered);
        let (mut client, worker) = start_runner(move |_, _| {
            worker_entered.store(true, Ordering::Release);
            while !worker_gate.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            SupportOutcome::Discarded
        })
        .expect("restart after actual join");
        client.submit(SupportJob::InspectIntegration).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !entered.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let status = Arc::clone(&worker.status);
        assert_eq!(worker.finish(Duration::ZERO), SupportFinish::HandedOff);
        assert!(
            matches!(start_runner(|_, _| SupportOutcome::Discarded), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(client);
        gate.store(true, Ordering::Release);
        while status.load(Ordering::Acquire) == JOIN_PENDING {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(status.load(Ordering::Acquire), JOIN_SUCCEEDED);
    }

    #[test]
    fn support_export_tokens_are_single_use_and_save_exact_preview_bytes() {
        use diskpie_app::diagnostics::{
            AppVersion, Architecture, CoarseOsVersion, OperatingSystem, Target,
        };
        let config = SupportConfig::default();
        let metadata = ExportMetadata {
            app_version: AppVersion::new(0, 1, 0),
            target: Target::new(OperatingSystem::Windows, Architecture::X86_64),
            coarse_os_version: CoarseOsVersion::new(10, 0, None),
            panic_marker_present: false,
        };
        let mut prepared = None;
        let SupportOutcome::Export(export) = run_job(
            &config,
            SupportJob::BuildExport {
                metadata,
                settings: AllowlistedSettings::default(),
                aggregates: AggregateMetrics::default(),
                counters: DiagnosticCounters::default(),
                include_paths: false,
            },
            &mut prepared,
        ) else {
            panic!("prepare redacted preview");
        };
        let directory =
            std::env::temp_dir().join(format!("diskpie-support-export-{}", std::process::id()));
        std::fs::create_dir(&directory).expect("create unique fixture");
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let destination = directory.join("diagnostics.txt");
        let SupportOutcome::ExportPrepared { token, replaces_existing, .. } = run_job(
            &config,
            SupportJob::PrepareExport {
                destination: destination.clone(),
                artifact: export.artifact().clone(),
            },
            &mut prepared,
        ) else {
            panic!("prepare destination");
        };
        assert!(!replaces_existing);
        assert!(!destination.exists());
        assert!(matches!(
            run_job(
                &config,
                SupportJob::CommitExport { token: ExportToken(u64::MAX) },
                &mut prepared
            ),
            SupportOutcome::Failed(SupportError::InvalidExportToken)
        ));
        assert!(matches!(
            run_job(&config, SupportJob::CommitExport { token }, &mut prepared),
            SupportOutcome::ExportWritten(_)
        ));
        assert_eq!(std::fs::read(&destination).unwrap(), export.preview().bytes());
        assert!(matches!(
            run_job(&config, SupportJob::CommitExport { token }, &mut prepared),
            SupportOutcome::Failed(SupportError::InvalidExportToken)
        ));
    }

    #[test]
    fn support_debug_omits_native_paths() {
        let config = SupportConfig {
            executable: Some(PathBuf::from(r"C:\private\secret.exe")),
            logs_directory: None,
            owned_files: vec![],
        };
        assert!(!format!("{config:?}").contains("secret"));
    }
}
