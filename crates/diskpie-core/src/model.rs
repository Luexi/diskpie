//! Compact, portable scan-tree model.
//!
//! The builder is the only mutation surface. A frozen [`TreeSnapshot`] uses
//! stable `u32` indices, exact native names, and precomputed bottom-up totals.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    path::PathBuf,
};

/// Stable index into one immutable tree revision.
///
/// A `NodeId` is meaningful only together with the snapshot's [`GenerationId`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct NodeId(u32);

impl NodeId {
    /// Creates an ID from its wire/storage representation.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the compact wire/storage representation.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[must_use]
    const fn index(self) -> usize {
        self.0 as usize
    }

    fn from_index(index: usize) -> Result<Self, ModelError> {
        u32::try_from(index).map(Self).map_err(|_| ModelError::ArenaExhausted)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Monotonically increasing identity for a scan and all of its snapshots.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct GenerationId(u64);

impl GenerationId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Portable volume identity used as part of hard-link identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct VolumeKey(u128);

impl VolumeKey {
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// Generation-local identity of one filesystem object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileIdentity {
    volume: VolumeKey,
    file_id: u128,
}

impl FileIdentity {
    #[must_use]
    pub const fn new(volume: VolumeKey, file_id: u128) -> Self {
        Self { volume, file_id }
    }

    #[must_use]
    pub const fn volume(self) -> VolumeKey {
        self.volume
    }

    #[must_use]
    pub const fn file_id(self) -> u128 {
        self.file_id
    }
}

/// What kind of entry a reparse point represents without following it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReparseKind {
    File,
    Directory,
    Other,
}

/// Domain kind of a tree node.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EntryKind {
    Root,
    Directory,
    File,
    ReparsePoint(ReparseKind),
    SyntheticGroup,
}

impl EntryKind {
    /// Whether the scanner may attach materialized children to this node.
    #[must_use]
    pub const fn can_have_children(self) -> bool {
        matches!(self, Self::Root | Self::Directory | Self::SyntheticGroup)
    }

    /// Whether this kind is valid without a parent.
    #[must_use]
    pub const fn can_be_top_level(self) -> bool {
        matches!(self, Self::Root | Self::SyntheticGroup)
    }

    const fn contributes_own_bytes(self) -> bool {
        matches!(self, Self::File | Self::ReparsePoint(ReparseKind::File | ReparseKind::Other))
    }

    const fn own_file_count(self) -> u64 {
        if matches!(self, Self::File | Self::ReparsePoint(ReparseKind::File)) { 1 } else { 0 }
    }

    const fn own_directory_count(self) -> u64 {
        if matches!(self, Self::Root | Self::Directory | Self::ReparsePoint(ReparseKind::Directory))
        {
            1
        } else {
            0
        }
    }
}

/// The provider or policy that produced a size value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MetricSource {
    PortableMetadata,
    NativeFileInformation,
    FilesystemAllocation,
    CompressedSizeFallback,
    HardLinkAlias,
    Policy,
}

/// Why a logical or allocated value could not be measured.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnknownReason {
    AccessDenied,
    NotSupported,
    NotAvailable,
    IoError,
    Cancelled,
    IdentityUnavailable,
}

/// One node's measured size, retaining uncertainty and provenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeMetric {
    Known { bytes: u64, source: MetricSource },
    Unknown { reason: UnknownReason, source: MetricSource },
}

impl SizeMetric {
    #[must_use]
    pub const fn known(bytes: u64, source: MetricSource) -> Self {
        Self::Known { bytes, source }
    }

    #[must_use]
    pub const fn unknown(reason: UnknownReason, source: MetricSource) -> Self {
        Self::Unknown { reason, source }
    }

    #[must_use]
    pub const fn policy_zero() -> Self {
        Self::Known { bytes: 0, source: MetricSource::Policy }
    }

    #[must_use]
    pub const fn known_bytes(self) -> Option<u64> {
        match self {
            Self::Known { bytes, .. } => Some(bytes),
            Self::Unknown { .. } => None,
        }
    }

    #[must_use]
    pub const fn source(self) -> MetricSource {
        match self {
            Self::Known { source, .. } | Self::Unknown { source, .. } => source,
        }
    }

    #[must_use]
    pub const fn unknown_reason(self) -> Option<UnknownReason> {
        match self {
            Self::Known { .. } => None,
            Self::Unknown { reason, .. } => Some(reason),
        }
    }
}

/// Logical and allocated contribution owned by one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnMetrics {
    logical: SizeMetric,
    allocated: SizeMetric,
}

impl OwnMetrics {
    pub const ZERO_BY_POLICY: Self =
        Self { logical: SizeMetric::policy_zero(), allocated: SizeMetric::policy_zero() };

    #[must_use]
    pub const fn new(logical: SizeMetric, allocated: SizeMetric) -> Self {
        Self { logical, allocated }
    }

    #[must_use]
    pub const fn logical(self) -> SizeMetric {
        self.logical
    }

    #[must_use]
    pub const fn allocated(self) -> SizeMetric {
        self.allocated
    }
}

/// Completion state at scan or subtree granularity.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ScanState {
    #[default]
    Complete,
    Partial,
    Cancelled,
}

impl ScanState {
    #[must_use]
    pub const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Cancelled, _) | (_, Self::Cancelled) => Self::Cancelled,
            (Self::Partial, _) | (_, Self::Partial) => Self::Partial,
            (Self::Complete, Self::Complete) => Self::Complete,
        }
    }
}

/// Known total plus the number of entries whose value remains unknown.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AggregateSize {
    known_bytes: u128,
    unknown_entries: u64,
}

impl AggregateSize {
    #[must_use]
    pub const fn known_bytes(self) -> u128 {
        self.known_bytes
    }

    #[must_use]
    pub const fn unknown_entries(self) -> u64 {
        self.unknown_entries
    }

    #[must_use]
    pub const fn is_fully_known(self) -> bool {
        self.unknown_entries == 0
    }

    fn from_metric(metric: SizeMetric) -> Self {
        match metric {
            SizeMetric::Known { bytes, .. } => {
                Self { known_bytes: u128::from(bytes), unknown_entries: 0 }
            }
            SizeMetric::Unknown { .. } => Self { known_bytes: 0, unknown_entries: 1 },
        }
    }

    fn checked_add(
        &mut self,
        other: Self,
        node: NodeId,
        bytes_field: AggregateField,
        unknown_field: AggregateField,
    ) -> Result<(), ModelError> {
        let known_bytes = self
            .known_bytes
            .checked_add(other.known_bytes)
            .ok_or(ModelError::AggregateOverflow { node, field: bytes_field })?;
        let unknown_entries = self
            .unknown_entries
            .checked_add(other.unknown_entries)
            .ok_or(ModelError::AggregateOverflow { node, field: unknown_field })?;
        self.known_bytes = known_bytes;
        self.unknown_entries = unknown_entries;
        Ok(())
    }
}

/// Fully aggregated subtree values and counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Aggregate {
    logical: AggregateSize,
    allocated: AggregateSize,
    file_count: u64,
    directory_count: u64,
    omission_count: u64,
    state: ScanState,
}

impl Aggregate {
    #[must_use]
    pub const fn logical(self) -> AggregateSize {
        self.logical
    }

    #[must_use]
    pub const fn allocated(self) -> AggregateSize {
        self.allocated
    }

    #[must_use]
    pub const fn file_count(self) -> u64 {
        self.file_count
    }

    #[must_use]
    pub const fn directory_count(self) -> u64 {
        self.directory_count
    }

    #[must_use]
    pub const fn omission_count(self) -> u64 {
        self.omission_count
    }

    #[must_use]
    pub const fn state(self) -> ScanState {
        self.state
    }

    fn from_node(node: &NodeRecord) -> Self {
        let omission_state =
            if node.own_omissions == 0 { ScanState::Complete } else { ScanState::Partial };
        Self {
            logical: AggregateSize::from_metric(node.own.logical),
            allocated: AggregateSize::from_metric(node.own.allocated),
            file_count: node.kind.own_file_count(),
            directory_count: node.kind.own_directory_count(),
            omission_count: node.own_omissions,
            state: node.local_state.combine(omission_state),
        }
    }

    fn checked_add(&mut self, other: Self, node: NodeId) -> Result<(), ModelError> {
        self.logical.checked_add(
            other.logical,
            node,
            AggregateField::LogicalBytes,
            AggregateField::LogicalUnknownEntries,
        )?;
        self.allocated.checked_add(
            other.allocated,
            node,
            AggregateField::AllocatedBytes,
            AggregateField::AllocatedUnknownEntries,
        )?;
        self.file_count =
            checked_count_add(self.file_count, other.file_count, node, AggregateField::FileCount)?;
        self.directory_count = checked_count_add(
            self.directory_count,
            other.directory_count,
            node,
            AggregateField::DirectoryCount,
        )?;
        self.omission_count = checked_count_add(
            self.omission_count,
            other.omission_count,
            node,
            AggregateField::OmissionCount,
        )?;
        self.state = self.state.combine(other.state);
        Ok(())
    }
}

fn checked_count_add(
    left: u64,
    right: u64,
    node: NodeId,
    field: AggregateField,
) -> Result<u64, ModelError> {
    left.checked_add(right).ok_or(ModelError::AggregateOverflow { node, field })
}

/// Input used to append a node to a [`TreeBuilder`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeSpec {
    name: OsString,
    kind: EntryKind,
    own: OwnMetrics,
    file_identity: Option<FileIdentity>,
    omissions: u64,
    state: ScanState,
}

impl NodeSpec {
    /// Creates a node input.
    ///
    /// Roots, directories, directory reparse points, and synthetic groups are
    /// containers and therefore have their own metrics canonicalized to zero.
    #[must_use]
    pub fn new(name: impl Into<OsString>, kind: EntryKind, own: OwnMetrics) -> Self {
        let own = if kind.contributes_own_bytes() { own } else { OwnMetrics::ZERO_BY_POLICY };
        Self {
            name: name.into(),
            kind,
            own,
            file_identity: None,
            omissions: 0,
            state: ScanState::Complete,
        }
    }

    #[must_use]
    pub fn root(name: impl Into<OsString>) -> Self {
        Self::new(name, EntryKind::Root, OwnMetrics::ZERO_BY_POLICY)
    }

    #[must_use]
    pub fn directory(name: impl Into<OsString>) -> Self {
        Self::new(name, EntryKind::Directory, OwnMetrics::ZERO_BY_POLICY)
    }

    #[must_use]
    pub fn synthetic_group(name: impl Into<OsString>) -> Self {
        Self::new(name, EntryKind::SyntheticGroup, OwnMetrics::ZERO_BY_POLICY)
    }

    #[must_use]
    pub fn file(name: impl Into<OsString>, own: OwnMetrics) -> Self {
        Self::new(name, EntryKind::File, own)
    }

    #[must_use]
    pub fn with_file_identity(mut self, identity: FileIdentity) -> Self {
        self.file_identity = Some(identity);
        self
    }

    #[must_use]
    pub fn with_omissions(mut self, omissions: u64) -> Self {
        self.omissions = omissions;
        if omissions != 0 {
            self.state = self.state.combine(ScanState::Partial);
        }
        self
    }

    #[must_use]
    pub fn with_state(mut self, state: ScanState) -> Self {
        self.state = self.state.combine(state);
        self
    }
}

/// One compact immutable arena record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRecord {
    name: OsString,
    kind: EntryKind,
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    next_sibling: Option<NodeId>,
    own: OwnMetrics,
    aggregate: Aggregate,
    file_identity: Option<FileIdentity>,
    own_omissions: u64,
    local_state: ScanState,
}

impl NodeRecord {
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    #[must_use]
    pub const fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    #[must_use]
    pub const fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    #[must_use]
    pub const fn next_sibling(&self) -> Option<NodeId> {
        self.next_sibling
    }

    #[must_use]
    pub const fn own_metrics(&self) -> OwnMetrics {
        self.own
    }

    #[must_use]
    pub const fn aggregate(&self) -> Aggregate {
        self.aggregate
    }

    #[must_use]
    pub const fn file_identity(&self) -> Option<FileIdentity> {
        self.file_identity
    }

    #[must_use]
    pub const fn own_omissions(&self) -> u64 {
        self.own_omissions
    }

    #[must_use]
    pub const fn local_state(&self) -> ScanState {
        self.local_state
    }
}

#[derive(Debug)]
struct BuildNode {
    record: NodeRecord,
    last_child: Option<NodeId>,
}

/// Mutable append-only arena that freezes into an immutable snapshot.
#[derive(Debug)]
pub struct TreeBuilder {
    generation: GenerationId,
    scan_state: ScanState,
    nodes: Vec<BuildNode>,
}

impl TreeBuilder {
    #[must_use]
    pub const fn new(generation: GenerationId) -> Self {
        Self { generation, scan_state: ScanState::Complete, nodes: Vec::new() }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn set_scan_state(&mut self, state: ScanState) {
        self.scan_state = self.scan_state.combine(state);
    }

    /// Appends a top-level root or synthetic group.
    pub fn add_root(&mut self, spec: NodeSpec) -> Result<NodeId, ModelError> {
        if !spec.kind.can_be_top_level() {
            return Err(ModelError::InvalidRootKind { kind: spec.kind });
        }
        self.push_node(None, spec)
    }

    /// Appends a child and preserves call order in the sibling chain.
    pub fn add_child(&mut self, parent: NodeId, spec: NodeSpec) -> Result<NodeId, ModelError> {
        let parent_index = self.validate_id(parent)?;
        let parent_kind = self.nodes[parent_index].record.kind;
        if !parent_kind.can_have_children() {
            return Err(ModelError::ParentCannotHaveChildren { parent, kind: parent_kind });
        }

        let child = self.push_node(Some(parent), spec)?;
        let previous_last = self.nodes[parent_index].last_child;
        if let Some(previous) = previous_last {
            self.nodes[previous.index()].record.next_sibling = Some(child);
        } else {
            self.nodes[parent_index].record.first_child = Some(child);
        }
        self.nodes[parent_index].last_child = Some(child);
        Ok(child)
    }

    /// Strengthens a node's local completion state.
    pub fn set_node_state(&mut self, node: NodeId, state: ScanState) -> Result<(), ModelError> {
        let index = self.validate_id(node)?;
        self.nodes[index].record.local_state = self.nodes[index].record.local_state.combine(state);
        Ok(())
    }

    /// Adds omissions reported directly under a materialized node.
    pub fn add_omissions(&mut self, node: NodeId, count: u64) -> Result<(), ModelError> {
        let index = self.validate_id(node)?;
        let omissions =
            self.nodes[index].record.own_omissions.checked_add(count).ok_or(
                ModelError::AggregateOverflow { node, field: AggregateField::OmissionCount },
            )?;
        self.nodes[index].record.own_omissions = omissions;
        if count != 0 {
            self.nodes[index].record.local_state =
                self.nodes[index].record.local_state.combine(ScanState::Partial);
        }
        Ok(())
    }

    /// Computes subtree totals iteratively and consumes the builder.
    pub fn freeze(mut self) -> Result<TreeSnapshot, ModelError> {
        for node in &mut self.nodes {
            node.record.aggregate = Aggregate::from_node(&node.record);
        }

        for index in (0..self.nodes.len()).rev() {
            let aggregate = self.nodes[index].record.aggregate;
            if let Some(parent) = self.nodes[index].record.parent {
                let parent_index = parent.index();
                self.nodes[parent_index].record.aggregate.checked_add(aggregate, parent)?;
            }
        }

        let mut snapshot_state = self.scan_state;
        for node in &mut self.nodes {
            if node.record.parent.is_none() {
                node.record.aggregate.state = node.record.aggregate.state.combine(self.scan_state);
                snapshot_state = snapshot_state.combine(node.record.aggregate.state);
            }
        }

        let nodes =
            self.nodes.into_iter().map(|node| node.record).collect::<Vec<_>>().into_boxed_slice();
        let snapshot = TreeSnapshot { generation: self.generation, state: snapshot_state, nodes };
        snapshot.validate_structure().map_err(ModelError::InvalidStructure)?;
        Ok(snapshot)
    }

    fn push_node(&mut self, parent: Option<NodeId>, spec: NodeSpec) -> Result<NodeId, ModelError> {
        let id = NodeId::from_index(self.nodes.len())?;
        self.nodes.push(BuildNode {
            record: NodeRecord {
                name: spec.name,
                kind: spec.kind,
                parent,
                first_child: None,
                next_sibling: None,
                own: spec.own,
                aggregate: Aggregate::default(),
                file_identity: spec.file_identity,
                own_omissions: spec.omissions,
                local_state: spec.state,
            },
            last_child: None,
        });
        Ok(id)
    }

    fn validate_id(&self, id: NodeId) -> Result<usize, ModelError> {
        let index = id.index();
        if index < self.nodes.len() { Ok(index) } else { Err(ModelError::InvalidNodeId { id }) }
    }
}

/// Immutable tree revision shared by application and renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeSnapshot {
    generation: GenerationId,
    state: ScanState,
    nodes: Box<[NodeRecord]>,
}

impl TreeSnapshot {
    #[must_use]
    pub const fn generation(&self) -> GenerationId {
        self.generation
    }

    #[must_use]
    pub const fn state(&self) -> ScanState {
        self.state
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[must_use]
    pub fn nodes(&self) -> &[NodeRecord] {
        &self.nodes
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&NodeRecord> {
        self.nodes.get(id.index())
    }

    /// Iterates top-level roots and synthetic groups in insertion order.
    pub fn roots(&self) -> impl Iterator<Item = (NodeId, &NodeRecord)> + '_ {
        self.nodes.iter().enumerate().filter_map(|(index, node)| {
            if node.parent.is_none() {
                // A valid snapshot cannot contain more than the NodeId range.
                NodeId::from_index(index).ok().map(|id| (id, node))
            } else {
                None
            }
        })
    }

    /// Iterates direct children in deterministic insertion order.
    pub fn children(&self, parent: NodeId) -> Result<Children<'_>, ModelError> {
        let parent = self.node(parent).ok_or(ModelError::InvalidNodeId { id: parent })?;
        Ok(Children { nodes: &self.nodes, next: parent.first_child })
    }

    /// Reconstructs a native path only when requested.
    pub fn path(&self, id: NodeId) -> Result<PathBuf, ModelError> {
        let mut path = PathBuf::new();
        self.path_into(id, &mut path)?;
        Ok(path)
    }

    /// Reuses `path` storage while reconstructing a native path iteratively.
    ///
    /// Synthetic ancestors above a real [`EntryKind::Root`] are excluded so a
    /// multi-root presentation group never becomes a filesystem component.
    pub fn path_into(&self, id: NodeId, path: &mut PathBuf) -> Result<(), ModelError> {
        let mut components = Vec::new();
        let mut cursor = id;
        loop {
            let node = self.node(cursor).ok_or(ModelError::InvalidNodeId { id: cursor })?;
            components.push(node.name.as_os_str());
            if node.kind == EntryKind::Root {
                break;
            }
            match node.parent {
                Some(parent) => cursor = parent,
                None => break,
            }
        }

        path.clear();
        for component in components.into_iter().rev() {
            path.push(component);
        }
        Ok(())
    }

    /// Checks all arena references, parent order, sibling order, and reachability.
    pub fn validate_structure(&self) -> Result<(), StructureError> {
        if let Some(last_index) = self.nodes.len().checked_sub(1)
            && NodeId::from_index(last_index).is_err()
        {
            return Err(StructureError::TooManyNodes { count: self.nodes.len() });
        }

        for (index, node) in self.nodes.iter().enumerate() {
            let id = NodeId::from_index(index)
                .map_err(|_| StructureError::TooManyNodes { count: self.nodes.len() })?;
            if let Some(parent) = node.parent {
                let parent_index = self.reference_index(id, LinkField::Parent, parent)?;
                if parent_index >= index {
                    return Err(StructureError::ParentMustPrecede { node: id, parent });
                }
                let parent_kind = self.nodes[parent_index].kind;
                if !parent_kind.can_have_children() {
                    return Err(StructureError::ParentCannotHaveChildren {
                        node: id,
                        parent,
                        kind: parent_kind,
                    });
                }
            } else {
                if !node.kind.can_be_top_level() {
                    return Err(StructureError::InvalidTopLevelKind { node: id, kind: node.kind });
                }
                if let Some(sibling) = node.next_sibling {
                    return Err(StructureError::TopLevelSiblingLink { node: id, sibling });
                }
            }
        }

        let mut seen_as_child = vec![false; self.nodes.len()];
        for (parent_index, parent_node) in self.nodes.iter().enumerate() {
            let parent = NodeId::from_index(parent_index)
                .map_err(|_| StructureError::TooManyNodes { count: self.nodes.len() })?;
            let mut previous = None;
            let mut next = parent_node.first_child;
            let mut link_field = LinkField::FirstChild;
            while let Some(child) = next {
                let child_index = self.reference_index(parent, link_field, child)?;
                let child_node = &self.nodes[child_index];
                if child_node.parent != Some(parent) {
                    return Err(StructureError::ChildParentMismatch {
                        parent,
                        child,
                        actual_parent: child_node.parent,
                    });
                }
                if seen_as_child[child_index] {
                    return Err(StructureError::DuplicateChildLink { child });
                }
                if let Some(previous_id) = previous
                    && previous_id >= child
                {
                    return Err(StructureError::SiblingOrder {
                        parent,
                        previous: previous_id,
                        child,
                    });
                }
                seen_as_child[child_index] = true;
                previous = Some(child);
                next = child_node.next_sibling;
                link_field = LinkField::NextSibling;
            }
        }

        for (index, node) in self.nodes.iter().enumerate() {
            if node.parent.is_some() && !seen_as_child[index] {
                let node = NodeId::from_index(index)
                    .map_err(|_| StructureError::TooManyNodes { count: self.nodes.len() })?;
                return Err(StructureError::OrphanedChild { node });
            }
        }
        Ok(())
    }

    fn reference_index(
        &self,
        owner: NodeId,
        field: LinkField,
        target: NodeId,
    ) -> Result<usize, StructureError> {
        let index = target.index();
        if index < self.nodes.len() {
            Ok(index)
        } else {
            Err(StructureError::InvalidReference { owner, field, target })
        }
    }
}

/// Direct-child iterator over one immutable snapshot.
#[derive(Clone, Debug)]
pub struct Children<'a> {
    nodes: &'a [NodeRecord],
    next: Option<NodeId>,
}

impl<'a> Iterator for Children<'a> {
    type Item = (NodeId, &'a NodeRecord);

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        let node = self.nodes.get(id.index())?;
        self.next = node.next_sibling;
        Some((id, node))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.nodes.len()))
    }
}

/// Aggregate field that could not be represented without wrapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateField {
    LogicalBytes,
    LogicalUnknownEntries,
    AllocatedBytes,
    AllocatedUnknownEntries,
    FileCount,
    DirectoryCount,
    OmissionCount,
}

/// Builder/freeze failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    ArenaExhausted,
    InvalidNodeId { id: NodeId },
    InvalidRootKind { kind: EntryKind },
    ParentCannotHaveChildren { parent: NodeId, kind: EntryKind },
    AggregateOverflow { node: NodeId, field: AggregateField },
    InvalidStructure(StructureError),
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArenaExhausted => formatter.write_str("the u32 node arena is exhausted"),
            Self::InvalidNodeId { id } => write!(formatter, "node ID {id} is not in the arena"),
            Self::InvalidRootKind { kind } => {
                write!(formatter, "entry kind {kind:?} cannot be a top-level root")
            }
            Self::ParentCannotHaveChildren { parent, kind } => {
                write!(formatter, "node {parent} of kind {kind:?} cannot have children")
            }
            Self::AggregateOverflow { node, field } => {
                write!(formatter, "aggregate field {field:?} overflowed at node {node}")
            }
            Self::InvalidStructure(error) => write!(formatter, "invalid tree structure: {error}"),
        }
    }
}

impl Error for ModelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidStructure(error) => Some(error),
            _ => None,
        }
    }
}

/// Arena link field used in structural diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkField {
    Parent,
    FirstChild,
    NextSibling,
}

/// Failure found while validating an immutable snapshot's topology.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructureError {
    TooManyNodes { count: usize },
    InvalidReference { owner: NodeId, field: LinkField, target: NodeId },
    ParentMustPrecede { node: NodeId, parent: NodeId },
    ParentCannotHaveChildren { node: NodeId, parent: NodeId, kind: EntryKind },
    InvalidTopLevelKind { node: NodeId, kind: EntryKind },
    TopLevelSiblingLink { node: NodeId, sibling: NodeId },
    ChildParentMismatch { parent: NodeId, child: NodeId, actual_parent: Option<NodeId> },
    DuplicateChildLink { child: NodeId },
    SiblingOrder { parent: NodeId, previous: NodeId, child: NodeId },
    OrphanedChild { node: NodeId },
}

impl fmt::Display for StructureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyNodes { count } => write!(formatter, "{count} nodes exceed the u32 arena"),
            Self::InvalidReference { owner, field, target } => {
                write!(formatter, "node {owner} has invalid {field:?} reference {target}")
            }
            Self::ParentMustPrecede { node, parent } => {
                write!(formatter, "parent {parent} does not precede child {node}")
            }
            Self::ParentCannotHaveChildren { node, parent, kind } => write!(
                formatter,
                "node {node} names {parent} ({kind:?}) as a parent that cannot have children"
            ),
            Self::InvalidTopLevelKind { node, kind } => {
                write!(formatter, "top-level node {node} has invalid kind {kind:?}")
            }
            Self::TopLevelSiblingLink { node, sibling } => {
                write!(formatter, "top-level node {node} links sibling {sibling}")
            }
            Self::ChildParentMismatch { parent, child, actual_parent } => write!(
                formatter,
                "parent {parent} links child {child}, whose parent is {actual_parent:?}"
            ),
            Self::DuplicateChildLink { child } => {
                write!(formatter, "child {child} occurs more than once in sibling chains")
            }
            Self::SiblingOrder { parent, previous, child } => write!(
                formatter,
                "parent {parent} has non-increasing siblings {previous} then {child}"
            ),
            Self::OrphanedChild { node } => {
                write!(formatter, "node {node} has a parent but is absent from its child chain")
            }
        }
    }
}

impl Error for StructureError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const LOGICAL_SOURCE: MetricSource = MetricSource::NativeFileInformation;
    const ALLOCATED_SOURCE: MetricSource = MetricSource::FilesystemAllocation;

    fn metrics(logical: u64, allocated: u64) -> OwnMetrics {
        OwnMetrics::new(
            SizeMetric::known(logical, LOGICAL_SOURCE),
            SizeMetric::known(allocated, ALLOCATED_SOURCE),
        )
    }

    fn complete_file(name: impl Into<OsString>, bytes: u64) -> NodeSpec {
        NodeSpec::file(name, metrics(bytes, bytes))
    }

    #[test]
    fn preserves_unicode_emoji_names_and_reconstructs_paths_lazily() {
        let mut builder = TreeBuilder::new(GenerationId::new(41));
        let root = builder.add_root(NodeSpec::root("C:\\datos")).expect("root");
        let directory =
            builder.add_child(root, NodeSpec::directory("📁-日本語-é")).expect("directory");
        let file = builder.add_child(directory, complete_file("résumé-🚀.bin", 7)).expect("file");

        let snapshot = builder.freeze().expect("snapshot");
        assert_eq!(snapshot.generation(), GenerationId::new(41));
        assert_eq!(snapshot.node(directory).expect("directory").name(), OsStr::new("📁-日本語-é"));
        assert_eq!(
            snapshot.path(file).expect("path"),
            PathBuf::from("C:\\datos").join("📁-日本語-é").join("résumé-🚀.bin")
        );

        let mut reused = PathBuf::from("stale");
        snapshot.path_into(file, &mut reused).expect("reused path");
        assert_eq!(reused, snapshot.path(file).expect("path again"));
    }

    #[test]
    fn reconstructs_very_deep_paths_and_aggregates_without_recursion() {
        const DEPTH: usize = 10_000;
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let mut current = builder.add_root(NodeSpec::root("root")).expect("root");
        for _ in 0..DEPTH {
            current = builder.add_child(current, NodeSpec::directory("d")).expect("directory");
        }
        let file = builder.add_child(current, complete_file("leaf", 1)).expect("file");

        let snapshot = builder.freeze().expect("deep tree");
        let path = snapshot.path(file).expect("deep path");
        assert_eq!(path.components().count(), DEPTH + 2);
        assert_eq!(
            snapshot.node(NodeId::from_raw(0)).expect("root").aggregate().logical().known_bytes(),
            1
        );
    }

    #[test]
    fn u128_totals_preserve_values_beyond_u64_and_four_gibibytes() {
        let mut builder = TreeBuilder::new(GenerationId::new(2));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        builder.add_child(root, complete_file("huge-a", u64::MAX)).expect("first file");
        builder.add_child(root, complete_file("huge-b", u64::MAX)).expect("second file");
        builder
            .add_child(root, complete_file("over-4-gib", 5 * 1024 * 1024 * 1024))
            .expect("third file");

        let snapshot = builder.freeze().expect("snapshot");
        let total = snapshot.node(root).expect("root").aggregate().logical().known_bytes();
        assert_eq!(total, u128::from(u64::MAX) * 2 + 5_u128 * 1024 * 1024 * 1024);
        assert!(total > u128::from(u64::MAX));
    }

    #[test]
    fn unknown_metrics_keep_reason_source_and_propagate_counts() {
        let unknown_logical =
            SizeMetric::unknown(UnknownReason::AccessDenied, MetricSource::NativeFileInformation);
        let unknown_allocated =
            SizeMetric::unknown(UnknownReason::NotSupported, MetricSource::FilesystemAllocation);
        let mut builder = TreeBuilder::new(GenerationId::new(3));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let first = builder
            .add_child(
                root,
                NodeSpec::file(
                    "first",
                    OwnMetrics::new(unknown_logical, SizeMetric::known(9, ALLOCATED_SOURCE)),
                ),
            )
            .expect("first");
        builder
            .add_child(
                root,
                NodeSpec::file(
                    "second",
                    OwnMetrics::new(SizeMetric::known(11, LOGICAL_SOURCE), unknown_allocated),
                ),
            )
            .expect("second");

        let snapshot = builder.freeze().expect("snapshot");
        let first_metric = snapshot.node(first).expect("first").own_metrics().logical();
        assert_eq!(first_metric.unknown_reason(), Some(UnknownReason::AccessDenied));
        assert_eq!(first_metric.source(), MetricSource::NativeFileInformation);

        let aggregate = snapshot.node(root).expect("root").aggregate();
        assert_eq!(aggregate.logical().known_bytes(), 11);
        assert_eq!(aggregate.logical().unknown_entries(), 1);
        assert_eq!(aggregate.allocated().known_bytes(), 9);
        assert_eq!(aggregate.allocated().unknown_entries(), 1);
        assert!(!aggregate.logical().is_fully_known());
    }

    #[test]
    fn insertion_order_drives_first_child_and_sibling_links_deterministically() {
        let mut builder = TreeBuilder::new(GenerationId::new(4));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let beta = builder.add_child(root, complete_file("beta", 1)).expect("beta");
        let alpha = builder.add_child(root, complete_file("alpha", 1)).expect("alpha");
        let gamma = builder.add_child(root, complete_file("gamma", 1)).expect("gamma");

        let snapshot = builder.freeze().expect("snapshot");
        assert_eq!(snapshot.node(root).expect("root").first_child(), Some(beta));
        assert_eq!(snapshot.node(beta).expect("beta").next_sibling(), Some(alpha));
        assert_eq!(snapshot.node(alpha).expect("alpha").next_sibling(), Some(gamma));
        assert_eq!(snapshot.node(gamma).expect("gamma").next_sibling(), None);

        let children = snapshot
            .children(root)
            .expect("children")
            .map(|(id, node)| (id, node.name().to_os_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            children,
            vec![
                (beta, OsString::from("beta")),
                (alpha, OsString::from("alpha")),
                (gamma, OsString::from("gamma")),
            ]
        );
        snapshot.validate_structure().expect("valid structure");
    }

    #[test]
    fn builder_and_snapshot_reject_invalid_ids_and_invalid_parents() {
        let mut builder = TreeBuilder::new(GenerationId::new(5));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let file = builder.add_child(root, complete_file("file", 1)).expect("file");
        let invalid = NodeId::from_raw(999);
        assert_eq!(
            builder.add_child(invalid, NodeSpec::directory("missing")),
            Err(ModelError::InvalidNodeId { id: invalid })
        );
        assert_eq!(
            builder.add_child(file, complete_file("impossible", 1)),
            Err(ModelError::ParentCannotHaveChildren { parent: file, kind: EntryKind::File })
        );
        assert_eq!(
            builder.add_root(complete_file("not-root", 1)),
            Err(ModelError::InvalidRootKind { kind: EntryKind::File })
        );

        let snapshot = builder.freeze().expect("snapshot");
        assert!(snapshot.node(invalid).is_none());
        assert!(matches!(
            snapshot.children(invalid),
            Err(ModelError::InvalidNodeId { id }) if id == invalid
        ));
        assert_eq!(snapshot.path(invalid), Err(ModelError::InvalidNodeId { id: invalid }));
    }

    #[test]
    fn partial_and_cancelled_states_propagate_without_erasing_complete_subtrees() {
        let mut partial_builder = TreeBuilder::new(GenerationId::new(6));
        let root = partial_builder.add_root(NodeSpec::root("root")).expect("root");
        let complete = partial_builder
            .add_child(root, NodeSpec::directory("complete"))
            .expect("complete directory");
        let partial = partial_builder
            .add_child(root, NodeSpec::directory("partial").with_omissions(2))
            .expect("partial directory");
        let partial_snapshot = partial_builder.freeze().expect("partial snapshot");
        assert_eq!(partial_snapshot.state(), ScanState::Partial);
        assert_eq!(
            partial_snapshot.node(complete).expect("complete").aggregate().state(),
            ScanState::Complete
        );
        assert_eq!(
            partial_snapshot.node(partial).expect("partial").aggregate().state(),
            ScanState::Partial
        );
        assert_eq!(
            partial_snapshot.node(root).expect("root").aggregate().state(),
            ScanState::Partial
        );
        assert_eq!(partial_snapshot.node(root).expect("root").aggregate().omission_count(), 2);

        let mut cancelled_builder = TreeBuilder::new(GenerationId::new(7));
        let cancelled_root = cancelled_builder.add_root(NodeSpec::root("root")).expect("root");
        let complete_child = cancelled_builder
            .add_child(cancelled_root, NodeSpec::directory("finished"))
            .expect("finished");
        cancelled_builder.set_scan_state(ScanState::Cancelled);
        let cancelled = cancelled_builder.freeze().expect("cancelled snapshot");
        assert_eq!(cancelled.state(), ScanState::Cancelled);
        assert_eq!(
            cancelled.node(cancelled_root).expect("root").aggregate().state(),
            ScanState::Cancelled
        );
        assert_eq!(
            cancelled.node(complete_child).expect("child").aggregate().state(),
            ScanState::Complete
        );
    }

    #[test]
    fn counts_files_directories_and_omissions_by_subtree() {
        let mut builder = TreeBuilder::new(GenerationId::new(8));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let directory = builder
            .add_child(root, NodeSpec::directory("directory").with_omissions(3))
            .expect("directory");
        builder.add_child(directory, complete_file("file", 1)).expect("file");
        builder
            .add_child(
                directory,
                NodeSpec::new("link", EntryKind::ReparsePoint(ReparseKind::File), metrics(2, 2)),
            )
            .expect("reparse file");

        let snapshot = builder.freeze().expect("snapshot");
        let aggregate = snapshot.node(root).expect("root").aggregate();
        assert_eq!(aggregate.file_count(), 2);
        assert_eq!(aggregate.directory_count(), 2);
        assert_eq!(aggregate.omission_count(), 3);
    }

    #[test]
    fn containers_always_contribute_zero_own_bytes() {
        let nonzero = metrics(123, 456);
        for kind in [
            EntryKind::Root,
            EntryKind::Directory,
            EntryKind::ReparsePoint(ReparseKind::Directory),
            EntryKind::SyntheticGroup,
        ] {
            let spec = NodeSpec::new("container", kind, nonzero);
            assert_eq!(spec.own, OwnMetrics::ZERO_BY_POLICY);
        }
    }

    #[test]
    fn file_identity_is_portable_and_optional() {
        let identity = FileIdentity::new(VolumeKey::new(0x1234), u128::MAX - 7);
        let mut builder = TreeBuilder::new(GenerationId::new(9));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let identified = builder
            .add_child(root, complete_file("identified", 1).with_file_identity(identity))
            .expect("identified");
        let anonymous = builder.add_child(root, complete_file("anonymous", 1)).expect("anonymous");
        let snapshot = builder.freeze().expect("snapshot");

        assert_eq!(snapshot.node(identified).expect("identified").file_identity(), Some(identity));
        assert_eq!(identity.volume().get(), 0x1234);
        assert_eq!(identity.file_id(), u128::MAX - 7);
        assert_eq!(snapshot.node(anonymous).expect("anonymous").file_identity(), None);
    }

    #[test]
    fn aggregate_addition_checks_instead_of_wrapping() {
        let node = NodeId::from_raw(0);
        let mut size = AggregateSize { known_bytes: u128::MAX, unknown_entries: 0 };
        let error = size
            .checked_add(
                AggregateSize { known_bytes: 1, unknown_entries: 0 },
                node,
                AggregateField::LogicalBytes,
                AggregateField::LogicalUnknownEntries,
            )
            .expect_err("overflow must be reported");
        assert_eq!(
            error,
            ModelError::AggregateOverflow { node, field: AggregateField::LogicalBytes }
        );
        assert_eq!(size.known_bytes, u128::MAX);

        let mut unknowns = AggregateSize { known_bytes: 17, unknown_entries: u64::MAX };
        let error = unknowns
            .checked_add(
                AggregateSize { known_bytes: 5, unknown_entries: 1 },
                node,
                AggregateField::LogicalBytes,
                AggregateField::LogicalUnknownEntries,
            )
            .expect_err("unknown counter overflow must be reported");
        assert_eq!(
            error,
            ModelError::AggregateOverflow { node, field: AggregateField::LogicalUnknownEntries }
        );
        assert_eq!(
            unknowns,
            AggregateSize { known_bytes: 17, unknown_entries: u64::MAX },
            "a failed checked addition must not partially update the total"
        );
    }

    #[test]
    fn structural_validation_detects_broken_parent_reference() {
        let mut builder = TreeBuilder::new(GenerationId::new(10));
        let root = builder.add_root(NodeSpec::root("root")).expect("root");
        let child = builder.add_child(root, NodeSpec::directory("child")).expect("child");
        let mut snapshot = builder.freeze().expect("snapshot");
        snapshot.nodes[child.index()].parent = Some(NodeId::from_raw(900));

        assert_eq!(
            snapshot.validate_structure(),
            Err(StructureError::InvalidReference {
                owner: child,
                field: LinkField::Parent,
                target: NodeId::from_raw(900),
            })
        );
    }

    #[test]
    fn synthetic_group_is_not_inserted_into_real_root_path() {
        let mut builder = TreeBuilder::new(GenerationId::new(11));
        let group = builder.add_root(NodeSpec::synthetic_group("All volumes")).expect("group");
        let root = builder.add_child(group, NodeSpec::root("C:\\")).expect("root");
        let file = builder.add_child(root, complete_file("file.bin", 1)).expect("file");
        let snapshot = builder.freeze().expect("snapshot");

        let path = snapshot.path(file).expect("path");
        assert_eq!(path, Path::new("C:\\").join("file.bin"));
        assert!(!path.to_string_lossy().contains("All volumes"));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn node_id_conversion_reports_arena_exhaustion() {
        assert_eq!(NodeId::from_index(u32::MAX as usize).expect("last ID").raw(), u32::MAX);
        assert_eq!(NodeId::from_index(u32::MAX as usize + 1), Err(ModelError::ArenaExhausted));
    }
}
