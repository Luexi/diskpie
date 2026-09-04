//! Deterministic, renderer-independent sunburst layout and polar hit testing.
//!
//! The engine consumes an immutable scan-tree snapshot and emits normalized
//! geometry.  It performs every tree walk iteratively, keeps byte arithmetic
//! in `u128`, and never attaches a filesystem target to synthetic sectors.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, VecDeque},
    error::Error,
    fmt,
    iter::FromIterator,
    ops::Range,
};

use crate::model::{EntryKind, GenerationId, NodeId, NodeRecord, TreeSnapshot};

/// One complete turn in radians.
pub const FULL_TURN: f64 = std::f64::consts::TAU;

/// Aggregate size used as angular weight.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SizeBasis {
    /// Logical file bytes.
    Logical,
    /// Bytes allocated by the filesystem.
    Allocated,
}

/// Persistent, generation-local set of branches hidden by the user.
///
/// Hiding a parent does not discard separately hidden descendants. Restoring
/// that parent therefore reveals any descendant `Hidden` sectors again. IDs
/// absent from a newer snapshot are harmless and remain available in case the
/// application restores a compatible snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HiddenBranches {
    nodes: BTreeSet<NodeId>,
}

impl HiddenBranches {
    /// Creates an empty hidden-branch set.
    #[must_use]
    pub const fn new() -> Self {
        Self { nodes: BTreeSet::new() }
    }

    /// Returns the number of tracked hidden roots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns whether no branch is hidden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns whether `node` is explicitly hidden.
    #[must_use]
    pub fn contains(&self, node: NodeId) -> bool {
        self.nodes.contains(&node)
    }

    /// Hides `node`, returning whether it was newly inserted.
    pub fn hide(&mut self, node: NodeId) -> bool {
        self.nodes.insert(node)
    }

    /// Restores `node`, returning whether it had been hidden.
    pub fn restore(&mut self, node: NodeId) -> bool {
        self.nodes.remove(&node)
    }

    /// Toggles `node` and returns its new hidden state.
    pub fn toggle(&mut self, node: NodeId) -> bool {
        if self.restore(node) {
            false
        } else {
            self.hide(node);
            true
        }
    }

    /// Removes all hidden roots.
    pub fn clear(&mut self) {
        self.nodes.clear();
    }

    /// Iterates hidden roots in increasing `NodeId` order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = NodeId> + '_ {
        self.nodes.iter().copied()
    }
}

impl FromIterator<NodeId> for HiddenBranches {
    fn from_iter<T: IntoIterator<Item = NodeId>>(iter: T) -> Self {
        Self { nodes: BTreeSet::from_iter(iter) }
    }
}

impl Extend<NodeId> for HiddenBranches {
    fn extend<T: IntoIterator<Item = NodeId>>(&mut self, iter: T) {
        self.nodes.extend(iter);
    }
}

/// Parameters for one immutable sunburst layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutOptions {
    /// Node displayed in the center and used as the zoom root.
    pub root: NodeId,
    /// Aggregate size used as weight.
    pub size_basis: SizeBasis,
    /// Clockwise start angle in radians; zero points right in screen space.
    pub start_angle: f64,
    /// Radius of the central root sector in normalized chart coordinates.
    pub center_radius: f64,
    /// Maximum number of descendant rings. The center has depth zero.
    pub max_depth: u32,
    /// Smallest desired child sweep in radians before threshold grouping.
    pub minimum_sweep: f64,
    /// Hard cap on all sectors, including the central root sector.
    pub max_sectors: usize,
}

impl LayoutOptions {
    /// Creates options with interactive defaults for `root`.
    #[must_use]
    pub const fn new(root: NodeId) -> Self {
        Self {
            root,
            size_basis: SizeBasis::Logical,
            start_angle: -std::f64::consts::FRAC_PI_2,
            center_radius: 0.2,
            max_depth: 8,
            minimum_sweep: 0.002,
            max_sectors: 10_000,
        }
    }
}

/// Stable range into a layout's synthetic-member pool.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MemberRange {
    start: usize,
    len: usize,
}

impl MemberRange {
    /// Offset of the first member in the layout pool.
    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    /// Number of IDs represented by the synthetic sector.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether the range contains no IDs.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    fn end(self) -> Option<usize> {
        self.start.checked_add(self.len)
    }
}

/// Why real siblings were combined into an `Other` sector.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OtherCause {
    /// Every represented real sibling fell below `minimum_sweep`.
    MinimumSweep,
    /// The global sector budget required combining the remaining siblings.
    SectorBudget,
    /// Budget grouping absorbed an existing minimum-sweep group.
    MinimumSweepAndSectorBudget,
}

/// Semantic state of one sector.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SectorKind {
    /// A real node. This is the only actionable sector kind.
    Real,
    /// One hidden branch represented neutrally and without a path target.
    Hidden {
        /// Range containing the hidden root ID, used only for restoration.
        members: MemberRange,
    },
    /// Real or hidden siblings combined without inventing a `NodeId`.
    Other {
        /// Real parent that owns the represented siblings.
        parent: NodeId,
        /// Range of underlying direct-child IDs in the layout member pool.
        members: MemberRange,
        /// Policy that caused aggregation.
        cause: OtherCause,
    },
}

impl SectorKind {
    /// Returns the synthetic-member range, if this is `Hidden` or `Other`.
    #[must_use]
    pub const fn members(self) -> Option<MemberRange> {
        match self {
            Self::Real => None,
            Self::Hidden { members } | Self::Other { members, .. } => Some(members),
        }
    }
}

/// Backend-neutral geometry and weight for one displayed sector.
///
/// Angles are normalized to `[0, TAU)`, sweeps to `[0, TAU]`, and radii to
/// `[0, 1]`. Angular intervals are half-open. Radial intervals are also
/// half-open except for the outer edge of the deepest displayed ring.
#[derive(Clone, Debug, PartialEq)]
pub struct Sector {
    /// Filesystem node for a real sector; always `None` for synthetic sectors.
    pub node_id: Option<NodeId>,
    /// Semantic state and, for synthetic sectors, member metadata.
    pub kind: SectorKind,
    /// Ring depth relative to the selected root; the center is zero.
    pub depth: u32,
    /// Clockwise normalized start angle in radians.
    pub start_angle: f64,
    /// Clockwise angular span in radians.
    pub sweep_angle: f64,
    /// Inclusive normalized inner radius.
    pub inner_radius: f64,
    /// Normally exclusive normalized outer radius.
    pub outer_radius: f64,
    /// Exact known-byte weight selected for this layout.
    pub weight: u128,
    /// Number of entries whose selected metric is unknown.
    pub unknown_entries: u64,
}

impl Sector {
    /// Returns the normalized angular end of the sector.
    #[must_use]
    pub fn end_angle(&self) -> f64 {
        normalize_angle(self.start_angle + self.sweep_angle)
    }

    /// Returns the only node ID that may be used for filesystem actions.
    ///
    /// Callers should use this method, rather than member or parent metadata,
    /// at the action boundary. It always returns `None` for `Hidden` and
    /// `Other`, even for a manually cloned and modified record.
    #[must_use]
    pub const fn action_target(&self) -> Option<NodeId> {
        match self.kind {
            SectorKind::Real => self.node_id,
            SectorKind::Hidden { .. } | SectorKind::Other { .. } => None,
        }
    }

    /// Returns whether this sector may resolve to a filesystem action target.
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        self.action_target().is_some()
    }

    fn contains_angle(&self, angle: f64) -> bool {
        if self.sweep_angle >= FULL_TURN {
            return true;
        }
        let relative = normalize_angle(angle - self.start_angle);
        relative < self.sweep_angle
    }
}

/// Contiguous sector index and radial bounds for one depth.
#[derive(Clone, Debug, PartialEq)]
pub struct Ring {
    /// Depth relative to the selected root.
    pub depth: u32,
    /// Inclusive normalized inner radius.
    pub inner_radius: f64,
    /// Normally exclusive normalized outer radius.
    pub outer_radius: f64,
    /// Sectors sorted by normalized start angle.
    pub sectors: Range<usize>,
}

/// Result of one deterministic layout computation.
#[derive(Clone, Debug, PartialEq)]
pub struct SunburstLayout {
    source_generation: GenerationId,
    root: NodeId,
    size_basis: SizeBasis,
    sectors: Vec<Sector>,
    rings: Vec<Ring>,
    member_pool: Vec<NodeId>,
    zero_weight_nodes: Vec<NodeId>,
    budget_exhausted: bool,
}

impl SunburstLayout {
    /// Generation of the source tree snapshot.
    #[must_use]
    pub const fn source_generation(&self) -> GenerationId {
        self.source_generation
    }

    /// Node displayed at the center.
    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Aggregate size used for angular weights.
    #[must_use]
    pub const fn size_basis(&self) -> SizeBasis {
        self.size_basis
    }

    /// Displayed sectors, grouped by depth and sorted by start angle per ring.
    #[must_use]
    pub fn sectors(&self) -> &[Sector] {
        &self.sectors
    }

    /// Radial index used by hit testing.
    #[must_use]
    pub fn rings(&self) -> &[Ring] {
        &self.rings
    }

    /// Direct children of the zoom root skipped because their selected
    /// known-byte weight is zero.
    ///
    /// This includes fully unknown children, which cannot receive a meaningful
    /// angle but remain available to a synchronized textual representation.
    /// Only the zoom root's own children are recorded, and at most
    /// [`LayoutOptions::max_sectors`] of them, so the list is bounded by the
    /// same budget as the geometry regardless of directory width.
    #[must_use]
    pub fn zero_weight_nodes(&self) -> &[NodeId] {
        &self.zero_weight_nodes
    }

    /// Returns whether eligible geometry was coalesced or omitted by the cap.
    #[must_use]
    pub const fn budget_exhausted(&self) -> bool {
        self.budget_exhausted
    }

    /// Resolves a synthetic sector's stable member range.
    ///
    /// A real sector returns `None`. The returned IDs are for restoration and
    /// textual inspection, never an implicit destructive-action target.
    #[must_use]
    pub fn members(&self, sector: &Sector) -> Option<&[NodeId]> {
        let range = sector.kind.members()?;
        let end = range.end()?;
        self.member_pool.get(range.start..end)
    }

    /// Hit-tests a point in normalized chart coordinates.
    ///
    /// `x` grows rightward and `y` downward, matching screen coordinates.
    /// The chart center is `(0, 0)` and its configured outer radius is at most
    /// one. The method performs no mutation or rendering work.
    pub fn hit_test(&self, x: f64, y: f64) -> Result<Option<&Sector>, HitTestError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(HitTestError::NonFinitePoint { x, y });
        }

        let radius = x.hypot(y);
        if !radius.is_finite() {
            return Ok(None);
        }
        let angle = normalize_angle(y.atan2(x));
        self.hit_test_polar(angle, radius)
    }

    /// Hit-tests a normalized radius and any finite angular representation.
    ///
    /// Angles wrap modulo `TAU`, so zero, `TAU`, negative turns, and multiple
    /// turns have identical ownership. Exact angular boundaries belong to the
    /// sector beginning at that boundary.
    pub fn hit_test_polar(&self, angle: f64, radius: f64) -> Result<Option<&Sector>, HitTestError> {
        if !angle.is_finite() {
            return Err(HitTestError::NonFiniteAngle { angle });
        }
        if !radius.is_finite() || radius < 0.0 {
            return Err(HitTestError::InvalidRadius { radius });
        }
        if self.rings.is_empty() {
            return Ok(None);
        }

        let last_ring = self.rings.len() - 1;
        let outer_radius = self.rings[last_ring].outer_radius;
        if radius > outer_radius {
            return Ok(None);
        }

        let insertion = self.rings.partition_point(|ring| ring.inner_radius <= radius);
        let Some(ring_index) = insertion.checked_sub(1) else {
            return Ok(None);
        };
        let ring = &self.rings[ring_index];
        let owns_outer_edge = ring_index == last_ring;
        if radius > ring.outer_radius || (radius == ring.outer_radius && !owns_outer_edge) {
            return Ok(None);
        }

        let sectors = &self.sectors[ring.sectors.clone()];
        if sectors.is_empty() {
            return Ok(None);
        }
        let angle = normalize_angle(angle);
        let insertion = sectors.partition_point(|sector| sector.start_angle <= angle);
        let candidate = if insertion == 0 { sectors.last() } else { sectors.get(insertion - 1) };

        Ok(candidate.filter(|sector| sector.contains_angle(angle)))
    }
}

/// Invalid request or inconsistent arithmetic detected while laying out a tree.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutError {
    /// The selected zoom root is absent from the snapshot.
    UnknownRoot {
        /// Missing node ID.
        root: NodeId,
    },
    /// `start_angle` was not finite.
    InvalidStartAngle {
        /// Rejected value.
        value: f64,
    },
    /// `center_radius` was not finite or not strictly between zero and one.
    InvalidCenterRadius {
        /// Rejected value.
        value: f64,
    },
    /// `minimum_sweep` was not finite or outside `[0, TAU]`.
    InvalidMinimumSweep {
        /// Rejected value.
        value: f64,
    },
    /// The cap could not hold the required central root sector.
    EmptySectorBudget,
    /// Direct-child weights overflowed despite the snapshot invariant.
    WeightOverflow {
        /// Parent whose children overflowed.
        parent: NodeId,
    },
    /// Direct-child unknown counts overflowed despite the snapshot invariant.
    UnknownCountOverflow {
        /// Parent whose children overflowed.
        parent: NodeId,
    },
    /// The immutable snapshot rejected a child traversal.
    InvalidTreeLink {
        /// Parent whose child link was invalid.
        parent: NodeId,
    },
}

impl LayoutError {
    /// Stable diagnostic code suitable for logs and tests.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownRoot { .. } => "sunburst.unknown_root",
            Self::InvalidStartAngle { .. } => "sunburst.invalid_start_angle",
            Self::InvalidCenterRadius { .. } => "sunburst.invalid_center_radius",
            Self::InvalidMinimumSweep { .. } => "sunburst.invalid_minimum_sweep",
            Self::EmptySectorBudget => "sunburst.empty_sector_budget",
            Self::WeightOverflow { .. } => "sunburst.weight_overflow",
            Self::UnknownCountOverflow { .. } => "sunburst.unknown_count_overflow",
            Self::InvalidTreeLink { .. } => "sunburst.invalid_tree_link",
        }
    }
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRoot { root } => write!(formatter, "zoom root {root} is not in the tree"),
            Self::InvalidStartAngle { value } => {
                write!(formatter, "start angle {value} is not finite")
            }
            Self::InvalidCenterRadius { value } => write!(
                formatter,
                "center radius {value} must be finite and strictly between zero and one"
            ),
            Self::InvalidMinimumSweep { value } => {
                write!(formatter, "minimum sweep {value} must be finite and between zero and TAU")
            }
            Self::EmptySectorBudget => {
                formatter.write_str("the sector budget must include the central root sector")
            }
            Self::WeightOverflow { parent } => {
                write!(formatter, "child weights overflowed u128 below node {parent}")
            }
            Self::UnknownCountOverflow { parent } => {
                write!(formatter, "unknown-entry counts overflowed u64 below node {parent}")
            }
            Self::InvalidTreeLink { parent } => {
                write!(formatter, "invalid child link below node {parent}")
            }
        }
    }
}

impl Error for LayoutError {}

/// Invalid point or polar input supplied to hit testing.
#[derive(Clone, Debug, PartialEq)]
pub enum HitTestError {
    /// At least one Cartesian coordinate was not finite.
    NonFinitePoint {
        /// Horizontal coordinate.
        x: f64,
        /// Vertical coordinate.
        y: f64,
    },
    /// The supplied angle was not finite.
    NonFiniteAngle {
        /// Rejected angle.
        angle: f64,
    },
    /// The supplied radius was negative or not finite.
    InvalidRadius {
        /// Rejected radius.
        radius: f64,
    },
}

impl HitTestError {
    /// Stable diagnostic code suitable for logs and tests.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NonFinitePoint { .. } => "sunburst.hit.non_finite_point",
            Self::NonFiniteAngle { .. } => "sunburst.hit.non_finite_angle",
            Self::InvalidRadius { .. } => "sunburst.hit.invalid_radius",
        }
    }
}

impl fmt::Display for HitTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinitePoint { x, y } => {
                write!(formatter, "hit-test point ({x}, {y}) must be finite")
            }
            Self::NonFiniteAngle { angle } => {
                write!(formatter, "hit-test angle {angle} must be finite")
            }
            Self::InvalidRadius { radius } => {
                write!(formatter, "hit-test radius {radius} must be finite and nonnegative")
            }
        }
    }
}

impl Error for HitTestError {}

#[derive(Clone, Debug)]
struct PendingSector {
    node_id: NodeId,
    depth: u32,
    start_angle: f64,
    sweep_angle: f64,
}

#[derive(Clone, Debug)]
struct ChildCandidate {
    node_id: NodeId,
    weight: u128,
    unknown_entries: u64,
    hidden: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnitKind {
    Real,
    Hidden,
    Other(OtherCause),
}

#[derive(Clone, Debug)]
struct DisplayUnit {
    node_id: Option<NodeId>,
    members: Vec<NodeId>,
    kind: UnitKind,
    weight: u128,
    unknown_entries: u64,
}

/// Computes a bounded, immutable sunburst layout.
///
/// The walk is breadth-first and iterative. Positive direct-child weights are
/// ordered by descending size, then by a deterministic native identity key.
/// Small real siblings become one synthetic `Other` unit; hidden siblings stay
/// individually restorable unless the hard global budget must coalesce them.
/// The selected root is always real and visible even if it appears in
/// `hidden_branches`.
pub fn compute_layout(
    snapshot: &TreeSnapshot,
    options: LayoutOptions,
    hidden_branches: &HiddenBranches,
) -> Result<SunburstLayout, LayoutError> {
    validate_options(snapshot, options)?;
    let root_record =
        snapshot.node(options.root).ok_or(LayoutError::UnknownRoot { root: options.root })?;
    let (root_weight, root_unknown_entries) = selected_size(root_record, options.size_basis);
    let start_angle = normalize_angle(options.start_angle);

    let mut sectors = Vec::with_capacity(options.max_sectors.min(snapshot.len().max(1)));
    sectors.push(Sector {
        node_id: Some(options.root),
        kind: SectorKind::Real,
        depth: 0,
        start_angle,
        sweep_angle: FULL_TURN,
        inner_radius: 0.0,
        outer_radius: options.center_radius,
        weight: root_weight,
        unknown_entries: root_unknown_entries,
    });

    let mut pending = VecDeque::new();
    if options.max_depth > 0 {
        pending.push_back(PendingSector {
            node_id: options.root,
            depth: 0,
            start_angle,
            sweep_angle: FULL_TURN,
        });
    }

    let ring_width = if options.max_depth == 0 {
        0.0
    } else {
        (1.0 - options.center_radius) / f64::from(options.max_depth)
    };
    let mut member_pool = Vec::new();
    let mut zero_weight_nodes = Vec::new();
    let mut budget_exhausted = false;

    while let Some(parent) = pending.pop_front() {
        if parent.depth >= options.max_depth {
            continue;
        }

        let mut children = collect_children(
            snapshot,
            options.size_basis,
            hidden_branches,
            parent.node_id,
            (parent.depth == 0).then_some((&mut zero_weight_nodes, options.max_sectors)),
        )?;
        if children.is_empty() {
            continue;
        }
        children.sort_by(|left, right| compare_candidates(left, right, snapshot));

        let total_weight = children.iter().try_fold(0_u128, |total, child| {
            total
                .checked_add(child.weight)
                .ok_or(LayoutError::WeightOverflow { parent: parent.node_id })
        })?;
        let mut units = group_small_children(
            children,
            total_weight,
            parent.sweep_angle,
            options.minimum_sweep,
            parent.node_id,
        )?;
        sort_units(&mut units, snapshot);

        let remaining = options.max_sectors - sectors.len();
        if units.len() > remaining {
            budget_exhausted = true;
            enforce_budget(&mut units, remaining, parent.node_id)?;
            sort_units(&mut units, snapshot);
        }
        if units.is_empty() {
            continue;
        }

        let displayed_total = units.iter().try_fold(0_u128, |total, unit| {
            total
                .checked_add(unit.weight)
                .ok_or(LayoutError::WeightOverflow { parent: parent.node_id })
        })?;
        debug_assert_eq!(displayed_total, total_weight);

        let child_depth = parent.depth + 1;
        let inner_radius = options.center_radius + ring_width * f64::from(child_depth - 1);
        let outer_radius = if child_depth == options.max_depth {
            1.0
        } else {
            options.center_radius + ring_width * f64::from(child_depth)
        };
        let parent_end = parent.start_angle + parent.sweep_angle;
        let mut prefix = 0_u128;
        let mut child_start = parent.start_angle;
        let unit_count = units.len();

        for (index, unit) in units.into_iter().enumerate() {
            prefix = prefix
                .checked_add(unit.weight)
                .ok_or(LayoutError::WeightOverflow { parent: parent.node_id })?;
            let mut child_end = if index + 1 == unit_count {
                parent_end
            } else {
                parent.start_angle + parent.sweep_angle * ratio_to_f64(prefix, displayed_total)
            };
            child_end = child_end.clamp(child_start, parent_end);
            let child_sweep = child_end - child_start;

            let (node_id, kind) = match unit.kind {
                UnitKind::Real => (unit.node_id, SectorKind::Real),
                UnitKind::Hidden => {
                    let members = append_members(&mut member_pool, &unit.members);
                    (None, SectorKind::Hidden { members })
                }
                UnitKind::Other(cause) => {
                    let members = append_members(&mut member_pool, &unit.members);
                    (None, SectorKind::Other { parent: parent.node_id, members, cause })
                }
            };

            sectors.push(Sector {
                node_id,
                kind,
                depth: child_depth,
                start_angle: normalize_angle(child_start),
                sweep_angle: child_sweep,
                inner_radius,
                outer_radius,
                weight: unit.weight,
                unknown_entries: unit.unknown_entries,
            });

            if matches!(unit.kind, UnitKind::Real)
                && child_depth < options.max_depth
                && child_sweep > 0.0
                && let Some(node_id) = unit.node_id
            {
                pending.push_back(PendingSector {
                    node_id,
                    depth: child_depth,
                    start_angle: child_start,
                    sweep_angle: child_sweep,
                });
            }
            child_start = child_end;
        }
    }

    sectors.sort_by(compare_sectors_for_index);
    let rings = build_rings(&sectors);

    Ok(SunburstLayout {
        source_generation: snapshot.generation(),
        root: options.root,
        size_basis: options.size_basis,
        sectors,
        rings,
        member_pool,
        zero_weight_nodes,
        budget_exhausted,
    })
}

fn validate_options(snapshot: &TreeSnapshot, options: LayoutOptions) -> Result<(), LayoutError> {
    if snapshot.node(options.root).is_none() {
        return Err(LayoutError::UnknownRoot { root: options.root });
    }
    if !options.start_angle.is_finite() {
        return Err(LayoutError::InvalidStartAngle { value: options.start_angle });
    }
    if !options.center_radius.is_finite()
        || options.center_radius <= 0.0
        || options.center_radius >= 1.0
    {
        return Err(LayoutError::InvalidCenterRadius { value: options.center_radius });
    }
    if !options.minimum_sweep.is_finite()
        || options.minimum_sweep < 0.0
        || options.minimum_sweep > FULL_TURN
    {
        return Err(LayoutError::InvalidMinimumSweep { value: options.minimum_sweep });
    }
    if options.max_sectors == 0 {
        return Err(LayoutError::EmptySectorBudget);
    }
    Ok(())
}

fn collect_children(
    snapshot: &TreeSnapshot,
    size_basis: SizeBasis,
    hidden_branches: &HiddenBranches,
    parent: NodeId,
    mut zero_weight_sink: Option<(&mut Vec<NodeId>, usize)>,
) -> Result<Vec<ChildCandidate>, LayoutError> {
    let children =
        snapshot.children(parent).map_err(|_| LayoutError::InvalidTreeLink { parent })?;
    let mut candidates = Vec::new();
    for (node_id, record) in children {
        let (weight, unknown_entries) = selected_size(record, size_basis);
        if weight == 0 {
            if let Some((sink, cap)) = zero_weight_sink.as_mut()
                && sink.len() < *cap
            {
                sink.push(node_id);
            }
        } else {
            candidates.push(ChildCandidate {
                node_id,
                weight,
                unknown_entries,
                hidden: hidden_branches.contains(node_id),
            });
        }
    }
    Ok(candidates)
}

fn group_small_children(
    children: Vec<ChildCandidate>,
    total_weight: u128,
    parent_sweep: f64,
    minimum_sweep: f64,
    parent: NodeId,
) -> Result<Vec<DisplayUnit>, LayoutError> {
    let mut units = Vec::with_capacity(children.len());
    let mut small_members = Vec::new();
    let mut small_weight = 0_u128;
    let mut small_unknown_entries = 0_u64;

    for child in children {
        let projected_sweep = parent_sweep * ratio_to_f64(child.weight, total_weight);
        if !child.hidden && projected_sweep < minimum_sweep {
            small_members.push(child.node_id);
            small_weight = small_weight
                .checked_add(child.weight)
                .ok_or(LayoutError::WeightOverflow { parent })?;
            small_unknown_entries = small_unknown_entries
                .checked_add(child.unknown_entries)
                .ok_or(LayoutError::UnknownCountOverflow { parent })?;
            continue;
        }

        units.push(DisplayUnit {
            node_id: Some(child.node_id),
            members: vec![child.node_id],
            kind: if child.hidden { UnitKind::Hidden } else { UnitKind::Real },
            weight: child.weight,
            unknown_entries: child.unknown_entries,
        });
    }

    if !small_members.is_empty() {
        units.push(DisplayUnit {
            node_id: None,
            members: small_members,
            kind: UnitKind::Other(OtherCause::MinimumSweep),
            weight: small_weight,
            unknown_entries: small_unknown_entries,
        });
    }
    Ok(units)
}

fn enforce_budget(
    units: &mut Vec<DisplayUnit>,
    remaining: usize,
    parent: NodeId,
) -> Result<(), LayoutError> {
    if units.len() <= remaining {
        return Ok(());
    }
    if remaining == 0 {
        units.clear();
        return Ok(());
    }

    let tail = units.split_off(remaining - 1);
    let includes_threshold_group = tail.iter().any(|unit| {
        matches!(
            unit.kind,
            UnitKind::Other(OtherCause::MinimumSweep | OtherCause::MinimumSweepAndSectorBudget)
        )
    });
    let mut members = Vec::new();
    let mut weight = 0_u128;
    let mut unknown_entries = 0_u64;
    for unit in tail {
        members.extend(unit.members);
        weight = weight.checked_add(unit.weight).ok_or(LayoutError::WeightOverflow { parent })?;
        unknown_entries = unknown_entries
            .checked_add(unit.unknown_entries)
            .ok_or(LayoutError::UnknownCountOverflow { parent })?;
    }
    units.push(DisplayUnit {
        node_id: None,
        members,
        kind: UnitKind::Other(if includes_threshold_group {
            OtherCause::MinimumSweepAndSectorBudget
        } else {
            OtherCause::SectorBudget
        }),
        weight,
        unknown_entries,
    });
    Ok(())
}

fn selected_size(record: &NodeRecord, size_basis: SizeBasis) -> (u128, u64) {
    let aggregate = record.aggregate();
    let size = match size_basis {
        SizeBasis::Logical => aggregate.logical(),
        SizeBasis::Allocated => aggregate.allocated(),
    };
    (size.known_bytes(), size.unknown_entries())
}

fn ratio_to_f64(numerator: u128, denominator: u128) -> f64 {
    debug_assert!(denominator > 0);
    (numerator as f64) / (denominator as f64)
}

fn append_members(pool: &mut Vec<NodeId>, members: &[NodeId]) -> MemberRange {
    let range = MemberRange { start: pool.len(), len: members.len() };
    pool.extend_from_slice(members);
    range
}

fn compare_candidates(
    left: &ChildCandidate,
    right: &ChildCandidate,
    snapshot: &TreeSnapshot,
) -> Ordering {
    right
        .weight
        .cmp(&left.weight)
        .then_with(|| compare_node_identity(left.node_id, right.node_id, snapshot))
}

fn sort_units(units: &mut [DisplayUnit], snapshot: &TreeSnapshot) {
    units.sort_by(|left, right| {
        right.weight.cmp(&left.weight).then_with(|| compare_unit_identity(left, right, snapshot))
    });
}

fn compare_unit_identity(
    left: &DisplayUnit,
    right: &DisplayUnit,
    snapshot: &TreeSnapshot,
) -> Ordering {
    match (left.node_id, right.node_id) {
        (Some(left), Some(right)) => compare_node_identity(left, right, snapshot),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left
            .members
            .iter()
            .map(|node| node.raw())
            .cmp(right.members.iter().map(|node| node.raw())),
    }
}

fn compare_node_identity(left: NodeId, right: NodeId, snapshot: &TreeSnapshot) -> Ordering {
    let Some(left_record) = snapshot.node(left) else {
        return left.cmp(&right);
    };
    let Some(right_record) = snapshot.node(right) else {
        return left.cmp(&right);
    };

    entry_kind_rank(left_record.kind())
        .cmp(&entry_kind_rank(right_record.kind()))
        .then_with(|| {
            left_record.name().as_encoded_bytes().cmp(right_record.name().as_encoded_bytes())
        })
        .then_with(|| left_record.file_identity().cmp(&right_record.file_identity()))
        .then_with(|| left.cmp(&right))
}

fn entry_kind_rank(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Root => 0,
        EntryKind::Directory => 1,
        EntryKind::File => 2,
        EntryKind::ReparsePoint(_) => 3,
        EntryKind::SyntheticGroup => 4,
    }
}

fn compare_sectors_for_index(left: &Sector, right: &Sector) -> Ordering {
    left.depth
        .cmp(&right.depth)
        .then_with(|| left.start_angle.total_cmp(&right.start_angle))
        .then_with(|| left.sweep_angle.total_cmp(&right.sweep_angle))
        .then_with(|| left.node_id.cmp(&right.node_id))
}

fn build_rings(sectors: &[Sector]) -> Vec<Ring> {
    let mut rings = Vec::new();
    let mut start = 0;
    while start < sectors.len() {
        let depth = sectors[start].depth;
        let inner_radius = sectors[start].inner_radius;
        let outer_radius = sectors[start].outer_radius;
        let mut end = start + 1;
        while end < sectors.len() && sectors[end].depth == depth {
            end += 1;
        }
        rings.push(Ring { depth, inner_radius, outer_radius, sectors: start..end });
        start = end;
    }
    rings
}

fn normalize_angle(angle: f64) -> f64 {
    let normalized = angle.rem_euclid(FULL_TURN);
    if normalized == 0.0 { 0.0 } else { normalized }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        GenerationId, MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder, UnknownReason,
    };

    const SOURCE: MetricSource = MetricSource::PortableMetadata;
    const EPSILON: f64 = 1.0e-11;

    fn metrics(logical: u64, allocated: u64) -> OwnMetrics {
        OwnMetrics::new(SizeMetric::known(logical, SOURCE), SizeMetric::known(allocated, SOURCE))
    }

    fn file(name: &str, logical: u64, allocated: u64) -> NodeSpec {
        NodeSpec::file(name, metrics(logical, allocated))
    }

    fn options(root: NodeId) -> LayoutOptions {
        LayoutOptions {
            root,
            size_basis: SizeBasis::Logical,
            start_angle: 0.0,
            center_radius: 0.2,
            max_depth: 8,
            minimum_sweep: 0.0,
            max_sectors: 10_000,
        }
    }

    fn sector_for(layout: &SunburstLayout, node_id: NodeId) -> &Sector {
        layout
            .sectors()
            .iter()
            .find(|sector| sector.node_id == Some(node_id))
            .expect("real node should have a sector")
    }

    fn synthetic_members<'a>(layout: &'a SunburstLayout, sector: &Sector) -> &'a [NodeId] {
        layout.members(sector).expect("sector should be synthetic")
    }

    fn assert_close(left: f64, right: f64) {
        assert!((left - right).abs() <= EPSILON, "{left} != {right}");
    }

    #[test]
    fn partitions_exact_weight_and_forces_the_final_boundary() {
        let mut builder = TreeBuilder::new(GenerationId::new(7));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let largest = builder.add_child(root, file("largest", 6, 6)).unwrap();
        let middle = builder.add_child(root, file("middle", 3, 3)).unwrap();
        let smallest = builder.add_child(root, file("smallest", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();

        let layout = compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();
        assert_eq!(layout.source_generation(), GenerationId::new(7));
        assert_eq!(layout.sectors().len(), 4);

        let largest = sector_for(&layout, largest);
        let middle = sector_for(&layout, middle);
        let smallest = sector_for(&layout, smallest);
        assert_close(largest.start_angle, 0.0);
        assert_close(largest.sweep_angle, FULL_TURN * 0.6);
        assert_close(middle.start_angle, FULL_TURN * 0.6);
        assert_close(middle.sweep_angle, FULL_TURN * 0.3);
        assert_close(smallest.start_angle, FULL_TURN * 0.9);
        assert_close(smallest.sweep_angle, FULL_TURN * 0.1);
        assert_close(largest.sweep_angle + middle.sweep_angle + smallest.sweep_angle, FULL_TURN);
        assert_eq!(smallest.end_angle(), 0.0);
    }

    #[test]
    fn equal_weights_use_native_name_then_node_id_as_a_stable_tie_break() {
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let zulu = builder.add_child(root, file("zulu", 5, 5)).unwrap();
        let alpha = builder.add_child(root, file("alpha", 5, 5)).unwrap();
        let mike = builder.add_child(root, file("mike", 5, 5)).unwrap();
        let snapshot = builder.freeze().unwrap();

        let first = compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();
        let second = compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();
        assert_eq!(first, second);
        assert_close(sector_for(&first, alpha).start_angle, 0.0);
        assert_close(sector_for(&first, mike).start_angle, FULL_TURN / 3.0);
        assert_close(sector_for(&first, zulu).start_angle, FULL_TURN * 2.0 / 3.0);
    }

    #[test]
    fn logical_and_allocated_metrics_select_independent_u128_weights() {
        let mut builder = TreeBuilder::new(GenerationId::new(2));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let logical = builder.add_child(root, file("logical", 100, 1)).unwrap();
        let allocated = builder.add_child(root, file("allocated", 1, 100)).unwrap();
        let snapshot = builder.freeze().unwrap();

        let logical_layout =
            compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();
        let mut allocated_options = options(root);
        allocated_options.size_basis = SizeBasis::Allocated;
        let allocated_layout =
            compute_layout(&snapshot, allocated_options, &HiddenBranches::new()).unwrap();

        assert_eq!(sector_for(&logical_layout, logical).weight, 100);
        assert_eq!(sector_for(&logical_layout, allocated).weight, 1);
        assert_eq!(sector_for(&logical_layout, logical).start_angle, 0.0);
        assert_eq!(sector_for(&allocated_layout, allocated).weight, 100);
        assert_eq!(sector_for(&allocated_layout, logical).weight, 1);
        assert_eq!(sector_for(&allocated_layout, allocated).start_angle, 0.0);
    }

    #[test]
    fn an_arbitrary_zoom_root_resets_depth_and_overrides_its_hidden_marker() {
        let mut builder = TreeBuilder::new(GenerationId::new(21));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let branch = builder.add_child(root, NodeSpec::directory("branch")).unwrap();
        let leaf = builder.add_child(branch, file("leaf", 7, 7)).unwrap();
        let outside = builder.add_child(root, file("outside", 9, 9)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let hidden = HiddenBranches::from_iter([branch]);

        let layout = compute_layout(&snapshot, options(branch), &hidden).unwrap();
        assert_eq!(layout.root(), branch);
        assert_eq!(layout.sectors()[0].node_id, Some(branch));
        assert_eq!(layout.sectors()[0].kind, SectorKind::Real);
        assert_eq!(layout.sectors()[0].depth, 0);
        assert_eq!(sector_for(&layout, leaf).depth, 1);
        assert!(!layout.sectors().iter().any(|sector| sector.node_id == Some(root)));
        assert!(!layout.sectors().iter().any(|sector| sector.node_id == Some(outside)));
    }

    #[test]
    fn zero_weight_list_is_scoped_to_the_zoom_root_and_capped() {
        let mut builder = TreeBuilder::new(GenerationId::new(31));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let branch = builder.add_child(root, NodeSpec::directory("branch")).unwrap();
        let _weight = builder.add_child(branch, file("weight", 5, 5)).unwrap();
        let deep_zero = builder.add_child(branch, file("deep-zero", 0, 0)).unwrap();
        let mut root_zeros = Vec::new();
        for index in 0..6 {
            root_zeros.push(builder.add_child(root, file(&format!("zero-{index}"), 0, 0)).unwrap());
        }
        let snapshot = builder.freeze().unwrap();

        let mut capped = options(root);
        capped.max_sectors = 4;
        let layout = compute_layout(&snapshot, capped, &HiddenBranches::new()).unwrap();
        assert_eq!(layout.zero_weight_nodes().len(), 4, "capped by max_sectors");
        assert!(layout.zero_weight_nodes().iter().all(|node| root_zeros.contains(node)));
        assert!(
            !layout.zero_weight_nodes().contains(&deep_zero),
            "children below the zoom root are not recorded"
        );

        let zoomed = compute_layout(&snapshot, options(branch), &HiddenBranches::new()).unwrap();
        assert_eq!(zoomed.zero_weight_nodes(), &[deep_zero]);
    }

    #[test]
    fn zero_and_unknown_metrics_never_create_nan_geometry() {
        let mut builder = TreeBuilder::new(GenerationId::new(3));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let zero = builder.add_child(root, file("zero", 0, 9)).unwrap();
        let unknown_metrics = OwnMetrics::new(
            SizeMetric::unknown(UnknownReason::AccessDenied, SOURCE),
            SizeMetric::unknown(UnknownReason::NotAvailable, SOURCE),
        );
        let unknown = builder.add_child(root, NodeSpec::file("unknown", unknown_metrics)).unwrap();
        let snapshot = builder.freeze().unwrap();

        let layout = compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();
        assert_eq!(layout.sectors().len(), 1);
        assert_eq!(layout.zero_weight_nodes(), &[zero, unknown]);
        let center = &layout.sectors()[0];
        assert_eq!(center.weight, 0);
        assert_eq!(center.unknown_entries, 1);
        for sector in layout.sectors() {
            assert!(sector.start_angle.is_finite());
            assert!(sector.sweep_angle.is_finite());
            assert!(sector.inner_radius.is_finite());
            assert!(sector.outer_radius.is_finite());
        }
    }

    #[test]
    fn aggregate_weights_above_four_gib_and_u64_use_u128_without_wrapping() {
        let mut builder = TreeBuilder::new(GenerationId::new(4));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let first = builder.add_child(root, file("a", u64::MAX, u64::MAX)).unwrap();
        let second = builder.add_child(root, file("b", u64::MAX, u64::MAX)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let layout = compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();

        let expected = u128::from(u64::MAX) * 2;
        assert!(expected > u128::from(u64::MAX));
        assert!(expected > u128::from(4_u64 * 1024 * 1024 * 1024));
        assert_eq!(sector_for(&layout, root).weight, expected);
        assert_eq!(sector_for(&layout, first).weight, u128::from(u64::MAX));
        assert_eq!(sector_for(&layout, second).weight, u128::from(u64::MAX));
        assert_close(sector_for(&layout, first).sweep_angle, FULL_TURN / 2.0);
        assert_close(sector_for(&layout, second).sweep_angle, FULL_TURN / 2.0);
    }

    #[test]
    fn a_very_deep_tree_is_laid_out_iteratively() {
        const DIRECTORY_DEPTH: u32 = 20_000;

        let mut builder = TreeBuilder::new(GenerationId::new(5));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let mut parent = root;
        for index in 0..DIRECTORY_DEPTH {
            parent = builder.add_child(parent, NodeSpec::directory(format!("d{index}"))).unwrap();
        }
        let leaf = builder.add_child(parent, file("leaf", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut deep_options = options(root);
        deep_options.max_depth = DIRECTORY_DEPTH + 1;
        deep_options.max_sectors = DIRECTORY_DEPTH as usize + 2;

        let layout = compute_layout(&snapshot, deep_options, &HiddenBranches::new()).unwrap();
        assert_eq!(layout.sectors().len(), DIRECTORY_DEPTH as usize + 2);
        assert_eq!(sector_for(&layout, leaf).depth, DIRECTORY_DEPTH + 1);
        assert!(!layout.budget_exhausted());
        assert!(layout.sectors().iter().all(|sector| {
            sector.start_angle.is_finite()
                && sector.sweep_angle.is_finite()
                && sector.inner_radius.is_finite()
                && sector.outer_radius.is_finite()
        }));
    }

    #[test]
    fn small_real_siblings_are_grouped_without_a_fake_node_id() {
        let mut builder = TreeBuilder::new(GenerationId::new(6));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let large = builder.add_child(root, file("large", 100, 100)).unwrap();
        let small_a = builder.add_child(root, file("small-a", 1, 1)).unwrap();
        let small_b = builder.add_child(root, file("small-b", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut grouped_options = options(root);
        grouped_options.minimum_sweep = 0.1;

        let layout = compute_layout(&snapshot, grouped_options, &HiddenBranches::new()).unwrap();
        assert_eq!(layout.sectors().len(), 3);
        assert!(sector_for(&layout, large).is_actionable());
        let other = layout
            .sectors()
            .iter()
            .find(|sector| matches!(sector.kind, SectorKind::Other { .. }))
            .unwrap();
        assert_eq!(other.node_id, None);
        assert!(!other.is_actionable());
        assert_eq!(other.action_target(), None);
        assert_eq!(other.weight, 2);
        assert_eq!(synthetic_members(&layout, other), &[small_a, small_b]);
        assert!(matches!(other.kind, SectorKind::Other { cause: OtherCause::MinimumSweep, .. }));
        assert_close(sector_for(&layout, large).sweep_angle + other.sweep_angle, FULL_TURN);
    }

    #[test]
    fn hidden_sectors_are_non_actionable_and_restore_exactly() {
        let mut builder = TreeBuilder::new(GenerationId::new(8));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let branch = builder.add_child(root, NodeSpec::directory("branch")).unwrap();
        let leaf = builder.add_child(branch, file("leaf", 10, 10)).unwrap();
        let peer = builder.add_child(root, file("peer", 5, 5)).unwrap();
        let snapshot = builder.freeze().unwrap();

        let mut hidden = HiddenBranches::new();
        assert!(hidden.hide(branch));
        assert!(hidden.hide(leaf));
        let hidden_layout = compute_layout(&snapshot, options(root), &hidden).unwrap();
        let hidden_sector = hidden_layout
            .sectors()
            .iter()
            .find(|sector| matches!(sector.kind, SectorKind::Hidden { .. }))
            .unwrap();
        assert_eq!(hidden_sector.node_id, None);
        assert!(!hidden_sector.is_actionable());
        assert_eq!(hidden_sector.action_target(), None);
        assert_eq!(synthetic_members(&hidden_layout, hidden_sector), &[branch]);
        assert!(!hidden_layout.sectors().iter().any(|sector| sector.node_id == Some(leaf)));
        assert!(sector_for(&hidden_layout, peer).is_actionable());

        assert!(hidden.restore(branch));
        let restored_layout = compute_layout(&snapshot, options(root), &hidden).unwrap();
        assert!(sector_for(&restored_layout, branch).is_actionable());
        let descendant_hidden = restored_layout
            .sectors()
            .iter()
            .find(|sector| {
                matches!(sector.kind, SectorKind::Hidden { .. })
                    && synthetic_members(&restored_layout, sector) == [leaf]
            })
            .unwrap();
        assert_eq!(descendant_hidden.node_id, None);
        assert!(!hidden.toggle(leaf));
        assert!(!hidden.contains(leaf));
    }

    #[test]
    fn a_small_hidden_branch_is_not_absorbed_by_threshold_grouping() {
        let mut builder = TreeBuilder::new(GenerationId::new(9));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        builder.add_child(root, file("large", 100, 100)).unwrap();
        let hidden_id = builder.add_child(root, file("hidden", 1, 1)).unwrap();
        let other_id = builder.add_child(root, file("other", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let hidden = HiddenBranches::from_iter([hidden_id]);
        let mut grouped_options = options(root);
        grouped_options.minimum_sweep = 0.1;

        let layout = compute_layout(&snapshot, grouped_options, &hidden).unwrap();
        let hidden_sector = layout
            .sectors()
            .iter()
            .find(|sector| matches!(sector.kind, SectorKind::Hidden { .. }))
            .unwrap();
        let other_sector = layout
            .sectors()
            .iter()
            .find(|sector| matches!(sector.kind, SectorKind::Other { .. }))
            .unwrap();
        assert_eq!(synthetic_members(&layout, hidden_sector), &[hidden_id]);
        assert_eq!(synthetic_members(&layout, other_sector), &[other_id]);
    }

    #[test]
    fn sector_budget_is_hard_and_coalesces_overflow_deterministically() {
        let mut builder = TreeBuilder::new(GenerationId::new(10));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let mut children = Vec::new();
        for weight in (1_u64..=10).rev() {
            children.push(
                builder.add_child(root, file(&format!("n{weight:02}"), weight, weight)).unwrap(),
            );
        }
        let snapshot = builder.freeze().unwrap();
        let mut bounded = options(root);
        bounded.max_sectors = 4;

        let first = compute_layout(&snapshot, bounded, &HiddenBranches::new()).unwrap();
        let second = compute_layout(&snapshot, bounded, &HiddenBranches::new()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.sectors().len(), 4);
        assert!(first.budget_exhausted());
        let other = first
            .sectors()
            .iter()
            .find(|sector| matches!(sector.kind, SectorKind::Other { .. }))
            .unwrap();
        assert_eq!(synthetic_members(&first, other), &children[2..]);
        assert!(matches!(other.kind, SectorKind::Other { cause: OtherCause::SectorBudget, .. }));
        let child_sweep: f64 = first
            .sectors()
            .iter()
            .filter(|sector| sector.depth == 1)
            .map(|sector| sector.sweep_angle)
            .sum();
        assert_close(child_sweep, FULL_TURN);

        bounded.max_sectors = 1;
        let center_only = compute_layout(&snapshot, bounded, &HiddenBranches::new()).unwrap();
        assert_eq!(center_only.sectors().len(), 1);
        assert!(center_only.budget_exhausted());
    }

    #[test]
    fn hit_testing_owns_center_angular_and_outer_boundaries_deterministically() {
        let mut builder = TreeBuilder::new(GenerationId::new(11));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let first = builder.add_child(root, file("a", 1, 1)).unwrap();
        let second = builder.add_child(root, file("b", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut hit_options = options(root);
        hit_options.max_depth = 1;
        let layout = compute_layout(&snapshot, hit_options, &HiddenBranches::new()).unwrap();

        assert_eq!(layout.hit_test(0.0, 0.0).unwrap().unwrap().node_id, Some(root));
        assert_eq!(layout.hit_test_polar(0.0, 0.2).unwrap().unwrap().node_id, Some(first));
        assert_eq!(
            layout.hit_test_polar(std::f64::consts::PI, 0.5).unwrap().unwrap().node_id,
            Some(second)
        );
        assert_eq!(layout.hit_test_polar(FULL_TURN, 0.5).unwrap().unwrap().node_id, Some(first));
        assert_eq!(layout.hit_test_polar(-FULL_TURN, 1.0).unwrap().unwrap().node_id, Some(first));
        assert!(layout.hit_test_polar(0.0, 1.000_001).unwrap().is_none());
    }

    #[test]
    fn wraparound_sectors_and_exact_shared_boundaries_hit_correctly() {
        let mut builder = TreeBuilder::new(GenerationId::new(12));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let first = builder.add_child(root, file("a", 1, 1)).unwrap();
        let second = builder.add_child(root, file("b", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut wrapped = options(root);
        wrapped.max_depth = 1;
        wrapped.start_angle = 3.0 * std::f64::consts::FRAC_PI_2;
        let layout = compute_layout(&snapshot, wrapped, &HiddenBranches::new()).unwrap();

        assert_eq!(layout.hit_test_polar(0.0, 0.5).unwrap().unwrap().node_id, Some(first));
        assert_eq!(
            layout.hit_test_polar(std::f64::consts::FRAC_PI_2, 0.5).unwrap().unwrap().node_id,
            Some(second)
        );
        assert_eq!(
            layout.hit_test_polar(3.0 * std::f64::consts::FRAC_PI_2, 0.5).unwrap().unwrap().node_id,
            Some(first)
        );
    }

    #[test]
    fn exact_radial_boundaries_belong_to_the_outer_ring() {
        let mut builder = TreeBuilder::new(GenerationId::new(13));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let directory = builder.add_child(root, NodeSpec::directory("directory")).unwrap();
        let leaf = builder.add_child(directory, file("leaf", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut radial = options(root);
        radial.max_depth = 2;
        let layout = compute_layout(&snapshot, radial, &HiddenBranches::new()).unwrap();
        let shared_boundary = sector_for(&layout, directory).outer_radius;

        assert_eq!(layout.hit_test_polar(0.0, 0.2).unwrap().unwrap().node_id, Some(directory));
        assert_eq!(
            layout.hit_test_polar(0.0, shared_boundary).unwrap().unwrap().node_id,
            Some(leaf)
        );
        assert_eq!(layout.hit_test_polar(0.0, 1.0).unwrap().unwrap().node_id, Some(leaf));
    }

    #[test]
    fn missing_descendants_are_real_angular_gaps_in_outer_rings() {
        let mut builder = TreeBuilder::new(GenerationId::new(14));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let directory = builder.add_child(root, NodeSpec::directory("a-directory")).unwrap();
        builder.add_child(directory, file("leaf", 1, 1)).unwrap();
        builder.add_child(root, file("z-file", 1, 1)).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut gap_options = options(root);
        gap_options.max_depth = 2;
        let layout = compute_layout(&snapshot, gap_options, &HiddenBranches::new()).unwrap();

        assert!(layout.hit_test_polar(3.0 * std::f64::consts::FRAC_PI_2, 0.8).unwrap().is_none());
    }

    #[test]
    fn invalid_parameters_and_unknown_ids_return_typed_errors() {
        let mut builder = TreeBuilder::new(GenerationId::new(15));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let snapshot = builder.freeze().unwrap();

        let unknown = NodeId::from_raw(999);
        assert!(matches!(
            compute_layout(&snapshot, options(unknown), &HiddenBranches::new()),
            Err(LayoutError::UnknownRoot { root }) if root == unknown
        ));

        let mut invalid = options(root);
        invalid.start_angle = f64::NAN;
        assert!(matches!(
            compute_layout(&snapshot, invalid, &HiddenBranches::new()),
            Err(LayoutError::InvalidStartAngle { .. })
        ));
        invalid = options(root);
        invalid.center_radius = f64::INFINITY;
        assert!(matches!(
            compute_layout(&snapshot, invalid, &HiddenBranches::new()),
            Err(LayoutError::InvalidCenterRadius { .. })
        ));
        invalid = options(root);
        invalid.minimum_sweep = -0.1;
        assert!(matches!(
            compute_layout(&snapshot, invalid, &HiddenBranches::new()),
            Err(LayoutError::InvalidMinimumSweep { .. })
        ));
        invalid = options(root);
        invalid.max_sectors = 0;
        assert_eq!(
            compute_layout(&snapshot, invalid, &HiddenBranches::new()),
            Err(LayoutError::EmptySectorBudget)
        );

        let layout = compute_layout(&snapshot, options(root), &HiddenBranches::new()).unwrap();
        assert!(matches!(layout.hit_test(f64::NAN, 0.0), Err(HitTestError::NonFinitePoint { .. })));
        assert!(matches!(
            layout.hit_test_polar(f64::INFINITY, 0.0),
            Err(HitTestError::NonFiniteAngle { .. })
        ));
        assert!(matches!(
            layout.hit_test_polar(0.0, -1.0),
            Err(HitTestError::InvalidRadius { .. })
        ));
    }

    #[derive(Clone, Copy)]
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            self.0
        }

        fn index(&mut self, len: usize) -> usize {
            usize::try_from(self.next() % len as u64).unwrap()
        }
    }

    #[test]
    fn deterministic_fuzz_like_trees_preserve_geometry_and_hit_invariants() {
        for seed in 0_u64..12 {
            let mut rng = Lcg(seed ^ 0xA5A5_5A5A_D3C1_B2E7);
            let mut builder = TreeBuilder::new(GenerationId::new(seed));
            let root = builder.add_root(NodeSpec::root("root")).unwrap();
            let mut containers = vec![root];
            let mut hidden = HiddenBranches::new();

            for index in 0..400_u32 {
                let parent = containers[rng.index(containers.len())];
                if index % 5 == 0 {
                    let node = builder
                        .add_child(parent, NodeSpec::directory(format!("d-{index:04}")))
                        .unwrap();
                    containers.push(node);
                    if rng.next().is_multiple_of(13) {
                        hidden.hide(node);
                    }
                } else {
                    let random = rng.next();
                    let logical = match random % 17 {
                        0 => 0,
                        1 => u64::MAX,
                        _ => random & 0x0000_FFFF_FFFF_FFFF,
                    };
                    let allocated = rng.next() & 0x0000_FFFF_FFFF_FFFF;
                    let node = builder
                        .add_child(parent, file(&format!("f-{index:04}"), logical, allocated))
                        .unwrap();
                    if rng.next().is_multiple_of(19) {
                        hidden.hide(node);
                    }
                }
            }

            let snapshot = builder.freeze().unwrap();
            let mut fuzz_options = options(root);
            fuzz_options.max_depth = 64;
            fuzz_options.minimum_sweep = 0.000_01;
            fuzz_options.max_sectors = 2_000;
            fuzz_options.start_angle = (rng.next() as f64 / u64::MAX as f64) * FULL_TURN;
            fuzz_options.size_basis =
                if seed % 2 == 0 { SizeBasis::Logical } else { SizeBasis::Allocated };

            let first = compute_layout(&snapshot, fuzz_options, &hidden).unwrap();
            let second = compute_layout(&snapshot, fuzz_options, &hidden).unwrap();
            assert_eq!(first, second, "seed {seed}");
            assert!(first.sectors().len() <= fuzz_options.max_sectors);
            assert_eq!(first.sectors()[0].depth, 0);

            for ring in first.rings() {
                let ring_sectors = &first.sectors()[ring.sectors.clone()];
                assert!(ring.inner_radius.is_finite());
                assert!(ring.outer_radius.is_finite());
                assert!(ring.inner_radius <= ring.outer_radius);
                assert!(
                    ring_sectors.windows(2).all(|pair| pair[0].start_angle <= pair[1].start_angle)
                );
            }

            for sector in first.sectors() {
                assert!(sector.start_angle.is_finite());
                assert!(sector.sweep_angle.is_finite());
                assert!(sector.inner_radius.is_finite());
                assert!(sector.outer_radius.is_finite());
                assert!((0.0..FULL_TURN).contains(&sector.start_angle));
                assert!((0.0..=FULL_TURN).contains(&sector.sweep_angle));
                assert!((0.0..=1.0).contains(&sector.inner_radius));
                assert!((0.0..=1.0).contains(&sector.outer_radius));
                assert_eq!(sector.is_actionable(), sector.node_id.is_some());
                if sector.node_id.is_none() {
                    assert!(!synthetic_members(&first, sector).is_empty());
                }

                if sector.sweep_angle > 1.0e-10 && sector.outer_radius > sector.inner_radius {
                    let angle = sector.start_angle + sector.sweep_angle / 2.0;
                    let radius = (sector.inner_radius + sector.outer_radius) / 2.0;
                    let hit = first
                        .hit_test(radius * angle.cos(), radius * angle.sin())
                        .unwrap()
                        .expect("sector midpoint should hit");
                    assert_eq!(hit, sector, "seed {seed}");
                }
            }

            assert_parent_containment(&first, &snapshot);
            assert_displayed_children_conserve_parent_sweeps(&first, &snapshot);
        }
    }

    fn assert_parent_containment(layout: &SunburstLayout, snapshot: &TreeSnapshot) {
        for child in layout.sectors().iter().filter(|sector| sector.depth > 0) {
            let child_id = match child.kind {
                SectorKind::Real => child.node_id.unwrap(),
                SectorKind::Hidden { .. } => layout.members(child).unwrap()[0],
                SectorKind::Other { parent, .. } => {
                    let parent_sector = sector_for(layout, parent);
                    let offset = normalize_angle(child.start_angle - parent_sector.start_angle);
                    assert!(
                        offset + child.sweep_angle <= parent_sector.sweep_angle + EPSILON,
                        "synthetic child escaped parent {parent}"
                    );
                    continue;
                }
            };
            let parent_id = snapshot.node(child_id).unwrap().parent().unwrap();
            let parent_sector = sector_for(layout, parent_id);
            let offset = normalize_angle(child.start_angle - parent_sector.start_angle);
            assert!(
                offset + child.sweep_angle <= parent_sector.sweep_angle + EPSILON,
                "child {child_id} escaped parent {parent_id}"
            );
        }
    }

    fn assert_displayed_children_conserve_parent_sweeps(
        layout: &SunburstLayout,
        snapshot: &TreeSnapshot,
    ) {
        let mut by_parent = std::collections::BTreeMap::<NodeId, Vec<&Sector>>::new();
        for child in layout.sectors().iter().filter(|sector| sector.depth > 0) {
            let parent = match child.kind {
                SectorKind::Real => {
                    let child_id = child.node_id.unwrap();
                    snapshot.node(child_id).unwrap().parent().unwrap()
                }
                SectorKind::Hidden { .. } => {
                    let child_id = layout.members(child).unwrap()[0];
                    snapshot.node(child_id).unwrap().parent().unwrap()
                }
                SectorKind::Other { parent, .. } => parent,
            };
            by_parent.entry(parent).or_default().push(child);
        }

        for (parent_id, mut children) in by_parent {
            let parent = sector_for(layout, parent_id);
            children.sort_by(|left, right| {
                let left_offset = normalize_angle(left.start_angle - parent.start_angle);
                let right_offset = normalize_angle(right.start_angle - parent.start_angle);
                left_offset.total_cmp(&right_offset)
            });

            let mut previous_end = 0.0_f64;
            let mut sweep_sum = 0.0_f64;
            for child in children {
                let offset = normalize_angle(child.start_angle - parent.start_angle);
                assert!(offset + EPSILON >= previous_end);
                previous_end = offset + child.sweep_angle;
                sweep_sum += child.sweep_angle;
            }
            assert_close(sweep_sum, parent.sweep_angle);
        }
    }
}
