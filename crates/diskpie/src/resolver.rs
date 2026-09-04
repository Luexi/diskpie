//! Root-owned resolver worker: path canonicalization, volume resolution, volume
//! discovery, and filesystem-provider construction off the frame thread.
//!
//! The egui callback only submits bounded jobs and polls bounded results. One
//! worker thread performs the blocking Win32 queries. The composition root
//! owns the join handle and joins it with a deadline after the event loop has
//! returned; the shell never joins.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use diskpie_app::format::format_path_for_display;
use diskpie_scan::{CancelToken, ScanFs, ScanRoot, StorageClass};

use crate::scan_ui::FailureClass;

/// Bounded number of queued, not yet started, resolver jobs.
pub const JOB_QUEUE_CAPACITY: usize = 4;
/// Bounded number of finished results the frame loop has not drained yet.
pub const RESULT_QUEUE_CAPACITY: usize = 8;

/// Correlates a submitted job with exactly one result.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResolveJobId(u64);

/// Work accepted by the resolver worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveJob {
    /// Resolve every native path into a scan root and build one provider.
    Paths(Vec<PathBuf>),
    /// Enumerate the volumes the current user can see.
    Volumes,
}

/// One validated scan root plus the non-fatal issues met while probing it.
#[derive(Clone, Debug)]
pub struct ResolvedRoot {
    pub root: ScanRoot,
    pub issues: Vec<String>,
}

/// A launch that the runtime can accept without further I/O.
pub struct ResolvedLaunch {
    pub fs: Arc<dyn ScanFs>,
    pub roots: Vec<ScanRoot>,
    pub cancel: CancelToken,
    /// Display strings for the status surfaces; never used as action paths.
    pub displays: Vec<String>,
    pub issues: Vec<String>,
}

impl std::fmt::Debug for ResolvedLaunch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedLaunch")
            .field("roots", &self.roots.len())
            .field("issues", &self.issues.len())
            .finish_non_exhaustive()
    }
}

/// A volume the hero surface can offer without a native dialog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeEntry {
    /// Exact native mount path used when the user chooses this volume.
    pub path: PathBuf,
    /// Lossy display of `path`.
    pub display: String,
    pub label: Option<String>,
    pub filesystem: Option<String>,
    pub storage_class: StorageClass,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Non-fatal discovery issues, already rendered as text for the UI.
    pub issues: Vec<String>,
}

/// Result of one volume enumeration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VolumeList {
    pub volumes: Vec<VolumeEntry>,
    /// Enumeration-level errors rendered as text for the UI.
    pub errors: Vec<String>,
}

/// Typed resolution failure plus a user-presentable detail line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveFailure {
    pub class: FailureClass,
    pub detail: String,
}

/// Terminal outcome of one job.
#[derive(Debug)]
pub enum ResolveOutcome {
    Launch(ResolvedLaunch),
    Volumes(VolumeList),
    Failed(ResolveFailure),
    /// The worker was asked to stop before starting the job.
    Abandoned,
}

/// One drained result.
#[derive(Debug)]
pub struct ResolveResult {
    pub id: ResolveJobId,
    pub outcome: ResolveOutcome,
}

/// Platform seam executed on the worker thread only.
pub trait ResolveBackend: Send + 'static {
    /// Resolves an absolute, dot-free directory path into a scan root.
    fn resolve_root(&self, path: &Path) -> Result<ResolvedRoot, ResolveFailure>;
    /// Enumerates volumes; per-volume issues never abort the listing.
    fn discover_volumes(&self) -> VolumeList;
    /// Builds the filesystem provider that shares `cancel` with the runtime.
    fn filesystem(&self, cancel: CancelToken) -> Arc<dyn ScanFs>;
}

/// Why a job could not be queued without waiting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitJobError {
    QueueFull,
    WorkerStopped,
}

struct QueuedJob {
    id: ResolveJobId,
    job: ResolveJob,
}

/// Frame-side handle: bounded submit and bounded drain, never a join.
pub struct ResolverClient {
    jobs: SyncSender<QueuedJob>,
    results: Receiver<ResolveResult>,
    stop: Arc<AtomicBool>,
    next_id: u64,
    outstanding: usize,
}

impl std::fmt::Debug for ResolverClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolverClient")
            .field("outstanding", &self.outstanding)
            .field("stop", &self.stop.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl ResolverClient {
    /// Queues a job without waiting for capacity.
    pub fn submit(&mut self, job: ResolveJob) -> Result<ResolveJobId, SubmitJobError> {
        if self.stop.load(Ordering::Acquire) {
            return Err(SubmitJobError::WorkerStopped);
        }
        let id = ResolveJobId(self.next_id);
        match self.jobs.try_send(QueuedJob { id, job }) {
            Ok(()) => {
                self.next_id = self.next_id.wrapping_add(1);
                self.outstanding = self.outstanding.saturating_add(1);
                Ok(id)
            }
            Err(TrySendError::Full(_)) => Err(SubmitJobError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(SubmitJobError::WorkerStopped),
        }
    }

    /// Receives at most one finished result without blocking.
    pub fn try_recv(&mut self) -> Option<ResolveResult> {
        match self.results.try_recv() {
            Ok(result) => {
                self.outstanding = self.outstanding.saturating_sub(1);
                Some(result)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Jobs submitted whose results have not been drained yet.
    #[cfg(test)]
    #[must_use]
    pub const fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Asks the worker to abandon queued jobs and exit. Atomic only.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Root-side handle owning the worker thread.
pub struct ResolverWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ResolverWorker {
    /// Requests stop and waits at most `timeout` for the thread to exit.
    ///
    /// Returns whether the worker was actually joined. A worker still inside a
    /// blocking Win32 volume query after the deadline is left to process exit;
    /// it holds no application state that must be flushed.
    pub fn finish(mut self, timeout: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        handle.join().is_ok()
    }
}

/// Starts the resolver worker with `backend`.
pub fn start(backend: impl ResolveBackend) -> std::io::Result<(ResolverClient, ResolverWorker)> {
    let (jobs, job_rx) = sync_channel::<QueuedJob>(JOB_QUEUE_CAPACITY);
    let (result_tx, results) = sync_channel::<ResolveResult>(RESULT_QUEUE_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let handle = thread::Builder::new()
        .name("diskpie-resolver".to_owned())
        .spawn(move || worker_loop(&backend, &job_rx, &result_tx, &worker_stop))?;
    Ok((
        ResolverClient { jobs, results, stop: Arc::clone(&stop), next_id: 1, outstanding: 0 },
        ResolverWorker { stop, handle: Some(handle) },
    ))
}

fn worker_loop(
    backend: &dyn ResolveBackend,
    jobs: &Receiver<QueuedJob>,
    results: &SyncSender<ResolveResult>,
    stop: &AtomicBool,
) {
    while let Ok(QueuedJob { id, job }) = jobs.recv() {
        let outcome = if stop.load(Ordering::Acquire) {
            ResolveOutcome::Abandoned
        } else {
            run_job(backend, job, stop)
        };
        if results.send(ResolveResult { id, outcome }).is_err() {
            return;
        }
    }
}

fn run_job(backend: &dyn ResolveBackend, job: ResolveJob, stop: &AtomicBool) -> ResolveOutcome {
    match job {
        ResolveJob::Volumes => ResolveOutcome::Volumes(backend.discover_volumes()),
        ResolveJob::Paths(paths) => {
            if paths.is_empty() {
                return ResolveOutcome::Failed(ResolveFailure {
                    class: FailureClass::PathUnavailable,
                    detail: "no path was provided".to_owned(),
                });
            }
            let mut roots = Vec::with_capacity(paths.len());
            let mut displays = Vec::with_capacity(paths.len());
            let mut issues = Vec::new();
            for path in &paths {
                if stop.load(Ordering::Acquire) {
                    return ResolveOutcome::Abandoned;
                }
                let normalized = normalize_for_resolution(path);
                match backend.resolve_root(&normalized) {
                    Ok(resolved) => {
                        displays.push(format_path_for_display(&resolved.root.path));
                        issues.extend(resolved.issues);
                        roots.push(resolved.root);
                    }
                    Err(failure) => return ResolveOutcome::Failed(failure),
                }
            }
            let cancel = CancelToken::new();
            let fs = backend.filesystem(cancel.clone());
            ResolveOutcome::Launch(ResolvedLaunch { fs, roots, cancel, displays, issues })
        }
    }
}

/// Produces the absolute, dot-free spelling the volume adapter requires.
///
/// The adapter deliberately performs no normalization because `\\?\` paths
/// bypass Win32 canonicalization; a relative, `.`, or `..` component reaching
/// it would either be rejected or become a literal name.
#[must_use]
pub fn normalize_for_resolution(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_error| path.to_path_buf());
    strip_dot_components(&absolute)
}

/// Removes `.` components and resolves `..` lexically against preceding
/// normal components. Prefix and root components are never popped.
#[must_use]
pub fn strip_dot_components(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut normal_depth = 0_usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => result.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normal_depth > 0 {
                    result.pop();
                    normal_depth -= 1;
                }
            }
            Component::Normal(name) => {
                result.push(name);
                normal_depth += 1;
            }
        }
    }
    if result.as_os_str().is_empty() { PathBuf::from(OsString::from(".")) } else { result }
}

/// Backend used where no native volume adapter exists.
#[cfg(any(not(windows), test))]
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedBackend;

#[cfg(any(not(windows), test))]
impl ResolveBackend for UnsupportedBackend {
    fn resolve_root(&self, _path: &Path) -> Result<ResolvedRoot, ResolveFailure> {
        Err(ResolveFailure {
            class: FailureClass::Other,
            detail: "volume resolution is not available on this platform".to_owned(),
        })
    }

    fn discover_volumes(&self) -> VolumeList {
        VolumeList {
            volumes: Vec::new(),
            errors: vec!["volume discovery is not available on this platform".to_owned()],
        }
    }

    fn filesystem(&self, _cancel: CancelToken) -> Arc<dyn ScanFs> {
        Arc::new(NoFilesystem)
    }
}

/// Provider that reports every directory as unavailable; only used when the
/// backend cannot resolve anything, so the runtime never actually receives it.
#[cfg(any(not(windows), test))]
struct NoFilesystem;

#[cfg(any(not(windows), test))]
impl ScanFs for NoFilesystem {
    fn visit_directory(
        &self,
        path: &Path,
        _visit: &mut dyn FnMut(diskpie_scan::DirectoryItem) -> diskpie_scan::VisitControl,
    ) -> Result<(), diskpie_scan::FsError> {
        Err(diskpie_scan::FsError::new(
            path,
            diskpie_scan::FsOperation::EnumerateDirectory,
            diskpie_scan::FsErrorKind::NotSupported,
        ))
    }
}

#[cfg(windows)]
pub use windows_backend::WindowsBackend;

#[cfg(windows)]
mod windows_backend {
    use super::{ResolveBackend, ResolveFailure, ResolvedRoot, VolumeEntry, VolumeList};
    use crate::scan_ui::FailureClass;
    use diskpie_app::format::format_path_for_display;
    use diskpie_platform::{
        VolumeError, WindowsFileSystem, WindowsVolume, discover_volumes, resolve_scan_root,
        windows::volumes::VolumeErrorKind,
    };
    use diskpie_scan::{CancelToken, ScanFs};
    use std::{path::Path, sync::Arc};

    /// Backend over the reviewed Windows volume and filesystem adapters.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct WindowsBackend;

    impl ResolveBackend for WindowsBackend {
        fn resolve_root(&self, path: &Path) -> Result<ResolvedRoot, ResolveFailure> {
            match resolve_scan_root(path) {
                Ok(resolved) => Ok(ResolvedRoot {
                    root: resolved.scan_root,
                    issues: resolved.issues.iter().map(issue_text).collect(),
                }),
                Err(error) => Err(ResolveFailure {
                    class: failure_class(error.kind),
                    detail: issue_text(&error),
                }),
            }
        }

        fn discover_volumes(&self) -> VolumeList {
            let discovery = discover_volumes();
            VolumeList {
                volumes: discovery.volumes.iter().filter_map(volume_entry).collect(),
                errors: discovery.errors.iter().map(issue_text).collect(),
            }
        }

        fn filesystem(&self, cancel: CancelToken) -> Arc<dyn ScanFs> {
            Arc::new(WindowsFileSystem::with_cancel_token(cancel))
        }
    }

    fn volume_entry(volume: &WindowsVolume) -> Option<VolumeEntry> {
        let path = volume.mount_points.first()?.clone();
        Some(VolumeEntry {
            display: format_path_for_display(&path),
            path,
            label: volume.label.as_ref().map(|label| label.to_string_lossy().into_owned()),
            filesystem: volume
                .filesystem
                .as_ref()
                .map(|filesystem| filesystem.to_string_lossy().into_owned()),
            storage_class: volume.storage_class,
            total_bytes: volume.space.map(|space| space.total_bytes),
            free_bytes: volume.space.map(|space| space.total_free_bytes),
            issues: volume.issues.iter().map(issue_text).collect(),
        })
    }

    /// Renders one issue for the UI. The path is shown in its display form;
    /// this text is never recorded in diagnostics.
    fn issue_text(error: &VolumeError) -> String {
        let mut text = format!(
            "{:?}: {:?} ({})",
            error.operation,
            error.kind,
            format_path_for_display(&error.path)
        );
        if let Some(code) = error.os_code {
            text.push_str(&format!(" [OS {code}]"));
        }
        text
    }

    const fn failure_class(kind: VolumeErrorKind) -> FailureClass {
        match kind {
            VolumeErrorKind::AccessDenied => FailureClass::PermissionDenied,
            VolumeErrorKind::NotFound
            | VolumeErrorKind::NotReady
            | VolumeErrorKind::InvalidPath => FailureClass::PathUnavailable,
            VolumeErrorKind::NotSupported
            | VolumeErrorKind::InvalidData
            | VolumeErrorKind::BufferTooSmall
            | VolumeErrorKind::Io => FailureClass::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::VolumeKey;
    use std::sync::Mutex;

    struct FakeBackend {
        seen: Arc<Mutex<Vec<PathBuf>>>,
        fail_on: Option<&'static str>,
    }

    impl ResolveBackend for FakeBackend {
        fn resolve_root(&self, path: &Path) -> Result<ResolvedRoot, ResolveFailure> {
            self.seen.lock().unwrap().push(path.to_path_buf());
            if self.fail_on.is_some_and(|needle| path.to_string_lossy().contains(needle)) {
                return Err(ResolveFailure {
                    class: FailureClass::PathUnavailable,
                    detail: "missing".to_owned(),
                });
            }
            Ok(ResolvedRoot {
                root: ScanRoot::new(path, VolumeKey::new(1), StorageClass::Unknown),
                issues: vec!["probe issue".to_owned()],
            })
        }

        fn discover_volumes(&self) -> VolumeList {
            VolumeList {
                volumes: vec![VolumeEntry {
                    path: PathBuf::from("Z:\\"),
                    display: "Z:\\".to_owned(),
                    label: None,
                    filesystem: None,
                    storage_class: StorageClass::Unknown,
                    total_bytes: None,
                    free_bytes: None,
                    issues: Vec::new(),
                }],
                errors: Vec::new(),
            }
        }

        fn filesystem(&self, _cancel: CancelToken) -> Arc<dyn ScanFs> {
            Arc::new(NoFilesystem)
        }
    }

    fn wait_result(client: &mut ResolverClient) -> ResolveResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = client.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "resolver result did not arrive");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn strips_dot_components_without_popping_the_root() {
        let stripped = strip_dot_components(Path::new(r"C:\a\.\b\..\c"));
        assert_eq!(stripped, PathBuf::from(r"C:\a\c"));
        let clamped = strip_dot_components(Path::new(r"C:\..\..\x"));
        assert_eq!(clamped, PathBuf::from(r"C:\x"));
        let relative = strip_dot_components(Path::new("./only/../."));
        assert_eq!(relative, PathBuf::from("."));
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_paths_are_also_lexically_normalized() {
        let stripped = strip_dot_components(Path::new(r"\\?\C:\deep\..\top\.\leaf"));
        assert_eq!(stripped, PathBuf::from(r"\\?\C:\top\leaf"));
        assert_eq!(normalize_for_resolution(Path::new(r"C:\x\..\y")), PathBuf::from(r"C:\y"));
    }

    #[test]
    fn worker_resolves_paths_and_volumes_and_reports_typed_failures() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend { seen: Arc::clone(&seen), fail_on: Some("missing") };
        let (mut client, worker) = start(backend).expect("worker starts");

        let launch_id = client.submit(ResolveJob::Paths(vec![PathBuf::from(r"C:\a\.\b")])).unwrap();
        let result = wait_result(&mut client);
        assert_eq!(result.id, launch_id);
        match result.outcome {
            ResolveOutcome::Launch(launch) => {
                assert_eq!(launch.roots.len(), 1);
                assert_eq!(launch.issues, vec!["probe issue".to_owned()]);
                assert!(!launch.cancel.is_cancelled());
            }
            other => panic!("unexpected outcome {other:?}"),
        }

        let volumes_id = client.submit(ResolveJob::Volumes).unwrap();
        let result = wait_result(&mut client);
        assert_eq!(result.id, volumes_id);
        assert!(matches!(result.outcome, ResolveOutcome::Volumes(list) if list.volumes.len() == 1));

        let failed_id = client
            .submit(ResolveJob::Paths(vec![PathBuf::from(r"C:\ok"), PathBuf::from(r"C:\missing")]))
            .unwrap();
        let result = wait_result(&mut client);
        assert_eq!(result.id, failed_id);
        assert!(matches!(
            result.outcome,
            ResolveOutcome::Failed(ResolveFailure { class: FailureClass::PathUnavailable, .. })
        ));
        assert_eq!(client.outstanding(), 0);

        let empty_id = client.submit(ResolveJob::Paths(Vec::new())).unwrap();
        let result = wait_result(&mut client);
        assert_eq!(result.id, empty_id);
        assert!(matches!(result.outcome, ResolveOutcome::Failed(_)));

        assert!(seen.lock().unwrap().iter().all(|path| !path.to_string_lossy().contains('.')
            || path.to_string_lossy().contains("missing")
            || !path.to_string_lossy().contains(r"\.\")));

        drop(client);
        assert!(worker.finish(Duration::from_secs(5)));
    }

    #[test]
    fn stop_abandons_queued_jobs_and_rejects_new_ones() {
        let backend = FakeBackend { seen: Arc::new(Mutex::new(Vec::new())), fail_on: None };
        let (mut client, worker) = start(backend).expect("worker starts");
        client.request_stop();
        assert_eq!(client.submit(ResolveJob::Volumes), Err(SubmitJobError::WorkerStopped));
        drop(client);
        assert!(worker.finish(Duration::from_secs(5)));
    }
}
