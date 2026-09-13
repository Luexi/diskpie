//! Transactional replacement of one directory without losing unrelated observations.

use diskpie_core::{
    EntryKind, FileIdentity, ModelError, NodeId, NodeRecord, NodeSpec, OwnMetrics, ScanState,
    TreeBuilder, TreeSnapshot,
};
use std::{collections::HashMap, error::Error, fmt, path::PathBuf, sync::Arc};

/// Capability tied to the exact immutable revision that supplied a branch target.
/// The runtime creates and validates this before consuming scan resources.
#[derive(Clone)]
pub struct BranchRescanIntent {
    pub(crate) snapshot: Arc<TreeSnapshot>,
    pub(crate) revision: u64,
    pub(crate) target: NodeId,
}

impl BranchRescanIntent {
    #[must_use]
    pub fn snapshot(&self) -> &Arc<TreeSnapshot> {
        &self.snapshot
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn target(&self) -> NodeId {
        self.target
    }

    /// Reconstructs the native target without using any display string.
    pub fn native_path(&self) -> Result<PathBuf, ModelError> {
        self.snapshot.path(self.target)
    }
}

impl fmt::Debug for BranchRescanIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BranchRescanIntent")
            .field("generation", &self.snapshot.generation())
            .field("revision", &self.revision)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BranchMergeError {
    InvalidTarget,
    InvalidReplacement,
    StaleGeneration,
    Cancelled,
    Model(ModelError),
}

impl fmt::Display for BranchMergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget => formatter.write_str("branch target is not a real directory"),
            Self::InvalidReplacement => {
                formatter.write_str("branch result must have one real root")
            }
            Self::StaleGeneration => formatter.write_str("branch result generation is not newer"),
            Self::Cancelled => formatter.write_str("cancelled branch results cannot be committed"),
            Self::Model(error) => error.fmt(formatter),
        }
    }
}

impl Error for BranchMergeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Model(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ModelError> for BranchMergeError {
    fn from(error: ModelError) -> Self {
        Self::Model(error)
    }
}

/// Rebuilds an indexed tree, recomputes all aggregates and reassigns hard-link
/// allocation ownership from original observations. Call off the UI thread.
pub fn merge_branch_snapshot(
    baseline: &TreeSnapshot,
    target: NodeId,
    replacement: &TreeSnapshot,
) -> Result<TreeSnapshot, BranchMergeError> {
    merge_branch_snapshot_cancellable(baseline, target, replacement, || false)
}

/// Same transactional replacement with cooperative cancellation during every
/// whole-tree pass. The baseline remains untouched even when work is abandoned.
pub fn merge_branch_snapshot_cancellable(
    baseline: &TreeSnapshot,
    target: NodeId,
    replacement: &TreeSnapshot,
    mut cancelled: impl FnMut() -> bool,
) -> Result<TreeSnapshot, BranchMergeError> {
    if cancelled() {
        return Err(BranchMergeError::Cancelled);
    }
    let original = baseline.node(target).ok_or(BranchMergeError::InvalidTarget)?;
    if !matches!(original.kind(), EntryKind::Root | EntryKind::Directory) {
        return Err(BranchMergeError::InvalidTarget);
    }
    if replacement.generation() <= baseline.generation() {
        return Err(BranchMergeError::StaleGeneration);
    }
    if replacement.state() == ScanState::Cancelled {
        return Err(BranchMergeError::Cancelled);
    }
    let mut roots = replacement.roots();
    let (replacement_root, root_record) =
        roots.next().ok_or(BranchMergeError::InvalidReplacement)?;
    if roots.next().is_some() || root_record.kind() != EntryKind::Root {
        return Err(BranchMergeError::InvalidReplacement);
    }

    // The freshly observed owner's allocated metric is the deterministic best
    // measurement for this identity, even when it came from a different alias.
    let mut fresh = HashMap::<FileIdentity, OwnMetrics>::new();
    for (index, record) in replacement.nodes().iter().enumerate() {
        if index.is_multiple_of(256) && cancelled() {
            return Err(BranchMergeError::Cancelled);
        }
        if record.hard_link_owner().is_some()
            && !record.is_hard_link_alias()
            && let Some(identity) = record.file_identity()
        {
            fresh.insert(identity, record.own_metrics());
        }
    }
    let mut removed = vec![false; baseline.len()];
    let mut removed_count = 0;
    for (index, record) in baseline.nodes().iter().enumerate() {
        if index.is_multiple_of(256) && cancelled() {
            return Err(BranchMergeError::Cancelled);
        }
        removed[index] = index == (target.raw() as usize)
            || record.parent().is_some_and(|parent| removed[parent.raw() as usize]);
        removed_count += usize::from(removed[index]);
    }
    let count = baseline.len() - removed_count + replacement.len();
    u32::try_from(count.saturating_sub(1)).map_err(|_| ModelError::ArenaExhausted)?;
    let mut builder = TreeBuilder::with_capacity(replacement.generation(), count);
    let mut old_to_new = vec![None; baseline.len()];
    let mut branch_to_new = vec![None; replacement.len()];
    for (index, record) in baseline.nodes().iter().enumerate() {
        if index.is_multiple_of(256) && cancelled() {
            return Err(BranchMergeError::Cancelled);
        }
        if index == (target.raw() as usize) {
            for (branch_index, branch) in replacement.nodes().iter().enumerate() {
                if branch_index.is_multiple_of(256) && cancelled() {
                    return Err(BranchMergeError::Cancelled);
                }
                let (parent, spec) = if branch_index == (replacement_root.raw() as usize) {
                    let parent =
                        record.parent().and_then(|parent| old_to_new[parent.raw() as usize]);
                    // ScanRoot does not carry directory identity. Leave it unknown
                    // rather than reusing identity from a possibly replaced object.
                    let spec =
                        NodeSpec::new(original.name(), original.kind(), branch.observed_metrics())
                            .with_state(branch.local_state())
                            .with_omissions(branch.own_omissions());
                    (parent, spec)
                } else {
                    let parent =
                        branch.parent().and_then(|parent| branch_to_new[parent.raw() as usize]);
                    (parent, observation_spec(branch, branch.observed_metrics()))
                };
                let mapped = append(&mut builder, parent, spec)?;
                branch_to_new[branch_index] = Some(mapped);
                if branch_index == (replacement_root.raw() as usize) {
                    old_to_new[index] = Some(mapped);
                }
            }
        } else if !removed[index] {
            let parent = record.parent().and_then(|parent| old_to_new[parent.raw() as usize]);
            old_to_new[index] = Some(append(&mut builder, parent, copy_spec(record, &fresh))?);
        }
    }
    builder.freeze_cancellable(&mut cancelled)?.ok_or(BranchMergeError::Cancelled)
}

fn copy_spec(record: &NodeRecord, fresh: &HashMap<FileIdentity, OwnMetrics>) -> NodeSpec {
    let metrics = record
        .file_identity()
        .and_then(|identity| fresh.get(&identity))
        .copied()
        .unwrap_or_else(|| record.observed_metrics());
    observation_spec(record, metrics)
}

fn observation_spec(record: &NodeRecord, metrics: OwnMetrics) -> NodeSpec {
    let mut spec = NodeSpec::new(record.name(), record.kind(), metrics)
        .with_state(record.local_state())
        .with_omissions(record.own_omissions());
    if let Some(identity) = record.file_identity() {
        spec = spec.with_file_identity(identity);
    }
    spec
}

fn append(
    builder: &mut TreeBuilder,
    parent: Option<NodeId>,
    spec: NodeSpec,
) -> Result<NodeId, ModelError> {
    match parent {
        Some(parent) => builder.add_child(parent, spec),
        None => builder.add_root(spec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::{NavigationAction, NavigationState};
    use diskpie_core::{GenerationId, MetricSource, SizeMetric, VolumeKey};

    fn metrics(bytes: u64) -> OwnMetrics {
        OwnMetrics::new(
            SizeMetric::known(bytes, MetricSource::PortableMetadata),
            SizeMetric::known(bytes, MetricSource::FilesystemAllocation),
        )
    }

    fn named(snapshot: &TreeSnapshot, name: &str) -> NodeId {
        snapshot
            .nodes()
            .iter()
            .position(|record| record.name() == name)
            .map(|index| NodeId::from_raw(index as u32))
            .expect("named node")
    }

    fn original() -> (TreeSnapshot, NodeId) {
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let branch = builder.add_child(root, NodeSpec::directory("a")).unwrap();
        let identity = FileIdentity::new(VolumeKey::new(1), 44);
        builder
            .add_child(
                branch,
                NodeSpec::file("old-owner", metrics(100)).with_file_identity(identity),
            )
            .unwrap();
        builder
            .add_child(root, NodeSpec::file("z-alias", metrics(100)).with_file_identity(identity))
            .unwrap();
        builder.add_child(branch, NodeSpec::directory("removed")).unwrap();
        let sibling = builder.add_child(root, NodeSpec::directory("sibling")).unwrap();
        builder.add_child(sibling, NodeSpec::file("untouched", metrics(7))).unwrap();
        (builder.freeze().unwrap(), branch)
    }

    #[test]
    fn removed_allocation_owner_promotes_preserved_alias_from_original_measurement() {
        let (baseline, branch) = original();
        let alias = baseline.node(named(&baseline, "z-alias")).unwrap();
        assert_eq!(alias.own_metrics().allocated().known_bytes(), Some(0));
        assert_eq!(alias.observed_metrics().allocated().known_bytes(), Some(100));
        let mut fresh = TreeBuilder::new(GenerationId::new(2));
        fresh.add_root(NodeSpec::root("root/a")).unwrap();
        let merged = merge_branch_snapshot(&baseline, branch, &fresh.freeze().unwrap()).unwrap();
        let alias = merged.node(named(&merged, "z-alias")).unwrap();
        assert!(!alias.is_hard_link_alias());
        assert_eq!(alias.own_metrics().allocated().known_bytes(), Some(100));
        assert_eq!(merged.nodes()[0].aggregate().allocated().known_bytes(), 107);
        assert!(!merged.nodes().iter().any(|node| node.name() == "old-owner"));
        assert_eq!(
            merged.path(named(&merged, "untouched")).unwrap(),
            PathBuf::from("root").join("sibling/untouched")
        );
    }

    #[test]
    fn fresh_identity_measurement_updates_aliases_outside_the_branch_and_omissions_propagate() {
        let (baseline, branch) = original();
        let mut fresh = TreeBuilder::new(GenerationId::new(2));
        let root = fresh.add_root(NodeSpec::root("root/a").with_omissions(1)).unwrap();
        fresh
            .add_child(
                root,
                NodeSpec::file("new", metrics(250))
                    .with_file_identity(FileIdentity::new(VolumeKey::new(1), 44)),
            )
            .unwrap();
        let merged = merge_branch_snapshot(&baseline, branch, &fresh.freeze().unwrap()).unwrap();
        assert_eq!(
            merged.node(named(&merged, "z-alias")).unwrap().observed_metrics(),
            metrics(250)
        );
        assert_eq!(merged.nodes()[0].aggregate().allocated().known_bytes(), 257);
        assert_eq!(merged.state(), ScanState::Partial);
        assert_eq!(merged.nodes()[0].aggregate().omission_count(), 1);
    }

    #[test]
    fn navigation_keeps_hidden_and_history_and_falls_back_from_deleted_selection() {
        let (baseline, branch) = original();
        let mut navigation = NavigationState::new(Arc::new(baseline)).unwrap();
        let sibling = named(navigation.snapshot(), "sibling");
        navigation.execute(navigation.command(NavigationAction::HideBranch(sibling))).unwrap();
        navigation.execute(navigation.command(NavigationAction::Activate(branch))).unwrap();
        let removed = named(navigation.snapshot(), "removed");
        navigation.execute(navigation.command(NavigationAction::Activate(removed))).unwrap();
        let mut fresh = TreeBuilder::new(GenerationId::new(2));
        fresh.add_root(NodeSpec::root("root/a")).unwrap();
        let merged =
            merge_branch_snapshot(navigation.snapshot(), branch, &fresh.freeze().unwrap()).unwrap();
        navigation.replace_snapshot(Arc::new(merged)).unwrap();
        let branch = named(navigation.snapshot(), "a");
        assert_eq!(navigation.view_root(), branch);
        assert_eq!(navigation.selected(), Some(branch));
        assert_eq!(navigation.hidden_branches_plan().len(), 1);
        navigation.execute(navigation.command(NavigationAction::Back)).unwrap();
        assert_eq!(navigation.view_root(), NodeId::from_raw(0));
    }

    #[test]
    fn rejects_cancelled_stale_and_file_targets_without_changing_baseline() {
        let (baseline, branch) = original();
        let mut fresh = TreeBuilder::new(GenerationId::new(2));
        fresh.add_root(NodeSpec::root("root/a")).unwrap();
        fresh.set_scan_state(ScanState::Cancelled);
        assert!(matches!(
            merge_branch_snapshot(&baseline, branch, &fresh.freeze().unwrap()),
            Err(BranchMergeError::Cancelled)
        ));
        assert!(matches!(
            merge_branch_snapshot(&baseline, branch, &baseline),
            Err(BranchMergeError::StaleGeneration)
        ));
        assert!(matches!(
            merge_branch_snapshot(&baseline, named(&baseline, "z-alias"), &baseline),
            Err(BranchMergeError::InvalidTarget)
        ));
        assert_eq!(baseline.nodes()[0].aggregate().allocated().known_bytes(), 107);
    }
}
