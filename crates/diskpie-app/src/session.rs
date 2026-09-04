//! UI-independent reduction of scan events into immutable core snapshots.
//!
//! This module never waits for scan work. Callers either supply an iterator of
//! already-ready events or let [`SessionReducer::drain_session`] poll
//! [`diskpie_scan::ScanSession::try_recv`]. Time is supplied by the caller, so
//! drain and snapshot budgets remain deterministic and testable.

#![forbid(unsafe_code)]

use diskpie_core::{
    EntryKind, FileIdentity, GenerationId, ModelError, NodeId, NodeSpec, OwnMetrics, ScanState,
    TreeBuilder, TreeSnapshot,
};
use diskpie_scan::{
    CounterField, DirectoryCompletion, DirectoryState, ScanBatch, ScanCounters, ScanEvent,
    ScanFailure, ScanGeneration, ScanNodeId, ScanOmission, ScanReceiveError, ScanSession,
    ScanTerminal, ScannedEntry, StartedRoot, TerminalState,
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    ffi::OsString,
    fmt,
    sync::Arc,
    time::Duration,
};

const MULTI_ROOT_SUMMARY_NAME: &str = "All volumes";

/// Reducer lifecycle independent of any widget toolkit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    AwaitingStart,
    Running,
    Complete,
    Partial,
    Cancelled,
    Failed(ScanFailure),
}

impl SessionPhase {
    /// Whether no more events may be applied to this generation.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Partial | Self::Cancelled | Self::Failed(_))
    }
}

/// Result of applying one event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    StaleGeneration,
}

/// Coherent progress derived from applied events and optional terminal totals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionProgress {
    observed: ScanCounters,
    terminal_reported: Option<ScanCounters>,
}

impl SessionProgress {
    /// Counters represented by batches and completions actually applied.
    #[must_use]
    pub const fn observed(self) -> ScanCounters {
        self.observed
    }

    /// Final counters reported by the coordinator, if a terminal arrived.
    #[must_use]
    pub const fn terminal_reported(self) -> Option<ScanCounters> {
        self.terminal_reported
    }

    /// Terminal counters when available, otherwise applied-event counters.
    #[must_use]
    pub const fn effective(self) -> ScanCounters {
        match self.terminal_reported {
            Some(counters) => counters,
            None => self.observed,
        }
    }
}

/// Pure rate-limit configuration for partial snapshot publication.
///
/// Every publication rebuilds the whole tree, so the spacing between partial
/// publications grows with the number of staged nodes:
/// `spacing = clamp(per_node_interval * node_count, min_interval, max_interval)`.
/// A small tree refreshes every `min_interval`; a tree of a million entries
/// refreshes every couple of seconds instead of spending the supervisor's
/// whole time materializing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotPolicy {
    /// Applied events required since the last publication before an
    /// event-driven publication is considered once the spacing has elapsed.
    pub min_events: u64,
    /// Smallest spacing between partial publications.
    pub min_interval: Duration,
    /// Spacing added per staged node so rebuild cost stays a bounded fraction
    /// of wall-clock time.
    pub per_node_interval: Duration,
    /// Largest spacing; a changed tree is published at least this often even
    /// when fewer than `min_events` arrived.
    pub max_interval: Duration,
}

impl SnapshotPolicy {
    /// Publishes any change immediately; used for terminal reconciliation.
    pub const IMMEDIATE: Self = Self {
        min_events: 1,
        min_interval: Duration::ZERO,
        per_node_interval: Duration::ZERO,
        max_interval: Duration::ZERO,
    };

    /// Spacing required before the next partial publication for `node_count`
    /// staged nodes. Never below `min_interval`; never above `max_interval`
    /// unless the configuration inverted the two, in which case `min_interval`
    /// wins so the clamp cannot panic.
    #[must_use]
    pub fn spacing(self, node_count: usize) -> Duration {
        let nodes = u32::try_from(node_count).unwrap_or(u32::MAX);
        let proportional = self.per_node_interval.saturating_mul(nodes);
        proportional.clamp(self.min_interval, self.max_interval.max(self.min_interval))
    }
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            min_events: 4,
            min_interval: Duration::from_millis(50),
            per_node_interval: Duration::from_micros(2),
            max_interval: Duration::from_secs(3),
        }
    }
}

/// Why a dirty reducer should or should not publish at the supplied time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotDecision {
    NoChanges,
    Deferred,
    PublishInitial,
    PublishEventThreshold,
    PublishTimeThreshold,
    PublishTerminal,
}

impl SnapshotDecision {
    #[must_use]
    pub const fn should_publish(self) -> bool {
        matches!(
            self,
            Self::PublishInitial
                | Self::PublishEventThreshold
                | Self::PublishTimeThreshold
                | Self::PublishTerminal
        )
    }
}

/// Immutable snapshot plus the selection resolved for that revision.
#[derive(Clone, Debug)]
pub struct SnapshotPublication {
    pub revision: u64,
    pub snapshot: Arc<TreeSnapshot>,
    pub selected: Option<NodeId>,
}

/// Maximum ready events and caller-clock duration consumed in one UI drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainBudget {
    pub max_events: usize,
    pub max_duration: Duration,
}

impl Default for DrainBudget {
    fn default() -> Self {
        Self { max_events: 32, max_duration: Duration::from_millis(4) }
    }
}

/// Reason a nonblocking drain returned control to the UI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainStop {
    CountLimit,
    TimeLimit,
    SourceEmpty,
    IteratorExhausted,
    Terminal,
}

/// Deterministic drain accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainReport {
    pub consumed: usize,
    pub applied: usize,
    pub stale: usize,
    pub elapsed: Duration,
    pub stop: DrainStop,
}

/// Opaque selection key transferable to a replacement generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionAnchor {
    lineage: Vec<SelectionSegment>,
}

impl SelectionAnchor {
    /// Root-to-selected lineage depth.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.lineage.len()
    }

    /// Preferred stable identity of the selected entry, when available.
    #[must_use]
    pub fn preferred_identity(&self) -> Option<FileIdentity> {
        self.lineage.last().and_then(|segment| segment.identity)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionSegment {
    name: OsString,
    kind: EntryKind,
    identity: Option<FileIdentity>,
}

#[derive(Clone, Debug)]
struct StagedNode {
    scan_id: ScanNodeId,
    core_id: NodeId,
    parent: Option<ScanNodeId>,
    name: OsString,
    kind: EntryKind,
    own_metrics: OwnMetrics,
    identity: Option<FileIdentity>,
    omissions: u64,
    local_state: ScanState,
}

/// Typed reducer/protocol failure. Invalid input never panics or partially
/// mutates an event application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    MissingStarted,
    DuplicateStarted,
    EmptyStartedRoots,
    EventAfterTerminal,
    DuplicateScanNode {
        id: ScanNodeId,
    },
    UnknownScanNode {
        id: ScanNodeId,
    },
    InvalidParent {
        parent: ScanNodeId,
        child: ScanNodeId,
    },
    ParentAlreadyComplete {
        parent: ScanNodeId,
    },
    InvalidChildKind {
        id: ScanNodeId,
        kind: EntryKind,
    },
    DuplicateDirectoryCompletion {
        id: ScanNodeId,
    },
    CounterMismatch {
        context: &'static str,
        expected: Box<ScanCounters>,
        actual: Box<ScanCounters>,
    },
    CounterOverflow {
        field: CounterField,
    },
    NodeIdExhausted,
    RevisionOverflow,
    ContradictoryTerminal {
        detail: &'static str,
    },
    TerminalCountersRegressed,
    EventSourceDisconnected,
    ClockWentBackwards,
    InvalidSelection {
        id: NodeId,
    },
    Core(ModelError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStarted => formatter.write_str("scan event arrived before Started"),
            Self::DuplicateStarted => formatter.write_str("Started was already applied"),
            Self::EmptyStartedRoots => formatter.write_str("Started contains no roots"),
            Self::EventAfterTerminal => formatter.write_str("event arrived after terminal state"),
            Self::DuplicateScanNode { id } => write!(formatter, "scan node {id} already exists"),
            Self::UnknownScanNode { id } => write!(formatter, "scan node {id} is unknown"),
            Self::InvalidParent { parent, child } => {
                write!(formatter, "node {child} has invalid parent {parent}")
            }
            Self::ParentAlreadyComplete { parent } => {
                write!(formatter, "batch arrived after directory {parent} completed")
            }
            Self::InvalidChildKind { id, kind } => {
                write!(formatter, "scan child {id} has invalid kind {kind:?}")
            }
            Self::DuplicateDirectoryCompletion { id } => {
                write!(formatter, "directory {id} completed more than once")
            }
            Self::CounterMismatch { context, expected, actual } => write!(
                formatter,
                "{context} counters differ: expected {expected:?}, got {actual:?}"
            ),
            Self::CounterOverflow { field } => write!(formatter, "counter {field:?} overflowed"),
            Self::NodeIdExhausted => formatter.write_str("the core u32 node arena is exhausted"),
            Self::RevisionOverflow => formatter.write_str("session revision overflowed"),
            Self::ContradictoryTerminal { detail } => {
                write!(formatter, "contradictory terminal state: {detail}")
            }
            Self::TerminalCountersRegressed => {
                formatter.write_str("terminal counters are lower than applied progress")
            }
            Self::EventSourceDisconnected => {
                formatter.write_str("scan event source disconnected before terminal state")
            }
            Self::ClockWentBackwards => formatter.write_str("caller clock moved backwards"),
            Self::InvalidSelection { id } => write!(formatter, "node {id} cannot be selected"),
            Self::Core(error) => write!(formatter, "core model rejected snapshot: {error}"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Core(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ModelError> for SessionError {
    fn from(error: ModelError) -> Self {
        Self::Core(error)
    }
}

/// Application-level state reducer for exactly one scan generation.
#[derive(Debug)]
pub struct SessionReducer {
    generation: ScanGeneration,
    phase: SessionPhase,
    scan_to_core: HashMap<ScanNodeId, NodeId>,
    nodes: Vec<StagedNode>,
    has_multi_root_summary: bool,
    completed_directories: HashSet<ScanNodeId>,
    directory_counters: HashMap<ScanNodeId, ScanCounters>,
    omissions: Vec<ScanOmission>,
    progress: SessionProgress,
    had_partial_data: bool,
    terminal: Option<ScanTerminal>,
    revision: u64,
    last_snapshot_revision: u64,
    last_snapshot_at: Option<Duration>,
    selection_anchor: Option<SelectionAnchor>,
    selected: Option<NodeId>,
    #[cfg(test)]
    materialization_count: std::cell::Cell<usize>,
}

impl SessionReducer {
    /// Creates an empty reducer that accepts only `generation`.
    #[must_use]
    pub fn new(generation: ScanGeneration) -> Self {
        Self {
            generation,
            phase: SessionPhase::AwaitingStart,
            scan_to_core: HashMap::new(),
            nodes: Vec::new(),
            has_multi_root_summary: false,
            completed_directories: HashSet::new(),
            directory_counters: HashMap::new(),
            omissions: Vec::new(),
            progress: SessionProgress::default(),
            had_partial_data: false,
            terminal: None,
            revision: 0,
            last_snapshot_revision: 0,
            last_snapshot_at: None,
            selection_anchor: None,
            selected: None,
            #[cfg(test)]
            materialization_count: std::cell::Cell::new(0),
        }
    }

    #[must_use]
    pub const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub const fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    #[must_use]
    pub const fn progress(&self) -> SessionProgress {
        self.progress
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn omissions(&self) -> &[ScanOmission] {
        &self.omissions
    }

    #[must_use]
    pub fn terminal(&self) -> Option<&ScanTerminal> {
        self.terminal.as_ref()
    }

    #[must_use]
    pub const fn selected_node(&self) -> Option<NodeId> {
        self.selected
    }

    #[must_use]
    pub fn selection_anchor(&self) -> Option<&SelectionAnchor> {
        self.selection_anchor.as_ref()
    }

    /// Returns the explicit protocol-to-core ID mapping.
    #[must_use]
    pub fn core_id(&self, scan_id: ScanNodeId) -> Option<NodeId> {
        self.scan_to_core.get(&scan_id).copied()
    }

    /// Applies one ready event atomically, or discards it when stale.
    pub fn apply_event(&mut self, event: ScanEvent) -> Result<ApplyOutcome, SessionError> {
        if event.generation() != self.generation {
            return Ok(ApplyOutcome::StaleGeneration);
        }
        if self.phase.is_terminal() {
            return Err(SessionError::EventAfterTerminal);
        }
        let next_revision = self.revision.checked_add(1).ok_or(SessionError::RevisionOverflow)?;

        match event {
            ScanEvent::Started { roots, .. } => self.apply_started(roots)?,
            ScanEvent::Batch(batch) => self.apply_batch(batch)?,
            ScanEvent::DirectoryComplete(completion) => {
                self.apply_directory_completion(completion)?;
            }
            ScanEvent::Terminal(terminal) => self.apply_terminal(terminal)?,
        }
        self.revision = next_revision;
        self.refresh_selection();
        Ok(ApplyOutcome::Applied)
    }

    /// Drains a nonblocking iterator of already-ready events.
    ///
    /// `clock` returns an arbitrary monotonic duration domain chosen by the
    /// caller. The reducer does not sleep or read the system clock.
    pub fn drain_ready<I, C>(
        &mut self,
        events: &mut I,
        budget: DrainBudget,
        mut clock: C,
    ) -> Result<DrainReport, SessionError>
    where
        I: Iterator<Item = ScanEvent>,
        C: FnMut() -> Duration,
    {
        let started = clock();
        let mut last = started;
        let mut report = DrainReport {
            consumed: 0,
            applied: 0,
            stale: 0,
            elapsed: Duration::ZERO,
            stop: DrainStop::IteratorExhausted,
        };
        loop {
            if report.consumed >= budget.max_events {
                report.stop = DrainStop::CountLimit;
                break;
            }
            let now = clock();
            if now < last {
                return Err(SessionError::ClockWentBackwards);
            }
            last = now;
            report.elapsed = now.checked_sub(started).ok_or(SessionError::ClockWentBackwards)?;
            if report.elapsed >= budget.max_duration {
                report.stop = DrainStop::TimeLimit;
                break;
            }
            let Some(event) = events.next() else {
                report.stop = DrainStop::IteratorExhausted;
                break;
            };
            report.consumed += 1;
            match self.apply_event(event)? {
                ApplyOutcome::Applied => report.applied += 1,
                ApplyOutcome::StaleGeneration => report.stale += 1,
            }
            if self.phase.is_terminal() {
                report.stop = DrainStop::Terminal;
                break;
            }
        }
        Ok(report)
    }

    /// Polls [`ScanSession::try_recv`] within count and caller-clock limits.
    /// This method never calls `recv`, `recv_timeout`, sleeps, or waits.
    pub fn drain_session<C>(
        &mut self,
        session: &ScanSession,
        budget: DrainBudget,
        mut clock: C,
    ) -> Result<DrainReport, SessionError>
    where
        C: FnMut() -> Duration,
    {
        let started = clock();
        let mut last = started;
        let mut report = DrainReport {
            consumed: 0,
            applied: 0,
            stale: 0,
            elapsed: Duration::ZERO,
            stop: DrainStop::SourceEmpty,
        };
        loop {
            if report.consumed >= budget.max_events {
                report.stop = DrainStop::CountLimit;
                break;
            }
            let now = clock();
            if now < last {
                return Err(SessionError::ClockWentBackwards);
            }
            last = now;
            report.elapsed = now.checked_sub(started).ok_or(SessionError::ClockWentBackwards)?;
            if report.elapsed >= budget.max_duration {
                report.stop = DrainStop::TimeLimit;
                break;
            }
            let event = match session.try_recv() {
                Ok(event) => event,
                Err(ScanReceiveError::Empty) => {
                    report.stop = DrainStop::SourceEmpty;
                    break;
                }
                Err(ScanReceiveError::Disconnected) if self.phase.is_terminal() => {
                    report.stop = DrainStop::Terminal;
                    break;
                }
                Err(ScanReceiveError::Disconnected) => {
                    return Err(SessionError::EventSourceDisconnected);
                }
                Err(ScanReceiveError::Timeout) => {
                    report.stop = DrainStop::SourceEmpty;
                    break;
                }
            };
            report.consumed += 1;
            match self.apply_event(event)? {
                ApplyOutcome::Applied => report.applied += 1,
                ApplyOutcome::StaleGeneration => report.stale += 1,
            }
            if self.phase.is_terminal() {
                report.stop = DrainStop::Terminal;
                break;
            }
        }
        Ok(report)
    }

    /// Purely decides whether a dirty snapshot should be published at `now`.
    pub fn snapshot_decision(
        &self,
        now: Duration,
        policy: SnapshotPolicy,
    ) -> Result<SnapshotDecision, SessionError> {
        if self.revision == self.last_snapshot_revision {
            return Ok(SnapshotDecision::NoChanges);
        }
        if let Some(last) = self.last_snapshot_at
            && now < last
        {
            return Err(SessionError::ClockWentBackwards);
        }
        if self.phase.is_terminal() {
            return Ok(SnapshotDecision::PublishTerminal);
        }
        let Some(last_at) = self.last_snapshot_at else {
            return Ok(SnapshotDecision::PublishInitial);
        };
        let elapsed = now.checked_sub(last_at).ok_or(SessionError::ClockWentBackwards)?;
        if elapsed < policy.spacing(self.nodes.len()) {
            return Ok(SnapshotDecision::Deferred);
        }
        let event_delta = self.revision.saturating_sub(self.last_snapshot_revision);
        if event_delta >= policy.min_events {
            return Ok(SnapshotDecision::PublishEventThreshold);
        }
        if elapsed >= policy.max_interval {
            return Ok(SnapshotDecision::PublishTimeThreshold);
        }
        Ok(SnapshotDecision::Deferred)
    }

    /// Materializes and records a publication regardless of policy.
    pub fn publish_snapshot(&mut self, now: Duration) -> Result<SnapshotPublication, SessionError> {
        self.publish_snapshot_inner(now, false)
    }

    /// Materializes the currently observed tree once with a cancelled aggregate state.
    ///
    /// This does not terminalize the reducer; the real coordinator terminal can still
    /// reconcile counters after the runtime has frozen an immediately observable frame.
    pub(crate) fn publish_cancelled_snapshot(
        &mut self,
        now: Duration,
    ) -> Result<SnapshotPublication, SessionError> {
        self.publish_snapshot_inner(now, true)
    }

    fn publish_snapshot_inner(
        &mut self,
        now: Duration,
        cancelled: bool,
    ) -> Result<SnapshotPublication, SessionError> {
        if let Some(last) = self.last_snapshot_at
            && now < last
        {
            return Err(SessionError::ClockWentBackwards);
        }
        let snapshot = Arc::new(self.materialize_snapshot_inner(cancelled)?);
        self.refresh_selection();
        self.last_snapshot_revision = self.revision;
        self.last_snapshot_at = Some(now);
        Ok(SnapshotPublication { revision: self.revision, snapshot, selected: self.selected })
    }

    /// Applies [`SnapshotPolicy`] and publishes only when its pure decision says so.
    pub fn maybe_publish_snapshot(
        &mut self,
        now: Duration,
        policy: SnapshotPolicy,
    ) -> Result<Option<SnapshotPublication>, SessionError> {
        if self.snapshot_decision(now, policy)?.should_publish() {
            self.publish_snapshot(now).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Builds a snapshot without changing rate-limit bookkeeping.
    pub fn materialize_snapshot(&self) -> Result<TreeSnapshot, SessionError> {
        self.materialize_snapshot_inner(false)
    }

    fn materialize_snapshot_inner(&self, cancelled: bool) -> Result<TreeSnapshot, SessionError> {
        #[cfg(test)]
        self.materialization_count.set(self.materialization_count.get().saturating_add(1));
        if matches!(&self.phase, SessionPhase::AwaitingStart) {
            return Err(SessionError::MissingStarted);
        }
        let mut builder = TreeBuilder::new(GenerationId::from(self.generation));
        let summary = if self.has_multi_root_summary {
            Some(builder.add_root(NodeSpec::synthetic_group(MULTI_ROOT_SUMMARY_NAME))?)
        } else {
            None
        };
        let mut materialized = HashMap::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let mut spec = NodeSpec::new(node.name.clone(), node.kind, node.own_metrics)
                .with_omissions(node.omissions)
                .with_state(node.local_state);
            if let Some(identity) = node.identity {
                spec = spec.with_file_identity(identity);
            }
            let actual = match (node.parent, summary) {
                (Some(parent), _) => {
                    let parent = materialized
                        .get(&parent)
                        .copied()
                        .ok_or(SessionError::UnknownScanNode { id: parent })?;
                    builder.add_child(parent, spec)?
                }
                (None, Some(summary)) => builder.add_child(summary, spec)?,
                (None, None) => builder.add_root(spec)?,
            };
            if actual != node.core_id {
                return Err(SessionError::ContradictoryTerminal {
                    detail: "protocol-to-core NodeId mapping changed while materializing",
                });
            }
            materialized.insert(node.scan_id, actual);
        }
        builder.set_scan_state(if cancelled {
            ScanState::Cancelled
        } else {
            match &self.phase {
                SessionPhase::Complete => ScanState::Complete,
                SessionPhase::Cancelled => ScanState::Cancelled,
                SessionPhase::AwaitingStart => return Err(SessionError::MissingStarted),
                SessionPhase::Running | SessionPhase::Partial | SessionPhase::Failed(_) => {
                    ScanState::Partial
                }
            }
        });
        builder.freeze().map_err(SessionError::Core)
    }

    /// Selects a current core node and captures its portable ancestry.
    pub fn select(&mut self, id: NodeId) -> Result<SelectionAnchor, SessionError> {
        let anchor = self.anchor_for(id)?;
        self.selected = Some(id);
        self.selection_anchor = Some(anchor.clone());
        Ok(anchor)
    }

    /// Clears current and transferable selection state.
    pub fn clear_selection(&mut self) {
        self.selected = None;
        self.selection_anchor = None;
    }

    /// Restores an anchor by exact file identity, then deepest surviving ancestor.
    pub fn restore_selection(&mut self, anchor: SelectionAnchor) -> Option<NodeId> {
        self.selection_anchor = Some(anchor);
        self.refresh_selection();
        self.selected
    }

    fn apply_started(&mut self, roots: Vec<StartedRoot>) -> Result<(), SessionError> {
        if !matches!(&self.phase, SessionPhase::AwaitingStart) {
            return Err(SessionError::DuplicateStarted);
        }
        if roots.is_empty() {
            return Err(SessionError::EmptyStartedRoots);
        }
        let has_multi_root_summary = roots.len() >= 2;
        ensure_node_id_capacity(
            self.nodes.len(),
            roots.len(),
            usize::from(has_multi_root_summary),
        )?;
        let mut seen = HashSet::with_capacity(roots.len());
        for root in &roots {
            if self.scan_to_core.contains_key(&root.id) || !seen.insert(root.id) {
                return Err(SessionError::DuplicateScanNode { id: root.id });
            }
        }
        let mut counters = self.progress.observed;
        for _ in &roots {
            observe_entry(&mut counters, EntryKind::Root)?;
        }
        self.has_multi_root_summary = has_multi_root_summary;
        for root in roots {
            self.insert_node(
                root.id,
                None,
                root.path.into_os_string(),
                EntryKind::Root,
                OwnMetrics::ZERO_BY_POLICY,
                None,
            )?;
            self.directory_counters.insert(root.id, ScanCounters::default());
        }
        self.progress.observed = counters;
        self.phase = SessionPhase::Running;
        Ok(())
    }

    fn apply_batch(&mut self, batch: ScanBatch) -> Result<(), SessionError> {
        self.require_started()?;
        let parent_core = self
            .scan_to_core
            .get(&batch.parent)
            .copied()
            .ok_or(SessionError::UnknownScanNode { id: batch.parent })?;
        if self.completed_directories.contains(&batch.parent) {
            return Err(SessionError::ParentAlreadyComplete { parent: batch.parent });
        }
        let parent_index = self
            .staged_index(parent_core)
            .ok_or(SessionError::UnknownScanNode { id: batch.parent })?;
        if !self.nodes[parent_index].kind.can_have_children() {
            return Err(SessionError::InvalidParent {
                parent: batch.parent,
                child: batch.entries.first().map_or(batch.parent, |entry| entry.id),
            });
        }
        self.ensure_capacity_for(batch.entries.len())?;
        let mut seen = HashSet::with_capacity(batch.entries.len());
        let mut expected = ScanCounters::default();
        for entry in &batch.entries {
            if entry.parent != batch.parent {
                return Err(SessionError::InvalidParent { parent: entry.parent, child: entry.id });
            }
            if matches!(entry.kind, EntryKind::Root | EntryKind::SyntheticGroup) {
                return Err(SessionError::InvalidChildKind { id: entry.id, kind: entry.kind });
            }
            if self.scan_to_core.contains_key(&entry.id) || !seen.insert(entry.id) {
                return Err(SessionError::DuplicateScanNode { id: entry.id });
            }
            observe_entry(&mut expected, entry.kind)?;
        }
        expected.omissions = usize_to_u64(batch.omissions.len(), CounterField::Omissions)?;
        expected.batches = 1;
        if expected != batch.counters {
            return Err(SessionError::CounterMismatch {
                context: "batch",
                expected: Box::new(expected),
                actual: Box::new(batch.counters),
            });
        }
        let observed = checked_add_counters(self.progress.observed, batch.counters)?;
        let directory_observed = self
            .directory_counters
            .get(&batch.parent)
            .copied()
            .ok_or(SessionError::UnknownScanNode { id: batch.parent })?;
        let directory_observed = checked_add_counters(directory_observed, batch.counters)?;
        let new_omissions = self.nodes[parent_index]
            .omissions
            .checked_add(usize_to_u64(batch.omissions.len(), CounterField::Omissions)?)
            .ok_or(SessionError::CounterOverflow { field: CounterField::Omissions })?;

        for entry in batch.entries {
            self.insert_entry(batch.parent, entry)?;
        }
        if !batch.omissions.is_empty() {
            self.nodes[parent_index].omissions = new_omissions;
            self.nodes[parent_index].local_state = ScanState::Partial;
            self.had_partial_data = true;
            self.omissions.extend(batch.omissions);
        }
        self.directory_counters.insert(batch.parent, directory_observed);
        self.progress.observed = observed;
        Ok(())
    }

    fn apply_directory_completion(
        &mut self,
        completion: DirectoryCompletion,
    ) -> Result<(), SessionError> {
        self.require_started()?;
        let core_id = self
            .scan_to_core
            .get(&completion.directory)
            .copied()
            .ok_or(SessionError::UnknownScanNode { id: completion.directory })?;
        if self.completed_directories.contains(&completion.directory) {
            return Err(SessionError::DuplicateDirectoryCompletion { id: completion.directory });
        }
        let index = self
            .staged_index(core_id)
            .ok_or(SessionError::UnknownScanNode { id: completion.directory })?;
        if !self.nodes[index].kind.can_have_children() {
            return Err(SessionError::InvalidParent {
                parent: completion.directory,
                child: completion.directory,
            });
        }
        let expected = self
            .directory_counters
            .get(&completion.directory)
            .copied()
            .ok_or(SessionError::UnknownScanNode { id: completion.directory })?;
        if completion.state != DirectoryState::Cancelled && completion.counters != expected {
            return Err(SessionError::CounterMismatch {
                context: "directory completion",
                expected: Box::new(expected),
                actual: Box::new(completion.counters),
            });
        }
        if completion.state == DirectoryState::Cancelled
            && !counters_dominate(completion.counters, expected)
        {
            return Err(SessionError::CounterMismatch {
                context: "cancelled directory completion",
                expected: Box::new(expected),
                actual: Box::new(completion.counters),
            });
        }
        let mut observed = self.progress.observed;
        observed.directories_completed =
            checked_counter(observed.directories_completed, 1, CounterField::DirectoriesCompleted)?;
        self.nodes[index].local_state = match completion.state {
            DirectoryState::Complete => self.nodes[index].local_state,
            DirectoryState::Partial => {
                self.had_partial_data = true;
                ScanState::Partial
            }
            DirectoryState::Cancelled => {
                self.had_partial_data = true;
                ScanState::Cancelled
            }
        };
        self.completed_directories.insert(completion.directory);
        self.progress.observed = observed;
        Ok(())
    }

    fn apply_terminal(&mut self, terminal: ScanTerminal) -> Result<(), SessionError> {
        self.require_started()?;
        let exact = matches!(&terminal.state, TerminalState::Complete | TerminalState::Partial);
        if exact && terminal.counters != self.progress.observed {
            return Err(SessionError::CounterMismatch {
                context: "terminal",
                expected: Box::new(self.progress.observed),
                actual: Box::new(terminal.counters),
            });
        }
        if !exact && !counters_dominate(terminal.counters, self.progress.observed) {
            return Err(SessionError::TerminalCountersRegressed);
        }
        if matches!(&terminal.state, TerminalState::Complete) {
            if self.had_partial_data || terminal.counters.omissions != 0 {
                return Err(SessionError::ContradictoryTerminal {
                    detail: "Complete followed partial data or omissions",
                });
            }
            let expected_completions =
                self.nodes.iter().filter(|node| node.kind.can_have_children()).count();
            if self.completed_directories.len() != expected_completions {
                return Err(SessionError::ContradictoryTerminal {
                    detail: "Complete left materialized directories unfinished",
                });
            }
        }
        self.phase = match &terminal.state {
            TerminalState::Complete => SessionPhase::Complete,
            TerminalState::Partial => SessionPhase::Partial,
            TerminalState::Cancelled => SessionPhase::Cancelled,
            TerminalState::Failed(failure) => SessionPhase::Failed(failure.clone()),
        };
        if !matches!(&terminal.state, TerminalState::Complete) {
            self.had_partial_data = true;
        }
        self.progress.terminal_reported = Some(terminal.counters);
        self.terminal = Some(terminal);
        Ok(())
    }

    fn insert_entry(
        &mut self,
        parent: ScanNodeId,
        entry: ScannedEntry,
    ) -> Result<NodeId, SessionError> {
        let id = self.insert_node(
            entry.id,
            Some(parent),
            entry.name,
            entry.kind,
            entry.own_metrics,
            entry.file_identity,
        )?;
        if entry.kind.can_have_children() {
            self.directory_counters.insert(entry.id, ScanCounters::default());
        }
        Ok(id)
    }

    fn insert_node(
        &mut self,
        scan_id: ScanNodeId,
        parent: Option<ScanNodeId>,
        name: OsString,
        kind: EntryKind,
        own_metrics: OwnMetrics,
        identity: Option<FileIdentity>,
    ) -> Result<NodeId, SessionError> {
        if self.scan_to_core.contains_key(&scan_id) {
            return Err(SessionError::DuplicateScanNode { id: scan_id });
        }
        if let Some(parent) = parent
            && !self.scan_to_core.contains_key(&parent)
        {
            return Err(SessionError::UnknownScanNode { id: parent });
        }
        let raw = self
            .nodes
            .len()
            .checked_add(usize::from(self.has_multi_root_summary))
            .and_then(|index| u32::try_from(index).ok())
            .ok_or(SessionError::NodeIdExhausted)?;
        let core_id = NodeId::from_raw(raw);
        self.nodes.push(StagedNode {
            scan_id,
            core_id,
            parent,
            name,
            kind,
            own_metrics,
            identity,
            omissions: 0,
            local_state: ScanState::Complete,
        });
        self.scan_to_core.insert(scan_id, core_id);
        Ok(core_id)
    }

    fn ensure_capacity_for(&self, additional: usize) -> Result<(), SessionError> {
        ensure_node_id_capacity(
            self.nodes.len(),
            additional,
            usize::from(self.has_multi_root_summary),
        )
    }

    fn require_started(&self) -> Result<(), SessionError> {
        if matches!(&self.phase, SessionPhase::AwaitingStart) {
            Err(SessionError::MissingStarted)
        } else {
            Ok(())
        }
    }

    fn anchor_for(&self, id: NodeId) -> Result<SelectionAnchor, SessionError> {
        let mut index = self.staged_index(id).ok_or(SessionError::InvalidSelection { id })?;
        let mut lineage = Vec::new();
        loop {
            let node = &self.nodes[index];
            lineage.push(SelectionSegment {
                name: node.name.clone(),
                kind: node.kind,
                identity: node.identity,
            });
            let Some(parent_scan) = node.parent else {
                break;
            };
            let parent = self
                .scan_to_core
                .get(&parent_scan)
                .copied()
                .ok_or(SessionError::UnknownScanNode { id: parent_scan })?;
            index = self
                .staged_index(parent)
                .ok_or(SessionError::UnknownScanNode { id: parent_scan })?;
        }
        lineage.reverse();
        Ok(SelectionAnchor { lineage })
    }

    fn refresh_selection(&mut self) {
        self.selected =
            self.selection_anchor.as_ref().and_then(|anchor| self.resolve_anchor(anchor));
    }

    fn resolve_anchor(&self, anchor: &SelectionAnchor) -> Option<NodeId> {
        let selected_segment = anchor.lineage.last()?;
        if let Some(identity) = selected_segment.identity
            && let Some(node) = self.nodes.iter().find(|node| node.identity == Some(identity))
        {
            return Some(node.core_id);
        }

        let mut depth = anchor.lineage.len();
        if selected_segment.identity.is_some() {
            depth = depth.saturating_sub(1);
        }
        while depth != 0 {
            let prefix = &anchor.lineage[..depth];
            if let Some(node) =
                self.nodes.iter().find(|node| self.node_matches_lineage(node.core_id, prefix))
            {
                return Some(node.core_id);
            }
            depth -= 1;
        }
        None
    }

    fn node_matches_lineage(&self, id: NodeId, expected: &[SelectionSegment]) -> bool {
        let Some(mut index) = self.staged_index(id) else {
            return false;
        };
        let mut actual = Vec::new();
        loop {
            let Some(node) = self.nodes.get(index) else {
                return false;
            };
            actual.push(node);
            let Some(parent) = node.parent else {
                break;
            };
            let Some(parent) = self.scan_to_core.get(&parent) else {
                return false;
            };
            let Some(parent_index) = self.staged_index(*parent) else {
                return false;
            };
            index = parent_index;
        }
        if actual.len() != expected.len() {
            return false;
        }
        actual.reverse();
        actual.iter().zip(expected).all(|(node, segment)| {
            if let Some(identity) = segment.identity {
                node.identity == Some(identity)
            } else {
                node.name == segment.name && node.kind == segment.kind
            }
        })
    }

    fn staged_index(&self, id: NodeId) -> Option<usize> {
        let raw = id.raw().checked_sub(u32::from(self.has_multi_root_summary))?;
        let index = raw as usize;
        self.nodes.get(index).filter(|node| node.core_id == id).map(|_| index)
    }
}

fn ensure_node_id_capacity(
    existing: usize,
    additional: usize,
    reserved: usize,
) -> Result<(), SessionError> {
    let count = existing
        .checked_add(additional)
        .and_then(|count| count.checked_add(reserved))
        .ok_or(SessionError::NodeIdExhausted)?;
    if let Some(last) = count.checked_sub(1) {
        u32::try_from(last).map_err(|_| SessionError::NodeIdExhausted)?;
    }
    Ok(())
}

fn observe_entry(counters: &mut ScanCounters, kind: EntryKind) -> Result<(), SessionError> {
    counters.entries = checked_counter(counters.entries, 1, CounterField::Entries)?;
    match kind {
        EntryKind::Root | EntryKind::Directory => {
            counters.directories =
                checked_counter(counters.directories, 1, CounterField::Directories)?;
        }
        EntryKind::File => {
            counters.files = checked_counter(counters.files, 1, CounterField::Files)?;
        }
        EntryKind::ReparsePoint(reparse) => {
            counters.reparse_points =
                checked_counter(counters.reparse_points, 1, CounterField::ReparsePoints)?;
            match reparse {
                diskpie_core::ReparseKind::File => {
                    counters.files = checked_counter(counters.files, 1, CounterField::Files)?;
                }
                diskpie_core::ReparseKind::Directory => {
                    counters.directories =
                        checked_counter(counters.directories, 1, CounterField::Directories)?;
                }
                diskpie_core::ReparseKind::Other => {}
            }
        }
        EntryKind::SyntheticGroup => {}
    }
    Ok(())
}

fn checked_add_counters(
    left: ScanCounters,
    right: ScanCounters,
) -> Result<ScanCounters, SessionError> {
    Ok(ScanCounters {
        entries: checked_counter(left.entries, right.entries, CounterField::Entries)?,
        files: checked_counter(left.files, right.files, CounterField::Files)?,
        directories: checked_counter(
            left.directories,
            right.directories,
            CounterField::Directories,
        )?,
        reparse_points: checked_counter(
            left.reparse_points,
            right.reparse_points,
            CounterField::ReparsePoints,
        )?,
        omissions: checked_counter(left.omissions, right.omissions, CounterField::Omissions)?,
        batches: checked_counter(left.batches, right.batches, CounterField::Batches)?,
        directories_completed: checked_counter(
            left.directories_completed,
            right.directories_completed,
            CounterField::DirectoriesCompleted,
        )?,
    })
}

fn checked_counter(left: u64, right: u64, field: CounterField) -> Result<u64, SessionError> {
    left.checked_add(right).ok_or(SessionError::CounterOverflow { field })
}

fn usize_to_u64(value: usize, field: CounterField) -> Result<u64, SessionError> {
    u64::try_from(value).map_err(|_| SessionError::CounterOverflow { field })
}

fn counters_dominate(left: ScanCounters, right: ScanCounters) -> bool {
    left.entries >= right.entries
        && left.files >= right.files
        && left.directories >= right.directories
        && left.reparse_points >= right.reparse_points
        && left.omissions >= right.omissions
        && left.batches >= right.batches
        && left.directories_completed >= right.directories_completed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::{NavigationAction, NavigationState};
    use diskpie_core::{MetricSource, SizeMetric, VolumeKey};
    use diskpie_scan::{FsError, FsErrorKind, FsOperation, StartedRoot, StorageClass};
    use std::{collections::VecDeque, path::PathBuf};

    fn generation(value: u64) -> ScanGeneration {
        ScanGeneration::new(value)
    }

    fn metrics(bytes: u64) -> OwnMetrics {
        OwnMetrics::new(
            SizeMetric::known(bytes, MetricSource::PortableMetadata),
            SizeMetric::known(bytes, MetricSource::FilesystemAllocation),
        )
    }

    fn started(generation: u64, roots: &[(u64, &str)]) -> ScanEvent {
        ScanEvent::Started {
            generation: self::generation(generation),
            roots: roots
                .iter()
                .map(|(id, path)| StartedRoot {
                    id: ScanNodeId::from_raw(*id),
                    path: PathBuf::from(path),
                    volume: VolumeKey::new(*id as u128 + 1),
                    storage_class: StorageClass::Unknown,
                })
                .collect(),
        }
    }

    fn entry(id: u64, parent: u64, name: &str, kind: EntryKind) -> ScannedEntry {
        ScannedEntry {
            id: ScanNodeId::from_raw(id),
            parent: ScanNodeId::from_raw(parent),
            name: OsString::from(name),
            kind,
            own_metrics: metrics(1),
            file_identity: None,
        }
    }

    fn batch_event(
        generation: u64,
        parent: u64,
        entries: Vec<ScannedEntry>,
        omissions: Vec<ScanOmission>,
    ) -> ScanEvent {
        let mut counters = ScanCounters::default();
        for entry in &entries {
            observe_entry(&mut counters, entry.kind).expect("entry counters");
        }
        counters.omissions = omissions.len() as u64;
        counters.batches = 1;
        ScanEvent::Batch(ScanBatch {
            generation: self::generation(generation),
            parent: ScanNodeId::from_raw(parent),
            entries,
            omissions,
            counters,
        })
    }

    fn completion_event(
        generation: u64,
        directory: u64,
        counters: ScanCounters,
        state: DirectoryState,
    ) -> ScanEvent {
        ScanEvent::DirectoryComplete(DirectoryCompletion {
            generation: self::generation(generation),
            directory: ScanNodeId::from_raw(directory),
            counters,
            state,
        })
    }

    fn terminal_event(generation: u64, state: TerminalState, counters: ScanCounters) -> ScanEvent {
        ScanEvent::Terminal(ScanTerminal {
            generation: self::generation(generation),
            state,
            counters,
            peak_active_workers: 1,
            peak_pending_directories: 1,
        })
    }

    fn apply(reducer: &mut SessionReducer, event: ScanEvent) {
        assert_eq!(reducer.apply_event(event).expect("apply event"), ApplyOutcome::Applied);
    }

    #[test]
    fn ordered_batches_build_explicit_mapping_and_complete_snapshot() {
        let mut reducer = SessionReducer::new(generation(1));
        apply(&mut reducer, started(1, &[(10, "root")]));
        let batch = batch_event(
            1,
            10,
            vec![entry(11, 10, "a", EntryKind::File), entry(12, 10, "b", EntryKind::File)],
            Vec::new(),
        );
        let batch_counters = match &batch {
            ScanEvent::Batch(batch) => batch.counters,
            _ => unreachable!(),
        };
        apply(&mut reducer, batch);
        apply(&mut reducer, completion_event(1, 10, batch_counters, DirectoryState::Complete));
        let counters = reducer.progress().observed();
        apply(&mut reducer, terminal_event(1, TerminalState::Complete, counters));

        assert_eq!(reducer.core_id(ScanNodeId::from_raw(10)), Some(NodeId::from_raw(0)));
        assert_eq!(reducer.core_id(ScanNodeId::from_raw(11)), Some(NodeId::from_raw(1)));
        assert_eq!(reducer.core_id(ScanNodeId::from_raw(12)), Some(NodeId::from_raw(2)));
        assert_eq!(reducer.phase(), &SessionPhase::Complete);
        let snapshot = reducer.materialize_snapshot().expect("snapshot");
        assert_eq!(snapshot.len(), 3);
        assert_eq!(
            snapshot.roots().map(|(id, _)| id).collect::<Vec<_>>(),
            vec![NodeId::from_raw(0)]
        );
        assert_eq!(snapshot.node(NodeId::from_raw(0)).expect("root").kind(), EntryKind::Root);
        assert!(snapshot.nodes().iter().all(|node| node.kind() != EntryKind::SyntheticGroup));
        let names = snapshot
            .children(NodeId::from_raw(0))
            .expect("children")
            .map(|(_, node)| node.name().to_os_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![OsString::from("a"), OsString::from("b")]);
        assert_eq!(snapshot.state(), ScanState::Complete);
    }

    #[test]
    fn two_or_more_roots_have_exactly_one_uncounted_summary() {
        let cases = [
            (20, vec![(10, "A:\\"), (20, "B:\\")]),
            (21, vec![(10, "A:\\"), (20, "B:\\"), (30, "C:\\")]),
            (22, vec![(10, "A:\\"), (20, "B:\\"), (30, "C:\\"), (40, "D:\\")]),
        ];

        for (generation, roots) in cases {
            let mut reducer = SessionReducer::new(self::generation(generation));
            apply(&mut reducer, started(generation, &roots));
            let snapshot = reducer.materialize_snapshot().expect("multi-root snapshot");
            let summary = NodeId::from_raw(0);

            assert_eq!(reducer.node_count(), roots.len());
            assert_eq!(snapshot.len(), roots.len() + 1);
            assert_eq!(snapshot.roots().map(|(id, _)| id).collect::<Vec<_>>(), vec![summary]);
            assert_eq!(snapshot.node(summary).expect("summary").kind(), EntryKind::SyntheticGroup);
            let summary_children = snapshot
                .children(summary)
                .expect("summary children")
                .map(|(id, node)| (id, node.kind(), node.name().to_os_string()))
                .collect::<Vec<_>>();
            assert_eq!(summary_children.len(), roots.len());

            for (index, (scan_id, root_path)) in roots.iter().enumerate() {
                let expected = NodeId::from_raw(u32::try_from(index + 1).expect("small test ID"));
                assert_eq!(reducer.core_id(ScanNodeId::from_raw(*scan_id)), Some(expected));
                let root = snapshot.node(expected).expect("real root");
                assert_eq!(root.kind(), EntryKind::Root);
                assert_eq!(root.parent(), Some(summary));
                assert_eq!(
                    snapshot.path(expected).expect("real root path"),
                    PathBuf::from(root_path)
                );
            }

            let observed = reducer.progress().observed();
            assert_eq!(observed.entries, roots.len() as u64);
            assert_eq!(observed.directories, roots.len() as u64);
            assert_eq!(observed.files, 0);
            let aggregate = snapshot.node(summary).expect("summary").aggregate();
            assert_eq!(aggregate.directory_count(), roots.len() as u64);
            assert_eq!(aggregate.file_count(), 0);
            assert_eq!(aggregate.omission_count(), 0);

            let navigation = NavigationState::new(snapshot).expect("valid summary navigation");
            assert_eq!(navigation.view_root(), summary);
            assert!(navigation.is_summary_view());
        }
    }

    #[test]
    fn multi_root_progression_preserves_mapping_selection_paths_and_navigation_replacement() {
        let identity = FileIdentity::new(VolumeKey::new(70), 700);
        let mut reducer = SessionReducer::new(generation(70));
        apply(&mut reducer, started(70, &[(10, "C:\\"), (20, "D:\\")]));

        let initial = Arc::new(reducer.materialize_snapshot().expect("initial snapshot"));
        let mut navigation = NavigationState::new(Arc::clone(&initial)).expect("navigation");
        assert!(navigation.is_summary_view());
        assert_eq!(navigation.view_root(), NodeId::from_raw(0));

        let mut chosen = entry(21, 20, "chosen.bin", EntryKind::File);
        chosen.file_identity = Some(identity);
        let d_batch = batch_event(70, 20, vec![chosen], Vec::new());
        let d_counters = match &d_batch {
            ScanEvent::Batch(batch) => batch.counters,
            _ => unreachable!(),
        };
        apply(&mut reducer, d_batch);
        let chosen_id = reducer.core_id(ScanNodeId::from_raw(21)).expect("chosen mapping");
        assert_eq!(chosen_id, NodeId::from_raw(3));
        let anchor = reducer.select(chosen_id).expect("selection anchor");

        let middle = Arc::new(reducer.materialize_snapshot().expect("middle snapshot"));
        navigation.replace_snapshot(Arc::clone(&middle)).expect("progressive replacement");
        let select = navigation.command(NavigationAction::Select(chosen_id));
        navigation.execute(select).expect("select real node");
        assert_eq!(navigation.selected(), Some(chosen_id));
        assert_eq!(
            middle.path(chosen_id).expect("chosen path"),
            PathBuf::from("D:\\").join("chosen.bin")
        );

        let c_batch =
            batch_event(70, 10, vec![entry(11, 10, "other.bin", EntryKind::File)], Vec::new());
        let c_counters = match &c_batch {
            ScanEvent::Batch(batch) => batch.counters,
            _ => unreachable!(),
        };
        apply(&mut reducer, c_batch);
        assert_eq!(reducer.core_id(ScanNodeId::from_raw(11)), Some(NodeId::from_raw(4)));
        assert_eq!(reducer.selected_node(), Some(chosen_id));
        apply(&mut reducer, completion_event(70, 10, c_counters, DirectoryState::Complete));
        apply(&mut reducer, completion_event(70, 20, d_counters, DirectoryState::Complete));
        let counters = reducer.progress().observed();
        apply(&mut reducer, terminal_event(70, TerminalState::Complete, counters));

        let complete = Arc::new(reducer.materialize_snapshot().expect("complete snapshot"));
        navigation.replace_snapshot(Arc::clone(&complete)).expect("complete replacement");
        assert!(navigation.is_summary_view());
        assert_eq!(navigation.selected(), Some(chosen_id));
        assert_eq!(complete.state(), ScanState::Complete);
        assert_eq!(complete.len(), reducer.node_count() + 1);
        let summary_aggregate = complete.node(NodeId::from_raw(0)).expect("summary").aggregate();
        assert_eq!(summary_aggregate.file_count(), 2);
        assert_eq!(summary_aggregate.directory_count(), 2);
        assert_eq!(counters.entries, 4);
        assert_eq!(counters.files, 2);
        assert_eq!(counters.directories, 2);

        let mut replacement = SessionReducer::new(generation(71));
        assert_eq!(replacement.restore_selection(anchor), None);
        apply(&mut replacement, started(71, &[(200, "D:\\"), (100, "C:\\")]));
        assert_eq!(replacement.selected_node(), Some(NodeId::from_raw(1)));
        let mut replacement_chosen = entry(202, 200, "chosen.bin", EntryKind::File);
        replacement_chosen.file_identity = Some(identity);
        apply(
            &mut replacement,
            batch_event(
                71,
                200,
                vec![entry(201, 200, "inserted-first.bin", EntryKind::File), replacement_chosen],
                Vec::new(),
            ),
        );
        let replacement_id = NodeId::from_raw(4);
        assert_eq!(replacement.core_id(ScanNodeId::from_raw(202)), Some(replacement_id));
        assert_eq!(replacement.selected_node(), Some(replacement_id));
        let replacement_snapshot =
            Arc::new(replacement.materialize_snapshot().expect("replacement generation snapshot"));
        navigation
            .replace_snapshot(Arc::clone(&replacement_snapshot))
            .expect("cross-generation replacement");
        assert!(navigation.is_summary_view());
        assert_eq!(navigation.selected(), Some(replacement_id));
        assert_eq!(
            replacement_snapshot.path(replacement_id).expect("replacement path"),
            PathBuf::from("D:\\").join("chosen.bin")
        );
    }

    #[test]
    fn stale_generation_is_rejected_without_mutation() {
        let mut reducer = SessionReducer::new(generation(2));
        assert_eq!(
            reducer.apply_event(started(1, &[(0, "old")])),
            Ok(ApplyOutcome::StaleGeneration)
        );
        assert_eq!(reducer.phase(), &SessionPhase::AwaitingStart);
        assert_eq!(reducer.revision(), 0);
        assert_eq!(reducer.node_count(), 0);
    }

    #[test]
    fn cancellation_preserves_partial_tree_and_terminal_progress() {
        let mut reducer = SessionReducer::new(generation(3));
        apply(&mut reducer, started(3, &[(0, "root")]));
        apply(
            &mut reducer,
            batch_event(3, 0, vec![entry(1, 0, "kept", EntryKind::File)], Vec::new()),
        );
        let counters = reducer.progress().observed();
        apply(&mut reducer, terminal_event(3, TerminalState::Cancelled, counters));

        assert_eq!(reducer.phase(), &SessionPhase::Cancelled);
        let snapshot = reducer.materialize_snapshot().expect("cancelled snapshot");
        assert_eq!(snapshot.state(), ScanState::Cancelled);
        assert_eq!(snapshot.len(), 2);
    }

    #[test]
    fn observed_cancellation_materializes_once_without_terminalizing_the_reducer() {
        let mut reducer = SessionReducer::new(generation(31));
        apply(&mut reducer, started(31, &[(0, "root")]));
        apply(
            &mut reducer,
            batch_event(31, 0, vec![entry(1, 0, "kept", EntryKind::File)], Vec::new()),
        );
        let observed = reducer.materialize_snapshot().expect("observed snapshot");
        let before = reducer.materialization_count.get();

        let cancelled = reducer
            .publish_cancelled_snapshot(Duration::from_millis(1))
            .expect("cancelled publication");
        assert_eq!(reducer.materialization_count.get(), before + 1);
        assert_eq!(cancelled.snapshot.state(), ScanState::Cancelled);
        assert_eq!(cancelled.snapshot.len(), observed.len());
        for (actual, expected) in cancelled.snapshot.nodes().iter().zip(observed.nodes()) {
            assert_eq!(actual.name(), expected.name());
            assert_eq!(actual.kind(), expected.kind());
            assert_eq!(actual.parent(), expected.parent());
            assert_eq!(actual.own_metrics(), expected.own_metrics());
            assert_eq!(actual.own_omissions(), expected.own_omissions());
            assert_eq!(actual.file_identity(), expected.file_identity());
            assert_eq!(actual.aggregate().logical(), expected.aggregate().logical());
            assert_eq!(actual.aggregate().allocated(), expected.aggregate().allocated());
            assert_eq!(actual.aggregate().file_count(), expected.aggregate().file_count());
            assert_eq!(
                actual.aggregate().directory_count(),
                expected.aggregate().directory_count()
            );
            assert_eq!(actual.aggregate().omission_count(), expected.aggregate().omission_count());
        }
        assert_eq!(reducer.phase(), &SessionPhase::Running);

        let mut real = reducer.progress().observed();
        real.entries += 2;
        real.files += 2;
        apply(&mut reducer, terminal_event(31, TerminalState::Cancelled, real));
        assert_eq!(reducer.phase(), &SessionPhase::Cancelled);
        assert_eq!(reducer.progress().terminal_reported(), Some(real));
    }

    #[test]
    fn access_denied_omission_marks_parent_and_session_partial() {
        let mut reducer = SessionReducer::new(generation(4));
        apply(&mut reducer, started(4, &[(0, "root")]));
        let omission = ScanOmission::new(FsError::new(
            "root/denied",
            FsOperation::EnumerateDirectory,
            FsErrorKind::AccessDenied,
        ));
        let batch = batch_event(4, 0, Vec::new(), vec![omission]);
        let batch_counters = match &batch {
            ScanEvent::Batch(batch) => batch.counters,
            _ => unreachable!(),
        };
        apply(&mut reducer, batch);
        apply(&mut reducer, completion_event(4, 0, batch_counters, DirectoryState::Partial));
        let counters = reducer.progress().observed();
        apply(&mut reducer, terminal_event(4, TerminalState::Partial, counters));

        assert_eq!(reducer.omissions().len(), 1);
        assert_eq!(reducer.phase(), &SessionPhase::Partial);
        let snapshot = reducer.materialize_snapshot().expect("partial snapshot");
        let root = snapshot.node(NodeId::from_raw(0)).expect("root");
        assert_eq!(root.own_omissions(), 1);
        assert_eq!(root.aggregate().state(), ScanState::Partial);
    }

    #[test]
    fn selection_prefers_file_identity_then_falls_back_to_deepest_ancestor() {
        let identity = FileIdentity::new(VolumeKey::new(9), 99);
        let mut original = SessionReducer::new(generation(5));
        apply(&mut original, started(5, &[(0, "root")]));
        apply(
            &mut original,
            batch_event(5, 0, vec![entry(1, 0, "folder", EntryKind::Directory)], Vec::new()),
        );
        let mut selected = entry(2, 1, "chosen", EntryKind::File);
        selected.file_identity = Some(identity);
        apply(&mut original, batch_event(5, 1, vec![selected], Vec::new()));
        let anchor = original.select(NodeId::from_raw(2)).expect("selection anchor");
        assert_eq!(anchor.preferred_identity(), Some(identity));

        let mut moved = SessionReducer::new(generation(6));
        moved.restore_selection(anchor.clone());
        apply(&mut moved, started(6, &[(10, "root")]));
        let mut same_identity = entry(12, 10, "renamed", EntryKind::File);
        same_identity.file_identity = Some(identity);
        apply(
            &mut moved,
            batch_event(
                6,
                10,
                vec![entry(11, 10, "other", EntryKind::File), same_identity],
                Vec::new(),
            ),
        );
        assert_eq!(moved.selected_node(), Some(NodeId::from_raw(2)));

        let mut missing = SessionReducer::new(generation(7));
        missing.restore_selection(anchor);
        apply(&mut missing, started(7, &[(20, "root")]));
        apply(
            &mut missing,
            batch_event(7, 20, vec![entry(21, 20, "folder", EntryKind::Directory)], Vec::new()),
        );
        assert_eq!(missing.selected_node(), Some(NodeId::from_raw(1)));
    }

    #[test]
    fn contradictory_complete_terminal_is_typed_error_and_not_applied() {
        let mut reducer = SessionReducer::new(generation(8));
        apply(&mut reducer, started(8, &[(0, "root")]));
        let omission = ScanOmission::new(FsError::new(
            "root/x",
            FsOperation::ReadMetadata,
            FsErrorKind::AccessDenied,
        ));
        apply(&mut reducer, batch_event(8, 0, Vec::new(), vec![omission]));
        let counters = reducer.progress().observed();
        assert!(matches!(
            reducer.apply_event(terminal_event(8, TerminalState::Complete, counters)),
            Err(SessionError::ContradictoryTerminal { .. })
        ));
        assert_eq!(reducer.phase(), &SessionPhase::Running);
        assert!(reducer.terminal().is_none());
    }

    #[test]
    fn invalid_ids_and_counter_overflow_do_not_partially_mutate() {
        let mut reducer = SessionReducer::new(generation(9));
        apply(&mut reducer, started(9, &[(0, "root")]));
        let before = reducer.node_count();
        assert!(matches!(
            reducer.apply_event(batch_event(
                9,
                999,
                vec![entry(1, 999, "orphan", EntryKind::File)],
                Vec::new(),
            )),
            Err(SessionError::UnknownScanNode { .. })
        ));
        assert_eq!(reducer.node_count(), before);

        reducer.progress.observed.entries = u64::MAX;
        let event = batch_event(9, 0, vec![entry(2, 0, "overflow", EntryKind::File)], Vec::new());
        assert_eq!(
            reducer.apply_event(event),
            Err(SessionError::CounterOverflow { field: CounterField::Entries })
        );
        assert_eq!(reducer.node_count(), before);
        assert!(reducer.core_id(ScanNodeId::from_raw(2)).is_none());
    }

    #[test]
    fn pure_snapshot_and_drain_limits_are_deterministic() {
        let mut reducer = SessionReducer::new(generation(10));
        let mut events = VecDeque::from([
            started(10, &[(0, "root")]),
            batch_event(10, 0, vec![entry(1, 0, "a", EntryKind::File)], Vec::new()),
            batch_event(10, 0, vec![entry(2, 0, "b", EntryKind::File)], Vec::new()),
        ])
        .into_iter();
        let ticks = [Duration::ZERO, Duration::ZERO, Duration::from_millis(1)];
        let mut tick = ticks.into_iter();
        let report = reducer
            .drain_ready(
                &mut events,
                DrainBudget { max_events: 1, max_duration: Duration::from_millis(5) },
                || tick.next().unwrap_or(Duration::from_millis(1)),
            )
            .expect("bounded drain");
        assert_eq!(report.consumed, 1);
        assert_eq!(report.stop, DrainStop::CountLimit);
        assert_eq!(
            reducer
                .snapshot_decision(Duration::ZERO, SnapshotPolicy::default())
                .expect("initial decision"),
            SnapshotDecision::PublishInitial
        );
        reducer.publish_snapshot(Duration::ZERO).expect("first snapshot");
        assert_eq!(
            reducer
                .snapshot_decision(Duration::from_millis(1), SnapshotPolicy::default())
                .expect("clean decision"),
            SnapshotDecision::NoChanges
        );
    }

    #[test]
    fn partial_publication_spacing_grows_with_staged_node_count() {
        let policy = SnapshotPolicy {
            min_events: 1,
            min_interval: Duration::from_millis(50),
            per_node_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(500),
        };
        assert_eq!(policy.spacing(0), Duration::from_millis(50));
        assert_eq!(policy.spacing(20), Duration::from_millis(50));
        assert_eq!(policy.spacing(201), Duration::from_millis(201));
        assert_eq!(policy.spacing(10_000), Duration::from_millis(500));
        assert_eq!(policy.spacing(usize::MAX), Duration::from_millis(500));

        let mut reducer = SessionReducer::new(generation(12));
        apply(&mut reducer, started(12, &[(0, "root")]));
        reducer.publish_snapshot(Duration::ZERO).expect("initial publication");
        apply(
            &mut reducer,
            batch_event(12, 0, vec![entry(1, 0, "a", EntryKind::File)], Vec::new()),
        );
        assert_eq!(
            reducer.snapshot_decision(Duration::from_millis(49), policy).expect("decision"),
            SnapshotDecision::Deferred,
            "two staged nodes still wait for the minimum spacing"
        );
        assert_eq!(
            reducer.snapshot_decision(Duration::from_millis(50), policy).expect("decision"),
            SnapshotDecision::PublishEventThreshold
        );
        reducer.publish_snapshot(Duration::from_millis(50)).expect("second publication");

        for id in 2..=200 {
            apply(
                &mut reducer,
                batch_event(12, 0, vec![entry(id, 0, "f", EntryKind::File)], Vec::new()),
            );
        }
        assert_eq!(reducer.node_count(), 201);
        assert_eq!(
            reducer.snapshot_decision(Duration::from_millis(50 + 200), policy).expect("decision"),
            SnapshotDecision::Deferred,
            "201 staged nodes stretch the spacing to 201 ms"
        );
        assert_eq!(
            reducer.snapshot_decision(Duration::from_millis(50 + 201), policy).expect("decision"),
            SnapshotDecision::PublishEventThreshold
        );
    }

    #[test]
    fn sparse_changes_publish_by_max_interval_and_immediate_policy_never_waits() {
        let policy = SnapshotPolicy {
            min_events: 4,
            min_interval: Duration::from_millis(50),
            per_node_interval: Duration::ZERO,
            max_interval: Duration::from_millis(400),
        };
        let mut reducer = SessionReducer::new(generation(13));
        apply(&mut reducer, started(13, &[(0, "root")]));
        reducer.publish_snapshot(Duration::ZERO).expect("initial publication");
        apply(
            &mut reducer,
            batch_event(13, 0, vec![entry(1, 0, "a", EntryKind::File)], Vec::new()),
        );

        assert_eq!(
            reducer.snapshot_decision(Duration::from_millis(60), policy).expect("decision"),
            SnapshotDecision::Deferred,
            "one event is below min_events and max_interval has not elapsed"
        );
        assert_eq!(
            reducer.snapshot_decision(Duration::from_millis(400), policy).expect("decision"),
            SnapshotDecision::PublishTimeThreshold
        );
        assert_eq!(
            reducer.snapshot_decision(Duration::ZERO, SnapshotPolicy::IMMEDIATE).expect("decision"),
            SnapshotDecision::PublishEventThreshold,
            "the immediate policy publishes any change at the same instant"
        );
    }

    #[test]
    fn ten_thousand_levels_materialize_without_recursion() {
        const DEPTH: u64 = 10_000;
        let mut reducer = SessionReducer::new(generation(11));
        apply(&mut reducer, started(11, &[(0, "root")]));
        for id in 1..=DEPTH {
            apply(
                &mut reducer,
                batch_event(
                    11,
                    id - 1,
                    vec![entry(id, id - 1, "d", EntryKind::Directory)],
                    Vec::new(),
                ),
            );
        }
        let snapshot = reducer.materialize_snapshot().expect("deep snapshot");
        assert_eq!(snapshot.len(), DEPTH as usize + 1);
        assert_eq!(
            snapshot.path(NodeId::from_raw(DEPTH as u32)).expect("deep path").components().count(),
            DEPTH as usize + 1
        );
        assert_eq!(snapshot.state(), ScanState::Partial);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn node_id_capacity_accounts_for_the_reserved_multi_root_summary() {
        let maximum_ids = u32::MAX as usize + 1;
        assert_eq!(ensure_node_id_capacity(maximum_ids - 1, 1, 0), Ok(()));
        assert_eq!(
            ensure_node_id_capacity(maximum_ids - 2, 1, 1),
            Ok(()),
            "summary plus u32::MAX real nodes exactly fills the arena"
        );
        assert_eq!(
            ensure_node_id_capacity(maximum_ids - 1, 1, 1),
            Err(SessionError::NodeIdExhausted)
        );
        assert_eq!(ensure_node_id_capacity(usize::MAX, 1, 0), Err(SessionError::NodeIdExhausted));
    }
}
