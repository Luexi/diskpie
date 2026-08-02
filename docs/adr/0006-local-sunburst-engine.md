# ADR 0006: Implement the sunburst engine locally

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must visualize coherent partial and final scan snapshots as a
navigable, DPI-aware sunburst while remaining responsive with millions of
model entries. It requires deterministic ordering and colors, independent
polar hit testing, zoom/back/parent navigation, hidden branches, synthetic
tiny-item groups, keyboard access, and a synchronized textual representation.

The portable domain and layout layers must remain independent of egui and
Windows APIs. The renderer cannot wait for filesystem or shell I/O. The
project is dual-licensed `MIT OR Apache-2.0` and cannot copy or link GPL/AGPL
implementations.

Supporting research, dependency review, primary sources, and benchmark plans
are recorded in
[sunburst rendering and UI component research](../research/sunburst-rendering-and-ui-components.md).

## Decision drivers

- Deterministic geometry across scan batch order and process launches.
- Cheap partial-snapshot replacement without blocking the UI thread.
- Fast, exact hit testing independent of rendering tessellation.
- Bounded memory and mesh work for high fan-out and deep trees.
- A portable, safe-Rust domain/layout implementation.
- Accessible keyboard and textual navigation in addition to the visual chart.
- Compatible licensing and a clear clean-room boundary.
- Low abandonment and dependency lock-in risk for a product-defining feature.

## Options considered

### Local partition, hit-test, and annular mesh engine

Implement the weighted hierarchy partition and polar hit testing in a portable
`sunburst` module, then generate bounded triangle-strip meshes in an egui
adapter. This requires local geometry tests and benchmarks, but keeps stable
identity, partial-snapshot policy, pruning semantics, and destructive-action
boundaries explicit and testable.

The portable module can forbid unsafe code. The egui mesh adapter inherits the
framework's reviewed graphics/platform boundary without exposing it to the
domain model. The implementation is licensed with DiskPie and has no external
algorithm ownership or abandonment risk.

### General-purpose Rust path tessellation

Use a crate such as `lyon_tessellation` for annular paths. Lyon is maintained
and license-compatible, but a general sweep-line path tessellator adds code,
allocations, and output variability for geometry that can be expressed as a
bounded triangle strip. It does not provide hierarchy partitioning, stable
identity, partial-snapshot scheduling, or polar hit testing.

### Existing chart or foreign-runtime sunburst

Adopt a chart widget or D3 through JavaScript/WebView integration. This may
reduce initial drawing code but introduces a second runtime, larger portable
binary, accessibility integration work, message serialization, and substantial
lock-in. No evaluated egui-native component met the complete navigation,
partial-result, deterministic, performance, and licensing requirements.

### Reuse an open-source disk analyzer implementation

KDE Filelight and SquirrelDisk demonstrate useful public behavior, but their
GPL and AGPL licenses are incompatible with DiskPie's license. Copying,
translation, linking, vendoring, or structural derivation from their source is
not acceptable. They may only be studied at the behavior level. Compatible
CLI/TUI analyzers do not provide a reusable egui sunburst engine.

## Decision

DiskPie will implement the sunburst engine locally.

1. The portable `sunburst` module will own weighted angular partitioning,
   stable identity ordering, path-derived stable color keys, pixel-aware
   pruning, hidden/synthetic-sector semantics, visible-depth selection, and
   binary-search polar hit testing. It must not depend on egui or Windows APIs
   and will use `#![forbid(unsafe_code)]` where crate/module organization permits.
2. Layout consumes immutable, versioned tree snapshots and produces immutable
   sector records keyed by `NodeId`. Generation tokens provide latest-wins
   cancellation. The UI retains the last complete layout until an atomic swap.
3. Weight prefixes use `u128`, angles use `f64`, final-child boundaries are
   exact, and interval ownership is half-open and explicitly tested.
4. The egui adapter generates bounded annular triangle strips directly into a
   cached `egui::Mesh`. Hover, focus, and selection are separate overlays and
   cannot trigger a full mesh rebuild on pointer movement.
5. Each ring is indexed by radial bounds and sorted angular intervals so hit
   testing is logarithmic in ring size and independent of mesh tessellation.
6. The synchronized textual tree/table, implemented with virtualized rows from
   `egui_extras`, is the canonical accessible representation. The chart remains
   one focusable summarized widget and uses eframe's AccessKit integration.
7. No charting or Lyon dependency will be added initially. Reconsideration
   requires a reproducible benchmark or visual-quality defect that the local
   bounded mesh cannot satisfy.
8. `proptest` and Criterion may be used as development-only dependencies for
   the required invariants and performance baselines.
9. GPL/AGPL analyzers remain study-only. No code, constants, assets, or
   implementation structure from them may enter DiskPie.

## Consequences

Positive outcomes:

- Layout semantics remain deterministic and independent of UI/render backends.
- Hit testing can be tested exactly without depending on painter output.
- Partial scan updates do not require per-file UI work or a renderer lock.
- Mesh allocations, subdivisions, visible depth, and aggregation can be
  explicitly bounded and measured.
- The project avoids incompatible licensing, foreign runtimes, and a critical
  third-party chart dependency.
- Accessible textual navigation can evolve independently from visual wedge
  labels.

Accepted tradeoffs:

- DiskPie owns geometric correctness, antialiasing quality, pruning policy,
  stable palette design, and performance tuning.
- The initial implementation needs more property tests and benchmarks than a
  superficial chart-widget integration.
- Very small entries are represented through an independently designed
  synthetic group and textual view rather than individually clickable wedges.
- A future egui renderer change requires a new mesh adapter, though not a
  domain/layout rewrite.

Fallback and reconsideration:

- If multisampling and bounded local subdivisions fail a documented visual
  quality target, prototype Lyon or another compatible tessellator behind the
  renderer adapter and compare output, latency, allocations, compile time, and
  binary size.
- If the chart cannot expose sufficient accessibility semantics through egui,
  retain the virtualized textual view as the full keyboard/screen-reader path
  and document platform limitations.
- Any proposal to reuse GPL/AGPL code requires a separate explicit relicensing
  decision; it is not an implementation fallback under this ADR.

## Validation

Property tests must generate zero-size, overflow-adjacent, deep, wide, hidden,
Unicode, and arbitrarily batched trees and prove:

- finite normalized bounds, parent containment, and non-overlapping siblings;
- exact final boundaries and conservation through visible, hidden, and
  synthetic weights;
- ordering/color invariance under insertion and batch permutation;
- stable identity across logical/allocated metric switches;
- deterministic angular/radial/gap/zero-`TAU` hit-test ownership;
- valid, finite, bounded, consistently wound mesh output;
- latest-generation cancellation and `NodeId` selection preservation; and
- absence of destructive targets on synthetic sectors.

Benchmarks must cover balanced, skewed, deep, million-node, and approximately
10,000-sibling fixtures; 1,000/10,000/50,000 visible-sector mesh cases;
multiple DPI/theme/viewport configurations; cached and invalidated frames; and
WGPU versus glow release-binary impact.

Initial reference-machine budgets are below 8 ms for a normal invalidated
layout plus mesh, below 2 ms CPU time for a warm cached chart, below 50
microseconds p99 for hit testing a 10,000-sector ring, no full mesh rebuild on
pointer movement, and no more than 20 layout publications per second during a
scan. These are provisional engineering budgets, not portable CI guarantees;
every baseline must record hardware, renderer, release profile, fixture seed,
DPI, and visible geometry counts.
