//! Immutable presentation indices derived from a coherent tree snapshot.

use diskpie_core::{
    EntryKind, GenerationId, NodeId, NodeRecord, TreeSnapshot, sunburst::SizeBasis,
};
use std::{cmp::Ordering, error::Error, ffi::OsStr, fmt};

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const PATH_COMPONENT_MARKER: u8 = 0xFF;

/// Stable family and variation inputs; the renderer chooses the actual palette.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ColorIdentity {
    pub family_key: u64,
    pub variant_key: u64,
    /// Depth below the first child of the native filesystem root.
    pub depth: u32,
}

/// Two words per node, reusing the existing path-key array for both hashes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ColorFamilyIndex {
    family_anchor: NodeId,
    depth_in_family: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ItemSort {
    Name,
    #[default]
    Size,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SortDirection {
    Ascending,
    #[default]
    Descending,
}

impl SortDirection {
    const fn apply(self, ordering: Ordering) -> Ordering {
        match self {
            Self::Ascending => ordering,
            Self::Descending => ordering.reverse(),
        }
    }
}

/// Invalid snapshot relationship detected while deriving presentation data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationError {
    UnknownNode { node: NodeId },
    ParentDidNotPrecedeChild { node: NodeId, parent: NodeId },
    InvalidChildLink { parent: NodeId },
}

impl fmt::Display for PresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownNode { node } => write!(formatter, "node {node} is not in the snapshot"),
            Self::ParentDidNotPrecedeChild { node, parent } => {
                write!(formatter, "parent {parent} did not precede presentation node {node}")
            }
            Self::InvalidChildLink { parent } => {
                write!(formatter, "snapshot children below {parent} are invalid")
            }
        }
    }
}

impl Error for PresentationError {}

/// Compact per-generation keys used to keep chart colors stable by native path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotPresentation {
    generation: GenerationId,
    path_color_keys: Box<[u64]>,
    color_families: Box<[ColorFamilyIndex]>,
}

impl SnapshotPresentation {
    /// Derives every key iteratively in arena order without reconstructing paths.
    pub fn new(snapshot: &TreeSnapshot) -> Result<Self, PresentationError> {
        Ok(Self::new_cancellable(snapshot, || false)?
            .expect("an uncancelled presentation produces an index"))
    }

    /// Builds a complete immutable index, abandoning obsolete work between batches.
    /// Cancellation never exposes a partially initialized family or path index.
    pub fn new_cancellable(
        snapshot: &TreeSnapshot,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<Self>, PresentationError> {
        if cancelled() {
            return Ok(None);
        }
        let mut path_color_keys = Vec::with_capacity(snapshot.len());
        let mut color_families: Vec<ColorFamilyIndex> = Vec::with_capacity(snapshot.len());
        for (index, record) in snapshot.nodes().iter().enumerate() {
            if index.is_multiple_of(256) && cancelled() {
                return Ok(None);
            }
            let node = NodeId::from_raw(u32::try_from(index).map_err(|_| {
                PresentationError::UnknownNode { node: NodeId::from_raw(u32::MAX) }
            })?);
            let parent_key = match record.parent() {
                Some(parent) if record.kind() != EntryKind::Root => Some(
                    *path_color_keys
                        .get(parent.raw() as usize)
                        .ok_or(PresentationError::ParentDidNotPrecedeChild { node, parent })?,
                ),
                Some(_) | None => None,
            };
            let initial = parent_key.unwrap_or(FNV_OFFSET_BASIS);
            path_color_keys.push(hash_component(initial, record.name()));
            let family = match record.parent() {
                Some(parent)
                    if record.kind() != EntryKind::Root
                        && snapshot
                            .node(parent)
                            .is_some_and(|record| record.kind() != EntryKind::Root) =>
                {
                    let ancestor = color_families
                        .get(parent.raw() as usize)
                        .ok_or(PresentationError::ParentDidNotPrecedeChild { node, parent })?;
                    ColorFamilyIndex {
                        family_anchor: ancestor.family_anchor,
                        depth_in_family: ancestor.depth_in_family.saturating_add(1),
                    }
                }
                Some(_) | None => ColorFamilyIndex { family_anchor: node, depth_in_family: 0 },
            };
            color_families.push(family);
        }
        if cancelled() {
            return Ok(None);
        }
        Ok(Some(Self {
            generation: snapshot.generation(),
            path_color_keys: path_color_keys.into(),
            color_families: color_families.into(),
        }))
    }

    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        self.generation
    }

    /// Returns the deterministic color identity for a real or synthetic node.
    pub fn path_color_key(&self, node: NodeId) -> Result<u64, PresentationError> {
        self.path_color_keys
            .get(node.raw() as usize)
            .copied()
            .ok_or(PresentationError::UnknownNode { node })
    }

    /// Constant-time lookup independent of the chart's zoom, order, and metric.
    pub fn color_identity(&self, node: NodeId) -> Result<ColorIdentity, PresentationError> {
        let family = self
            .color_families
            .get(node.raw() as usize)
            .ok_or(PresentationError::UnknownNode { node })?;
        Ok(ColorIdentity {
            family_key: self.path_color_key(family.family_anchor)?,
            variant_key: self.path_color_key(node)?,
            depth: family.depth_in_family,
        })
    }
}

/// Returns the largest direct children for the synchronized textual view.
///
/// Ordering matches the chart's primary size rule and native-name tie break.
/// Unknown-entry count is a secondary descending key so partial branches stay
/// visible when their known-byte totals are equal.
pub fn largest_children(
    snapshot: &TreeSnapshot,
    parent: NodeId,
    basis: SizeBasis,
    limit: usize,
) -> Result<Vec<NodeId>, PresentationError> {
    if snapshot.node(parent).is_none() {
        return Err(PresentationError::UnknownNode { node: parent });
    }
    let children =
        snapshot.children(parent).map_err(|_| PresentationError::InvalidChildLink { parent })?;
    let mut nodes = children.map(|(node, _)| node).collect::<Vec<_>>();
    if limit == 0 {
        return Ok(Vec::new());
    }
    if nodes.len() > limit {
        nodes.select_nth_unstable_by(limit, |left, right| {
            compare_nodes(snapshot, *left, *right, basis, ItemSort::Size, SortDirection::Descending)
        });
        nodes.truncate(limit);
    }
    nodes.sort_by(|left, right| {
        compare_nodes(snapshot, *left, *right, basis, ItemSort::Size, SortDirection::Descending)
    });
    Ok(nodes)
}

/// Complete ordering for a worker-owned textual index. Chunked merge sort
/// checks cancellation without making the ordering comparator inconsistent.
pub fn ranked_children_cancellable(
    snapshot: &TreeSnapshot,
    parent: NodeId,
    basis: SizeBasis,
    sort: ItemSort,
    direction: SortDirection,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<Vec<NodeId>>, PresentationError> {
    let children =
        snapshot.children(parent).map_err(|_| PresentationError::InvalidChildLink { parent })?;
    let mut nodes = Vec::new();
    for (index, (node, _)) in children.enumerate() {
        if index % 256 == 0 && cancelled() {
            return Ok(None);
        }
        nodes.push(node);
    }
    const CHUNK: usize = 2048;
    for chunk in nodes.chunks_mut(CHUNK) {
        if cancelled() {
            return Ok(None);
        }
        chunk.sort_unstable_by(|left, right| {
            compare_nodes(snapshot, *left, *right, basis, sort, direction)
        });
    }
    let mut width = CHUNK;
    if nodes.len() > width {
        let mut scratch = nodes.clone();
        while width < nodes.len() {
            for start in (0..nodes.len()).step_by(width.saturating_mul(2)) {
                let middle = (start + width).min(nodes.len());
                let end = (middle + width).min(nodes.len());
                let (mut left, mut right) = (start, middle);
                for (offset, slot) in scratch[start..end].iter_mut().enumerate() {
                    if offset % 256 == 0 && cancelled() {
                        return Ok(None);
                    }
                    if left < middle
                        && (right == end
                            || compare_nodes(
                                snapshot,
                                nodes[left],
                                nodes[right],
                                basis,
                                sort,
                                direction,
                            ) != Ordering::Greater)
                    {
                        *slot = nodes[left];
                        left += 1;
                    } else {
                        *slot = nodes[right];
                        right += 1;
                    }
                }
            }
            std::mem::swap(&mut nodes, &mut scratch);
            width = width.saturating_mul(2);
        }
    }
    Ok((!cancelled()).then_some(nodes))
}

/// Finds a selected row in a complete or filtered sorted index in O(log n).
pub fn ranked_position(
    snapshot: &TreeSnapshot,
    nodes: &[NodeId],
    basis: SizeBasis,
    sort: ItemSort,
    direction: SortDirection,
    node: NodeId,
) -> Option<usize> {
    snapshot.node(node)?;
    nodes
        .binary_search_by(|candidate| {
            compare_nodes(snapshot, *candidate, node, basis, sort, direction)
        })
        .ok()
}

fn compare_nodes(
    snapshot: &TreeSnapshot,
    left: NodeId,
    right: NodeId,
    basis: SizeBasis,
    sort: ItemSort,
    direction: SortDirection,
) -> Ordering {
    let left_record = snapshot.node(left).expect("ranked child is in the validated snapshot");
    let right_record = snapshot.node(right).expect("ranked child is in the validated snapshot");
    let folded_name_order = || {
        left_record
            .name()
            .to_string_lossy()
            .chars()
            .flat_map(char::to_lowercase)
            .cmp(right_record.name().to_string_lossy().chars().flat_map(char::to_lowercase))
    };
    let primary = match sort {
        ItemSort::Name => direction.apply(folded_name_order()),
        ItemSort::Size => {
            let (left_bytes, left_unknown) = selected_size(left_record, basis);
            let (right_bytes, right_unknown) = selected_size(right_record, basis);
            direction
                .apply(left_bytes.cmp(&right_bytes).then_with(|| left_unknown.cmp(&right_unknown)))
                .then_with(folded_name_order)
        }
    };
    primary
        .then_with(|| compare_native_names(left_record.name(), right_record.name()))
        .then_with(|| left.cmp(&right))
}

fn selected_size(record: &NodeRecord, basis: SizeBasis) -> (u128, u64) {
    let size = match basis {
        SizeBasis::Logical => record.aggregate().logical(),
        SizeBasis::Allocated => record.aggregate().allocated(),
    };
    (size.known_bytes(), size.unknown_entries())
}

fn hash_component(mut hash: u64, name: &OsStr) -> u64 {
    hash = hash_byte(hash, PATH_COMPONENT_MARKER);
    hash_native_name(hash, name)
}

const fn hash_byte(hash: u64, byte: u8) -> u64 {
    (hash ^ byte as u64).wrapping_mul(FNV_PRIME)
}

#[cfg(windows)]
fn hash_native_name(mut hash: u64, name: &OsStr) -> u64 {
    use std::os::windows::ffi::OsStrExt;

    let length = name.encode_wide().count() as u64;
    for byte in length.to_le_bytes() {
        hash = hash_byte(hash, byte);
    }
    for unit in name.encode_wide() {
        for byte in unit.to_le_bytes() {
            hash = hash_byte(hash, byte);
        }
    }
    hash
}

#[cfg(not(windows))]
fn hash_native_name(mut hash: u64, name: &OsStr) -> u64 {
    let encoded = name.as_encoded_bytes();
    for byte in (encoded.len() as u64).to_le_bytes() {
        hash = hash_byte(hash, byte);
    }
    for &byte in encoded {
        hash = hash_byte(hash, byte);
    }
    hash
}

#[cfg(windows)]
fn compare_native_names(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt;

    left.encode_wide().cmp(right.encode_wide())
}

#[cfg(not(windows))]
fn compare_native_names(left: &OsStr, right: &OsStr) -> Ordering {
    left.cmp(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder};

    fn metrics(logical: u64, allocated: u64) -> OwnMetrics {
        OwnMetrics::new(
            SizeMetric::known(logical, MetricSource::PortableMetadata),
            SizeMetric::known(allocated, MetricSource::FilesystemAllocation),
        )
    }

    #[test]
    fn root_color_does_not_depend_on_a_synthetic_summary_label() {
        fn build(summary_name: &str) -> (TreeSnapshot, NodeId) {
            let mut builder = TreeBuilder::new(GenerationId::new(1));
            let summary =
                builder.add_root(NodeSpec::synthetic_group(summary_name)).expect("summary");
            let root = builder.add_child(summary, NodeSpec::root(r"C:\")).expect("root");
            (builder.freeze().expect("snapshot"), root)
        }

        let (first, first_root) = build("All volumes");
        let (second, second_root) = build("Todos los volúmenes");
        let first_index = SnapshotPresentation::new(&first).expect("first index");
        let second_index = SnapshotPresentation::new(&second).expect("second index");

        assert_eq!(
            first_index.path_color_key(first_root).expect("first key"),
            second_index.path_color_key(second_root).expect("second key")
        );
    }

    #[test]
    fn identical_names_under_different_parents_have_different_path_keys() {
        let mut builder = TreeBuilder::new(GenerationId::new(2));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let first = builder.add_child(root, NodeSpec::directory("a")).expect("a");
        let second = builder.add_child(root, NodeSpec::directory("b")).expect("b");
        let first_file =
            builder.add_child(first, NodeSpec::file("same", metrics(1, 1))).expect("first file");
        let second_file =
            builder.add_child(second, NodeSpec::file("same", metrics(1, 1))).expect("second file");
        let snapshot = builder.freeze().expect("snapshot");
        let index = SnapshotPresentation::new(&snapshot).expect("index");

        assert_ne!(
            index.path_color_key(first_file).expect("first key"),
            index.path_color_key(second_file).expect("second key")
        );
    }

    #[test]
    fn largest_children_switches_metric_and_breaks_ties_by_native_name() {
        let mut builder = TreeBuilder::new(GenerationId::new(3));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let logical =
            builder.add_child(root, NodeSpec::file("z-logical", metrics(9, 1))).expect("logical");
        let allocated =
            builder.add_child(root, NodeSpec::file("allocated", metrics(1, 9))).expect("allocated");
        let a_tie = builder.add_child(root, NodeSpec::file("a-tie", metrics(2, 2))).expect("a tie");
        let b_tie = builder.add_child(root, NodeSpec::file("b-tie", metrics(2, 2))).expect("b tie");
        let snapshot = builder.freeze().expect("snapshot");

        assert_eq!(
            largest_children(&snapshot, root, SizeBasis::Logical, 4).expect("logical order"),
            vec![logical, a_tie, b_tie, allocated]
        );
        assert_eq!(
            largest_children(&snapshot, root, SizeBasis::Allocated, 2).expect("allocated order"),
            vec![allocated, a_tie]
        );
    }

    #[test]
    fn rejects_unknown_nodes_without_panicking() {
        let mut builder = TreeBuilder::new(GenerationId::new(4));
        builder.add_root(NodeSpec::root("root")).expect("root");
        let snapshot = builder.freeze().expect("snapshot");
        assert_eq!(
            largest_children(&snapshot, NodeId::from_raw(99), SizeBasis::Logical, 10),
            Err(PresentationError::UnknownNode { node: NodeId::from_raw(99) })
        );
    }
    #[test]
    fn cancellable_complete_ranking_matches_reference_across_merge_chunks() {
        let mut builder = TreeBuilder::new(GenerationId::new(5));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        for n in (0..10_000).rev() {
            builder
                .add_child(root, NodeSpec::file(format!("name-{n:05}"), metrics(n % 37, n % 97)))
                .expect("file");
        }
        let snapshot = builder.freeze().expect("snapshot");
        for basis in [SizeBasis::Logical, SizeBasis::Allocated] {
            let ranked = ranked_children_cancellable(
                &snapshot,
                root,
                basis,
                ItemSort::Size,
                SortDirection::Descending,
                || false,
            )
            .expect("ranking")
            .expect("uncancelled");
            let reference =
                largest_children(&snapshot, root, basis, usize::MAX).expect("reference");
            assert_eq!(ranked, reference);
            assert_eq!(
                largest_children(&snapshot, root, basis, 200).expect("top 200"),
                ranked[..200]
            );
            for position in [0, 199, 5_000, 9_999] {
                assert_eq!(
                    ranked_position(
                        &snapshot,
                        &ranked,
                        basis,
                        ItemSort::Size,
                        SortDirection::Descending,
                        ranked[position]
                    ),
                    Some(position)
                );
            }
        }
        let mut checks = 0;
        assert!(
            ranked_children_cancellable(
                &snapshot,
                root,
                SizeBasis::Logical,
                ItemSort::Size,
                SortDirection::Descending,
                || {
                    checks += 1;
                    checks > 45
                }
            )
            .expect("cancel")
            .is_none()
        );
    }

    #[test]
    fn family_identity_survives_generation_reorder_metric_and_branch_replacement() {
        fn build(generation: u64, reverse: bool, changed_size: u64) -> (TreeSnapshot, [NodeId; 4]) {
            let mut builder = TreeBuilder::new(GenerationId::new(generation));
            let root = builder.add_root(NodeSpec::root(r"C:\fixture")).unwrap();
            let (first, second) = if reverse {
                let second = builder.add_child(root, NodeSpec::directory("b")).unwrap();
                let first = builder.add_child(root, NodeSpec::directory("a")).unwrap();
                (first, second)
            } else {
                let first = builder.add_child(root, NodeSpec::directory("a")).unwrap();
                let second = builder.add_child(root, NodeSpec::directory("b")).unwrap();
                (first, second)
            };
            let nested = builder.add_child(first, NodeSpec::directory("nested")).unwrap();
            let leaf = builder
                .add_child(nested, NodeSpec::file("file", metrics(changed_size, 3)))
                .unwrap();
            (builder.freeze().unwrap(), [first, second, nested, leaf])
        }
        let (original, nodes) = build(1, false, 7);
        let (replacement, replacement_nodes) = build(2, true, 80);
        let first = SnapshotPresentation::new(&original).unwrap();
        let second = SnapshotPresentation::new(&replacement).unwrap();
        assert_eq!(std::mem::size_of::<ColorFamilyIndex>(), 8);
        for (node, replacement_node) in nodes.into_iter().zip(replacement_nodes) {
            assert_eq!(
                first.color_identity(node).unwrap(),
                second.color_identity(replacement_node).unwrap()
            );
        }
        let [a, b, nested, leaf] = nodes.map(|node| first.color_identity(node).unwrap());
        assert_ne!(a.family_key, b.family_key);
        assert_eq!((a.family_key, a.depth), (nested.family_key, 0));
        assert_eq!((nested.depth, leaf.depth), (1, 2));
        assert_eq!(leaf.family_key, a.family_key);
        assert_ne!(leaf.variant_key, nested.variant_key);
        // A real merge changes arena indices and metrics while retaining native paths.
        let mut builder = TreeBuilder::new(GenerationId::new(3));
        let root = builder.add_root(NodeSpec::root(r"C:\fixture\a")).unwrap();
        let nested = builder.add_child(root, NodeSpec::directory("nested")).unwrap();
        builder.add_child(nested, NodeSpec::file("file", metrics(150, 70))).unwrap();
        let merged = crate::branch_rescan::merge_branch_snapshot(
            &original,
            nodes[0],
            &builder.freeze().unwrap(),
        )
        .unwrap();
        let merged_index = SnapshotPresentation::new(&merged).unwrap();
        for (index, record) in merged.nodes().iter().enumerate() {
            if record.name() == "file" {
                assert_eq!(
                    merged_index.color_identity(NodeId::from_raw(index as u32)).unwrap(),
                    leaf
                );
            }
        }
        assert!(first.color_identity(NodeId::from_raw(999)).is_err());
    }

    #[test]
    fn all_sort_modes_use_the_same_binary_search_order_beyond_six_hundred_rows() {
        let mut builder = TreeBuilder::new(GenerationId::new(6));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        for n in (0..640).rev() {
            let name = if n % 2 == 0 { format!("File-{n:04}") } else { format!("file-{n:04}") };
            builder.add_child(root, NodeSpec::file(name, metrics(n, 639 - n))).unwrap();
        }
        let snapshot = builder.freeze().unwrap();
        for basis in [SizeBasis::Logical, SizeBasis::Allocated] {
            for sort in [ItemSort::Name, ItemSort::Size] {
                for direction in [SortDirection::Ascending, SortDirection::Descending] {
                    let rows = ranked_children_cancellable(
                        &snapshot,
                        root,
                        basis,
                        sort,
                        direction,
                        || false,
                    )
                    .unwrap()
                    .unwrap();
                    let ascending_n = (sort == ItemSort::Name || basis == SizeBasis::Logical)
                        == (direction == SortDirection::Ascending);
                    let expected_first = if ascending_n { "File-0000" } else { "file-0639" };
                    assert_eq!(snapshot.node(rows[0]).unwrap().name(), expected_first);
                    for (index, &node) in rows.iter().enumerate() {
                        assert_eq!(
                            ranked_position(&snapshot, &rows, basis, sort, direction, node),
                            Some(index)
                        );
                    }
                    let filtered: Vec<_> =
                        rows.iter().copied().filter(|node| node.raw() % 3 == 0).collect();
                    for (index, &node) in filtered.iter().enumerate() {
                        assert_eq!(
                            ranked_position(&snapshot, &filtered, basis, sort, direction, node),
                            Some(index)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn folded_name_ties_use_exact_native_name_then_node_identity() {
        let mut builder = TreeBuilder::new(GenerationId::new(7));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let lower = builder.add_child(root, NodeSpec::file("alpha", metrics(1, 1))).unwrap();
        let upper = builder.add_child(root, NodeSpec::file("ALPHA", metrics(1, 1))).unwrap();
        let duplicate = builder.add_child(root, NodeSpec::file("alpha", metrics(1, 1))).unwrap();
        let snapshot = builder.freeze().unwrap();
        for direction in [SortDirection::Ascending, SortDirection::Descending] {
            let rows = ranked_children_cancellable(
                &snapshot,
                root,
                SizeBasis::Allocated,
                ItemSort::Name,
                direction,
                || false,
            )
            .unwrap()
            .unwrap();
            assert_eq!(rows, [upper, lower, duplicate]);
        }
    }
}
