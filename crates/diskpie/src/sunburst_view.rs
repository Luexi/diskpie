//! Bounded egui rendering and interaction adapter for the portable sunburst layout.

#![forbid(unsafe_code)]

use std::{borrow::Cow, sync::Arc};

use diskpie_app::presentation::ColorIdentity;
use diskpie_core::{
    NodeId,
    sunburst::{FULL_TURN, OtherCause, Sector, SectorKind, SizeBasis, SunburstLayout},
};
use eframe::egui::{
    self, Color32, Key, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2, WidgetInfo, WidgetType,
    epaint::Vertex,
};

const DEFAULT_SIDE_POINTS: f32 = 480.0;
const HARD_MAX_SIDE_POINTS: f32 = 8_192.0;
const HARD_MAX_SUBDIVISIONS: usize = 512;
const HARD_MAX_VERTICES: usize = 1_000_000;
const HARD_MAX_INDICES: usize = 3_000_000;
const MIN_RENDER_SWEEP: f64 = 1.0e-7;
const GEOMETRY_QUANTUM: f32 = 1.0 / 64.0;

const SCAN_CURRENT: Color32 = Color32::from_rgb(5, 188, 235);
const SECTOR_EMBER: Color32 = Color32::from_rgb(247, 154, 30);
const PLATTER_MIDNIGHT: Color32 = Color32::from_rgb(6, 47, 87);
const SEPARATOR_PORCELAIN: Color32 = Color32::from_rgb(254, 251, 240);

/// Resource and visual-quality limits used to build one cached chart mesh.
///
/// Values are sanitized and clamped to hard internal ceilings before use, so
/// untrusted settings cannot cause an unbounded allocation or non-finite mesh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunburstMeshOptions {
    /// Empty space between neighboring sectors, in logical points.
    pub gap_points: f32,
    /// Space kept outside the chart for the keyboard-focus outline.
    pub outer_margin_points: f32,
    /// Maximum radial chord error in physical pixels.
    pub max_chord_error_pixels: f32,
    /// Per-sector angular subdivision ceiling.
    pub max_subdivisions_per_sector: usize,
    /// Base-mesh vertex budget.
    pub max_vertices: usize,
    /// Base-mesh index budget.
    pub max_indices: usize,
    /// Maximum allocated chart side in logical points.
    pub max_side_points: f32,
}

impl Default for SunburstMeshOptions {
    fn default() -> Self {
        Self {
            gap_points: 1.0,
            outer_margin_points: 4.0,
            max_chord_error_pixels: 0.75,
            max_subdivisions_per_sector: 192,
            max_vertices: 300_000,
            max_indices: 900_000,
            max_side_points: 4_096.0,
        }
    }
}

impl SunburstMeshOptions {
    fn sanitized(self) -> Self {
        Self {
            gap_points: finite_or(self.gap_points, 1.0).clamp(0.0, 16.0),
            outer_margin_points: finite_or(self.outer_margin_points, 4.0).clamp(0.0, 64.0),
            max_chord_error_pixels: finite_or(self.max_chord_error_pixels, 0.75).clamp(0.05, 8.0),
            max_subdivisions_per_sector: self
                .max_subdivisions_per_sector
                .clamp(1, HARD_MAX_SUBDIVISIONS),
            max_vertices: self.max_vertices.min(HARD_MAX_VERTICES),
            max_indices: self.max_indices.min(HARD_MAX_INDICES),
            max_side_points: finite_or(self.max_side_points, 4_096.0)
                .clamp(1.0, HARD_MAX_SIDE_POINTS),
        }
    }
}

/// Measurements from the most recently prepared base mesh.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SunburstMeshStats {
    pub vertices: usize,
    pub indices: usize,
    pub bytes: usize,
    pub rendered_sectors: usize,
    pub skipped_degenerate_sectors: usize,
    pub skipped_budget_sectors: usize,
}

/// Persistent base-mesh cache. Interaction-only state is deliberately absent
/// from its key, so pointer movement, hover, focus, and selection do not rebuild it.
#[derive(Debug, Default)]
pub struct SunburstMeshCache {
    entry: Option<CachedMesh>,
    build_count: u64,
}

impl SunburstMeshCache {
    #[must_use]
    pub const fn new() -> Self {
        Self { entry: None, build_count: 0 }
    }

    /// Number of actual base-mesh builds since construction or the last clear.
    #[must_use]
    pub const fn build_count(&self) -> u64 {
        self.build_count
    }

    #[must_use]
    pub fn stats(&self) -> Option<SunburstMeshStats> {
        self.entry.as_ref().map(|entry| entry.stats)
    }

    /// Exposes the immutable cached mesh for diagnostics and renderer tests.
    #[must_use]
    pub fn mesh(&self) -> Option<&Arc<Mesh>> {
        self.entry.as_ref().map(|entry| &entry.mesh)
    }

    pub fn clear(&mut self) {
        self.entry = None;
        self.build_count = 0;
    }

    fn prepare(
        &mut self,
        layout: &SunburstLayout,
        geometry: ChartGeometry,
        options: SunburstMeshOptions,
        pixels_per_point: f32,
        dark_mode: bool,
        color_identity: &dyn Fn(NodeId) -> ColorIdentity,
    ) -> PreparedMesh {
        let options = options.sanitized();
        let pixels_per_point = sanitize_pixels_per_point(pixels_per_point);
        let key = MeshCacheKey::new(
            layout,
            geometry,
            options,
            pixels_per_point,
            dark_mode,
            color_identity,
        );

        if let Some(entry) = self.entry.as_ref().filter(|entry| entry.key == key) {
            return PreparedMesh {
                mesh: Arc::clone(&entry.mesh),
                stats: entry.stats,
                rebuilt: false,
            };
        }

        let (mesh, stats) =
            build_base_mesh(layout, geometry, options, pixels_per_point, dark_mode, color_identity);
        let mesh = Arc::new(mesh);
        self.build_count = self.build_count.saturating_add(1);
        self.entry = Some(CachedMesh { key, mesh: Arc::clone(&mesh), stats });

        PreparedMesh { mesh, stats, rebuilt: true }
    }
}

#[derive(Debug)]
struct CachedMesh {
    key: MeshCacheKey,
    mesh: Arc<Mesh>,
    stats: SunburstMeshStats,
}

struct PreparedMesh {
    mesh: Arc<Mesh>,
    stats: SunburstMeshStats,
    rebuilt: bool,
}

/// Sanitized description of the sector under the pointer.
///
/// Synthetic sectors intentionally expose no `NodeId`, even though the core
/// layout retains member metadata for synchronized textual inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SunburstHit {
    pub kind: SunburstHitKind,
    pub depth: u32,
    pub weight: u128,
    pub unknown_entries: u64,
}

impl SunburstHit {
    /// The only node that may be used for a filesystem or zoom action.
    #[must_use]
    pub const fn action_target(self) -> Option<NodeId> {
        match self.kind {
            SunburstHitKind::Real { node_id } => Some(node_id),
            SunburstHitKind::UnavailableReal
            | SunburstHitKind::Hidden { .. }
            | SunburstHitKind::Other { .. } => None,
        }
    }

    #[must_use]
    pub const fn is_actionable(self) -> bool {
        self.action_target().is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SunburstHitKind {
    Real {
        node_id: NodeId,
    },
    /// Defensive representation of a malformed `Real` sector without a node.
    UnavailableReal,
    Hidden {
        member_count: usize,
    },
    Other {
        member_count: usize,
        cause: OtherCause,
    },
}

/// Data supplied to a caller-owned, localized tooltip renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunburstTooltip {
    pub sector: SunburstHit,
    /// Known-weight share of the displayed root, if the root has known weight.
    pub root_fraction: Option<f64>,
}

/// Requested external state transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeChange {
    Set(NodeId),
    Clear,
}

/// Typed interaction requests emitted during a chart frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SunburstRequests {
    /// Single primary click outside the center; the shell decides file/folder policy.
    pub primary_click: Option<NodeId>,
    /// Primary click on the center, subject to the shell's parent availability.
    pub parent: bool,
    /// Zoom request from keyboard Enter.
    pub zoom: Option<NodeId>,
    /// Context-menu request from secondary-click or Shift+F10.
    pub context_menu: Option<NodeId>,
    pub selection: Option<NodeChange>,
    pub focus: Option<NodeChange>,
}

impl SunburstRequests {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.primary_click.is_none()
            && !self.parent
            && self.zoom.is_none()
            && self.context_menu.is_none()
            && self.selection.is_none()
            && self.focus.is_none()
    }
}

/// Result of rendering and interacting with one sunburst frame.
#[derive(Debug)]
pub struct SunburstResponse {
    pub widget: egui::Response,
    pub hovered: Option<SunburstHit>,
    pub tooltip: Option<SunburstTooltip>,
    pub requests: SunburstRequests,
    pub mesh_stats: SunburstMeshStats,
    pub cache_rebuilt: bool,
    pub chart_center: Pos2,
    pub chart_radius: f32,
    pub center_radius_normalized: f64,
}

/// One-frame view over an immutable layout and a caller-owned mesh cache.
pub struct SunburstView<'a> {
    layout: &'a SunburstLayout,
    cache: &'a mut SunburstMeshCache,
    selected: Option<NodeId>,
    focused: Option<NodeId>,
    desired_size: Option<Vec2>,
    options: SunburstMeshOptions,
    color_identity: Option<&'a dyn Fn(NodeId) -> ColorIdentity>,
    accessibility_label: Cow<'a, str>,
}

impl<'a> SunburstView<'a> {
    #[must_use]
    pub fn new(layout: &'a SunburstLayout, cache: &'a mut SunburstMeshCache) -> Self {
        Self {
            layout,
            cache,
            selected: None,
            focused: None,
            desired_size: None,
            options: SunburstMeshOptions::default(),
            color_identity: None,
            accessibility_label: Cow::Borrowed("Disk usage sunburst"),
        }
    }

    #[must_use]
    pub const fn selected(mut self, selected: Option<NodeId>) -> Self {
        self.selected = selected;
        self
    }

    #[must_use]
    pub const fn focused(mut self, focused: Option<NodeId>) -> Self {
        self.focused = focused;
        self
    }

    #[must_use]
    pub const fn desired_size(mut self, desired_size: Vec2) -> Self {
        self.desired_size = Some(desired_size);
        self
    }

    #[must_use]
    pub const fn mesh_options(mut self, options: SunburstMeshOptions) -> Self {
        self.options = options;
        self
    }

    /// Supplies immutable family/path identities and absolute ancestry depth.
    /// The callback must be pure and constant-time for the duration of `show`.
    #[must_use]
    pub fn color_identity(mut self, color_identity: &'a dyn Fn(NodeId) -> ColorIdentity) -> Self {
        self.color_identity = Some(color_identity);
        self
    }

    #[must_use]
    pub fn accessibility_label(mut self, label: impl Into<Cow<'a, str>>) -> Self {
        self.accessibility_label = label.into();
        self
    }

    /// Draws without creating a visual tooltip. Tooltip data remains available
    /// in [`SunburstResponse::tooltip`] for an external tooltip/details surface.
    pub fn show(self, ui: &mut egui::Ui) -> SunburstResponse {
        self.show_internal(ui, None::<fn(&mut egui::Ui, SunburstTooltip)>)
    }

    /// Draws and delegates tooltip contents to a caller-owned localization/data callback.
    pub fn show_with_tooltip(
        self,
        ui: &mut egui::Ui,
        add_tooltip: impl FnOnce(&mut egui::Ui, SunburstTooltip),
    ) -> SunburstResponse {
        self.show_internal(ui, Some(add_tooltip))
    }

    fn show_internal(
        self,
        ui: &mut egui::Ui,
        add_tooltip: Option<impl FnOnce(&mut egui::Ui, SunburstTooltip)>,
    ) -> SunburstResponse {
        let Self {
            layout,
            cache,
            selected,
            focused,
            desired_size,
            options,
            color_identity,
            accessibility_label,
        } = self;
        let options = options.sanitized();
        let desired_size = desired_chart_size(ui, desired_size, options.max_side_points);
        let (rect, mut response) = ui.allocate_exact_size(desired_size, Sense::click());
        let geometry = ChartGeometry::from_rect(rect, options);
        let pixels_per_point = sanitize_pixels_per_point(ui.ctx().pixels_per_point());
        let dark_mode = ui.visuals().dark_mode;
        let default_color_identity = |node: NodeId| ColorIdentity {
            family_key: u64::from(node.raw()),
            variant_key: u64::from(node.raw()),
            depth: 0,
        };
        let color_identity = color_identity.unwrap_or(&default_color_identity);

        let prepared =
            cache.prepare(layout, geometry, options, pixels_per_point, dark_mode, color_identity);
        if !prepared.mesh.indices.is_empty() {
            ui.painter().add(Shape::Mesh(Arc::clone(&prepared.mesh)));
        }
        if let Some(hub) =
            layout.sectors().first().and_then(|sector| visible_sector(sector, geometry, options))
        {
            let border = if dark_mode {
                Color32::from_rgb(64, 67, 73)
            } else {
                Color32::from_rgb(222, 223, 225)
            };
            ui.painter().circle_stroke(geometry.center, hub.outer_radius, Stroke::new(1.0, border));
        }

        let hovered_sector = response
            .hover_pos()
            .and_then(|pointer| hit_visible_sector(layout, geometry, options, pointer));
        let hovered = hovered_sector.map(SunburstHit::from_sector);
        let tooltip = hovered.map(|sector| tooltip_data(layout, sector));

        draw_interaction_overlays(
            ui,
            layout,
            geometry,
            options,
            pixels_per_point,
            dark_mode,
            OverlayState {
                hovered: hovered_sector,
                selected,
                focused,
                widget_has_focus: response.has_focus(),
            },
        );

        let signals = collect_interaction_signals(ui, &response);
        let requests = resolve_interactions(layout, hovered_sector, selected, focused, signals);
        if !requests.is_empty() {
            response.mark_changed();
        }
        response.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::Other,
                true,
                selected.is_some(),
                accessibility_label.as_ref(),
            )
        });

        if let (Some(add_tooltip), Some(tooltip)) = (add_tooltip, tooltip) {
            response = response.on_hover_ui_at_pointer(move |ui| add_tooltip(ui, tooltip));
        }

        SunburstResponse {
            widget: response,
            hovered,
            tooltip,
            requests,
            mesh_stats: prepared.stats,
            cache_rebuilt: prepared.rebuilt,
            chart_center: geometry.center,
            chart_radius: geometry.radius,
            center_radius_normalized: layout
                .sectors()
                .first()
                .map_or(1.0, |sector| sector.outer_radius),
        }
    }
}

impl SunburstHit {
    fn from_sector(sector: &Sector) -> Self {
        let kind = match sector.kind {
            SectorKind::Real => {
                sector.action_target().map_or(SunburstHitKind::UnavailableReal, |node_id| {
                    SunburstHitKind::Real { node_id }
                })
            }
            SectorKind::Hidden { members } => {
                SunburstHitKind::Hidden { member_count: members.len() }
            }
            SectorKind::Other { members, cause, .. } => {
                SunburstHitKind::Other { member_count: members.len(), cause }
            }
        };
        Self {
            kind,
            depth: sector.depth,
            weight: sector.weight,
            unknown_entries: sector.unknown_entries,
        }
    }
}

fn tooltip_data(layout: &SunburstLayout, sector: SunburstHit) -> SunburstTooltip {
    let root_weight = layout
        .sectors()
        .iter()
        .find(|candidate| candidate.depth == 0)
        .map_or(0, |candidate| candidate.weight);
    let root_fraction = (root_weight > 0).then(|| {
        let fraction = sector.weight as f64 / root_weight as f64;
        if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 }
    });
    SunburstTooltip { sector, root_fraction }
}

#[derive(Clone, Copy, Debug)]
struct ChartGeometry {
    center: Pos2,
    radius: f32,
}

impl ChartGeometry {
    fn from_rect(rect: Rect, options: SunburstMeshOptions) -> Self {
        let center = rect.center();
        let center = Pos2::new(
            quantize_finite(center.x, GEOMETRY_QUANTUM),
            quantize_finite(center.y, GEOMETRY_QUANTUM),
        );
        let half_side = finite_or(rect.width().min(rect.height()) * 0.5, 0.0).max(0.0);
        let radius = quantize_finite(
            (half_side - options.outer_margin_points.min(half_side)).max(0.0),
            GEOMETRY_QUANTUM,
        );
        Self { center, radius }
    }
}

#[derive(Clone, Copy, Debug)]
struct VisualSector {
    start_angle: f64,
    sweep_angle: f64,
    inner_radius: f32,
    outer_radius: f32,
}

impl VisualSector {
    fn contains(self, geometry: ChartGeometry, point: Pos2) -> bool {
        if !point.x.is_finite() || !point.y.is_finite() {
            return false;
        }
        let delta = point - geometry.center;
        let radius = delta.length();
        if !radius.is_finite() || radius < self.inner_radius || radius > self.outer_radius {
            return false;
        }
        if self.sweep_angle >= FULL_TURN - f64::EPSILON {
            return true;
        }
        let angle = f64::from(delta.y).atan2(f64::from(delta.x));
        let relative = normalize_angle(angle - self.start_angle);
        relative < self.sweep_angle
    }
}

fn visible_sector(
    sector: &Sector,
    geometry: ChartGeometry,
    options: SunburstMeshOptions,
) -> Option<VisualSector> {
    if geometry.radius <= 0.0
        || !sector.start_angle.is_finite()
        || !sector.sweep_angle.is_finite()
        || !sector.inner_radius.is_finite()
        || !sector.outer_radius.is_finite()
    {
        return None;
    }

    let sweep = sector.sweep_angle.clamp(0.0, FULL_TURN);
    if sweep < MIN_RENDER_SWEEP {
        return None;
    }
    let normalized_inner = sector.inner_radius.clamp(0.0, 1.0) as f32;
    let normalized_outer = sector.outer_radius.clamp(0.0, 1.0) as f32;
    let half_gap = options.gap_points * 0.5;
    let inner = if normalized_inner <= f32::EPSILON {
        0.0
    } else {
        normalized_inner.mul_add(geometry.radius, half_gap)
    };
    let outer = normalized_outer.mul_add(geometry.radius, -half_gap);
    if !inner.is_finite() || !outer.is_finite() || outer <= inner {
        return None;
    }

    let angular_inset = if sweep >= FULL_TURN - f64::EPSILON || options.gap_points == 0.0 {
        0.0
    } else {
        f64::from(half_gap / outer.max(1.0))
    };
    let visible_sweep = sweep - angular_inset * 2.0;
    if !visible_sweep.is_finite() || visible_sweep < MIN_RENDER_SWEEP {
        return None;
    }

    Some(VisualSector {
        start_angle: normalize_angle(sector.start_angle + angular_inset),
        sweep_angle: visible_sweep,
        inner_radius: inner,
        outer_radius: outer,
    })
}

fn hit_visible_sector(
    layout: &SunburstLayout,
    geometry: ChartGeometry,
    options: SunburstMeshOptions,
    point: Pos2,
) -> Option<&Sector> {
    if geometry.radius <= 0.0 || !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    let local = point - geometry.center;
    let normalized_x = f64::from(local.x / geometry.radius);
    let normalized_y = f64::from(local.y / geometry.radius);
    let sector = layout.hit_test(normalized_x, normalized_y).ok().flatten()?;
    visible_sector(sector, geometry, options)
        .filter(|visible| visible.contains(geometry, point))
        .map(|_| sector)
}

fn build_base_mesh(
    layout: &SunburstLayout,
    geometry: ChartGeometry,
    options: SunburstMeshOptions,
    pixels_per_point: f32,
    dark_mode: bool,
    color_identity: &dyn Fn(NodeId) -> ColorIdentity,
) -> (Mesh, SunburstMeshStats) {
    let mut mesh = Mesh::default();
    let reserve_vertices = layout.sectors().len().saturating_mul(8).min(options.max_vertices);
    let reserve_indices = layout.sectors().len().saturating_mul(18).min(options.max_indices);
    mesh.vertices.reserve(reserve_vertices);
    mesh.indices.reserve(reserve_indices);
    let mut stats = SunburstMeshStats::default();

    for sector in layout.sectors() {
        let Some(visible) = visible_sector(sector, geometry, options) else {
            stats.skipped_degenerate_sectors = stats.skipped_degenerate_sectors.saturating_add(1);
            continue;
        };
        let desired_subdivisions =
            subdivisions(visible.sweep_angle, visible.outer_radius, pixels_per_point, options);
        let color = sector_fill_color(sector, dark_mode, color_identity);
        let append =
            append_sector(&mut mesh, geometry, visible, desired_subdivisions, color, options);
        match append {
            AppendResult::Rendered => {
                stats.rendered_sectors = stats.rendered_sectors.saturating_add(1);
            }
            AppendResult::Degenerate => {
                stats.skipped_degenerate_sectors =
                    stats.skipped_degenerate_sectors.saturating_add(1);
            }
            AppendResult::Budget => {
                stats.skipped_budget_sectors = stats.skipped_budget_sectors.saturating_add(1);
            }
        }
    }

    stats.vertices = mesh.vertices.len();
    stats.indices = mesh.indices.len();
    stats.bytes = mesh.bytes_used();
    (mesh, stats)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppendResult {
    Rendered,
    Degenerate,
    Budget,
}

fn append_sector(
    mesh: &mut Mesh,
    geometry: ChartGeometry,
    sector: VisualSector,
    desired_subdivisions: usize,
    color: Color32,
    options: SunburstMeshOptions,
) -> AppendResult {
    let center_fan = sector.inner_radius <= f32::EPSILON;
    let available_vertices = options.max_vertices.saturating_sub(mesh.vertices.len());
    let available_indices = options.max_indices.saturating_sub(mesh.indices.len());
    let budget_subdivisions = if center_fan {
        available_vertices.saturating_sub(2).min(available_indices / 3)
    } else {
        (available_vertices / 2).saturating_sub(1).min(available_indices / 6)
    };
    let subdivisions = desired_subdivisions.min(budget_subdivisions);
    if subdivisions == 0 {
        return AppendResult::Budget;
    }

    let vertex_start = mesh.vertices.len();
    let index_start = mesh.indices.len();
    let Some(base) = u32::try_from(vertex_start).ok() else {
        return AppendResult::Budget;
    };

    if center_fan {
        mesh.vertices.push(Vertex::untextured(geometry.center, color));
        for sample in 0..=subdivisions {
            let fraction = sample as f64 / subdivisions as f64;
            let angle = sector.start_angle + sector.sweep_angle * fraction;
            mesh.vertices.push(Vertex::untextured(
                polar_position(geometry.center, sector.outer_radius, angle),
                color,
            ));
        }
        for segment in 0..subdivisions {
            let Some(segment) = u32::try_from(segment).ok() else {
                rollback_mesh(mesh, vertex_start, index_start);
                return AppendResult::Budget;
            };
            mesh.indices.extend_from_slice(&[base, base + segment + 1, base + segment + 2]);
        }
    } else {
        for sample in 0..=subdivisions {
            let fraction = sample as f64 / subdivisions as f64;
            let angle = sector.start_angle + sector.sweep_angle * fraction;
            mesh.vertices.push(Vertex::untextured(
                polar_position(geometry.center, sector.inner_radius, angle),
                color,
            ));
            mesh.vertices.push(Vertex::untextured(
                polar_position(geometry.center, sector.outer_radius, angle),
                color,
            ));
        }
        for segment in 0..subdivisions {
            let Some(segment) = u32::try_from(segment).ok() else {
                rollback_mesh(mesh, vertex_start, index_start);
                return AppendResult::Budget;
            };
            let inner = base + segment * 2;
            let outer = inner + 1;
            let next_inner = inner + 2;
            let next_outer = inner + 3;
            mesh.indices
                .extend_from_slice(&[inner, outer, next_outer, inner, next_outer, next_inner]);
        }
    }

    if new_geometry_is_valid(mesh, vertex_start, index_start) {
        AppendResult::Rendered
    } else {
        rollback_mesh(mesh, vertex_start, index_start);
        AppendResult::Degenerate
    }
}

fn rollback_mesh(mesh: &mut Mesh, vertex_start: usize, index_start: usize) {
    mesh.vertices.truncate(vertex_start);
    mesh.indices.truncate(index_start);
}

fn new_geometry_is_valid(mesh: &Mesh, vertex_start: usize, index_start: usize) -> bool {
    mesh.vertices[vertex_start..]
        .iter()
        .all(|vertex| vertex.pos.x.is_finite() && vertex.pos.y.is_finite())
        && mesh.indices[index_start..].chunks_exact(3).all(|triangle| {
            let Some(a) = mesh.vertices.get(triangle[0] as usize).map(|vertex| vertex.pos) else {
                return false;
            };
            let Some(b) = mesh.vertices.get(triangle[1] as usize).map(|vertex| vertex.pos) else {
                return false;
            };
            let Some(c) = mesh.vertices.get(triangle[2] as usize).map(|vertex| vertex.pos) else {
                return false;
            };
            signed_double_area(a, b, c).is_finite() && signed_double_area(a, b, c) > 0.0
        })
}

fn signed_double_area(a: Pos2, b: Pos2, c: Pos2) -> f32 {
    (b.x - a.x).mul_add(c.y - a.y, -(b.y - a.y) * (c.x - a.x))
}

fn subdivisions(
    sweep_angle: f64,
    outer_radius_points: f32,
    pixels_per_point: f32,
    options: SunburstMeshOptions,
) -> usize {
    let radius_pixels =
        f64::from((outer_radius_points * sanitize_pixels_per_point(pixels_per_point)).max(0.01));
    let error_pixels = f64::from(options.max_chord_error_pixels).min(radius_pixels);
    let cosine = (1.0 - error_pixels / radius_pixels).clamp(-1.0, 1.0);
    let max_step = (2.0 * cosine.acos()).max(1.0e-6);
    let desired = (sweep_angle / max_step).ceil();
    if !desired.is_finite() || desired <= 1.0 {
        1
    } else {
        (desired as usize).clamp(1, options.max_subdivisions_per_sector)
    }
}

fn polar_position(center: Pos2, radius: f32, angle: f64) -> Pos2 {
    let x = center.x + radius * angle.cos() as f32;
    let y = center.y + radius * angle.sin() as f32;
    Pos2::new(x, y)
}

fn draw_interaction_overlays(
    ui: &egui::Ui,
    layout: &SunburstLayout,
    geometry: ChartGeometry,
    options: SunburstMeshOptions,
    pixels_per_point: f32,
    dark_mode: bool,
    state: OverlayState<'_>,
) {
    let painter = ui.painter();
    if let Some(sector) = state.selected.and_then(|node| sector_for_node(layout, node)) {
        draw_sector_outline(
            painter,
            sector,
            geometry,
            options,
            pixels_per_point,
            Stroke::new(2.5, if dark_mode { SECTOR_EMBER } else { PLATTER_MIDNIGHT }),
        );
    }
    if let Some(sector) = state.hovered {
        draw_sector_outline(
            painter,
            sector,
            geometry,
            options,
            pixels_per_point,
            Stroke::new(1.5, if dark_mode { SEPARATOR_PORCELAIN } else { PLATTER_MIDNIGHT }),
        );
    }
    if state.widget_has_focus {
        if geometry.radius > 0.0 {
            painter.circle_stroke(
                geometry.center,
                geometry.radius + 2.0,
                Stroke::new(3.5, if dark_mode { PLATTER_MIDNIGHT } else { SEPARATOR_PORCELAIN }),
            );
            painter.circle_stroke(
                geometry.center,
                geometry.radius + 2.0,
                Stroke::new(1.75, SCAN_CURRENT),
            );
        }
        if let Some(sector) = state.focused.and_then(|node| sector_for_node(layout, node)) {
            draw_sector_outline(
                painter,
                sector,
                geometry,
                options,
                pixels_per_point,
                Stroke::new(3.5, if dark_mode { PLATTER_MIDNIGHT } else { SEPARATOR_PORCELAIN }),
            );
            draw_sector_outline(
                painter,
                sector,
                geometry,
                options,
                pixels_per_point,
                Stroke::new(1.75, SCAN_CURRENT),
            );
        }
    }
}

#[derive(Clone, Copy)]
struct OverlayState<'a> {
    hovered: Option<&'a Sector>,
    selected: Option<NodeId>,
    focused: Option<NodeId>,
    widget_has_focus: bool,
}

fn draw_sector_outline(
    painter: &egui::Painter,
    sector: &Sector,
    geometry: ChartGeometry,
    options: SunburstMeshOptions,
    pixels_per_point: f32,
    stroke: Stroke,
) {
    let Some(visible) = visible_sector(sector, geometry, options) else {
        return;
    };
    if visible.sweep_angle >= FULL_TURN - f64::EPSILON {
        painter.circle_stroke(geometry.center, visible.outer_radius, stroke);
        if visible.inner_radius > f32::EPSILON {
            painter.circle_stroke(geometry.center, visible.inner_radius, stroke);
        }
        return;
    }
    let samples =
        subdivisions(visible.sweep_angle, visible.outer_radius, pixels_per_point, options);
    let mut points = Vec::with_capacity(samples.saturating_mul(2).saturating_add(2));
    for sample in 0..=samples {
        let fraction = sample as f64 / samples as f64;
        points.push(polar_position(
            geometry.center,
            visible.outer_radius,
            visible.start_angle + visible.sweep_angle * fraction,
        ));
    }
    if visible.inner_radius <= f32::EPSILON {
        points.push(geometry.center);
    } else {
        for sample in (0..=samples).rev() {
            let fraction = sample as f64 / samples as f64;
            points.push(polar_position(
                geometry.center,
                visible.inner_radius,
                visible.start_angle + visible.sweep_angle * fraction,
            ));
        }
    }
    if points.iter().all(|point| point.x.is_finite() && point.y.is_finite()) {
        painter.add(Shape::closed_line(points, stroke));
    }
}

fn sector_for_node(layout: &SunburstLayout, node: NodeId) -> Option<&Sector> {
    layout.sectors().iter().find(|sector| sector.action_target() == Some(node))
}

#[derive(Clone, Copy, Debug, Default)]
struct InteractionSignals {
    pointer_present: bool,
    primary_clicked: bool,
    double_clicked: bool,
    secondary_clicked: bool,
    enter: bool,
    space: bool,
    escape: bool,
    context_key: bool,
    navigation: Option<Navigation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Navigation {
    Left,
    Right,
    Up,
    Down,
}

fn collect_interaction_signals(ui: &egui::Ui, response: &egui::Response) -> InteractionSignals {
    let mut signals = InteractionSignals {
        pointer_present: response.interact_pointer_pos().is_some(),
        primary_clicked: response.clicked(),
        double_clicked: response.double_clicked(),
        secondary_clicked: response.secondary_clicked(),
        ..InteractionSignals::default()
    };
    if response.has_focus() {
        ui.input(|input| {
            signals.enter = input.key_pressed(Key::Enter);
            signals.space = input.key_pressed(Key::Space);
            signals.escape = input.key_pressed(Key::Escape);
            signals.context_key = input.modifiers.shift && input.key_pressed(Key::F10);
            signals.navigation = if input.key_pressed(Key::ArrowLeft) {
                Some(Navigation::Left)
            } else if input.key_pressed(Key::ArrowRight) {
                Some(Navigation::Right)
            } else if input.key_pressed(Key::ArrowUp) {
                Some(Navigation::Up)
            } else if input.key_pressed(Key::ArrowDown) {
                Some(Navigation::Down)
            } else {
                None
            };
        });
    }
    signals
}

fn resolve_interactions(
    layout: &SunburstLayout,
    hovered: Option<&Sector>,
    selected: Option<NodeId>,
    focused: Option<NodeId>,
    signals: InteractionSignals,
) -> SunburstRequests {
    let mut requests = SunburstRequests::default();
    let hovered_target = hovered.and_then(Sector::action_target);
    let current = current_action_target(layout, focused, selected);

    // The first click already activates a directory. A recognized second click
    // must not activate the newly zoomed content underneath the same pointer.
    if signals.primary_clicked && !signals.double_clicked && !signals.enter && !signals.space {
        let target = if signals.pointer_present { hovered_target } else { current };
        if let Some(target) = target {
            if target == layout.root() {
                requests.parent = true;
            } else {
                requests.primary_click = Some(target);
                requests.focus = Some(NodeChange::Set(target));
            }
        } else if signals.pointer_present && hovered.is_none() {
            requests.selection = Some(NodeChange::Clear);
            requests.focus = Some(NodeChange::Clear);
        }
    }
    if signals.secondary_clicked {
        requests.context_menu = hovered_target;
    }
    if signals.enter {
        requests.zoom = current;
    }
    if signals.space
        && let Some(target) = current
    {
        requests.selection = Some(NodeChange::Set(target));
        requests.focus = Some(NodeChange::Set(target));
    }
    if signals.context_key {
        requests.context_menu = current;
    }
    if let Some(navigation) = signals.navigation
        && let Some(target) = navigate(layout, current, navigation)
    {
        requests.focus = Some(NodeChange::Set(target));
    }
    if signals.escape {
        requests.selection = Some(NodeChange::Clear);
        requests.focus = Some(NodeChange::Clear);
    }
    requests
}

fn current_action_target(
    layout: &SunburstLayout,
    focused: Option<NodeId>,
    selected: Option<NodeId>,
) -> Option<NodeId> {
    focused
        .filter(|node| sector_for_node(layout, *node).is_some())
        .or_else(|| selected.filter(|node| sector_for_node(layout, *node).is_some()))
        .or_else(|| sector_for_node(layout, layout.root()).and_then(Sector::action_target))
        .or_else(|| layout.sectors().iter().find_map(Sector::action_target))
}

fn navigate(
    layout: &SunburstLayout,
    current: Option<NodeId>,
    navigation: Navigation,
) -> Option<NodeId> {
    let current = current.and_then(|node| sector_for_node(layout, node));
    let Some(current) = current else {
        return layout.sectors().iter().find_map(Sector::action_target);
    };
    match navigation {
        Navigation::Left => horizontal_neighbor(layout, current, false),
        Navigation::Right => horizontal_neighbor(layout, current, true),
        Navigation::Up => radial_neighbor(layout, current, false),
        Navigation::Down => radial_neighbor(layout, current, true),
    }
}

fn horizontal_neighbor(layout: &SunburstLayout, current: &Sector, forward: bool) -> Option<NodeId> {
    let mut first = None;
    let mut previous = None;
    let mut found_current = false;
    for sector in layout.sectors().iter().filter(|sector| sector.depth == current.depth) {
        let Some(node) = sector.action_target() else {
            continue;
        };
        first.get_or_insert(node);
        if std::ptr::eq(sector, current) {
            found_current = true;
            if !forward {
                return previous.or_else(|| {
                    layout
                        .sectors()
                        .iter()
                        .filter(|candidate| candidate.depth == current.depth)
                        .filter_map(Sector::action_target)
                        .next_back()
                });
            }
        } else if found_current && forward {
            return Some(node);
        }
        previous = Some(node);
    }
    if forward { first } else { previous }
}

fn radial_neighbor(layout: &SunburstLayout, current: &Sector, outward: bool) -> Option<NodeId> {
    let target_depth =
        if outward { current.depth.checked_add(1)? } else { current.depth.checked_sub(1)? };
    let target_ring = layout.rings().iter().find(|ring| ring.depth == target_depth)?;
    let radius = (target_ring.inner_radius + target_ring.outer_radius) * 0.5;
    let angle = current.start_angle + current.sweep_angle * 0.5;
    if let Some(target) =
        layout.hit_test_polar(angle, radius).ok().flatten().and_then(Sector::action_target)
    {
        return Some(target);
    }

    layout.sectors()[target_ring.sectors.clone()]
        .iter()
        .filter_map(|sector| {
            let target = sector.action_target()?;
            let offset = normalize_angle(sector.start_angle - current.start_angle);
            (offset < current.sweep_angle).then_some(target)
        })
        .next()
}

/// Stable branch-family color shared by the chart and the optional item list.
/// Absolute ancestry depth keeps the tone unchanged when the view root changes.
#[must_use]
pub fn stable_sector_color(identity: ColorIdentity, dark_mode: bool) -> Color32 {
    // Use the full hue circle rather than a handful of palette slots: unrelated
    // branches should not routinely collapse to identical colors. Native-path
    // identity, never ordering or the visible siblings, determines the hue.
    let hue = (mix64(identity.family_key) >> 40) as f32 * (360.0 / 16_777_216.0);
    let variation = ((mix64(identity.variant_key) >> 32) & 15) as f32 / 15.0 - 0.5;
    let depth = identity.depth.min(4) as f32;
    let (saturation, lightness) = if dark_mode {
        (0.45, 0.55 + depth * 0.05 + variation * 0.02)
    } else {
        (0.52, 0.43 + depth * 0.07 + variation * 0.02)
    };
    // CSS HSL works in sRGB; egui's Hsva is linear, so convert explicitly.
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let section = hue / 60.0;
    let secondary = chroma * (1.0 - (section % 2.0 - 1.0).abs());
    let channels = match section as u8 {
        0 => [chroma, secondary, 0.0],
        1 => [secondary, chroma, 0.0],
        2 => [0.0, chroma, secondary],
        3 => [0.0, secondary, chroma],
        4 => [secondary, 0.0, chroma],
        _ => [chroma, 0.0, secondary],
    };
    let offset = lightness - chroma * 0.5;
    let [red, green, blue] = channels.map(|channel| ((channel + offset) * 255.0).round() as u8);
    Color32::from_rgb(red, green, blue)
}

fn sector_fill_color(
    sector: &Sector,
    dark_mode: bool,
    color_identity: &dyn Fn(NodeId) -> ColorIdentity,
) -> Color32 {
    // The hub carries text and parent navigation, never a misleading colored
    // "usage" fill for an empty or entirely unknown snapshot.
    if sector.depth == 0 {
        return if dark_mode { Color32::from_rgb(32, 33, 36) } else { Color32::WHITE };
    }
    match sector.kind {
        SectorKind::Real => sector.action_target().map_or_else(
            || synthetic_hidden_color(dark_mode),
            |node| stable_sector_color(color_identity(node), dark_mode),
        ),
        SectorKind::Hidden { .. } => synthetic_hidden_color(dark_mode),
        SectorKind::Other { .. } => synthetic_other_color(dark_mode),
    }
}

fn synthetic_hidden_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(116, 125, 136) } else { Color32::from_rgb(89, 99, 109) }
}

fn synthetic_other_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(147, 118, 79) } else { Color32::from_rgb(103, 79, 48) }
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MeshCacheKey {
    layout_fingerprint: u64,
    center_x: u32,
    center_y: u32,
    radius: u32,
    pixels_per_point: u32,
    dark_mode: bool,
    gap_points: u32,
    chord_error: u32,
    max_subdivisions: usize,
    max_vertices: usize,
    max_indices: usize,
}

impl MeshCacheKey {
    fn new(
        layout: &SunburstLayout,
        geometry: ChartGeometry,
        options: SunburstMeshOptions,
        pixels_per_point: f32,
        dark_mode: bool,
        color_identity: &dyn Fn(NodeId) -> ColorIdentity,
    ) -> Self {
        Self {
            layout_fingerprint: layout_fingerprint(layout, color_identity),
            center_x: geometry.center.x.to_bits(),
            center_y: geometry.center.y.to_bits(),
            radius: geometry.radius.to_bits(),
            pixels_per_point: pixels_per_point.to_bits(),
            dark_mode,
            gap_points: options.gap_points.to_bits(),
            chord_error: options.max_chord_error_pixels.to_bits(),
            max_subdivisions: options.max_subdivisions_per_sector,
            max_vertices: options.max_vertices,
            max_indices: options.max_indices,
        }
    }
}

fn layout_fingerprint(
    layout: &SunburstLayout,
    color_identity: &dyn Fn(NodeId) -> ColorIdentity,
) -> u64 {
    let mut fingerprint = Fingerprint::new();
    fingerprint.write_u64(layout.source_generation().get());
    fingerprint.write_u32(layout.root().raw());
    fingerprint.write_u32(layout.effective_depth());
    fingerprint.write_u8(match layout.size_basis() {
        SizeBasis::Logical => 0,
        SizeBasis::Allocated => 1,
    });
    fingerprint.write_u8(u8::from(layout.budget_exhausted()));
    fingerprint.write_usize(layout.sectors().len());
    for sector in layout.sectors() {
        fingerprint.write_u32(sector.node_id.map_or(u32::MAX, NodeId::raw));
        fingerprint.write_u32(sector.depth);
        fingerprint.write_u64(sector.start_angle.to_bits());
        fingerprint.write_u64(sector.sweep_angle.to_bits());
        fingerprint.write_u64(sector.inner_radius.to_bits());
        fingerprint.write_u64(sector.outer_radius.to_bits());
        fingerprint.write_u64(sector.weight as u64);
        fingerprint.write_u64((sector.weight >> 64) as u64);
        fingerprint.write_u64(sector.unknown_entries);
        match sector.kind {
            SectorKind::Real => {
                fingerprint.write_u8(0);
                if let Some(node) = sector.action_target() {
                    let identity = color_identity(node);
                    fingerprint.write_u64(identity.family_key);
                    fingerprint.write_u64(identity.variant_key);
                    fingerprint.write_u32(identity.depth);
                }
            }
            SectorKind::Hidden { members } => {
                fingerprint.write_u8(1);
                fingerprint.write_usize(members.start());
                fingerprint.write_usize(members.len());
            }
            SectorKind::Other { parent, members, cause } => {
                fingerprint.write_u8(2);
                fingerprint.write_u32(parent.raw());
                fingerprint.write_usize(members.start());
                fingerprint.write_usize(members.len());
                fingerprint.write_u8(match cause {
                    OtherCause::MinimumSweep => 0,
                    OtherCause::SectorBudget => 1,
                    OtherCause::MinimumSweepAndSectorBudget => 2,
                });
            }
        }
    }
    fingerprint.finish()
}

struct Fingerprint(u64);

impl Fingerprint {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write_u8(&mut self, value: u8) {
        self.0 ^= u64::from(value);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.write_u8(byte);
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(u64::try_from(value).unwrap_or(u64::MAX));
    }

    fn finish(self) -> u64 {
        mix64(self.0)
    }
}

fn desired_chart_size(ui: &egui::Ui, desired_size: Option<Vec2>, max_side_points: f32) -> Vec2 {
    let fallback_side = {
        let available = ui.available_size_before_wrap();
        let available_side = available.x.min(available.y);
        if available_side.is_finite() { available_side.max(0.0) } else { DEFAULT_SIDE_POINTS }
    };
    let desired = desired_size.unwrap_or(Vec2::splat(fallback_side));
    Vec2::new(
        sanitize_dimension(desired.x, fallback_side, max_side_points),
        sanitize_dimension(desired.y, fallback_side, max_side_points),
    )
}

fn sanitize_dimension(value: f32, fallback: f32, maximum: f32) -> f32 {
    finite_or(value, fallback).clamp(0.0, maximum.min(HARD_MAX_SIDE_POINTS))
}

fn sanitize_pixels_per_point(value: f32) -> f32 {
    finite_or(value, 1.0).clamp(0.25, 16.0)
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn quantize_finite(value: f32, quantum: f32) -> f32 {
    if !value.is_finite() || !quantum.is_finite() || quantum <= 0.0 {
        return 0.0;
    }
    let quantized = (value / quantum).round() * quantum;
    if quantized.is_finite() { quantized } else { 0.0 }
}

fn normalize_angle(angle: f64) -> f64 {
    if angle.is_finite() { angle.rem_euclid(FULL_TURN) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_core::{
        GenerationId, MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder,
        sunburst::{HiddenBranches, LayoutOptions, compute_layout},
    };

    const SOURCE: MetricSource = MetricSource::PortableMetadata;

    fn metrics(bytes: u64) -> OwnMetrics {
        OwnMetrics::new(SizeMetric::known(bytes, SOURCE), SizeMetric::known(bytes, SOURCE))
    }

    fn layout_with_children(count: usize) -> SunburstLayout {
        let mut builder = TreeBuilder::new(GenerationId::new(41));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        for index in 0..count {
            builder
                .add_child(
                    root,
                    NodeSpec::file(
                        format!("file-{index:04}"),
                        metrics(u64::try_from(index + 1).unwrap()),
                    ),
                )
                .unwrap();
        }
        let snapshot = builder.freeze().unwrap();
        let mut options = LayoutOptions::new(root);
        options.start_angle = 0.0;
        options.minimum_sweep = 0.0;
        options.max_depth = 4;
        options.max_sectors = count.saturating_add(1).max(1);
        compute_layout(&snapshot, options, &HiddenBranches::new()).unwrap()
    }

    fn test_geometry() -> ChartGeometry {
        ChartGeometry { center: Pos2::new(240.0, 240.0), radius: 220.0 }
    }

    fn key(node: NodeId) -> ColorIdentity {
        let identity = u64::from(node.raw()).wrapping_mul(0x9e37_79b9);
        ColorIdentity { family_key: identity, variant_key: identity, depth: 0 }
    }

    #[test]
    fn mesh_is_finite_consistently_wound_and_within_budget() {
        let layout = layout_with_children(96);
        let options = SunburstMeshOptions::default().sanitized();
        let (mesh, stats) = build_base_mesh(&layout, test_geometry(), options, 1.75, true, &key);

        assert!(!mesh.indices.is_empty());
        assert_eq!(mesh.indices.len() % 3, 0);
        assert_eq!(stats.vertices, mesh.vertices.len());
        assert_eq!(stats.indices, mesh.indices.len());
        assert!(stats.vertices <= options.max_vertices);
        assert!(stats.indices <= options.max_indices);
        assert!(
            mesh.vertices.iter().all(|vertex| vertex.pos.x.is_finite() && vertex.pos.y.is_finite())
        );
        for triangle in mesh.indices.chunks_exact(3) {
            let a = mesh.vertices[triangle[0] as usize].pos;
            let b = mesh.vertices[triangle[1] as usize].pos;
            let c = mesh.vertices[triangle[2] as usize].pos;
            assert!(signed_double_area(a, b, c) > 0.0);
        }
    }

    #[test]
    fn mesh_budget_is_hard_and_degrades_without_panicking() {
        let layout = layout_with_children(128);
        let options = SunburstMeshOptions {
            gap_points: 0.0,
            max_subdivisions_per_sector: HARD_MAX_SUBDIVISIONS,
            max_vertices: 64,
            max_indices: 96,
            ..SunburstMeshOptions::default()
        }
        .sanitized();
        let (mesh, stats) = build_base_mesh(&layout, test_geometry(), options, 4.0, false, &key);

        assert!(mesh.vertices.len() <= 64);
        assert!(mesh.indices.len() <= 96);
        assert!(stats.skipped_budget_sectors > 0);
        assert!(mesh.indices.iter().all(|index| (*index as usize) < mesh.vertices.len()));
    }

    #[test]
    fn tessellation_adapts_to_physical_dpi() {
        let layout = layout_with_children(8);
        let options = SunburstMeshOptions { gap_points: 0.0, ..Default::default() }.sanitized();
        let (low, _) = build_base_mesh(&layout, test_geometry(), options, 1.0, true, &key);
        let (high, _) = build_base_mesh(&layout, test_geometry(), options, 3.0, true, &key);

        assert!(high.vertices.len() > low.vertices.len());
        assert!(high.indices.len() > low.indices.len());
    }

    #[test]
    fn mesh_and_colors_are_deterministic_and_hover_does_not_invalidate_cache() {
        let layout = layout_with_children(24);
        let geometry = test_geometry();
        let options = SunburstMeshOptions::default().sanitized();
        let mut first_cache = SunburstMeshCache::new();
        let first = first_cache.prepare(&layout, geometry, options, 1.5, true, &key);
        assert!(first.rebuilt);
        assert_eq!(first_cache.build_count(), 1);

        let sector = &layout.sectors()[1];
        let angle = sector.start_angle + sector.sweep_angle * 0.5;
        let radius = ((sector.inner_radius + sector.outer_radius) * 0.5) as f32 * geometry.radius;
        let pointer = polar_position(geometry.center, radius, angle);
        assert!(hit_visible_sector(&layout, geometry, options, pointer).is_some());

        let warm = first_cache.prepare(&layout, geometry, options, 1.5, true, &key);
        assert!(!warm.rebuilt);
        assert_eq!(first_cache.build_count(), 1);
        assert!(Arc::ptr_eq(&first.mesh, &warm.mesh));

        let mut second_cache = SunburstMeshCache::new();
        let second = second_cache.prepare(&layout, geometry, options, 1.5, true, &key);
        assert_eq!(first.mesh.as_ref(), second.mesh.as_ref());
    }

    #[test]
    fn a_changed_partial_layout_with_the_same_generation_invalidates_cache() {
        let first_layout = layout_with_children(2);
        let second_layout = layout_with_children(3);
        assert_eq!(first_layout.source_generation(), second_layout.source_generation());
        let mut cache = SunburstMeshCache::new();
        let options = SunburstMeshOptions::default();
        assert!(cache.prepare(&first_layout, test_geometry(), options, 1.0, false, &key).rebuilt);
        assert!(cache.prepare(&second_layout, test_geometry(), options, 1.0, false, &key).rebuilt);
        assert_eq!(cache.build_count(), 2);
    }

    #[test]
    fn palette_is_stable_and_uses_lighter_descendant_tones_in_both_themes() {
        let dark_surface = Color32::from_rgb(32, 36, 43);
        let light_surface = Color32::from_rgb(244, 247, 249);
        for identity in 0..128_u64 {
            for depth in 0..20 {
                let color = ColorIdentity { family_key: identity, variant_key: identity, depth };
                let dark = stable_sector_color(color, true);
                let light = stable_sector_color(color, false);
                assert_eq!(dark, stable_sector_color(color, true));
                assert_eq!(light, stable_sector_color(color, false));
                // Sector fills carry hierarchy; text and interaction outlines
                // remain separate. Outer tones intentionally become pastel.
                assert!(contrast_ratio(dark, dark_surface) >= 2.5, "{dark:?}");
                assert!(contrast_ratio(light, light_surface) >= 1.3, "{light:?}");
                let base = ColorIdentity { depth: 0, ..color };
                assert!(
                    relative_luminance(dark) >= relative_luminance(stable_sector_color(base, true))
                );
                assert!(
                    relative_luminance(light)
                        >= relative_luminance(stable_sector_color(base, false))
                );
            }
        }
        assert!(contrast_ratio(synthetic_hidden_color(true), dark_surface) >= 3.0);
        assert!(contrast_ratio(synthetic_other_color(true), dark_surface) >= 3.0);
        assert!(contrast_ratio(synthetic_hidden_color(false), light_surface) >= 3.0);
        assert!(contrast_ratio(synthetic_other_color(false), light_surface) >= 3.0);
    }

    #[test]
    fn hit_test_delegates_to_layout_then_rejects_rendered_gaps() {
        let layout = layout_with_children(2);
        let geometry = test_geometry();
        let options = SunburstMeshOptions::default().sanitized();
        let sector = layout.sectors().iter().find(|sector| sector.depth == 1).unwrap();
        let radius = ((sector.inner_radius + sector.outer_radius) * 0.5) as f32 * geometry.radius;
        let midpoint =
            polar_position(geometry.center, radius, sector.start_angle + sector.sweep_angle * 0.5);
        assert_eq!(hit_visible_sector(&layout, geometry, options, midpoint), Some(sector));

        let boundary = polar_position(geometry.center, radius, sector.start_angle);
        let local = boundary - geometry.center;
        assert!(
            layout
                .hit_test(
                    f64::from(local.x / geometry.radius),
                    f64::from(local.y / geometry.radius),
                )
                .unwrap()
                .is_some()
        );
        assert!(hit_visible_sector(&layout, geometry, options, boundary).is_none());
    }

    #[test]
    fn synthetic_hits_never_emit_click_zoom_or_context_requests() {
        let mut builder = TreeBuilder::new(GenerationId::new(7));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let large = builder.add_child(root, NodeSpec::file("large", metrics(100))).unwrap();
        let small = builder.add_child(root, NodeSpec::file("small", metrics(1))).unwrap();
        let snapshot = builder.freeze().unwrap();
        let mut options = LayoutOptions::new(root);
        options.start_angle = 0.0;
        options.minimum_sweep = 0.2;
        let layout = compute_layout(&snapshot, options, &HiddenBranches::new()).unwrap();
        let other = layout
            .sectors()
            .iter()
            .find(|sector| matches!(sector.kind, SectorKind::Other { .. }))
            .unwrap();
        let hit = SunburstHit::from_sector(other);
        assert!(!hit.is_actionable());
        assert_eq!(hit.action_target(), None);

        let signals = InteractionSignals {
            pointer_present: true,
            primary_clicked: true,
            double_clicked: true,
            secondary_clicked: true,
            ..Default::default()
        };
        let requests =
            resolve_interactions(&layout, Some(other), Some(large), Some(large), signals);
        assert!(requests.is_empty());

        let real = sector_for_node(&layout, large).unwrap();
        let requests = resolve_interactions(&layout, Some(real), None, None, signals);
        assert_eq!(requests.primary_click, None, "second click is suppressed");
        assert_eq!(requests.zoom, None);
        assert_eq!(requests.context_menu, Some(large));
        assert_ne!(large, small);
    }

    #[test]
    fn keyboard_navigation_enter_and_escape_are_typed_requests() {
        let layout = layout_with_children(3);
        let root = layout.root();
        let child = layout
            .sectors()
            .iter()
            .find(|sector| sector.depth == 1)
            .and_then(Sector::action_target)
            .unwrap();
        let enter = resolve_interactions(
            &layout,
            None,
            Some(child),
            Some(child),
            InteractionSignals { enter: true, ..Default::default() },
        );
        assert_eq!(enter.zoom, Some(child));

        let space = resolve_interactions(
            &layout,
            None,
            None,
            Some(child),
            InteractionSignals { space: true, primary_clicked: true, ..Default::default() },
        );
        assert_eq!(space.selection, Some(NodeChange::Set(child)));
        assert_eq!(space.zoom, None);
        assert_eq!(space.primary_click, None);
        assert!(!space.parent);

        let up = resolve_interactions(
            &layout,
            None,
            Some(child),
            Some(child),
            InteractionSignals { navigation: Some(Navigation::Up), ..Default::default() },
        );
        assert_eq!(up.focus, Some(NodeChange::Set(root)));

        let escape = resolve_interactions(
            &layout,
            None,
            Some(child),
            Some(child),
            InteractionSignals { escape: true, ..Default::default() },
        );
        assert_eq!(escape.selection, Some(NodeChange::Clear));
        assert_eq!(escape.focus, Some(NodeChange::Clear));
    }

    #[test]
    fn primary_click_routes_real_nodes_and_hub_without_double_activation() {
        let layout = layout_with_children(2);
        let hub = sector_for_node(&layout, layout.root()).unwrap();
        let child = layout.sectors().iter().find(|sector| sector.depth == 1).unwrap();
        let click = InteractionSignals {
            pointer_present: true,
            primary_clicked: true,
            ..Default::default()
        };
        let request = resolve_interactions(&layout, Some(child), None, None, click);
        assert_eq!(request.primary_click, child.action_target());
        assert_eq!(request.selection, None, "shell determines file/folder policy");
        assert!(!request.parent);
        assert_eq!(request.zoom, None);
        let parent = resolve_interactions(&layout, Some(hub), None, None, click);
        assert!(parent.parent);
        assert_eq!(parent.primary_click, None);
        for sector in [hub, child] {
            let second = resolve_interactions(
                &layout,
                Some(sector),
                None,
                None,
                InteractionSignals { double_clicked: true, ..click },
            );
            assert!(second.is_empty());
        }
    }

    #[test]
    fn ancestry_colors_remain_identical_when_zoom_changes_sector_depth() {
        let mut builder = TreeBuilder::new(GenerationId::new(17));
        let root = builder.add_root(NodeSpec::root("root")).unwrap();
        let directory = builder.add_child(root, NodeSpec::directory("directory")).unwrap();
        let leaf = builder.add_child(directory, NodeSpec::file("leaf", metrics(100))).unwrap();
        let snapshot = builder.freeze().unwrap();
        let presentation = diskpie_app::presentation::SnapshotPresentation::new(&snapshot).unwrap();
        let original =
            compute_layout(&snapshot, LayoutOptions::new(root), &HiddenBranches::new()).unwrap();
        let zoomed =
            compute_layout(&snapshot, LayoutOptions::new(directory), &HiddenBranches::new())
                .unwrap();
        let identity = |node| presentation.color_identity(node).unwrap();
        let before = sector_for_node(&original, leaf).unwrap();
        let after = sector_for_node(&zoomed, leaf).unwrap();
        assert_ne!(before.depth, after.depth);
        for dark_mode in [true, false] {
            assert_eq!(
                sector_fill_color(before, dark_mode, &identity),
                sector_fill_color(after, dark_mode, &identity)
            );
        }
    }

    #[test]
    fn visual_fixture_families_have_distinct_colors_independent_of_order() {
        let build = |reversed: bool| {
            let mut builder = TreeBuilder::new(GenerationId::new(29));
            let root = builder.add_root(NodeSpec::root(r"C:\DiskPie-Visual-Fixture")).unwrap();
            let mut names = ["Documentos", "Juegos", "Fotos", "Vídeos", "Trabajo"];
            if reversed {
                names.reverse();
            }
            let branches = names.map(|name| {
                let node = builder.add_child(root, NodeSpec::directory(name)).unwrap();
                builder.add_child(node, NodeSpec::file("leaf", metrics(1))).unwrap();
                (name, node)
            });
            let snapshot = builder.freeze().unwrap();
            let presentation =
                diskpie_app::presentation::SnapshotPresentation::new(&snapshot).unwrap();
            branches.map(|(name, node)| (name, presentation.color_identity(node).unwrap()))
        };
        let original = build(false);
        let reordered = build(true);
        for dark in [false, true] {
            for (index, (name, identity)) in original.iter().enumerate() {
                let color = stable_sector_color(*identity, dark);
                let same = reordered.iter().find(|(other, _)| other == name).unwrap().1;
                assert_eq!(color, stable_sector_color(same, dark));
                for (_, other) in &original[index + 1..] {
                    assert_ne!(color, stable_sector_color(*other, dark));
                }
            }
        }
    }

    #[test]
    fn family_variant_and_absolute_depth_invalidate_mesh_but_interaction_does_not() {
        let layout = layout_with_children(2);
        let mut cache = SunburstMeshCache::new();
        let options = SunburstMeshOptions::default();
        for identity in [
            ColorIdentity { family_key: 1, variant_key: 2, depth: 0 },
            ColorIdentity { family_key: 3, variant_key: 2, depth: 0 },
            ColorIdentity { family_key: 3, variant_key: 4, depth: 0 },
            ColorIdentity { family_key: 3, variant_key: 4, depth: 1 },
        ] {
            assert!(
                cache.prepare(&layout, test_geometry(), options, 1.0, false, &|_| identity).rebuilt
            );
        }
        assert_eq!(cache.build_count(), 4);

        let ctx = egui::Context::default();
        let child = layout.sectors()[1].action_target();
        let mut cache = SunburstMeshCache::new();
        for (selected, focused, pointer) in [
            (None, None, false),
            (child, None, false),
            (child, child, false),
            (None, child, true),
            (None, None, false),
        ] {
            let mut input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::splat(480.0))),
                ..Default::default()
            };
            if pointer {
                input.events.push(egui::Event::PointerMoved(Pos2::new(240.0, 50.0)));
            }
            let _ = ctx.run_ui(input, |ui| {
                SunburstView::new(&layout, &mut cache)
                    .desired_size(Vec2::splat(400.0))
                    .selected(selected)
                    .focused(focused)
                    .color_identity(&key)
                    .show(ui);
            });
        }
        assert_eq!(cache.build_count(), 1);
    }

    #[test]
    fn zero_weight_partial_layout_still_builds_a_finite_center() {
        let layout = layout_with_children(0);
        let (mesh, stats) = build_base_mesh(
            &layout,
            test_geometry(),
            SunburstMeshOptions::default().sanitized(),
            1.0,
            true,
            &key,
        );
        assert_eq!(stats.rendered_sectors, 1);
        assert_eq!(layout.effective_depth(), 0);
        assert_eq!(layout.sectors()[0].outer_radius, 1.0);
        assert_eq!(sector_fill_color(&layout.sectors()[0], false, &key), Color32::WHITE);
        assert_eq!(
            sector_fill_color(&layout.sectors()[0], true, &key),
            Color32::from_rgb(32, 33, 36)
        );
        assert!(!mesh.indices.is_empty());
        assert!(
            mesh.vertices.iter().all(|vertex| vertex.pos.x.is_finite() && vertex.pos.y.is_finite())
        );
    }

    fn contrast_ratio(left: Color32, right: Color32) -> f64 {
        let left = relative_luminance(left);
        let right = relative_luminance(right);
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }

    fn relative_luminance(color: Color32) -> f64 {
        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.040_45 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        }
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }
}
