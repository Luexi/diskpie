//! One cancellable worker for the complete, searchable directory listing.
//! Mailboxes and retirement are bounded; filesystem identities remain NodeIds.

use crate::presentation::{
    ItemSort, PresentationError, SortDirection, ranked_children_cancellable,
};
use diskpie_core::{GenerationId, NodeId, TreeSnapshot, sunburst::SizeBasis};
use std::{
    fmt, io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemListKey {
    pub generation: GenerationId,
    pub revision: u64,
    pub root: NodeId,
    pub size_basis: SizeBasis,
    pub sort: ItemSort,
    pub direction: SortDirection,
    pub query: String,
}

pub struct ItemListRequest {
    pub key: ItemListKey,
    pub snapshot: Arc<TreeSnapshot>,
}

impl fmt::Debug for ItemListRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemListRequest")
            .field("generation", &self.key.generation)
            .field("revision", &self.key.revision)
            .field("root", &self.key.root)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ItemListError {
    Presentation(PresentationError),
    GenerationMismatch,
    WorkerPanicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemListSubmitError {
    WorkerStopped,
    RetirementBackpressure,
    RequestIdExhausted,
}

#[derive(Debug)]
pub struct ItemListRejected {
    pub error: ItemListSubmitError,
    pub request: ItemListRequest,
}

impl ItemListRejected {
    pub fn into_parts(self) -> (ItemListSubmitError, ItemListRequest) {
        (self.error, self.request)
    }
}

#[derive(Debug)]
pub struct ItemListCompletion {
    pub request_id: u64,
    pub key: ItemListKey,
    pub result: Result<Arc<[NodeId]>, ItemListError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemListFinish {
    Joined,
    TimedOut,
    WorkerPanicked,
}

struct Job {
    id: u64,
    request: ItemListRequest,
}
#[derive(Default)]
struct Mailbox {
    pending: Option<Job>,
    completed: Option<ItemListCompletion>,
    retired: Option<(Option<Job>, Option<ItemListCompletion>)>,
}

#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    latest: AtomicU64,
    active: AtomicBool,
    stop: AtomicBool,
}

const MAX_SERVICES: usize = 16;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static REAPER_STOPPED: AtomicBool = AtomicBool::new(false);
static REAPER: OnceLock<Result<SyncSender<RetiredService>, io::ErrorKind>> = OnceLock::new();
static ORPHANS: OnceLock<Mutex<Vec<RetiredService>>> = OnceLock::new();
struct Lease;
impl Drop for Lease {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}
struct RetiredService {
    worker: JoinHandle<()>,
    shared: Arc<Shared>,
    lease: Lease,
}

fn reaper() -> io::Result<SyncSender<RetiredService>> {
    match REAPER.get_or_init(|| {
        ORPHANS.get_or_init(|| Mutex::new(Vec::with_capacity(MAX_SERVICES)));
        let (sender, receiver) = sync_channel::<RetiredService>(MAX_SERVICES);
        thread::Builder::new()
            .name("diskpie-items-reaper".into())
            .spawn(move || {
                while let Ok(retired) = receiver.recv() {
                    let _ = retired.worker.join();
                    drop(retired.shared);
                    drop(retired.lease);
                }
            })
            .map_err(|error| error.kind())?;
        Ok(sender)
    }) {
        Ok(sender) if !REAPER_STOPPED.load(Ordering::Acquire) => Ok(sender.clone()),
        Ok(_) => Err(io::Error::other("item-list retirement is unavailable")),
        Err(kind) => Err(io::Error::new(*kind, "cannot start item-list reaper")),
    }
}

/// Lifecycle finish is explicit and off-frame. Drop only requests stop and
/// hands all owned state to the precreated reaper, including finished workers.
pub struct ItemListService {
    shared: Option<Arc<Shared>>,
    worker: Option<JoinHandle<()>>,
    lease: Option<Lease>,
    reaper: SyncSender<RetiredService>,
    next_id: u64,
}

impl ItemListService {
    pub fn new() -> io::Result<Self> {
        let reaper = reaper()?;
        LIVE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_SERVICES).then_some(count + 1)
        })
        .map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "item-list worker capacity reached")
        })?;
        let lease = Lease;
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("diskpie-items".into())
            .spawn(move || worker_loop(worker_shared))?;
        Ok(Self {
            shared: Some(shared),
            worker: Some(worker),
            lease: Some(lease),
            reaper,
            next_id: 0,
        })
    }

    pub fn submit(&mut self, request: ItemListRequest) -> Result<u64, ItemListRejected> {
        let Some(shared) = self.shared.as_ref() else {
            return Err(ItemListRejected { error: ItemListSubmitError::WorkerStopped, request });
        };
        if shared.stop.load(Ordering::Acquire)
            || self.worker.as_ref().is_none_or(JoinHandle::is_finished)
        {
            return Err(ItemListRejected { error: ItemListSubmitError::WorkerStopped, request });
        }
        let Some(id) = self.next_id.checked_add(1) else {
            return Err(ItemListRejected {
                error: ItemListSubmitError::RequestIdExhausted,
                request,
            });
        };
        let mut mailbox = shared.mailbox.lock().unwrap_or_else(|p| p.into_inner());
        if mailbox.retired.is_some() && (mailbox.pending.is_some() || mailbox.completed.is_some()) {
            return Err(ItemListRejected {
                error: ItemListSubmitError::RetirementBackpressure,
                request,
            });
        }
        if mailbox.pending.is_some() || mailbox.completed.is_some() {
            mailbox.retired = Some((mailbox.pending.take(), mailbox.completed.take()));
        }
        mailbox.pending = Some(Job { id, request });
        shared.latest.store(id, Ordering::Release);
        self.next_id = id;
        drop(mailbox);
        shared.wake.notify_one();
        Ok(id)
    }

    pub fn try_take(&self) -> Option<ItemListCompletion> {
        self.shared.as_ref()?.mailbox.lock().unwrap_or_else(|p| p.into_inner()).completed.take()
    }

    pub fn is_busy(&self) -> bool {
        self.shared.as_ref().is_some_and(|shared| {
            shared.active.load(Ordering::Acquire)
                || shared.mailbox.lock().unwrap_or_else(|p| p.into_inner()).pending.is_some()
        })
    }

    pub fn request_shutdown(&self) {
        if let Some(shared) = &self.shared {
            shared.stop.store(true, Ordering::Release);
            shared.wake.notify_all();
        }
    }

    pub fn finish(mut self, timeout: Duration) -> ItemListFinish {
        self.request_shutdown();
        let started = Instant::now();
        while self.worker.as_ref().is_some_and(|worker| !worker.is_finished())
            && started.elapsed() < timeout
        {
            thread::sleep(Duration::from_millis(1).min(timeout.saturating_sub(started.elapsed())));
        }
        if self.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return ItemListFinish::TimedOut;
        }
        let joined = self.worker.take().is_none_or(|worker| worker.join().is_ok());
        self.shared.take();
        self.lease.take();
        if joined { ItemListFinish::Joined } else { ItemListFinish::WorkerPanicked }
    }
}

impl Drop for ItemListService {
    fn drop(&mut self) {
        self.request_shutdown();
        if let (Some(worker), Some(shared), Some(lease)) =
            (self.worker.take(), self.shared.take(), self.lease.take())
        {
            let retired = RetiredService { worker, shared, lease };
            if let Err(error) = self.reaper.try_send(retired) {
                // Capacity is reserved before creating a service. Failure here
                // terminalizes admission and retains bounded ownership safely.
                REAPER_STOPPED.store(true, Ordering::Release);
                let retired = match error {
                    std::sync::mpsc::TrySendError::Full(value)
                    | std::sync::mpsc::TrySendError::Disconnected(value) => value,
                };
                ORPHANS
                    .get()
                    .expect("reaper preparation initialized fallback")
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(retired);
            }
        }
    }
}

struct RankingCache {
    key: ItemListKey,
    snapshot: Arc<TreeSnapshot>,
    nodes: Arc<[NodeId]>,
}

fn worker_loop(shared: Arc<Shared>) {
    let mut cache: Option<RankingCache> = None;
    loop {
        let (job, retired) = {
            let mut mailbox = shared.mailbox.lock().unwrap_or_else(|p| p.into_inner());
            while mailbox.pending.is_none()
                && mailbox.retired.is_none()
                && !shared.stop.load(Ordering::Acquire)
            {
                // Timeout closes the atomic-stop/condvar notification race.
                mailbox = shared
                    .wake
                    .wait_timeout(mailbox, Duration::from_millis(25))
                    .unwrap_or_else(|p| p.into_inner())
                    .0;
            }
            if shared.stop.load(Ordering::Acquire) {
                return;
            }
            let job = mailbox.pending.take();
            shared.active.store(job.is_some(), Ordering::Release);
            (job, mailbox.retired.take())
        };
        drop(retired);
        let Some(job) = job else {
            continue;
        };
        let cancelled = || {
            shared.stop.load(Ordering::Acquire) || shared.latest.load(Ordering::Acquire) != job.id
        };
        let computed =
            catch_unwind(AssertUnwindSafe(|| compute(&job.request, &mut cache, cancelled)))
                .unwrap_or(Some(Err(ItemListError::WorkerPanicked)));
        let replaced = {
            let mut mailbox = shared.mailbox.lock().unwrap_or_else(|p| p.into_inner());
            shared.active.store(false, Ordering::Release);
            if !cancelled() {
                computed.and_then(|result| {
                    mailbox.completed.replace(ItemListCompletion {
                        request_id: job.id,
                        key: job.request.key.clone(),
                        result,
                    })
                })
            } else {
                None
            }
        };
        drop(replaced);
    }
}

fn compute(
    request: &ItemListRequest,
    cache: &mut Option<RankingCache>,
    cancelled: impl Fn() -> bool,
) -> Option<Result<Arc<[NodeId]>, ItemListError>> {
    if request.key.generation != request.snapshot.generation() {
        return Some(Err(ItemListError::GenerationMismatch));
    }
    let reusable = cache.as_ref().is_some_and(|cache| {
        cache.key.generation == request.key.generation
            && cache.key.revision == request.key.revision
            && cache.key.root == request.key.root
            && cache.key.size_basis == request.key.size_basis
            && cache.key.sort == request.key.sort
            && cache.key.direction == request.key.direction
            && Arc::ptr_eq(&cache.snapshot, &request.snapshot)
    });
    if !reusable {
        let nodes = match ranked_children_cancellable(
            &request.snapshot,
            request.key.root,
            request.key.size_basis,
            request.key.sort,
            request.key.direction,
            &cancelled,
        ) {
            Ok(Some(nodes)) => nodes,
            Ok(None) => return None,
            Err(error) => return Some(Err(ItemListError::Presentation(error))),
        };
        *cache = Some(RankingCache {
            key: request.key.clone(),
            snapshot: Arc::clone(&request.snapshot),
            nodes: nodes.into(),
        });
    }
    let cache = cache.as_ref().expect("ranking cached");
    if request.key.query.is_empty() {
        return Some(Ok(Arc::clone(&cache.nodes)));
    }
    let query = request.key.query.to_lowercase();
    let mut filtered = Vec::new();
    for (index, &node) in cache.nodes.iter().enumerate() {
        if index % 256 == 0 && cancelled() {
            return None;
        }
        if request
            .snapshot
            .node(node)
            .expect("ranked node exists")
            .name()
            .to_string_lossy()
            .to_lowercase()
            .contains(&query)
        {
            filtered.push(node);
        }
    }
    Some(Ok(filtered.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{NodeSpec, OwnMetrics, TreeBuilder};

    fn request(count: usize, query: &str) -> ItemListRequest {
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        for n in 0..count {
            builder
                .add_child(root, NodeSpec::file(format!("FILE-{n:07}"), OwnMetrics::ZERO_BY_POLICY))
                .unwrap();
        }
        ItemListRequest {
            key: ItemListKey {
                generation: GenerationId::new(1),
                revision: 1,
                root,
                size_basis: SizeBasis::Logical,
                sort: ItemSort::Size,
                direction: SortDirection::Descending,
                query: query.into(),
            },
            snapshot: Arc::new(builder.freeze().unwrap()),
        }
    }

    fn wait(service: &ItemListService) -> ItemListCompletion {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(completion) = service.try_take() {
                return completion;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn all_children_and_case_insensitive_filter_keep_native_ids() {
        let mut service = ItemListService::new().unwrap();
        let request = request(10_000, "");
        let snapshot = Arc::clone(&request.snapshot);
        let mut key = request.key.clone();
        service.submit(request).unwrap();
        let all = wait(&service).result.unwrap();
        assert_eq!(all.len(), 10_000);
        key.query = "file-0009999".into();
        service.submit(ItemListRequest { key, snapshot }).unwrap();
        let filtered = wait(&service).result.unwrap();
        assert_eq!(&*filtered, &[NodeId::from_raw(10_000)]);
        assert_eq!(service.finish(Duration::from_secs(1)), ItemListFinish::Joined);
    }

    #[test]
    fn obsolete_request_never_publishes_and_drop_does_not_join() {
        let mut service = ItemListService::new().unwrap();
        let request = request(50_000, "");
        let snapshot = Arc::clone(&request.snapshot);
        let mut key = request.key.clone();
        service.submit(request).unwrap();
        key.query = "not-present".into();
        key.sort = ItemSort::Name;
        key.direction = SortDirection::Ascending;
        let mut next = ItemListRequest { key, snapshot };
        let latest = loop {
            match service.submit(next) {
                Ok(id) => break id,
                Err(rejected) => {
                    assert_eq!(rejected.error, ItemListSubmitError::RetirementBackpressure);
                    next = rejected.request;
                    thread::yield_now();
                }
            }
        };
        let completion = wait(&service);
        assert_eq!(completion.request_id, latest);
        assert_eq!(completion.key.sort, ItemSort::Name);
        assert_eq!(completion.key.direction, SortDirection::Ascending);
        assert!(completion.result.unwrap().is_empty());
        let started = Instant::now();
        drop(service);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn changing_sort_invalidates_rank_cache_but_query_keeps_that_order() {
        let request = request(640, "");
        let snapshot = Arc::clone(&request.snapshot);
        let mut cache = None;
        let all = compute(&request, &mut cache, || false).unwrap().unwrap();
        assert_eq!(all[0], NodeId::from_raw(1));
        let mut changed = ItemListRequest { key: request.key.clone(), snapshot };
        changed.key.sort = ItemSort::Name;
        changed.key.direction = SortDirection::Descending;
        let descending = compute(&changed, &mut cache, || false).unwrap().unwrap();
        assert_eq!(descending[0], NodeId::from_raw(640));
        changed.key.query = "file-00006".into();
        let filtered = compute(&changed, &mut cache, || false).unwrap().unwrap();
        assert_eq!(filtered.len(), 40);
        assert_eq!(filtered[0], NodeId::from_raw(640));
        assert_eq!(filtered[39], NodeId::from_raw(601));
        assert!(Arc::ptr_eq(&descending, &cache.as_ref().unwrap().nodes));
        changed.key.direction = SortDirection::Ascending;
        let ascending = compute(&changed, &mut cache, || false).unwrap().unwrap();
        assert_eq!(ascending[0], NodeId::from_raw(601));
        assert_eq!(ascending[39], NodeId::from_raw(640));
    }
}
