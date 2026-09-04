use crate::{
    CancelToken, DirectoryItem, DirectoryState, FsEntry, FsError, FsErrorKind, FsOperation,
    PermitCaps, ScanEvent, ScanFs, ScanGeneration, ScanOptions, ScanRequest, ScanRoot, ScanSession,
    StorageClass, TerminalState, VisitControl, start_scan, start_scan_with_cancel,
};
use diskpie_core::{EntryKind, MetricSource, OwnMetrics, SizeMetric, VolumeKey};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
struct FakeDirectory {
    items: Vec<DirectoryItem>,
    result: Result<(), FsError>,
    panic: bool,
    item_delay: Duration,
}

impl FakeDirectory {
    fn items(items: Vec<DirectoryItem>) -> Self {
        Self { items, result: Ok(()), panic: false, item_delay: Duration::ZERO }
    }

    fn error(error: FsError) -> Self {
        Self { items: Vec::new(), result: Err(error), panic: false, item_delay: Duration::ZERO }
    }

    fn panicking() -> Self {
        Self { items: Vec::new(), result: Ok(()), panic: true, item_delay: Duration::ZERO }
    }

    fn with_item_delay(mut self, delay: Duration) -> Self {
        self.item_delay = delay;
        self
    }
}

#[derive(Debug, Default)]
struct FakeFs {
    directories: Mutex<HashMap<PathBuf, FakeDirectory>>,
    active: AtomicUsize,
    peak_active: AtomicUsize,
    visits: AtomicUsize,
    visited_items: AtomicUsize,
}

impl FakeFs {
    fn insert(&self, path: impl Into<PathBuf>, directory: FakeDirectory) {
        self.directories.lock().expect("fake filesystem lock").insert(path.into(), directory);
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    fn peak_active(&self) -> usize {
        self.peak_active.load(Ordering::SeqCst)
    }

    fn visits(&self) -> usize {
        self.visits.load(Ordering::SeqCst)
    }

    fn visited_items(&self) -> usize {
        self.visited_items.load(Ordering::SeqCst)
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ScanFs for FakeFs {
    fn visit_directory(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
    ) -> Result<(), FsError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_active.fetch_max(active, Ordering::SeqCst);
        let _active_guard = ActiveGuard(&self.active);
        self.visits.fetch_add(1, Ordering::SeqCst);

        let plan = self
            .directories
            .lock()
            .expect("fake filesystem lock")
            .get(directory)
            .cloned()
            .unwrap_or_else(|| {
                FakeDirectory::error(FsError::new(
                    directory,
                    FsOperation::EnumerateDirectory,
                    FsErrorKind::NotFound,
                ))
            });
        assert!(!plan.panic, "intentional fake provider panic for {}", directory.display());
        for item in plan.items {
            if !plan.item_delay.is_zero() {
                thread::sleep(plan.item_delay);
            }
            self.visited_items.fetch_add(1, Ordering::SeqCst);
            if visitor(item) == VisitControl::Stop {
                return Ok(());
            }
        }
        plan.result
    }
}

fn metrics(bytes: u64) -> OwnMetrics {
    OwnMetrics::new(
        SizeMetric::known(bytes, MetricSource::PortableMetadata),
        SizeMetric::known(bytes, MetricSource::FilesystemAllocation),
    )
}

fn file(name: impl Into<std::ffi::OsString>) -> DirectoryItem {
    DirectoryItem::Entry(FsEntry::new(name, EntryKind::File, metrics(1)))
}

fn directory(name: impl Into<std::ffi::OsString>) -> DirectoryItem {
    DirectoryItem::Entry(FsEntry::new(name, EntryKind::Directory, OwnMetrics::ZERO_BY_POLICY))
}

fn options(worker_count: usize) -> ScanOptions {
    ScanOptions {
        worker_count,
        batch_entries: 2,
        result_capacity: 4,
        event_capacity: 32,
        flush_interval: Duration::from_secs(10),
        backpressure_park: Duration::from_millis(1),
        coordinator_poll: Duration::from_millis(1),
        permit_caps: PermitCaps {
            local_low_seek_penalty: worker_count.max(1),
            rotational: worker_count.clamp(1, 2),
            removable: 1,
            network: 1,
            unknown: worker_count.clamp(1, 2),
        },
    }
}

fn root(path: impl Into<PathBuf>, volume: u128, class: StorageClass) -> ScanRoot {
    ScanRoot::new(path, VolumeKey::new(volume), class)
}

fn request(generation: u64, roots: Vec<ScanRoot>, options: ScanOptions) -> ScanRequest {
    ScanRequest::new(ScanGeneration::new(generation), roots).with_options(options)
}

fn collect_session(session: ScanSession) -> (Vec<ScanEvent>, crate::ScanTerminal) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "scan timed out; collected events: {events:#?}");
        let event = session.recv_timeout(remaining).expect("scan event");
        let terminal = matches!(event, ScanEvent::Terminal(_));
        events.push(event);
        if terminal {
            break;
        }
    }
    let terminal = session.join().expect("coordinator join");
    (events, terminal)
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn exposes_nonblocking_coordinator_completion_for_ui_polling() {
    let fake = Arc::new(FakeFs::default());
    fake.insert(
        "root",
        FakeDirectory::items(vec![file("slow")]).with_item_delay(Duration::from_millis(25)),
    );
    let session = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(0, vec![root("root", 0, StorageClass::Unknown)], options(1)),
    )
    .expect("start scan");

    wait_until(Duration::from_secs(2), || fake.active() == 1);
    assert!(!session.is_finished());

    loop {
        if matches!(
            session.recv_timeout(Duration::from_secs(2)).expect("scan event"),
            ScanEvent::Terminal(_)
        ) {
            break;
        }
    }
    wait_until(Duration::from_secs(2), || session.is_finished());
    let terminal = session.join().expect("coordinator join");

    assert_eq!(terminal.state, TerminalState::Complete);
    assert_eq!(fake.active(), 0);
}

#[test]
fn emits_bounded_partial_batches_and_directory_completion() {
    let fake = Arc::new(FakeFs::default());
    fake.insert(
        "root",
        FakeDirectory::items(vec![file("a"), file("b"), file("c"), file("d"), file("e")]),
    );
    let session = start_scan(
        fake,
        request(1, vec![root("root", 1, StorageClass::LocalLowSeekPenalty)], options(1)),
    )
    .expect("start scan");
    let completion_token = session.cancel_token();
    let (events, terminal) = collect_session(session);

    let batch_lengths = events
        .iter()
        .filter_map(|event| match event {
            ScanEvent::Batch(batch) => Some(batch.entries.len()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(batch_lengths, vec![2, 2, 1]);
    let started_root = events
        .iter()
        .find_map(|event| match event {
            ScanEvent::Started { roots, .. } => roots.first().map(|root| root.id),
            _ => None,
        })
        .expect("started root");
    let batches = events
        .iter()
        .filter_map(|event| match event {
            ScanEvent::Batch(batch) => Some(batch),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(batches.iter().all(|batch| batch.generation == ScanGeneration::new(1)));
    assert!(batches.iter().all(|batch| batch.parent == started_root));
    let assigned_ids = batches
        .iter()
        .flat_map(|batch| batch.entries.iter().map(|entry| entry.id.raw()))
        .collect::<Vec<_>>();
    assert!(assigned_ids.windows(2).all(|pair| pair[0] < pair[1]));
    let completion = events
        .iter()
        .find_map(|event| match event {
            ScanEvent::DirectoryComplete(completion) => Some(completion),
            _ => None,
        })
        .expect("directory completion");
    assert_eq!(completion.state, DirectoryState::Complete);
    assert_eq!(completion.counters.entries, 5);
    assert_eq!(completion.counters.batches, 3);
    assert_eq!(terminal.state, TerminalState::Complete);
    assert_eq!(terminal.counters.entries, 6, "root plus five children");
    assert_eq!(terminal.counters.files, 5);
    assert_eq!(terminal.counters.directories_completed, 1);
    assert!(!completion_token.is_cancelled(), "normal cleanup is not cancellation");
}

#[test]
fn access_denied_is_an_omission_and_other_directories_continue() {
    let fake = Arc::new(FakeFs::default());
    let root_path = PathBuf::from("root");
    fake.insert(
        &root_path,
        FakeDirectory::items(vec![directory("blocked"), file("visible"), directory("open")]),
    );
    fake.insert(
        root_path.join("blocked"),
        FakeDirectory::error(
            FsError::new(
                root_path.join("blocked"),
                FsOperation::EnumerateDirectory,
                FsErrorKind::AccessDenied,
            )
            .with_os_code(5),
        ),
    );
    fake.insert(root_path.join("open"), FakeDirectory::items(vec![file("inside")]));

    let session = start_scan(
        fake,
        request(2, vec![root(&root_path, 2, StorageClass::LocalLowSeekPenalty)], options(2)),
    )
    .expect("start scan");
    let (events, terminal) = collect_session(session);

    let omissions = events
        .iter()
        .filter_map(|event| match event {
            ScanEvent::Batch(batch) => Some(batch.omissions.as_slice()),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(omissions.len(), 1);
    assert_eq!(omissions[0].error.kind, FsErrorKind::AccessDenied);
    assert_eq!(terminal.state, TerminalState::Partial);
    assert_eq!(terminal.counters.files, 2);
    assert_eq!(terminal.counters.omissions, 1);
    assert_eq!(terminal.counters.directories_completed, 3);
}

#[test]
fn scans_deep_and_wide_trees_iteratively() {
    const DEPTH: usize = 1_000;
    const WIDTH: usize = 500;
    let fake = Arc::new(FakeFs::default());
    let root_path = PathBuf::from("root");
    let mut root_items = (0..WIDTH).map(|index| file(format!("file-{index}"))).collect::<Vec<_>>();
    root_items.push(directory("deep"));
    fake.insert(&root_path, FakeDirectory::items(root_items));

    let mut parent = root_path.join("deep");
    for depth in 0..DEPTH {
        let child_name = format!("d-{depth}");
        fake.insert(&parent, FakeDirectory::items(vec![directory(&child_name)]));
        parent.push(child_name);
    }
    fake.insert(&parent, FakeDirectory::items(vec![file("leaf")]));

    let session = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(3, vec![root(&root_path, 3, StorageClass::LocalLowSeekPenalty)], options(4)),
    )
    .expect("start scan");
    let (_, terminal) = collect_session(session);

    assert_eq!(terminal.state, TerminalState::Complete);
    assert_eq!(terminal.counters.files, WIDTH as u64 + 1);
    assert_eq!(terminal.counters.directories, DEPTH as u64 + 2);
    assert_eq!(terminal.counters.directories_completed, DEPTH as u64 + 2);
    assert_eq!(fake.active(), 0);
}

#[test]
fn event_and_result_backpressure_resume_without_busy_spinning() {
    let fake = Arc::new(FakeFs::default());
    fake.insert(
        "root",
        FakeDirectory::items((0..100).map(|index| file(format!("file-{index}"))).collect()),
    );
    let mut scan_options = options(2);
    scan_options.batch_entries = 1;
    scan_options.result_capacity = 1;
    scan_options.event_capacity = 1;
    let session = start_scan(
        fake,
        request(4, vec![root("root", 4, StorageClass::LocalLowSeekPenalty)], scan_options),
    )
    .expect("start scan");

    thread::sleep(Duration::from_millis(25));
    let (_, terminal) = collect_session(session);
    assert_eq!(terminal.state, TerminalState::Complete);
    assert_eq!(terminal.counters.files, 100);
    assert_eq!(terminal.counters.batches, 100);
}

#[test]
fn already_cancelled_token_prevents_any_filesystem_visit() {
    let fake = Arc::new(FakeFs::default());
    fake.insert("root", FakeDirectory::items(vec![file("never-visited")]));
    let token = CancelToken::new();
    token.cancel();
    let session = start_scan_with_cancel(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(5, vec![root("root", 5, StorageClass::LocalLowSeekPenalty)], options(1)),
        token,
    )
    .expect("start cancelled scan");
    let (_, terminal) = collect_session(session);

    assert_eq!(terminal.state, TerminalState::Cancelled);
    assert_eq!(fake.visits(), 0);
    assert_eq!(fake.active(), 0);
}

#[test]
fn cancellation_mid_directory_returns_promptly_with_partial_progress() {
    let fake = Arc::new(FakeFs::default());
    fake.insert(
        "root",
        FakeDirectory::items((0..500).map(|index| file(format!("file-{index}"))).collect())
            .with_item_delay(Duration::from_millis(1)),
    );
    let mut scan_options = options(1);
    scan_options.batch_entries = 4;
    let session = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(6, vec![root("root", 6, StorageClass::LocalLowSeekPenalty)], scan_options),
    )
    .expect("start scan");
    wait_until(Duration::from_secs(2), || fake.visited_items() >= 8);
    let cancelled_at = Instant::now();
    session.cancel();
    let (_, terminal) = collect_session(session);

    assert_eq!(terminal.state, TerminalState::Cancelled);
    assert!(terminal.counters.files < 500);
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    assert_eq!(fake.active(), 0);
}

#[test]
fn cancellation_escapes_full_event_and_result_channels() {
    let fake = Arc::new(FakeFs::default());
    fake.insert(
        "root",
        FakeDirectory::items((0..1_000).map(|index| file(format!("file-{index}"))).collect()),
    );
    let mut scan_options = options(1);
    scan_options.batch_entries = 1;
    scan_options.result_capacity = 1;
    scan_options.event_capacity = 1;
    let session = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(7, vec![root("root", 7, StorageClass::LocalLowSeekPenalty)], scan_options),
    )
    .expect("start scan");
    assert!(matches!(
        session.recv_timeout(Duration::from_secs(2)).expect("started event"),
        ScanEvent::Started { .. }
    ));
    wait_until(Duration::from_secs(2), || fake.visited_items() >= 4);
    thread::sleep(Duration::from_millis(10));

    let cancelled_at = Instant::now();
    session.cancel();
    let (_, terminal) = collect_session(session);
    assert_eq!(terminal.state, TerminalState::Cancelled);
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    assert_eq!(fake.active(), 0);
}

#[test]
fn generations_reject_late_events_from_a_cancelled_scan() {
    let fake = Arc::new(FakeFs::default());
    fake.insert(
        "root",
        FakeDirectory::items((0..80).map(|index| file(format!("file-{index}"))).collect())
            .with_item_delay(Duration::from_millis(1)),
    );
    let first = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(10, vec![root("root", 10, StorageClass::LocalLowSeekPenalty)], options(1)),
    )
    .expect("first generation");
    wait_until(Duration::from_secs(2), || fake.visited_items() >= 4);
    first.cancel();

    let second = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(11, vec![root("root", 10, StorageClass::LocalLowSeekPenalty)], options(1)),
    )
    .expect("second generation");
    let (second_events, second_terminal) = collect_session(second);
    let (late_first_events, first_terminal) = collect_session(first);

    assert_eq!(second_terminal.state, TerminalState::Complete);
    assert_eq!(first_terminal.state, TerminalState::Cancelled);
    assert!(second_events.iter().all(|event| ScanGeneration::new(11).accepts(event)));
    assert!(late_first_events.iter().all(|event| ScanGeneration::new(10).accepts(event)));
    assert!(late_first_events.iter().all(|event| !ScanGeneration::new(11).accepts(event)));
}

#[test]
fn per_volume_network_cap_bounds_workers_and_join_releases_every_arc() {
    let fake = Arc::new(FakeFs::default());
    let mut roots = Vec::new();
    for index in 0..8 {
        let path = PathBuf::from(format!("root-{index}"));
        fake.insert(
            &path,
            FakeDirectory::items(vec![file("file")]).with_item_delay(Duration::from_millis(10)),
        );
        roots.push(root(path, 99, StorageClass::Network));
    }
    let session = start_scan(Arc::clone(&fake) as Arc<dyn ScanFs>, request(12, roots, options(4)))
        .expect("start scan");
    let (_, terminal) = collect_session(session);

    assert_eq!(terminal.state, TerminalState::Complete);
    assert!(terminal.peak_active_workers <= 1);
    assert!(fake.peak_active() <= 1);
    assert_eq!(fake.active(), 0);
    assert_eq!(Arc::strong_count(&fake), 1, "coordinator and workers released provider Arcs");
}

#[test]
fn provider_panic_surfaces_as_typed_failure_and_cleans_up() {
    let fake = Arc::new(FakeFs::default());
    fake.insert("root", FakeDirectory::panicking());
    let session = start_scan(
        Arc::clone(&fake) as Arc<dyn ScanFs>,
        request(13, vec![root("root", 13, StorageClass::LocalLowSeekPenalty)], options(2)),
    )
    .expect("start scan");
    let (_, terminal) = collect_session(session);

    assert!(matches!(
        terminal.state,
        TerminalState::Failed(crate::ScanFailure::WorkerPanicked { .. })
    ));
    assert_eq!(fake.active(), 0);
    assert_eq!(Arc::strong_count(&fake), 1);
}
