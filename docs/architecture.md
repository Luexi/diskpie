# DiskPie architecture

> Current direction: Windows 10/11 x64 only, permanently; free and open source.
> Updated 2026-09-11; entry points and baseline refreshed 2026-09-13. This guide replaces the paused-work assumptions in the
> [historical architecture](https://github.com/Luexi/diskpie/blob/f9a9a5fe8c6c5a108fae9edadda0f674a74a63b1/docs/architecture.md).
> Product decisions and delivery order live in [ROADMAP](../ROADMAP.md);
> verified status lives in [requirements traceability](requirements-traceability.md).

## Implementation direction

Keep Rust, egui/eframe, and the existing five-crate application. Build a polished
exploration-and-cleanup slice before considering a UI technology migration;
changing the stack requires the owner's approval. Simplify incrementally when a
specific task benefits. Do not start a broad rewrite, a plugin framework, or
new operating-system adapters in anticipation of future needs.

Portable domain code and Linux CI remain useful implementation details. They
are not a commitment to ship Linux or macOS. Folder size, executable size, RAM,
and scan performance are separate measurements; generated build files do not
establish that the application architecture is oversized.

## Existing boundaries and entry points

| Crate | Responsibility | Useful source entry points |
| --- | --- | --- |
| `diskpie-core` | Native names, node identity, immutable trees, logical/allocated metrics, sunburst layout and hit testing. No UI or OS API dependency. | `src/model.rs`, `src/sunburst.rs` |
| `diskpie-scan` | Blocking scan orchestration through `ScanFs`: coordinator, bounded workers/channels, batches and cancellation. | `src/fs.rs`, `src/protocol.rs`, `src/coordinator.rs` |
| `diskpie-app` | UI-independent runtime, sessions, navigation, presentation, layout and list services, branch rescan, action policy, diagnostics export and settings. | `src/runtime.rs`, `src/session.rs`, `src/navigation.rs`, `src/actions.rs`, `src/layout_service.rs`, `src/item_list_service.rs`, `src/branch_rescan.rs` |
| `diskpie-platform` | Windows filesystem/volume access, Shell STA, native settings, registry and diagnostic transport. Normal home for project-authored unsafe code. | `src/windows/filesystem.rs`, `src/windows/volumes.rs`, `src/windows/shell_service.rs`, `src/windows/shell_actions.rs`, `src/windows/settings_file.rs` |
| `diskpie` | Executable composition, responsive egui surfaces, action and support dialogs, theme, localization and lifecycle handoff. No project-authored unsafe code. | `src/main.rs`, `src/shell.rs`, `src/shell/actions_ui.rs`, `src/shell/support_ui.rs`, `src/support_service.rs`, `src/scan_ui.rs`, `src/resolver.rs`, `src/sunburst_view.rs` |

Paths are relative to `crates/<crate>/`. The executable composes app and
platform services; platform may implement app/scan ports; app uses scan/core;
scan uses core. Preserve the acyclic dependency graph. Reuse a suitable existing
module before adding another layer or crate.

## Current foundation versus planned behavior

The connected application already scans real folders/drives, streams results,
shows a sunburst and synchronized list, navigates, cancels, and persists
preferences. Native settings transport and shutdown coordination are connected;
do not recreate them from older documents that call them planned. Diagnostics,
panic-marker lifecycle, English/Spanish strings and Windows font fallback also
exist. See the traceability matrix for remaining verification gaps.

Open/reveal, single-item recycle, Installed Apps handoff, Explorer-integration
preferences and diagnostic export are connected through a bounded support
worker and the Shell STA (ADR 0020). Branch rescan merges an isolated
enumeration into the committed tree after the coordinator joins. Windows
enumeration and child metadata share one retained directory handle (ADR 0022).
The complete child list is ranked and filtered off-thread by `ItemListService`.
The staged cleanup basket and the expanded views/tools below are future
product work. Permanent-delete and empty-bin flows were connected before the
recycle-only decision; remove those controls rather than extending them.

## Scan, frame and lifetime invariants

- Resolve roots and perform filesystem, COM, settings and other blocking I/O
  outside the egui callback. Render immutable committed state and drain only
  bounded work per frame; never join a worker from the frame callback.
- Keep one scheduling coordinator, bounded/fixed workers and channels, batched
  events and backpressure. Never spawn a task or thread per file.
- Carry scan generation and revision through work/results. A stale scan, layout
  or action must not mutate the current session. Publish compatible snapshot,
  presentation, navigation and layout state together.
- Preserve partial results and typed omissions. Cancellation/replacement must
  retain ownership of work still returning from Windows calls.
- Use existing bounded lifecycle handoff and explicit completion outside the
  event loop. Preserve the real runtime, Shell, resolver, diagnostic-bridge and
  diagnostics receipts used for shutdown reconciliation.
- Cache geometry; hovering and idle frames must not rebuild a whole tree or
  request continuous decorative repaints. Virtualize large textual lists.

`RuntimeController`, `SessionReducer`, `LayoutService` and `ScanFlow` already
provide these responsibilities. Read the affected implementation and relevant
ADRs before changing ownership; do not recreate a parallel coordinator.

## One shared scan, multiple views (target)

The target views are folders, Sunburst, Flame, bubbles, mind map, largest items,
last-modified age and Treemap. They share one scan model, stable selection,
navigation, size basis, inspector and cleanup selection. Switching views must
not trigger a filesystem rescan. Each view may compute a bounded projection or
layout of the same snapshot off-thread; an existing sunburst layout is not a
universal geometry format for the other views.

Only real nodes are actionable. Synthetic summaries, Other/Hidden sectors and
chart aggregates must resolve to explicit real items before staging anything.
Do not add eight scanners, eight action policies or a general plugin system.
Build common abstractions when a second implemented view demonstrates a need.

## Filesystem truth

- Preserve identity with `Path`, `PathBuf`, `OsStr` and `OsString`. Display text
  may be escaped or lossy, but must never become an action path.
- Keep logical bytes distinct from allocated bytes, with source/precision and
  unknown reasons. Unknown is not zero; totals use checked accumulation.
- Account for hard links using stable file identity scoped to the volume.
  Multiple names do not automatically represent reclaimable duplicate storage.
- Do not follow directory reparse points, links or cloud boundaries by default.
  Never hydrate online-only content automatically for scanning, preview or
  duplicate detection. Report unavailable metadata and skipped content.
- Treat access denial and isolated I/O errors as omissions, not total failure.
  Preserve sparse/compressed allocation semantics and conservative scheduling
  for remote, removable and unknown storage.
- Last-modified age means the timestamp observed, not proof of non-use or
  permission to clean an item.

## Recycle-only cleanup (target)

The user stages explicit items, reviews exact paths and the estimated space,
then confirms recycling. Reuse and adapt the existing app action policy and
Windows Shell service; keep COM objects confined to their STA. Bind confirmed
requests to the exact native target, kind, generation and available file
identity, revalidate immediately before execution, and consume confirmation
once. Retain protected-root/executable rules and reject stale/synthetic targets.

Never substitute permanent deletion when an item is oversized, the bin is
full/disabled, the provider lacks recycling, or Windows proposes another mode.
If recycling cannot be established for the target/provider, leave it untouched
and report why. Emptying the Recycle Bin is outside the product. Remove or
isolate obsolete destructive entry points deliberately when implementing this
policy; do not weaken identity checks in the name of simplification.

Validate cancellation, unsupported-provider behavior and Shell outcomes with
self-created disposable fixtures before enabling a provider class. Keep
unverified directory-recycle cases unavailable. Report per-item outcomes,
partial completion and uncertainty honestly. Reconcile affected scan data after
an attempt through the existing branch or full rescan. Staged selection and space estimates must not double-count nested items
or imply that hard-link/cloud logical bytes are all reclaimable.

## Adding the remaining tools (target)

- **Duplicates:** reduce candidates by metadata, compare actual content with
  bounded I/O, preserve one chosen copy and stage others through the same
  recycle policy. Names alone are not proof; hard links need separate handling.
- **Quick cleanup:** show explained candidates for review, never an automatic
  deletion rule based only on folder name, age or extension.
- **Applications and leftovers:** launch the official Windows/uninstaller UI
  through an explicit flow, separate from file cleanup. Review attributable
  leftovers through recycling. Do not run silent uninstall commands or treat
  deletion of an application directory as a substitute for uninstalling it.
- **Snapshots/comparison:** store bounded, versioned local scan data; explain
  missing/partial coverage and metric differences before claiming growth.
- **Preview and system monitor:** load metadata/content or sample CPU, memory,
  network and processes on bounded workers, cancel obsolete work and avoid
  background polling when the feature is inactive. Preview must not execute
  files or silently download cloud content.

## Maintenance and validation

Freeze expansion of diagnostics, panic handling and recovery infrastructure
unless a reproduced defect or the current deliverable requires it. Preserve
working safeguards; do not remove mature lifecycle code blindly. Link to an
existing ADR or implementation instead of duplicating internals here.

Measure release executable size, startup, RAM, scan latency and responsiveness
on a repeatable workload when affected. Do not invent a 1 MB release target or
an automatic disk-cleaning task without explicit authorization.
Follow the proportional checks in [AGENTS](../AGENTS.md), relevant Windows
fixtures and a real user journey. Record unavailable checks without claiming
success. A connected feature still needs the evidence named by its acceptance
criteria; this document does not certify the planned modules as implemented.
