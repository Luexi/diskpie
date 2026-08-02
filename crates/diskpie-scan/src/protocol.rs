//! Generation-tagged request and event protocol.

use crate::fs::{FsEntry, FsError, ScanOmission};
use diskpie_core::{
    EntryKind, FileIdentity, GenerationId, NodeSpec, OwnMetrics, ReparseKind, VolumeKey,
};
use std::{error::Error, ffi::OsString, fmt, path::PathBuf, time::Duration};

/// Monotonically increasing scan generation used to reject stale events.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ScanGeneration(u64);

impl ScanGeneration {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn accepts(self, event: &ScanEvent) -> bool {
        event.generation() == self
    }
}

impl From<ScanGeneration> for GenerationId {
    fn from(generation: ScanGeneration) -> Self {
        Self::new(generation.get())
    }
}

impl From<GenerationId> for ScanGeneration {
    fn from(generation: GenerationId) -> Self {
        Self::new(generation.get())
    }
}

/// Generation-local protocol ID assigned by the coordinator.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ScanNodeId(u64);

impl ScanNodeId {
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ScanNodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Stable worker slot identifier within one session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct WorkerId(u16);

impl WorkerId {
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Conservative device class used to choose per-volume concurrency.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StorageClass {
    LocalLowSeekPenalty,
    Rotational,
    Removable,
    Network,
    Unknown,
}

/// Initial root supplied to a scan request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRoot {
    pub path: PathBuf,
    pub volume: VolumeKey,
    pub storage_class: StorageClass,
}

impl ScanRoot {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, volume: VolumeKey, storage_class: StorageClass) -> Self {
        Self { path: path.into(), volume, storage_class }
    }
}

/// Per-storage-class in-flight directory limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermitCaps {
    pub local_low_seek_penalty: usize,
    pub rotational: usize,
    pub removable: usize,
    pub network: usize,
    pub unknown: usize,
}

impl PermitCaps {
    #[must_use]
    pub const fn for_class(self, class: StorageClass) -> usize {
        match class {
            StorageClass::LocalLowSeekPenalty => self.local_low_seek_penalty,
            StorageClass::Rotational => self.rotational,
            StorageClass::Removable => self.removable,
            StorageClass::Network => self.network,
            StorageClass::Unknown => self.unknown,
        }
    }
}

impl Default for PermitCaps {
    fn default() -> Self {
        Self { local_low_seek_penalty: 4, rotational: 2, removable: 1, network: 1, unknown: 2 }
    }
}

/// Bounded scheduling and batching knobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    pub worker_count: usize,
    pub batch_entries: usize,
    pub result_capacity: usize,
    pub event_capacity: usize,
    pub cancellation_check_interval: usize,
    pub flush_interval: Duration,
    pub backpressure_park: Duration,
    pub coordinator_poll: Duration,
    pub permit_caps: PermitCaps,
}

impl Default for ScanOptions {
    fn default() -> Self {
        let available = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        Self {
            worker_count: available.clamp(1, 4),
            batch_entries: 512,
            result_capacity: 16,
            event_capacity: 8,
            cancellation_check_interval: 64,
            flush_interval: Duration::from_millis(25),
            backpressure_park: Duration::from_millis(1),
            coordinator_poll: Duration::from_millis(2),
            permit_caps: PermitCaps::default(),
        }
    }
}

impl ScanOptions {
    pub(crate) fn validate(&self) -> Result<(), ScanFailure> {
        let positive = [
            ("worker_count", self.worker_count),
            ("batch_entries", self.batch_entries),
            ("result_capacity", self.result_capacity),
            ("event_capacity", self.event_capacity),
            ("cancellation_check_interval", self.cancellation_check_interval),
            ("permit_caps.local_low_seek_penalty", self.permit_caps.local_low_seek_penalty),
            ("permit_caps.rotational", self.permit_caps.rotational),
            ("permit_caps.removable", self.permit_caps.removable),
            ("permit_caps.network", self.permit_caps.network),
            ("permit_caps.unknown", self.permit_caps.unknown),
        ];
        for (field, value) in positive {
            if value == 0 {
                return Err(ScanFailure::InvalidOptions { field });
            }
        }
        if self.worker_count > usize::from(u16::MAX) + 1 {
            return Err(ScanFailure::InvalidOptions { field: "worker_count" });
        }
        if self.flush_interval.is_zero() {
            return Err(ScanFailure::InvalidOptions { field: "flush_interval" });
        }
        if self.backpressure_park.is_zero() {
            return Err(ScanFailure::InvalidOptions { field: "backpressure_park" });
        }
        if self.coordinator_poll.is_zero() {
            return Err(ScanFailure::InvalidOptions { field: "coordinator_poll" });
        }
        Ok(())
    }
}

/// Typed request for one independent scan generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    pub generation: ScanGeneration,
    pub roots: Vec<ScanRoot>,
    pub options: ScanOptions,
}

impl ScanRequest {
    #[must_use]
    pub fn new(generation: ScanGeneration, roots: Vec<ScanRoot>) -> Self {
        Self { generation, roots, options: ScanOptions::default() }
    }

    #[must_use]
    pub fn with_options(mut self, options: ScanOptions) -> Self {
        self.options = options;
        self
    }
}

/// Root identity announced before any child batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedRoot {
    pub id: ScanNodeId,
    pub path: PathBuf,
    pub volume: VolumeKey,
    pub storage_class: StorageClass,
}

/// Entry assigned an ID by the coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedEntry {
    pub id: ScanNodeId,
    pub parent: ScanNodeId,
    pub name: OsString,
    pub kind: EntryKind,
    pub own_metrics: OwnMetrics,
    pub file_identity: Option<FileIdentity>,
}

impl ScannedEntry {
    pub(crate) fn from_fs(id: ScanNodeId, parent: ScanNodeId, entry: FsEntry) -> Self {
        Self {
            id,
            parent,
            name: entry.name,
            kind: entry.kind,
            own_metrics: entry.own_metrics,
            file_identity: entry.file_identity,
        }
    }

    /// Converts protocol metadata into the core builder's input type.
    #[must_use]
    pub fn into_node_spec(self) -> NodeSpec {
        let spec = NodeSpec::new(self.name, self.kind, self.own_metrics);
        if let Some(identity) = self.file_identity {
            spec.with_file_identity(identity)
        } else {
            spec
        }
    }
}

/// Counter fields protected from integer wrapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterField {
    Entries,
    Files,
    Directories,
    ReparsePoints,
    Omissions,
    Batches,
    DirectoriesCompleted,
}

/// Delta or total progress counters carried by scan events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanCounters {
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub reparse_points: u64,
    pub omissions: u64,
    pub batches: u64,
    pub directories_completed: u64,
}

impl ScanCounters {
    pub(crate) fn observe_entry(&mut self, kind: EntryKind) -> Result<(), ScanFailure> {
        self.entries = checked_counter(self.entries, 1, CounterField::Entries)?;
        match kind {
            EntryKind::File => {
                self.files = checked_counter(self.files, 1, CounterField::Files)?;
            }
            EntryKind::Directory | EntryKind::Root => {
                self.directories = checked_counter(self.directories, 1, CounterField::Directories)?;
            }
            EntryKind::ReparsePoint(reparse_kind) => {
                self.reparse_points =
                    checked_counter(self.reparse_points, 1, CounterField::ReparsePoints)?;
                match reparse_kind {
                    ReparseKind::File => {
                        self.files = checked_counter(self.files, 1, CounterField::Files)?;
                    }
                    ReparseKind::Directory => {
                        self.directories =
                            checked_counter(self.directories, 1, CounterField::Directories)?;
                    }
                    ReparseKind::Other => {}
                }
            }
            EntryKind::SyntheticGroup => {}
        }
        Ok(())
    }

    pub(crate) fn observe_omission(&mut self) -> Result<(), ScanFailure> {
        self.omissions = checked_counter(self.omissions, 1, CounterField::Omissions)?;
        Ok(())
    }

    pub(crate) fn observe_batch(&mut self) -> Result<(), ScanFailure> {
        self.batches = checked_counter(self.batches, 1, CounterField::Batches)?;
        Ok(())
    }

    pub(crate) fn observe_directory_completion(&mut self) -> Result<(), ScanFailure> {
        self.directories_completed =
            checked_counter(self.directories_completed, 1, CounterField::DirectoriesCompleted)?;
        Ok(())
    }

    pub(crate) fn checked_add(&mut self, other: Self) -> Result<(), ScanFailure> {
        let entries = checked_counter(self.entries, other.entries, CounterField::Entries)?;
        let files = checked_counter(self.files, other.files, CounterField::Files)?;
        let directories =
            checked_counter(self.directories, other.directories, CounterField::Directories)?;
        let reparse_points = checked_counter(
            self.reparse_points,
            other.reparse_points,
            CounterField::ReparsePoints,
        )?;
        let omissions = checked_counter(self.omissions, other.omissions, CounterField::Omissions)?;
        let batches = checked_counter(self.batches, other.batches, CounterField::Batches)?;
        let directories_completed = checked_counter(
            self.directories_completed,
            other.directories_completed,
            CounterField::DirectoriesCompleted,
        )?;
        *self = Self {
            entries,
            files,
            directories,
            reparse_points,
            omissions,
            batches,
            directories_completed,
        };
        Ok(())
    }
}

fn checked_counter(left: u64, right: u64, field: CounterField) -> Result<u64, ScanFailure> {
    left.checked_add(right).ok_or(ScanFailure::CounterOverflow { field })
}

/// Partial result for one materialized directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanBatch {
    pub generation: ScanGeneration,
    pub parent: ScanNodeId,
    pub entries: Vec<ScannedEntry>,
    pub omissions: Vec<ScanOmission>,
    pub counters: ScanCounters,
}

/// Completion state of one directory enumeration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryState {
    Complete,
    Partial,
    Cancelled,
}

/// Directory-level completion event after all of its batches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryCompletion {
    pub generation: ScanGeneration,
    pub directory: ScanNodeId,
    pub counters: ScanCounters,
    pub state: DirectoryState,
}

/// Terminal state for one generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalState {
    Complete,
    Partial,
    Cancelled,
    Failed(ScanFailure),
}

/// Final result returned by join and emitted as the final event when possible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanTerminal {
    pub generation: ScanGeneration,
    pub state: TerminalState,
    pub counters: ScanCounters,
    pub peak_active_workers: usize,
    pub peak_pending_directories: usize,
}

/// Coherent application-facing event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanEvent {
    Started { generation: ScanGeneration, roots: Vec<StartedRoot> },
    Batch(ScanBatch),
    DirectoryComplete(DirectoryCompletion),
    Terminal(ScanTerminal),
}

impl ScanEvent {
    #[must_use]
    pub const fn generation(&self) -> ScanGeneration {
        match self {
            Self::Started { generation, .. } => *generation,
            Self::Batch(batch) => batch.generation,
            Self::DirectoryComplete(completion) => completion.generation,
            Self::Terminal(terminal) => terminal.generation,
        }
    }
}

/// Role involved in a thread-spawn failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadRole {
    Coordinator,
    Worker(WorkerId),
}

/// Typed fatal failure. Recoverable filesystem failures are omissions instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanFailure {
    EmptyRoots,
    InvalidOptions { field: &'static str },
    ThreadSpawn { role: ThreadRole, detail: String },
    Provider(FsError),
    WorkerPanicked { worker: WorkerId, directory: Option<ScanNodeId> },
    WorkerDisconnected { worker: WorkerId, directory: Option<ScanNodeId> },
    ResultChannelDisconnected,
    EventChannelDisconnected,
    CoordinatorPanicked,
    IdExhausted,
    CounterOverflow { field: CounterField },
    ProtocolViolation { detail: String },
}

impl fmt::Display for ScanFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoots => formatter.write_str("a scan requires at least one root"),
            Self::InvalidOptions { field } => write!(formatter, "invalid scan option {field}"),
            Self::ThreadSpawn { role, detail } => {
                write!(formatter, "could not spawn {role:?}: {detail}")
            }
            Self::Provider(error) => write!(formatter, "filesystem provider failed: {error}"),
            Self::WorkerPanicked { worker, directory } => {
                write!(formatter, "worker {worker} panicked while scanning {directory:?}")
            }
            Self::WorkerDisconnected { worker, directory } => {
                write!(formatter, "worker {worker} disconnected while scanning {directory:?}")
            }
            Self::ResultChannelDisconnected => {
                formatter.write_str("the worker result channel disconnected")
            }
            Self::EventChannelDisconnected => {
                formatter.write_str("the application event channel disconnected")
            }
            Self::CoordinatorPanicked => formatter.write_str("the scan coordinator panicked"),
            Self::IdExhausted => formatter.write_str("scan node IDs are exhausted"),
            Self::CounterOverflow { field } => {
                write!(formatter, "scan counter {field:?} overflowed")
            }
            Self::ProtocolViolation { detail } => {
                write!(formatter, "filesystem provider protocol violation: {detail}")
            }
        }
    }
}

impl Error for ScanFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error),
            _ => None,
        }
    }
}

/// Nonfatal receive status exposed by [`crate::ScanSession`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanReceiveError {
    Empty,
    Timeout,
    Disconnected,
}

impl fmt::Display for ScanReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("no scan event is currently available"),
            Self::Timeout => formatter.write_str("timed out waiting for a scan event"),
            Self::Disconnected => formatter.write_str("the scan event channel disconnected"),
        }
    }
}

impl Error for ScanReceiveError {}
