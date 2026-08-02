# Sunburst rendering and UI component research

- Research date: 2026-08-02
- Baseline: stable Rust, `egui`/`eframe`, Windows 10/11 x86-64 portable
  executable
- License target: `MIT OR Apache-2.0`
- Decision record: [ADR 0006](../adr/0006-local-sunburst-engine.md)

## Recommendation

Implement DiskPie's sunburst layout, pruning, stable colors, hit testing, and
annular mesh generation locally in safe Rust. The domain layout must not
depend on egui or Windows APIs. Adapt its immutable output to a cached
`egui::Mesh` at the UI boundary.

Reuse focused ecosystem components at the seams:

- adopt `egui_extras` for the virtualized textual tree/table;
- use AccessKit through eframe's existing integration rather than adding a
  second platform adapter;
- put folder selection behind an application-owned `FolderPicker` trait;
- put localization behind an application-owned typed `I18n` facade, with
  Fluent as the preferred full-featured backend;
- keep icons local and vector-based initially; and
- use `proptest` and Criterion as development-only dependencies.

Do not add a charting crate or `lyon_tessellation` for the initial sunburst.
The geometry is simple enough to generate analytically, and a generic path
tessellator would not solve hit testing, stable identity, partial-snapshot
behavior, or accessible navigation.

## Clean-room and license boundary

Open-source analyzers were consulted only for externally visible behavior and
general algorithms. KDE Filelight is GPL and SquirrelDisk is AGPL-3.0. Their
source must not be copied, translated, linked, vendored, or used to derive
DiskPie constants or implementation structure. The observations below are
requirements-level notes that will be reimplemented independently.

The same boundary applies to the proprietary Scanner reference artifacts in
the parent directory: none of their code, strings, images, palettes, or other
assets may enter this repository.

## Local engine design

### Input and output

The scanner should publish immutable, coherent, versioned tree snapshots.
Layout work consumes an `Arc<TreeSnapshot>` without holding a scanner lock and
produces compact records keyed by arena `NodeId`, not copied paths or strings:

```rust
struct Sector {
    node_id: NodeId,
    depth: u16,
    start: f64,
    end: f64,
    inner: f32,
    outer: f32,
    kind: SectorKind,
}

struct LayoutSnapshot {
    source_generation: u64,
    sectors: Vec<Sector>,
    rings: Vec<Range<usize>>,
}
```

The UI adapter may add an `Arc<egui::Mesh>` cache, but the portable layout
module must not name egui types. Selection and hover remain `NodeId`-based so
they can survive a new snapshot when the node still exists.

### Angular partitioning

For each displayed parent:

1. collect visible children and their selected metric (logical or allocated);
2. order them by a stable identity key;
3. accumulate weights in `u128` to avoid overflow near `u64::MAX`;
4. calculate boundaries in `f64` from prefix weight divided by total weight;
5. force the final child's end to equal the parent's end exactly; and
6. store half-open intervals, except for a deliberately inclusive outermost
   radial edge.

Zero-total parents produce no child sectors. Every nonzero child interval must
remain inside its parent's interval, and siblings must neither overlap nor
leave unexplained mass.

The recommended stable sibling key is `(kind, case-folded display name,
lossless native name bytes, stable node/path identity)`. It deliberately does
not use live size. A size-sorted textual view can coexist with identity-stable
chart ordering. If future usability testing demands size order, freeze that
order for the scan generation and only reconsider it at an explicit final
snapshot; continuously re-sorting partial results is not acceptable.

### Stable colors

Use a specified, deterministic hash implemented locally over volume/root
identity and the lossless root-relative path. Do not use `DefaultHasher`, whose
output is not a persistence contract. Derive a top-level hue or palette slot
from the hash, then vary lightness and chroma by depth and additional hash
bits. Use separate light- and dark-theme luminance ranges.

Colors must remain stable across process launches, partial batch order, and
logical/allocated metric changes. Renaming a path may intentionally change its
color. Selection, focus, separators, and the textual list ensure color is
never the only carrier of state.

### Pruning, hidden branches, and visible depth

Prune from a pixel threshold rather than a fixed percentage. A useful starting
point is `min_angle = minimum_readable_arc_points / outer_radius_points`.
Coalesce consecutive sub-threshold siblings under the same parent into a
synthetic `Other (N)` sector that retains their exact weight and underlying
IDs. Such a sector has no filesystem path and may open a filtered textual
view, but must never expose open, recycle, delete, or permanent-delete actions.

Represent a hidden child with an explicit neutral `Hidden` sector of the same
weight. This preserves parent geometry and makes restoration discoverable.
Determine displayed depth from available radius and a minimum useful ring
width rather than visiting every descendant.

### Partial snapshots

The scanner may publish frequently, but the application should coalesce UI
layout requests to at most roughly 10--20 Hz. A worker lays out an immutable
snapshot with a generation token. If a newer generation arrives, stale work is
cancelled or its result is discarded. The UI continues drawing the last
complete layout and swaps the replacement atomically.

Stable identity ordering and path-derived colors provide continuity while
angles change as totals become complete. Do not animate every partial update.
Reserve short, approximately 120--200 ms transitions for explicit zoom and,
optionally, the final snapshot, and honor a reduced-motion preference.

Suggested mesh cache key fields are source generation, selected root, size
metric, hidden-state revision, quantized viewport, theme/palette, pixels per
point, and visible depth.

## Rendering with egui

Generate each annular sector directly as a triangle strip: two inner/outer
vertices per angular sample and two triangles per subdivision. A center sector
uses a fan. Select subdivision count from a bounded chord-error calculation,
preallocate vertices and indices, and cap depth, visible sectors, and
subdivisions to prevent adversarial allocation growth.

Keep a cached base `Arc<Mesh>` and draw hover, focus, and selection as small
separate overlays. Pointer movement must not rebuild the full mesh. Use inset
geometry or explicit separator triangles for gaps and keep hit-test gaps equal
to the rendered gaps.

This is preferable to a single filled `PathShape`: epaint documents
[`PathShape` fills](https://docs.rs/epaint/latest/epaint/struct.PathShape.html)
as requiring a convex polygon, while an annular sector is concave. egui
supports [`Shape::Mesh(Arc<Mesh>)`](https://docs.rs/egui/latest/egui/enum.Shape.html),
and [`Mesh::bytes_used`](https://docs.rs/egui/latest/egui/struct.Mesh.html)
supports memory measurement. Start with eframe multisampling and local mesh
generation; reconsider another tessellator only after a visual-quality and
performance benchmark demonstrates a real shortfall.

### Polar hit testing

Store each ring's sectors in ascending start-angle order. For a pointer:

1. calculate squared radius and reject the center or exterior as appropriate;
2. binary-search radial ring bounds;
3. calculate `atan2` once, rotate zero to twelve o'clock, and normalize to
   `[0, TAU)`;
4. binary-search the greatest sector start not exceeding the angle; and
5. confirm the half-open end angle, radial ownership, and visual-gap inset.

This is `O(log n)` in the selected ring rather than scanning all visible
shapes. The center circle is a separate parent/back target. Recompute hover
only after pointer movement or a layout-generation change. Define and test
ownership at zero/`TAU`, exact angular boundaries, ring boundaries, and the
outermost edge.

## Labels and accessible textual navigation

Do not force text into narrow wedges. Render an inline wedge label only when
both available arc length and ring width exceed DPI-aware thresholds. Show the
full path, logical and allocated sizes, percentage, file count, and scan state
in a center summary or tooltip.

The canonical accessible representation is a synchronized virtualized
tree/table. `egui_extras::TableBody::rows` creates only visible rows, which is
important for large trees. Include name, logical/allocated size, percentage,
file count, omission/error state, and scan progress. Recommended keyboard
behavior is:

- arrows, Page Up/Down, Home, and End for navigation;
- Enter to zoom or open;
- Backspace or Alt-Up to navigate to the parent;
- Space to select or toggle branch visibility; and
- the platform context-menu key or Shift-F10 for actions.

Synchronize selection between chart and list. Expose the chart as one
focusable widget with a meaningful summary instead of creating thousands of
overlapping accessibility nodes. egui's
[`Response`](https://docs.rs/egui/latest/egui/response/struct.Response.html)
supports keyboard/accessibility activation and widget metadata. eframe enables
AccessKit by default; verify the result manually with Windows Narrator and
Accessibility Insights because upstream notes that platform semantic coverage
is not complete for every element.

Sources: [`TableBody::rows`](https://docs.rs/egui_extras/latest/egui_extras/struct.TableBody.html),
[eframe feature set](https://docs.rs/crate/eframe/latest/features), and
[AccessKit platform architecture](https://accesskit.dev/how-it-works/).

## Dependency evaluation

Versions and upstream activity were checked on 2026-08-02. "No direct advisory
identified" is not a guarantee about a future locked transitive graph. Once a
lockfile exists, CI must inspect it with `cargo audit` and license/source rules
such as `cargo deny`; review new advisories from the
[RustSec advisory database](https://github.com/rustsec/advisory-db).

### egui, eframe, and epaint 0.35.0

- **License:** MIT OR Apache-2.0; compatible.
- **Maintenance:** released 2026-06-25; upstream repository activity was
  current on the research date.
- **Security/unsafe:** the public UI code is safe to call, but native window,
  clipboard, accessibility, graphics, and OS adapters transitively require
  unsafe/FFI. Keep those dependencies outside the portable domain/layout
  crates and audit the locked graph.
- **Platform:** supports Windows, macOS, Linux, and web; satisfies Windows
  10/11. DiskPie only promises the desktop subset.
- **Build/binary impact:** eframe is the dominant UI cost. Default native
  features include AccessKit and WGPU; upstream notes that glow can reduce
  binary size significantly. Compare WGPU and glow using the portable release
  profile rather than assuming either winner.
- **Value over local:** essential application framework and rendering API.
- **Abandonment/lock-in:** active but high framework lock-in and version-coupled
  ecosystem. Contain it in the UI/render adapter.
- **Action:** adopt the workspace-selected 0.35 line and pin all egui-family
  crates together.

Sources: [eframe 0.35.0](https://docs.rs/crate/eframe/latest),
[eframe features](https://docs.rs/crate/eframe/latest/features), and
[egui package metadata](https://docs.rs/crate/egui/latest/source/Cargo.toml).

### egui_extras 0.35.0

- **License:** MIT OR Apache-2.0; compatible.
- **Maintenance:** released with the active egui 0.35 line.
- **Security/unsafe:** no additional direct unsafe boundary is needed for the
  table-only use; it inherits egui and enabled-feature dependencies.
- **Platform:** same portable UI platforms as egui, including Windows 10/11.
- **Build/binary impact:** small when default and media/image features are
  disabled; avoid enabling unrelated loaders.
- **Value over local:** proven striped/resizable tables and virtualized row
  production avoid building another complex accessible list widget.
- **Abandonment/lock-in:** version-coupled to egui, but replacement is localized
  to the textual-view adapter.
- **Action:** adopt with the minimum feature set.

Sources: [package metadata](https://docs.rs/crate/egui_extras/latest/source/Cargo.toml)
and [`TableBody`](https://docs.rs/egui_extras/latest/egui_extras/struct.TableBody.html).

### AccessKit 0.24.1 through eframe

- **License:** MIT OR Apache-2.0; compatible.
- **Maintenance:** released 2026-06-12; upstream remained active in July 2026.
- **Security/unsafe:** adapters bridge Windows UI Automation, macOS
  NSAccessibility, and Unix AT-SPI using platform FFI. Do not introduce custom
  unsafe accessibility bindings in DiskPie.
- **Platform:** native adapters cover DiskPie's Windows requirement and leave a
  portable route for macOS/Linux.
- **Build/binary impact:** already present in eframe's default feature set, so
  using it does not justify a second accessibility stack.
- **Value over local:** operating-system accessibility integration cannot be
  responsibly recreated as a small local component.
- **Abandonment/lock-in:** adapter APIs can evolve and semantic coverage is
  incomplete; preserve the synchronized textual view and manual assistive-tech
  checks as fallback evidence.
- **Action:** adopt indirectly through eframe; do not add a second adapter.

Sources: [AccessKit 0.24.1 metadata](https://docs.rs/crate/accesskit/latest/source/Cargo.toml),
[changelog](https://docs.rs/crate/accesskit/latest/source/CHANGELOG.md), and
[platform architecture](https://accesskit.dev/how-it-works/).

### rfd 0.17.2

- **License:** MIT; compatible.
- **Maintenance:** released 2026-01-12; repository activity was current through
  2026-07-17.
- **Security/unsafe:** native dialogs cross COM/Win32 and other platform FFI.
  Its `FileDialog` source includes `unsafe impl Send` and `unsafe impl Sync`
  around an optional raw window handle with an explicitly cautionary comment.
  A parented dialog must not be moved casually between threads.
- **Platform:** Windows, macOS, Linux/BSD, and asynchronous web APIs. It meets
  Windows 10/11 folder-selection needs.
- **Build/binary impact:** modest on Windows but feature/backend choices expand
  Linux dependencies; only enable the required backend features.
- **Value over local:** native modality, familiar folder selection, shell
  integration, and accessibility are expensive to reproduce.
- **Abandonment/lock-in:** medium platform-backend and raw-handle risk. A local
  `FolderPicker` trait provides an exit to a Windows `IFileOpenDialog` adapter
  or an egui dialog.
- **Action:** prototype behind `FolderPicker`. If used, invoke synchronously on
  the UI thread, avoid moving a parented dialog across threads, and pin/audit
  the version. If the `windows` crate is already unavoidable elsewhere, also
  compare a small isolated `IFileOpenDialog` adapter.

Sources: [rfd 0.17.2](https://docs.rs/crate/rfd/latest),
[platform/API documentation](https://docs.rs/rfd/latest/rfd/), and
[`FileDialog` source](https://docs.rs/rfd/latest/src/rfd/file_dialog.rs.html).

### egui-file-dialog 0.14.1

- **License:** MIT; compatible.
- **Maintenance:** released 2026-06-28 and tracks current egui releases.
- **Security/unsafe:** its application-facing implementation is safe Rust; its
  filesystem and system-information dependencies remain part of the audit
  surface. Threaded directory loading must return immutable application data,
  not UI references.
- **Platform:** renders anywhere egui does and supports native desktop folder
  picking; it does not provide the same native Windows look or shell behavior.
- **Build/binary impact:** adds `directories`, `dunce`, `sysinfo`, and its own
  filesystem UI. This is more code than a thin native adapter.
- **Value over local:** customizable in-app dialog, keyboard navigation,
  translation hooks, custom filesystem support, and threaded directory
  loading (`load_via_thread` defaults to true on native platforms).
- **Abandonment/lock-in:** tied to egui releases; DiskPie would own testing for
  modality, screen-reader behavior, and native-user expectations.
- **Action:** keep as the fallback/prototype alternative to rfd, not a
  simultaneous default dependency. Choose based on an accessibility and
  portable-binary comparison.

Sources: [egui-file-dialog 0.14.1](https://docs.rs/crate/egui-file-dialog/latest),
[API documentation](https://docs.rs/egui-file-dialog/latest/egui_file_dialog/),
and [`load_via_thread`](https://docs.rs/egui-file-dialog/latest/egui_file_dialog/struct.FileDialogConfig.html).

### egui-phosphor 0.13.0

- **License:** crate MIT OR Apache-2.0; bundled Phosphor artwork/font MIT;
  compatible with attribution obligations preserved.
- **Maintenance:** current repository activity through 2026-07-24 and aligned
  with the egui release train.
- **Security/unsafe:** no platform FFI or direct unsafe boundary is needed; it
  loads font data into egui.
- **Platform:** portable wherever egui fonts render.
- **Build/binary impact:** approximately 0.49 MB for the regular font asset;
  additional variants add roughly another 0.45--0.54 MB each before executable
  compression effects.
- **Value over local:** a broad, consistent icon vocabulary and fast UI
  implementation.
- **Abandonment/lock-in:** glyph constants and font packaging couple the UI to
  the crate and icon set. Icon-only controls still require text, tooltips, and
  accessibility labels.
- **Action:** defer. Use a small original vector icon set and original app icon;
  reconsider one font variant only if the required icon count makes local
  maintenance measurably worse.

Source: [egui-phosphor repository and license](https://github.com/amPerl/egui-phosphor).

### fluent-bundle 0.16.0 and unic-langid 0.9.6

- **License:** both MIT OR Apache-2.0; compatible.
- **Maintenance:** Fluent repository activity was current through 2026-03-27;
  `unic-langid` 0.9.6 was published 2025-05-09. These are mature components.
- **Security/unsafe:** no application-level unsafe boundary is required.
  Resource parsing and interpolation still require fuzz/adversarial limits if
  catalogs ever become externally supplied; DiskPie should embed reviewed
  catalogs.
- **Platform:** pure portable Rust suitable for Windows and the portable core.
- **Build/binary impact:** moderate dependency/runtime cost for Fluent syntax,
  language negotiation, plural rules, memoization, and language IDs.
- **Value over local:** robust pluralization, fallback, locale negotiation, and
  structured messages avoid a home-grown format that would fail beyond simple
  English/Spanish strings.
- **Abandonment/lock-in:** Fluent syntax is a documented ecosystem format, but
  direct `FluentBundle` use would leak into UI code. Typed message IDs and an
  application-owned `I18n` facade preserve an exit.
- **Action:** adopt for production localization behind the facade; embed an
  English fallback and validate complete English/Spanish key sets in tests.

Sources: [`fluent-bundle`](https://docs.rs/fluent-bundle/latest/fluent_bundle/),
[upstream package metadata](https://github.com/projectfluent/fluent-rs/blob/main/fluent-bundle/Cargo.toml),
and [`unic-langid` metadata](https://docs.rs/crate/unic-langid/0.9.6/source/Cargo.toml).

### rust-i18n 4.2.1

- **License:** MIT; compatible.
- **Maintenance:** released 2026-07-16 with repository activity through
  2026-07-22, but the recent 4.x line implies migration churn.
- **Security/unsafe:** no runtime platform FFI; procedural macros and build-time
  YAML/JSON/TOML processing expand the compile-time supply-chain surface.
- **Platform:** portable Rust.
- **Build/binary impact:** compile-time generators, macros, and selected catalog
  parsers; runtime lookup is straightforward.
- **Value over local:** very easy translation lookup, fallback locales, and
  embedded catalogs.
- **Abandonment/lock-in:** global `t!` use and generated code spread framework
  assumptions through the application. Recent major changes increase upgrade
  cost.
- **Action:** reject for the initial architecture in favor of the typed facade
  plus Fluent. Do not include both localization stacks.

Source: [rust-i18n 4.2.1](https://docs.rs/crate/rust-i18n/latest).

### lyon_tessellation 1.0.20

- **License:** MIT OR Apache-2.0; compatible.
- **Maintenance:** mature 1.x crate with repository activity through
  2026-05-03.
- **Security/unsafe:** designed as portable/no-std-capable geometry code; no new
  OS/FFI boundary. Complex input can still cause allocation and tessellation
  work, so limits remain necessary.
- **Platform:** portable across DiskPie's targets.
- **Build/binary impact:** adds the Lyon geometry/path stack and general-purpose
  sweep-line tessellation work.
- **Value over local:** excellent for arbitrary complex vector paths, but an
  annular sector is analytically structured and needs only a bounded triangle
  strip.
- **Abandonment/lock-in:** stable but would make renderer performance and mesh
  output depend on a general algorithm without helping polar hit testing.
- **Action:** reject initially. Reconsider only if a benchmarked visual defect
  cannot be solved by local subdivisions plus multisampling.

Source: [`FillTessellator`](https://docs.rs/lyon_tessellation/latest/lyon_tessellation/struct.FillTessellator.html).

### proptest 1.11.0

- **License:** MIT OR Apache-2.0; compatible.
- **Maintenance:** released 2026-03-24; mature and explicitly described as
  passively maintained, with repository activity through 2026-07-27.
- **Security/unsafe:** development-only randomized input generation; it adds no
  production unsafe boundary. Its locked dev dependency graph still belongs in
  supply-chain audits.
- **Platform:** portable and suitable for Windows CI.
- **Build/binary impact:** increases test compile time only; absent from the
  portable release binary.
- **Value over local:** shrinking and reproducible arbitrary-tree generation
  provide much greater coverage than hand-written layout cases.
- **Abandonment/lock-in:** passive maintenance is acceptable for a mature test
  tool; generated strategies can be replaced without changing production APIs.
- **Action:** adopt as a dev dependency with deterministic failure seeds.

Sources: [proptest 1.11.0](https://docs.rs/crate/proptest/latest) and
[maintenance statement](https://docs.rs/crate/proptest/latest/source/README.md).

### Criterion 0.8.2

- **License:** MIT OR Apache-2.0; compatible.
- **Maintenance:** released 2026-02-04 with repository activity through
  2026-04-23.
- **Security/unsafe:** development-only harness; no production unsafe boundary.
  Its comparatively broad reporting/statistics dependency graph must remain
  outside release dependencies.
- **Platform:** supports Windows and the other intended development hosts.
- **Build/binary impact:** meaningful benchmark compile time, zero portable
  release-binary impact when correctly scoped as a dev dependency.
- **Value over local:** warmup, sampling, statistical comparison, and reports
  make renderer regressions more visible than an ad hoc timer.
- **Abandonment/lock-in:** benchmark functions are easy to port if the crate
  changes; CI wall-clock thresholds must not depend on Criterion internals.
- **Action:** adopt for local/reference-host benchmarks; use CI primarily for
  correctness and major regression signals.

Sources: [Criterion 0.8.2](https://docs.rs/crate/criterion/latest) and
[changelog](https://docs.rs/crate/criterion/latest/source/CHANGELOG.md).

## Compatible algorithm and incompatible application references

### D3 hierarchy 3.1.2: compatible study reference

[`d3-hierarchy`](https://www.npmjs.com/package/d3-hierarchy) is ISC-licensed
and its documented
[`partition`](https://d3js.org/d3-hierarchy/partition) mapping is a useful
description of converting hierarchy weights to angular/radial intervals. It
is not a proposed runtime dependency: DiskPie should independently implement
the small partition algorithm in Rust without JavaScript, Node, or a WebView.

### KDE Filelight: GPL, study only

[Filelight](https://apps.kde.org/filelight/) was current at version 26.04.3 on
2026-07-02 and remained actively maintained. Useful observed behavior includes
concentric navigation, a central parent control, synchronized list/segment
selection, tooltip summaries, tiny-item grouping, and limiting displayed ring
depth. Its source is GPL-licensed and therefore cannot be copied or linked.

DiskPie should improve on two general pitfalls visible in analyzers of this
style: derive color from stable identity rather than a mutable start angle,
and ensure a synthetic group can never be mistaken for a destructive
filesystem target. A previous Filelight fix concerning clickable fake groups
illustrates the latter risk, but DiskPie must implement its own behavior:
[upstream issue fix](https://invent.kde.org/utilities/filelight/-/commit/c08c827d48f67e63bb24fc2b2b976e3343832ee4).

### SquirrelDisk: AGPL and inactive, study only

[SquirrelDisk](https://github.com/adileo/squirreldisk) combines Tauri, React,
and D3 and demonstrates depth-limited zoom, a center-back interaction,
animated transitions, and grouping of very small items. The repository is
AGPL-3.0, warns that it is not fully stable, and showed no activity after
2023-08-04. It presents both an incompatible-license boundary and a significant
abandonment risk. No code, constants, assets, or implementation structure may
be reused.

### Other analyzers

[`dua-cli`](https://github.com/Byron/dua-cli) is MIT and active but is a TUI,
not a reusable sunburst engine. `dust` (Apache-2.0), `gdu` (MIT), and
`diskonaut` (MIT) may inform staged navigation and progressive-result behavior,
but they offer no direct egui chart component. Their scanning internals require
a separate scan-layer evaluation before reuse. WinDirStat and other GPL tools
remain behavior-study-only.

## Property-based validation

Generate bounded trees containing zero sizes, near-`u64::MAX` totals, very deep
chains, very wide sibling sets, hidden branches, Unicode/native names, omitted
nodes, and arbitrary partial insertion orders. Required properties are:

- every angle and radius is finite and normalized;
- children stay within the parent interval and ordered siblings do not overlap;
- the final child ends exactly at the parent end;
- visible, hidden, and synthetic weights conserve parent mass within the
  documented floating-point tolerance;
- layout order and color are invariant under input and scan-batch permutation;
- switching logical/allocated metrics changes weights, not identity order or
  colors;
- synthetic sectors never carry a destructive filesystem target;
- an interior point sampled from a sector hit-tests back to that sector;
- rendered gaps and points outside the chart return no sector;
- exact angular, radial, zero/`TAU`, center, and outer-boundary ownership is
  deterministic;
- all mesh vertices are finite, indices are in range, triangle orientation is
  consistent, and vertex/index counts remain under configured bounds;
- latest-generation cancellation wins and selection survives the same
  `NodeId`; and
- every action has complete, nonempty English and Spanish text, including
  labels for icon-only controls.

Focused unit tests should additionally cover hand-verifiable partitions,
single-child and zero-total parents, hidden/synthetic semantics, tiny-sector
aggregation, and reduced-motion transitions.

## Benchmark plan and provisional targets

Use fixed-seed balanced, skewed, deep, and million-node arena fixtures. Bound
the visible chart independently from total model size and include worst-case
rings with approximately 10,000 visible siblings.

Measure:

- layout time for initial, partial, final, hidden-state, metric, and zoom
  invalidations;
- random and worst-ring hit-test p50/p95/p99 latency;
- mesh generation at 1,000, 10,000, and 50,000 visible sectors;
- vertex/index counts and `Mesh::bytes_used`;
- a warm cached frame versus a fully invalidated frame;
- snapshot coalescing/cancellation and time to first partial chart;
- memory per visible sector and total cache memory;
- light/dark themes, multiple viewport sizes, and multiple DPI scales; and
- WGPU versus glow frame latency, visual correctness, compile time, and final
  portable executable size.

Initial engineering targets on a documented reference Windows machine are:

- invalidated layout plus mesh generation below 8 ms for the normal interactive
  fixture;
- warm cached chart painting below 2 ms CPU time;
- hit testing below 50 microseconds at p99 for the 10,000-sector ring;
- no full mesh rebuild caused solely by pointer movement; and
- no more than 20 chart-layout publications per second during scanning.

These are provisional budgets, not CI promises. Record hardware, release
profile, renderer, resolution, DPI, fixture seed, vertex count, and visible
sector count with every baseline. Criterion should detect trends on controlled
hosts; shared CI should fail only on correctness, resource bounds, or robustly
large regressions rather than fragile absolute wall-clock thresholds.

## Action summary

- **Adopt:** local safe-Rust layout/hit-test/mesh engine, `egui_extras`, eframe's
  AccessKit path, Fluent behind a typed facade, `proptest`, and Criterion.
- **Prototype behind an adapter:** `rfd` versus an isolated Windows native
  folder-picker implementation; retain `egui-file-dialog` as the in-egui
  fallback candidate.
- **Defer:** `egui-phosphor` until icon breadth justifies its font payload.
- **Reject initially:** generic chart crates, `lyon_tessellation`, and
  `rust-i18n` alongside Fluent.
- **Study without copying:** D3 documentation; GPL Filelight, AGPL SquirrelDisk,
  and other incompatible analyzers.
