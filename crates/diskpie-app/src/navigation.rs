//! Pure, generation-aware navigation for immutable scan snapshots.
//!
//! The state in this module is independent of widgets and filesystem I/O. A
//! caller turns UI or keyboard input into a [`NavigationAction`], seals it for
//! the current snapshot with [`NavigationState::command`], inspects its
//! [`CommandAvailability`], and finally applies it with
//! [`NavigationState::execute`].

#![forbid(unsafe_code)]

use diskpie_core::{
    EntryKind, FileIdentity, GenerationId, NodeId, TreeSnapshot,
    sunburst::{HiddenBranches, SizeBasis},
};
use std::{
    collections::VecDeque,
    error::Error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Default maximum number of prior view roots retained for Back navigation.
pub const DEFAULT_HISTORY_LIMIT: usize = 128;

/// A UI-agnostic action understood by [`NavigationState`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavigationAction {
    /// Selects a real node without changing the zoom root.
    Select(NodeId),
    /// Clears the current selection.
    ClearSelection,
    /// Handles primary activation: select and zoom to a navigable real node.
    Activate(NodeId),
    /// Changes only the zoom root to a navigable real node.
    Zoom(NodeId),
    /// Returns to the most recent zoom root.
    Back,
    /// Changes the zoom root to its navigable parent.
    Parent,
    /// Chooses one real filesystem root or volume.
    ChooseRoot(NodeId),
    /// Shows the synthetic summary that owns the current multi-root view.
    ShowSummary,
    /// Hides one real branch.
    HideBranch(NodeId),
    /// Restores one explicitly hidden real branch.
    RestoreBranch(NodeId),
    /// Hides or restores one real branch.
    ToggleBranch(NodeId),
    /// Restores every explicitly hidden branch.
    RestoreAllBranches,
    /// Changes the size metric used by the chart.
    SetSizeBasis(SizeBasis),
    /// Produces a request to rescan every real filesystem root.
    RescanAll,
    /// Produces a request to rescan one real navigable branch.
    RescanBranch(NodeId),
}

impl NavigationAction {
    /// Returns the payload-free command kind used in diagnostics.
    #[must_use]
    pub const fn kind(self) -> CommandKind {
        match self {
            Self::Select(_) => CommandKind::Select,
            Self::ClearSelection => CommandKind::ClearSelection,
            Self::Activate(_) => CommandKind::Activate,
            Self::Zoom(_) => CommandKind::Zoom,
            Self::Back => CommandKind::Back,
            Self::Parent => CommandKind::Parent,
            Self::ChooseRoot(_) => CommandKind::ChooseRoot,
            Self::ShowSummary => CommandKind::ShowSummary,
            Self::HideBranch(_) => CommandKind::HideBranch,
            Self::RestoreBranch(_) => CommandKind::RestoreBranch,
            Self::ToggleBranch(_) => CommandKind::ToggleBranch,
            Self::RestoreAllBranches => CommandKind::RestoreAllBranches,
            Self::SetSizeBasis(_) => CommandKind::SetSizeBasis,
            Self::RescanAll => CommandKind::RescanAll,
            Self::RescanBranch(_) => CommandKind::RescanBranch,
        }
    }
}

/// Payload-free command identity suitable for menus, shortcuts, and errors.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandKind {
    /// Select a real node.
    Select,
    /// Clear the selection.
    ClearSelection,
    /// Select and zoom by primary activation.
    Activate,
    /// Change the zoom root.
    Zoom,
    /// Return through zoom history.
    Back,
    /// Go to the current view's parent.
    Parent,
    /// Choose a filesystem root.
    ChooseRoot,
    /// Show the multi-root summary.
    ShowSummary,
    /// Hide a real branch.
    HideBranch,
    /// Restore a real branch.
    RestoreBranch,
    /// Toggle a real branch's visibility.
    ToggleBranch,
    /// Restore all hidden branches.
    RestoreAllBranches,
    /// Change the displayed size metric.
    SetSizeBasis,
    /// Rescan every filesystem root.
    RescanAll,
    /// Rescan one filesystem branch.
    RescanBranch,
}

/// A command sealed to the immutable snapshot generation that produced it.
///
/// This prevents a delayed pointer or keyboard event from reusing a `NodeId`
/// after a newer generation replaces the snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavigationCommand {
    generation: GenerationId,
    action: NavigationAction,
}

impl NavigationCommand {
    /// Creates a command for an explicit generation.
    ///
    /// Most callers should prefer [`NavigationState::command`].
    #[must_use]
    pub const fn new(generation: GenerationId, action: NavigationAction) -> Self {
        Self { generation, action }
    }

    /// Returns the generation that supplied every `NodeId` in this command.
    #[must_use]
    pub const fn generation(self) -> GenerationId {
        self.generation
    }

    /// Returns the typed action carried by this command.
    #[must_use]
    pub const fn action(self) -> NavigationAction {
        self.action
    }

    /// Returns the payload-free command identity.
    #[must_use]
    pub const fn kind(self) -> CommandKind {
        self.action.kind()
    }
}

/// Why a command cannot currently be invoked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    /// The command was created from a different snapshot generation.
    StaleGeneration {
        /// Generation currently owned by the navigation state.
        current: GenerationId,
        /// Generation carried by the command.
        command: GenerationId,
    },
    /// The node ID does not exist in the current snapshot.
    UnknownNode {
        /// Unknown generation-local ID.
        node: NodeId,
    },
    /// Synthetic presentation nodes cannot be action targets.
    SyntheticTarget {
        /// Rejected synthetic node.
        node: NodeId,
    },
    /// The node exists but cannot become a zoom or branch-rescan root.
    NotNavigable {
        /// Rejected node.
        node: NodeId,
        /// Domain kind that made the node non-navigable.
        kind: EntryKind,
    },
    /// Root selection accepts only a real [`EntryKind::Root`].
    NotFilesystemRoot {
        /// Rejected node.
        node: NodeId,
        /// Actual kind of the node.
        kind: EntryKind,
    },
    /// Back history is empty.
    NoHistory,
    /// The current view has no navigable parent.
    NoParent,
    /// No valid multi-root synthetic summary exists for the current view.
    NoSummary,
    /// No node is selected.
    NoSelection,
    /// The target is a filesystem root, which may never be hidden.
    FilesystemRootCannotBeHidden {
        /// Rejected filesystem root.
        node: NodeId,
    },
    /// Hiding this node would hide the current view root or one of its ancestors.
    ViewRootCannotBeHidden {
        /// Rejected branch.
        node: NodeId,
    },
    /// The branch is already explicitly hidden.
    AlreadyHidden {
        /// Already hidden branch.
        node: NodeId,
    },
    /// The branch is not explicitly hidden.
    NotHidden {
        /// Visible branch.
        node: NodeId,
    },
    /// No hidden branches exist to restore.
    NoHiddenBranches,
    /// A hidden branch or one of its ancestors cannot be activated or zoomed.
    HiddenTarget {
        /// Rejected hidden node or descendant.
        node: NodeId,
    },
    /// The snapshot contains no real filesystem roots to rescan.
    NoRescanRoots,
    /// A model node did not reconstruct to a nonempty real path.
    NoRealPath {
        /// Node without a usable path.
        node: NodeId,
    },
}

/// Whether a command should be enabled by a UI or keyboard dispatcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandAvailability {
    /// The command is valid for the current state.
    Available,
    /// The command is invalid, with a stable typed explanation.
    Unavailable(UnavailableReason),
}

impl CommandAvailability {
    /// Returns whether the command may be executed.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    /// Returns the reason a command is unavailable.
    #[must_use]
    pub const fn reason(self) -> Option<UnavailableReason> {
        match self {
            Self::Available => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// One validated real filesystem target for a rescan request.
///
/// Fields are private so application adapters cannot accidentally construct a
/// target from a synthetic summary or grouping sector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RescanTarget {
    generation: GenerationId,
    node: NodeId,
    path: PathBuf,
    identity: Option<FileIdentity>,
}

impl RescanTarget {
    /// Returns the snapshot generation in which the target was validated.
    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        self.generation
    }

    /// Returns the validated real node ID.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// Returns the exact native path reconstructed by the core model.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the filesystem identity captured with the target, when known.
    #[must_use]
    pub const fn identity(&self) -> Option<FileIdentity> {
        self.identity
    }
}

/// Typed work emitted for a scanner adapter; constructing it performs no I/O.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RescanRequest {
    /// Rescan all real filesystem roots in deterministic `NodeId` order.
    Full {
        /// Generation whose roots are being replaced.
        generation: GenerationId,
        /// Validated, non-synthetic root paths.
        roots: Box<[RescanTarget]>,
    },
    /// Rescan one real root or directory.
    Branch(RescanTarget),
}

impl RescanRequest {
    /// Returns the snapshot generation from which this request was created.
    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        match self {
            Self::Full { generation, .. } => *generation,
            Self::Branch(target) => target.generation,
        }
    }

    /// Returns all targets for a full rescan, or an empty slice for a branch request.
    #[must_use]
    pub fn full_roots(&self) -> &[RescanTarget] {
        match self {
            Self::Full { roots, .. } => roots,
            Self::Branch(_) => &[],
        }
    }

    /// Returns the target for a branch rescan.
    #[must_use]
    pub const fn branch_target(&self) -> Option<&RescanTarget> {
        match self {
            Self::Full { .. } => None,
            Self::Branch(target) => Some(target),
        }
    }
}

/// State domain changed by a successfully applied command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationChange {
    /// Only the selected node changed.
    Selection,
    /// Only the zoom root (and possibly bounded history) changed.
    ViewRoot,
    /// Primary activation changed both selection and zoom root.
    SelectionAndViewRoot,
    /// The explicit hidden-branch set changed.
    HiddenBranches,
    /// The displayed size basis changed.
    SizeBasis,
}

/// Result of executing one available command.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandOutcome {
    /// The command changed one state domain.
    Applied(NavigationChange),
    /// The valid command already described the current state.
    Unchanged,
    /// The pure state produced typed scanner work for an outer adapter.
    RescanRequested(RescanRequest),
}

/// Summary of identity-preserving snapshot replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplacementReport {
    /// New snapshot generation.
    pub generation: GenerationId,
    /// Resolved zoom root in the new snapshot.
    pub view_root: NodeId,
    /// Resolved selection, if any.
    pub selected: Option<NodeId>,
    /// Number of retained Back entries.
    pub history_len: usize,
    /// Number of retained hidden branches.
    pub hidden_len: usize,
}

/// Typed failure from construction, replacement, or command execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationError {
    /// The snapshot has no real root or valid multi-root summary to display.
    NoNavigableRoot,
    /// A replacement generation is older than the currently displayed one.
    StaleSnapshot {
        /// Current generation.
        current: GenerationId,
        /// Rejected replacement generation.
        replacement: GenerationId,
    },
    /// A caller executed a command that availability had rejected.
    CommandUnavailable {
        /// Command identity.
        command: CommandKind,
        /// Stable reason for rejection.
        reason: UnavailableReason,
    },
    /// A supposedly real node could not be converted to a usable native path.
    InvalidRealPath {
        /// Invalid target node.
        node: NodeId,
    },
}

impl fmt::Display for NavigationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoNavigableRoot => {
                formatter.write_str("snapshot has no navigable filesystem root")
            }
            Self::StaleSnapshot { current, replacement } => write!(
                formatter,
                "snapshot generation {} is older than current generation {}",
                replacement.get(),
                current.get()
            ),
            Self::CommandUnavailable { command, reason } => {
                write!(formatter, "command {command:?} is unavailable: {reason:?}")
            }
            Self::InvalidRealPath { node } => {
                write!(formatter, "node {node} has no usable real filesystem path")
            }
        }
    }
}

impl Error for NavigationError {}

/// Pure navigation state for one immutable snapshot revision.
#[derive(Clone, Debug)]
pub struct NavigationState {
    snapshot: Arc<TreeSnapshot>,
    view_root: NodeId,
    selected: Option<NodeId>,
    history: VecDeque<NodeId>,
    history_limit: usize,
    hidden: HiddenBranches,
    size_basis: SizeBasis,
}

impl NavigationState {
    /// Creates navigation with [`DEFAULT_HISTORY_LIMIT`] and logical sizes.
    pub fn new(snapshot: impl Into<Arc<TreeSnapshot>>) -> Result<Self, NavigationError> {
        Self::with_history_limit(snapshot, DEFAULT_HISTORY_LIMIT)
    }

    /// Creates navigation with an explicit bounded Back-history capacity.
    ///
    /// A zero limit is valid and disables Back history deterministically.
    pub fn with_history_limit(
        snapshot: impl Into<Arc<TreeSnapshot>>,
        history_limit: usize,
    ) -> Result<Self, NavigationError> {
        let snapshot = snapshot.into();
        let view_root = default_view_root(&snapshot).ok_or(NavigationError::NoNavigableRoot)?;
        Ok(Self {
            snapshot,
            view_root,
            selected: None,
            history: VecDeque::with_capacity(history_limit.min(DEFAULT_HISTORY_LIMIT)),
            history_limit,
            hidden: HiddenBranches::new(),
            size_basis: SizeBasis::Logical,
        })
    }

    /// Returns the current immutable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> &TreeSnapshot {
        &self.snapshot
    }

    /// Clones the cheaply shared snapshot handle.
    #[must_use]
    pub fn snapshot_handle(&self) -> Arc<TreeSnapshot> {
        Arc::clone(&self.snapshot)
    }

    /// Returns the current snapshot generation.
    #[must_use]
    pub fn generation(&self) -> GenerationId {
        self.snapshot.generation()
    }

    /// Returns the node displayed as the chart's zoom root.
    ///
    /// This may be a validated synthetic multi-root summary. It is deliberately
    /// independent from [`Self::selected`].
    #[must_use]
    pub const fn view_root(&self) -> NodeId {
        self.view_root
    }

    /// Returns whether the current view is the synthetic multi-root summary.
    #[must_use]
    pub fn is_summary_view(&self) -> bool {
        is_summary_node(&self.snapshot, self.view_root)
    }

    /// Returns the independently selected real node.
    #[must_use]
    pub const fn selected(&self) -> Option<NodeId> {
        self.selected
    }

    /// Returns the metric basis consumed by a sunburst layout adapter.
    #[must_use]
    pub const fn size_basis(&self) -> SizeBasis {
        self.size_basis
    }

    /// Returns the immutable hidden-branch set consumed by the sunburst engine.
    #[must_use]
    pub const fn hidden_branches(&self) -> &HiddenBranches {
        &self.hidden
    }

    /// Returns the configured maximum number of Back entries.
    #[must_use]
    pub const fn history_limit(&self) -> usize {
        self.history_limit
    }

    /// Returns the current number of Back entries.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Iterates Back history from oldest to newest.
    pub fn history(&self) -> impl ExactSizeIterator<Item = NodeId> + DoubleEndedIterator + '_ {
        self.history.iter().copied()
    }

    /// Seals an action to the current generation.
    #[must_use]
    pub fn command(&self, action: NavigationAction) -> NavigationCommand {
        NavigationCommand::new(self.generation(), action)
    }

    /// Computes whether a menu item or keyboard action is currently valid.
    #[must_use]
    pub fn availability(&self, command: NavigationCommand) -> CommandAvailability {
        match self.unavailable_reason(command) {
            Some(reason) => CommandAvailability::Unavailable(reason),
            None => CommandAvailability::Available,
        }
    }

    /// Executes one typed command without performing I/O.
    pub fn execute(
        &mut self,
        command: NavigationCommand,
    ) -> Result<CommandOutcome, NavigationError> {
        if let Some(reason) = self.unavailable_reason(command) {
            return Err(NavigationError::CommandUnavailable { command: command.kind(), reason });
        }

        let outcome = match command.action {
            NavigationAction::Select(node) => {
                if self.selected == Some(node) {
                    CommandOutcome::Unchanged
                } else {
                    self.selected = Some(node);
                    CommandOutcome::Applied(NavigationChange::Selection)
                }
            }
            NavigationAction::ClearSelection => {
                self.selected = None;
                CommandOutcome::Applied(NavigationChange::Selection)
            }
            NavigationAction::Activate(node) => {
                let selection_changed = self.selected != Some(node);
                let root_changed = self.change_view(node);
                self.selected = Some(node);
                match (selection_changed, root_changed) {
                    (false, false) => CommandOutcome::Unchanged,
                    (true, false) => CommandOutcome::Applied(NavigationChange::Selection),
                    (false, true) => CommandOutcome::Applied(NavigationChange::ViewRoot),
                    (true, true) => CommandOutcome::Applied(NavigationChange::SelectionAndViewRoot),
                }
            }
            NavigationAction::Zoom(node) | NavigationAction::ChooseRoot(node) => {
                if self.change_view(node) {
                    CommandOutcome::Applied(NavigationChange::ViewRoot)
                } else {
                    CommandOutcome::Unchanged
                }
            }
            NavigationAction::Back => {
                let previous =
                    self.history.pop_back().ok_or(NavigationError::CommandUnavailable {
                        command: CommandKind::Back,
                        reason: UnavailableReason::NoHistory,
                    })?;
                self.view_root = previous;
                CommandOutcome::Applied(NavigationChange::ViewRoot)
            }
            NavigationAction::Parent => {
                let parent = self.parent_view().ok_or(NavigationError::CommandUnavailable {
                    command: CommandKind::Parent,
                    reason: UnavailableReason::NoParent,
                })?;
                self.change_view(parent);
                CommandOutcome::Applied(NavigationChange::ViewRoot)
            }
            NavigationAction::ShowSummary => {
                let summary =
                    self.summary_for_current_view().ok_or(NavigationError::CommandUnavailable {
                        command: CommandKind::ShowSummary,
                        reason: UnavailableReason::NoSummary,
                    })?;
                if self.change_view(summary) {
                    CommandOutcome::Applied(NavigationChange::ViewRoot)
                } else {
                    CommandOutcome::Unchanged
                }
            }
            NavigationAction::HideBranch(node) => {
                self.hidden.hide(node);
                CommandOutcome::Applied(NavigationChange::HiddenBranches)
            }
            NavigationAction::RestoreBranch(node) => {
                self.hidden.restore(node);
                CommandOutcome::Applied(NavigationChange::HiddenBranches)
            }
            NavigationAction::ToggleBranch(node) => {
                self.hidden.toggle(node);
                CommandOutcome::Applied(NavigationChange::HiddenBranches)
            }
            NavigationAction::RestoreAllBranches => {
                self.hidden.clear();
                CommandOutcome::Applied(NavigationChange::HiddenBranches)
            }
            NavigationAction::SetSizeBasis(size_basis) => {
                if self.size_basis == size_basis {
                    CommandOutcome::Unchanged
                } else {
                    self.size_basis = size_basis;
                    CommandOutcome::Applied(NavigationChange::SizeBasis)
                }
            }
            NavigationAction::RescanAll => {
                CommandOutcome::RescanRequested(self.full_rescan_request()?)
            }
            NavigationAction::RescanBranch(node) => {
                CommandOutcome::RescanRequested(RescanRequest::Branch(self.rescan_target(node)?))
            }
        };
        Ok(outcome)
    }

    /// Replaces the immutable snapshot while preserving semantic navigation.
    ///
    /// Current root, selection, history, and hidden branches are resolved by
    /// `FileIdentity` first and exact native path second. Root, selection, and
    /// history then fall back to the deepest surviving real ancestor. Hidden
    /// branches do not use ancestor fallback, because hiding a deleted child
    /// must never unexpectedly hide its parent. Metric choice is unchanged.
    /// Same-generation replacements support progressive snapshots; older
    /// generations are rejected atomically.
    pub fn replace_snapshot(
        &mut self,
        replacement: impl Into<Arc<TreeSnapshot>>,
    ) -> Result<ReplacementReport, NavigationError> {
        let replacement = replacement.into();
        let current_generation = self.generation();
        let replacement_generation = replacement.generation();
        if replacement_generation < current_generation {
            return Err(NavigationError::StaleSnapshot {
                current: current_generation,
                replacement: replacement_generation,
            });
        }
        let default_root =
            default_view_root(&replacement).ok_or(NavigationError::NoNavigableRoot)?;

        let root_anchor = Anchor::capture(&self.snapshot, self.view_root);
        let selection_anchor = self.selected.and_then(|node| Anchor::capture(&self.snapshot, node));
        let history_anchors = self
            .history
            .iter()
            .filter_map(|node| Anchor::capture(&self.snapshot, *node))
            .collect::<Vec<_>>();
        let hidden_anchors = self
            .hidden
            .iter()
            .filter_map(|node| Anchor::capture(&self.snapshot, node))
            .collect::<Vec<_>>();

        let view_root = root_anchor
            .as_ref()
            .and_then(|anchor| {
                anchor.resolve_with_ancestors(&replacement, |snapshot, node| {
                    is_view_node(snapshot, node)
                })
            })
            .unwrap_or(default_root);
        let selected = selection_anchor
            .as_ref()
            .and_then(|anchor| anchor.resolve_with_ancestors(&replacement, is_real_node));

        let mut history = VecDeque::new();
        for anchor in &history_anchors {
            let Some(node) = anchor.resolve_with_ancestors(&replacement, |snapshot, node| {
                is_view_node(snapshot, node)
            }) else {
                continue;
            };
            if history.back().copied() != Some(node) {
                push_bounded(&mut history, self.history_limit, node);
            }
        }
        while history.back().copied() == Some(view_root) {
            history.pop_back();
        }

        let hidden = hidden_anchors
            .iter()
            .filter_map(|anchor| {
                anchor.resolve_exact(&replacement, |snapshot, node| {
                    is_hideable_kind(snapshot, node)
                        && !is_ancestor_or_same(snapshot, node, view_root)
                })
            })
            .collect::<HiddenBranches>();

        self.snapshot = replacement;
        self.view_root = view_root;
        self.selected = selected;
        self.history = history;
        self.hidden = hidden;

        Ok(ReplacementReport {
            generation: replacement_generation,
            view_root,
            selected,
            history_len: self.history.len(),
            hidden_len: self.hidden.len(),
        })
    }

    fn unavailable_reason(&self, command: NavigationCommand) -> Option<UnavailableReason> {
        if command.generation != self.generation() {
            return Some(UnavailableReason::StaleGeneration {
                current: self.generation(),
                command: command.generation,
            });
        }

        match command.action {
            NavigationAction::Select(node) => self.real_target_reason(node),
            NavigationAction::ClearSelection => {
                self.selected.is_none().then_some(UnavailableReason::NoSelection)
            }
            NavigationAction::Activate(node) | NavigationAction::Zoom(node) => self
                .real_target_reason(node)
                .or_else(|| self.navigable_reason(node))
                .or_else(|| {
                    self.is_effectively_hidden(node)
                        .then_some(UnavailableReason::HiddenTarget { node })
                }),
            NavigationAction::Back => {
                self.history.is_empty().then_some(UnavailableReason::NoHistory)
            }
            NavigationAction::Parent => {
                self.parent_view().is_none().then_some(UnavailableReason::NoParent)
            }
            NavigationAction::ChooseRoot(node) => {
                self.real_target_reason(node).or_else(|| self.filesystem_root_reason(node))
            }
            NavigationAction::ShowSummary => {
                self.summary_for_current_view().is_none().then_some(UnavailableReason::NoSummary)
            }
            NavigationAction::HideBranch(node) => self.hide_reason(node),
            NavigationAction::RestoreBranch(node) => self.real_target_reason(node).or_else(|| {
                (!self.hidden.contains(node)).then_some(UnavailableReason::NotHidden { node })
            }),
            NavigationAction::ToggleBranch(node) => {
                if self.hidden.contains(node) {
                    self.real_target_reason(node)
                } else {
                    self.hide_reason(node)
                }
            }
            NavigationAction::RestoreAllBranches => {
                self.hidden.is_empty().then_some(UnavailableReason::NoHiddenBranches)
            }
            NavigationAction::SetSizeBasis(_) => None,
            NavigationAction::RescanAll => self.full_rescan_unavailable_reason(),
            NavigationAction::RescanBranch(node) => self
                .real_target_reason(node)
                .or_else(|| self.navigable_reason(node))
                .or_else(|| self.real_path_reason(node)),
        }
    }

    fn real_target_reason(&self, node: NodeId) -> Option<UnavailableReason> {
        let record = match self.snapshot.node(node) {
            Some(record) => record,
            None => return Some(UnavailableReason::UnknownNode { node }),
        };
        (record.kind() == EntryKind::SyntheticGroup)
            .then_some(UnavailableReason::SyntheticTarget { node })
    }

    fn navigable_reason(&self, node: NodeId) -> Option<UnavailableReason> {
        let record = self.snapshot.node(node)?;
        (!is_navigable_kind(record.kind()))
            .then_some(UnavailableReason::NotNavigable { node, kind: record.kind() })
    }

    fn filesystem_root_reason(&self, node: NodeId) -> Option<UnavailableReason> {
        let record = self.snapshot.node(node)?;
        (record.kind() != EntryKind::Root)
            .then_some(UnavailableReason::NotFilesystemRoot { node, kind: record.kind() })
    }

    fn hide_reason(&self, node: NodeId) -> Option<UnavailableReason> {
        if let Some(reason) = self.real_target_reason(node) {
            return Some(reason);
        }
        let record = self.snapshot.node(node)?;
        if record.kind() == EntryKind::Root {
            return Some(UnavailableReason::FilesystemRootCannotBeHidden { node });
        }
        if is_ancestor_or_same(&self.snapshot, node, self.view_root) {
            return Some(UnavailableReason::ViewRootCannotBeHidden { node });
        }
        self.hidden.contains(node).then_some(UnavailableReason::AlreadyHidden { node })
    }

    fn real_path_reason(&self, node: NodeId) -> Option<UnavailableReason> {
        self.snapshot
            .path(node)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .is_none()
            .then_some(UnavailableReason::NoRealPath { node })
    }

    fn full_rescan_unavailable_reason(&self) -> Option<UnavailableReason> {
        let mut found = false;
        for node in filesystem_roots(&self.snapshot) {
            found = true;
            if let Some(reason) = self.real_path_reason(node) {
                return Some(reason);
            }
        }
        (!found).then_some(UnavailableReason::NoRescanRoots)
    }

    fn parent_view(&self) -> Option<NodeId> {
        let parent = self.snapshot.node(self.view_root)?.parent()?;
        is_view_node(&self.snapshot, parent).then_some(parent)
    }

    fn summary_for_current_view(&self) -> Option<NodeId> {
        let mut cursor = self.view_root;
        loop {
            if is_summary_node(&self.snapshot, cursor) {
                return Some(cursor);
            }
            cursor = self.snapshot.node(cursor)?.parent()?;
        }
    }

    fn is_effectively_hidden(&self, node: NodeId) -> bool {
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if self.hidden.contains(current) {
                return true;
            }
            cursor = self.snapshot.node(current).and_then(|record| record.parent());
        }
        false
    }

    fn change_view(&mut self, next: NodeId) -> bool {
        if self.view_root == next {
            return false;
        }
        push_bounded(&mut self.history, self.history_limit, self.view_root);
        self.view_root = next;
        true
    }

    fn rescan_target(&self, node: NodeId) -> Result<RescanTarget, NavigationError> {
        let record = self.snapshot.node(node).ok_or(NavigationError::InvalidRealPath { node })?;
        let path =
            self.snapshot.path(node).map_err(|_| NavigationError::InvalidRealPath { node })?;
        if path.as_os_str().is_empty() || record.kind() == EntryKind::SyntheticGroup {
            return Err(NavigationError::InvalidRealPath { node });
        }
        Ok(RescanTarget {
            generation: self.generation(),
            node,
            path,
            identity: record.file_identity(),
        })
    }

    fn full_rescan_request(&self) -> Result<RescanRequest, NavigationError> {
        let roots = filesystem_roots(&self.snapshot)
            .map(|node| self.rescan_target(node))
            .collect::<Result<Vec<_>, _>>()?;
        if roots.is_empty() {
            return Err(NavigationError::CommandUnavailable {
                command: CommandKind::RescanAll,
                reason: UnavailableReason::NoRescanRoots,
            });
        }
        Ok(RescanRequest::Full { generation: self.generation(), roots: roots.into_boxed_slice() })
    }
}

fn push_bounded(history: &mut VecDeque<NodeId>, limit: usize, node: NodeId) {
    if limit == 0 {
        return;
    }
    while history.len() >= limit {
        history.pop_front();
    }
    history.push_back(node);
}

fn is_navigable_kind(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::Root | EntryKind::Directory)
}

fn is_real_node(snapshot: &TreeSnapshot, node: NodeId) -> bool {
    snapshot.node(node).is_some_and(|record| record.kind() != EntryKind::SyntheticGroup)
}

fn is_hideable_kind(snapshot: &TreeSnapshot, node: NodeId) -> bool {
    snapshot
        .node(node)
        .is_some_and(|record| !matches!(record.kind(), EntryKind::Root | EntryKind::SyntheticGroup))
}

fn is_view_node(snapshot: &TreeSnapshot, node: NodeId) -> bool {
    snapshot.node(node).is_some_and(|record| is_navigable_kind(record.kind()))
        || is_summary_node(snapshot, node)
}

fn is_summary_node(snapshot: &TreeSnapshot, node: NodeId) -> bool {
    let Some(record) = snapshot.node(node) else {
        return false;
    };
    if record.kind() != EntryKind::SyntheticGroup || record.parent().is_some() {
        return false;
    }
    let Ok(children) = snapshot.children(node) else {
        return false;
    };
    let mut roots = 0_usize;
    for (_, child) in children {
        if child.kind() != EntryKind::Root {
            return false;
        }
        roots += 1;
    }
    roots >= 2
}

fn default_view_root(snapshot: &TreeSnapshot) -> Option<NodeId> {
    snapshot
        .roots()
        .map(|(node, _)| node)
        .find(|node| is_summary_node(snapshot, *node))
        .or_else(|| filesystem_roots(snapshot).next())
}

fn filesystem_roots(snapshot: &TreeSnapshot) -> impl Iterator<Item = NodeId> + '_ {
    snapshot.nodes().iter().enumerate().filter_map(|(index, record)| {
        (record.kind() == EntryKind::Root)
            .then(|| u32::try_from(index).ok().map(NodeId::from_raw))
            .flatten()
    })
}

fn is_ancestor_or_same(snapshot: &TreeSnapshot, ancestor: NodeId, node: NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = snapshot.node(current).and_then(|record| record.parent());
    }
    false
}

#[derive(Clone, Debug)]
enum Anchor {
    Summary,
    Real(Vec<AnchorSegment>),
}

impl Anchor {
    fn capture(snapshot: &TreeSnapshot, node: NodeId) -> Option<Self> {
        if is_summary_node(snapshot, node) {
            return Some(Self::Summary);
        }
        let mut cursor = node;
        let mut lineage = Vec::new();
        loop {
            let record = snapshot.node(cursor)?;
            if record.kind() == EntryKind::SyntheticGroup {
                return None;
            }
            lineage.push(AnchorSegment {
                name: record.name().to_os_string(),
                kind: record.kind(),
                identity: record.file_identity(),
            });
            if record.kind() == EntryKind::Root {
                break;
            }
            cursor = record.parent()?;
        }
        lineage.reverse();
        Some(Self::Real(lineage))
    }

    fn resolve_exact<F>(&self, snapshot: &TreeSnapshot, mut acceptable: F) -> Option<NodeId>
    where
        F: FnMut(&TreeSnapshot, NodeId) -> bool,
    {
        match self {
            Self::Summary => default_summary(snapshot).filter(|node| acceptable(snapshot, *node)),
            Self::Real(lineage) => {
                let matched = match_lineage(snapshot, lineage);
                let target = lineage.last()?;
                let preferred =
                    matched.get(lineage.len().saturating_sub(1)).copied().filter(|node| {
                        snapshot.node(*node).and_then(|record| record.file_identity())
                            == target.identity
                    });
                if let Some(identity) = target.identity
                    && let Some(node) =
                        find_identity(snapshot, identity, preferred, &mut acceptable)
                {
                    return Some(node);
                }
                matched
                    .get(lineage.len().saturating_sub(1))
                    .copied()
                    .filter(|node| acceptable(snapshot, *node))
            }
        }
    }

    fn resolve_with_ancestors<F>(
        &self,
        snapshot: &TreeSnapshot,
        mut acceptable: F,
    ) -> Option<NodeId>
    where
        F: FnMut(&TreeSnapshot, NodeId) -> bool,
    {
        match self {
            Self::Summary => default_summary(snapshot).filter(|node| acceptable(snapshot, *node)),
            Self::Real(lineage) => {
                let matched = match_lineage(snapshot, lineage);
                let target_index = lineage.len().checked_sub(1)?;
                let target = &lineage[target_index];
                let preferred = matched.get(target_index).copied().filter(|node| {
                    snapshot.node(*node).and_then(|record| record.file_identity())
                        == target.identity
                });
                if let Some(identity) = target.identity
                    && let Some(node) =
                        find_identity(snapshot, identity, preferred, &mut acceptable)
                {
                    return Some(node);
                }
                if let Some(node) = matched.get(target_index).copied()
                    && acceptable(snapshot, node)
                {
                    return Some(node);
                }

                for index in (0..target_index).rev() {
                    let segment = &lineage[index];
                    let preferred = matched.get(index).copied().filter(|node| {
                        snapshot.node(*node).and_then(|record| record.file_identity())
                            == segment.identity
                    });
                    if let Some(identity) = segment.identity
                        && let Some(node) =
                            find_identity(snapshot, identity, preferred, &mut acceptable)
                    {
                        return Some(node);
                    }
                    if let Some(node) = matched.get(index).copied()
                        && acceptable(snapshot, node)
                    {
                        return Some(node);
                    }
                }
                None
            }
        }
    }
}

#[derive(Clone, Debug)]
struct AnchorSegment {
    name: OsString,
    kind: EntryKind,
    identity: Option<FileIdentity>,
}

fn default_summary(snapshot: &TreeSnapshot) -> Option<NodeId> {
    snapshot.roots().map(|(node, _)| node).find(|node| is_summary_node(snapshot, *node))
}

fn match_lineage(snapshot: &TreeSnapshot, lineage: &[AnchorSegment]) -> Vec<NodeId> {
    let Some(first) = lineage.first() else {
        return Vec::new();
    };
    let Some(root) = filesystem_roots(snapshot).find(|node| {
        snapshot.node(*node).is_some_and(|record| {
            record.kind() == first.kind && record.name() == first.name.as_os_str()
        })
    }) else {
        return Vec::new();
    };

    let mut matched = Vec::with_capacity(lineage.len());
    matched.push(root);
    let mut parent = root;
    for segment in &lineage[1..] {
        let Ok(children) = snapshot.children(parent) else {
            break;
        };
        let Some((child, _)) = children.into_iter().find(|(_, record)| {
            record.kind() == segment.kind && record.name() == segment.name.as_os_str()
        }) else {
            break;
        };
        matched.push(child);
        parent = child;
    }
    matched
}

fn find_identity<F>(
    snapshot: &TreeSnapshot,
    identity: FileIdentity,
    preferred: Option<NodeId>,
    acceptable: &mut F,
) -> Option<NodeId>
where
    F: FnMut(&TreeSnapshot, NodeId) -> bool,
{
    if let Some(node) = preferred
        && acceptable(snapshot, node)
    {
        return Some(node);
    }
    snapshot.nodes().iter().enumerate().find_map(|(index, record)| {
        if record.file_identity() != Some(identity) {
            return None;
        }
        let node = u32::try_from(index).ok().map(NodeId::from_raw)?;
        acceptable(snapshot, node).then_some(node)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder, VolumeKey};

    fn identity(file: u128) -> FileIdentity {
        FileIdentity::new(VolumeKey::new(7), file)
    }

    fn file(name: &str, file_identity: Option<FileIdentity>) -> NodeSpec {
        let metrics = OwnMetrics::new(
            SizeMetric::known(1, MetricSource::PortableMetadata),
            SizeMetric::known(2, MetricSource::FilesystemAllocation),
        );
        let spec = NodeSpec::file(name, metrics);
        match file_identity {
            Some(file_identity) => spec.with_file_identity(file_identity),
            None => spec,
        }
    }

    struct Fixture {
        snapshot: TreeSnapshot,
        root: NodeId,
        directory: NodeId,
        nested: NodeId,
        leaf: NodeId,
    }

    fn fixture(generation: u64) -> Fixture {
        let mut builder = TreeBuilder::new(GenerationId::new(generation));
        let root = builder.add_root(NodeSpec::root("C:\\")).expect("root");
        let directory = builder
            .add_child(root, NodeSpec::directory("directory").with_file_identity(identity(10)))
            .expect("directory");
        let nested = builder
            .add_child(directory, NodeSpec::directory("nested").with_file_identity(identity(11)))
            .expect("nested");
        let leaf = builder.add_child(nested, file("leaf.bin", Some(identity(12)))).expect("leaf");
        Fixture { snapshot: builder.freeze().expect("snapshot"), root, directory, nested, leaf }
    }

    fn apply(state: &mut NavigationState, action: NavigationAction) -> CommandOutcome {
        state.execute(state.command(action)).expect("available command")
    }

    #[test]
    fn zoom_back_parent_and_selection_are_independent() {
        let fixture = fixture(1);
        let mut state = NavigationState::new(fixture.snapshot).expect("navigation");
        assert_eq!(state.view_root(), fixture.root);
        assert_eq!(state.selected(), None);

        assert_eq!(
            apply(&mut state, NavigationAction::Select(fixture.leaf)),
            CommandOutcome::Applied(NavigationChange::Selection)
        );
        apply(&mut state, NavigationAction::Zoom(fixture.directory));
        assert_eq!(state.view_root(), fixture.directory);
        assert_eq!(state.selected(), Some(fixture.leaf));

        apply(&mut state, NavigationAction::Zoom(fixture.nested));
        apply(&mut state, NavigationAction::Parent);
        assert_eq!(state.view_root(), fixture.directory);
        apply(&mut state, NavigationAction::Back);
        assert_eq!(state.view_root(), fixture.nested);
        assert_eq!(state.selected(), Some(fixture.leaf));

        let file_zoom = state.command(NavigationAction::Zoom(fixture.leaf));
        assert_eq!(
            state.availability(file_zoom),
            CommandAvailability::Unavailable(UnavailableReason::NotNavigable {
                node: fixture.leaf,
                kind: EntryKind::File,
            })
        );
    }

    #[test]
    fn history_is_bounded_and_zero_limit_is_deterministic() {
        let fixture = fixture(2);
        let mut state =
            NavigationState::with_history_limit(fixture.snapshot, 2).expect("navigation");
        apply(&mut state, NavigationAction::Zoom(fixture.directory));
        apply(&mut state, NavigationAction::Zoom(fixture.nested));
        apply(&mut state, NavigationAction::Zoom(fixture.root));
        assert_eq!(state.history().collect::<Vec<_>>(), vec![fixture.directory, fixture.nested]);
        apply(&mut state, NavigationAction::Back);
        assert_eq!(state.view_root(), fixture.nested);
        apply(&mut state, NavigationAction::Back);
        assert_eq!(state.view_root(), fixture.directory);
        assert!(!state.availability(state.command(NavigationAction::Back)).is_available());

        let fixture = self::fixture(3);
        let mut no_history =
            NavigationState::with_history_limit(fixture.snapshot, 0).expect("navigation");
        apply(&mut no_history, NavigationAction::Zoom(fixture.directory));
        assert_eq!(no_history.history_len(), 0);
    }

    #[test]
    fn replacement_preserves_moved_ids_by_identity_and_metric_does_not_change_identity() {
        let original = fixture(4);
        let mut state = NavigationState::new(original.snapshot).expect("navigation");
        apply(&mut state, NavigationAction::Zoom(original.directory));
        apply(&mut state, NavigationAction::Zoom(original.nested));
        apply(&mut state, NavigationAction::Select(original.leaf));
        let before_history = state.history().collect::<Vec<_>>();
        apply(&mut state, NavigationAction::SetSizeBasis(SizeBasis::Allocated));
        assert_eq!(state.view_root(), original.nested);
        assert_eq!(state.selected(), Some(original.leaf));
        assert_eq!(state.history().collect::<Vec<_>>(), before_history);

        let mut builder = TreeBuilder::new(GenerationId::new(5));
        let root = builder.add_root(NodeSpec::root("C:\\")).expect("root");
        builder.add_child(root, file("inserted", None)).expect("inserted");
        let moved_directory = builder
            .add_child(
                root,
                NodeSpec::directory("renamed-directory").with_file_identity(identity(10)),
            )
            .expect("moved directory");
        let moved_nested = builder
            .add_child(
                moved_directory,
                NodeSpec::directory("renamed-nested").with_file_identity(identity(11)),
            )
            .expect("moved nested");
        let moved_leaf = builder
            .add_child(moved_nested, file("renamed.bin", Some(identity(12))))
            .expect("moved leaf");
        let report =
            state.replace_snapshot(builder.freeze().expect("replacement")).expect("replace");

        assert_eq!(report.view_root, moved_nested);
        assert_eq!(report.selected, Some(moved_leaf));
        assert_eq!(state.size_basis(), SizeBasis::Allocated);
        assert_eq!(state.history().collect::<Vec<_>>(), vec![root, moved_directory]);
    }

    #[test]
    fn deleted_targets_fall_back_to_the_deepest_safe_ancestor() {
        let original = fixture(6);
        let mut state = NavigationState::new(original.snapshot).expect("navigation");
        apply(&mut state, NavigationAction::Zoom(original.nested));
        apply(&mut state, NavigationAction::Select(original.leaf));

        let mut builder = TreeBuilder::new(GenerationId::new(7));
        let root = builder.add_root(NodeSpec::root("C:\\")).expect("root");
        let directory = builder
            .add_child(root, NodeSpec::directory("directory").with_file_identity(identity(10)))
            .expect("directory");
        let report =
            state.replace_snapshot(builder.freeze().expect("replacement")).expect("replace");
        assert_eq!(report.view_root, directory);
        assert_eq!(report.selected, Some(directory));
        assert_eq!(state.history().collect::<Vec<_>>(), vec![root]);
    }

    #[test]
    fn hidden_branches_are_validated_restored_and_remapped_exactly() {
        let original = fixture(8);
        let mut state = NavigationState::new(original.snapshot).expect("navigation");
        apply(&mut state, NavigationAction::HideBranch(original.nested));
        assert!(state.hidden_branches().contains(original.nested));
        assert!(matches!(
            state.availability(state.command(NavigationAction::HideBranch(original.root))),
            CommandAvailability::Unavailable(
                UnavailableReason::FilesystemRootCannotBeHidden { .. }
            )
        ));
        apply(&mut state, NavigationAction::RestoreBranch(original.nested));
        assert!(state.hidden_branches().is_empty());
        apply(&mut state, NavigationAction::ToggleBranch(original.leaf));
        assert!(state.hidden_branches().contains(original.leaf));

        let mut builder = TreeBuilder::new(GenerationId::new(9));
        let root = builder.add_root(NodeSpec::root("C:\\")).expect("root");
        builder.add_child(root, file("padding", None)).expect("padding");
        let directory = builder
            .add_child(root, NodeSpec::directory("directory").with_file_identity(identity(10)))
            .expect("directory");
        let nested = builder
            .add_child(directory, NodeSpec::directory("nested").with_file_identity(identity(11)))
            .expect("nested");
        let moved_leaf =
            builder.add_child(nested, file("renamed.bin", Some(identity(12)))).expect("leaf");
        state.replace_snapshot(builder.freeze().expect("replacement")).expect("replace");
        assert_eq!(state.hidden_branches().iter().collect::<Vec<_>>(), vec![moved_leaf]);
        apply(&mut state, NavigationAction::RestoreAllBranches);
        assert!(state.hidden_branches().is_empty());
    }

    #[test]
    fn synthetic_multi_volume_summary_is_a_view_but_never_an_action_target() {
        let mut builder = TreeBuilder::new(GenerationId::new(10));
        let summary = builder.add_root(NodeSpec::synthetic_group("All volumes")).expect("summary");
        let first = builder.add_child(summary, NodeSpec::root("C:\\")).expect("C root");
        let second = builder.add_child(summary, NodeSpec::root("D:\\")).expect("D root");
        let child = builder.add_child(first, NodeSpec::directory("Users")).expect("branch");
        let mut state =
            NavigationState::new(builder.freeze().expect("snapshot")).expect("navigation");
        assert_eq!(state.view_root(), summary);
        assert!(state.is_summary_view());

        for action in [
            NavigationAction::Select(summary),
            NavigationAction::Zoom(summary),
            NavigationAction::HideBranch(summary),
            NavigationAction::RescanBranch(summary),
        ] {
            assert!(matches!(
                state.availability(state.command(action)),
                CommandAvailability::Unavailable(UnavailableReason::SyntheticTarget { node })
                    if node == summary
            ));
        }

        apply(&mut state, NavigationAction::ChooseRoot(first));
        assert_eq!(state.view_root(), first);
        apply(&mut state, NavigationAction::Parent);
        assert_eq!(state.view_root(), summary);
        apply(&mut state, NavigationAction::ChooseRoot(second));
        apply(&mut state, NavigationAction::ShowSummary);
        assert_eq!(state.view_root(), summary);

        let full = apply(&mut state, NavigationAction::RescanAll);
        let CommandOutcome::RescanRequested(RescanRequest::Full { roots, .. }) = full else {
            panic!("expected full rescan request");
        };
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].path(), Path::new("C:\\"));
        assert_eq!(roots[1].path(), Path::new("D:\\"));

        let branch = apply(&mut state, NavigationAction::RescanBranch(child));
        let CommandOutcome::RescanRequested(RescanRequest::Branch(target)) = branch else {
            panic!("expected branch rescan request");
        };
        assert_eq!(target.path(), Path::new("C:\\").join("Users"));
    }

    #[test]
    fn replacement_and_parent_walk_ten_thousand_levels_iteratively() {
        const DEPTH: u32 = 10_000;
        let mut builder = TreeBuilder::new(GenerationId::new(11));
        let root = builder.add_root(NodeSpec::root("C:\\")).expect("root");
        let mut parent = root;
        for index in 1..=DEPTH {
            parent = builder
                .add_child(
                    parent,
                    NodeSpec::directory(format!("d{index}"))
                        .with_file_identity(identity(u128::from(index) + 100)),
                )
                .expect("directory");
        }
        let mut state =
            NavigationState::new(builder.freeze().expect("snapshot")).expect("navigation");
        apply(&mut state, NavigationAction::Zoom(parent));

        let mut replacement = TreeBuilder::new(GenerationId::new(12));
        let replacement_root = replacement.add_root(NodeSpec::root("C:\\")).expect("root");
        let mut replacement_parent = replacement_root;
        for index in 1..=DEPTH {
            replacement_parent = replacement
                .add_child(
                    replacement_parent,
                    NodeSpec::directory(format!("renamed{index}"))
                        .with_file_identity(identity(u128::from(index) + 100)),
                )
                .expect("directory");
        }
        state.replace_snapshot(replacement.freeze().expect("replacement")).expect("replace");
        assert_eq!(state.view_root(), replacement_parent);
        apply(&mut state, NavigationAction::Parent);
        assert_eq!(state.view_root().raw(), replacement_parent.raw() - 1);
    }

    #[test]
    fn stale_snapshots_and_commands_are_rejected_without_mutation() {
        let fixture = fixture(20);
        let old_command = NavigationCommand::new(
            GenerationId::new(19),
            NavigationAction::Zoom(fixture.directory),
        );
        let mut state = NavigationState::new(fixture.snapshot).expect("navigation");
        assert!(matches!(
            state.execute(old_command),
            Err(NavigationError::CommandUnavailable {
                reason: UnavailableReason::StaleGeneration { .. },
                ..
            })
        ));
        let original_root = state.view_root();

        let stale = self::fixture(19).snapshot;
        assert_eq!(
            state.replace_snapshot(stale),
            Err(NavigationError::StaleSnapshot {
                current: GenerationId::new(20),
                replacement: GenerationId::new(19),
            })
        );
        assert_eq!(state.generation(), GenerationId::new(20));
        assert_eq!(state.view_root(), original_root);
    }
}
