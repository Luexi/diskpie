//! Immutable presentation indices derived from a coherent tree snapshot.

use diskpie_core::{
    EntryKind, GenerationId, NodeId, NodeRecord, TreeSnapshot, sunburst::SizeBasis,
};
use std::{cmp::Ordering, error::Error, ffi::OsStr, fmt};

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const PATH_COMPONENT_MARKER: u8 = 0xFF;

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
}

impl SnapshotPresentation {
    /// Derives every key iteratively in arena order without reconstructing paths.
    pub fn new(snapshot: &TreeSnapshot) -> Result<Self, PresentationError> {
        let mut path_color_keys = Vec::with_capacity(snapshot.len());
        for (index, record) in snapshot.nodes().iter().enumerate() {
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
        }
        Ok(Self { generation: snapshot.generation(), path_color_keys: path_color_keys.into() })
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
    nodes.sort_by(|left, right| compare_nodes(snapshot, *left, *right, basis));
    nodes.truncate(limit);
    Ok(nodes)
}

fn compare_nodes(
    snapshot: &TreeSnapshot,
    left: NodeId,
    right: NodeId,
    basis: SizeBasis,
) -> Ordering {
    let left_record = snapshot.node(left).expect("ranked child is in the validated snapshot");
    let right_record = snapshot.node(right).expect("ranked child is in the validated snapshot");
    let (left_bytes, left_unknown) = selected_size(left_record, basis);
    let (right_bytes, right_unknown) = selected_size(right_record, basis);
    right_bytes
        .cmp(&left_bytes)
        .then_with(|| right_unknown.cmp(&left_unknown))
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
}
