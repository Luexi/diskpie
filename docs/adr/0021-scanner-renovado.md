# ADR 0021: map-first workspace with an optional list

- Status: accepted
- Date: 2026-09-06

## Context

The user rejected the permanent inspector, multiple control rails, low-contrast
information and small unlabeled map. The approved replacement is DiskPie's
original mockup A with B available through Show list. Table-first C is excluded.
Eight reserved radial levels also made a two-level tree occupy only 40% of the
drawable radius; rearranging panels alone would not correct this.

## Decision

Apply `docs/research/ui-product-direction.md`, preserving Rust/egui and all scan,
action, reconciliation and support-service contracts. Preserve System theme,
Allocated metric and existing preferences.

Calculate positive-weight depth off-thread before hiding/grouping and sector
limits. The configured maximum remains a ceiling. Use native path-derived color
families and absolute family depth so navigation does not recolor an item.
Selection, focus and hover remain independent overlays.

Add defaulted `show_item_list` and `item_list_width_points` preferences without
changing the version-1 envelope. Add sort/direction to the immutable list key;
share its comparator with selection lookup. Older documents remain readable;
transient navigation and hover are not persisted. Tools and context menus keep
existing functions accessible.

## Consequences

The map becomes the primary view while the optional list supports exact and
keyboard-based inspection. Partial data remains explicit. The family index adds
eight bytes per node. Adaptive depth adds a cancellable, iterative traversal
that stops when the depth ceiling is reached. A tiny deep branch can reserve
rings even when grouped; this avoids budget-driven scale changes.

No production dependency or platform API is added. Compare performance against
the pre-redesign working tree, including deliveries 1-3, rather than the older
initial source used for the separate implementation benchmark. No release is
published as part of this decision.

## Native visual fixture dependency

The Windows-only test target references the already locked `winit 0.30.13`
directly, with default features disabled, to allow the native event loop on a
libtest worker thread. It is the same MIT/Apache-2.0 event-loop implementation
already used by eframe, maintained and pinned with that frontend stack. This
adds no package download, production feature, binary payload or project unsafe
code. Its existing platform unsafe footprint is unchanged. The alternative is
custom process orchestration or a production test flag; the small dev-only
reference keeps fixtures isolated and can be removed with the screenshot test.

## Implementation refinement, 2026-09-07

The positive-depth contract is unchanged, but its original per-layout traversal
is replaced by a depth field per metric computed during snapshot aggregation.
It occupies existing aggregate padding on x64. Layout uses the stored value
clamped to its configured ceiling; hiding, grouping and budget still do not
alter scale. Snapshot root indexes also remove the inherited full-arena
availability check. Large sorting/grouping operations check supersession.

The original validation and performance reports remain historical evidence for
their recorded sources. [Review remediation validation](../research/review-remediation-validation.md)
records new checks and measurements separately, including preparation/scratch
costs and limits; this refinement is not a claim of a universal speedup.
