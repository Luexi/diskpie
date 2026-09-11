# DiskPie product roadmap and agent handoff

> Product decisions updated 2026-09-11. This documentation change does not
> implement features or establish new runtime verification. Existing evidence
> below is recorded against source baseline
> `f9a9a5fe8c6c5a108fae9edadda0f674a74a63b1`.

## Product contract

DiskPie is a free, open-source disk analyzer and cleanup application for
**Windows only, permanently**. Target Windows 10/11 x86-64; keep the existing
`MIT OR Apache-2.0` license. Deliver a polished modern interface, clear storage
exploration, and file cleanup through the Recycle Bin. Rust and egui/eframe
remain the implementation stack. A technology migration or large rewrite
requires the owner's explicit agreement.

Publish useful portable releases progressively. An installer may be considered
later if a concrete distribution need justifies it. The complete product goal
includes DiskBuddy's publicly described options adapted independently for
Windows; the feature inventory below makes that scope explicit. Do not copy
proprietary assets, code, wording, or reference binaries.

This roadmap supersedes the former Scanner-parity-only contract, the
all-or-nothing release phases, and the old “75% complete” estimate. Historical
Scanner research remains useful behavior evidence, not the current backlog.
The [previous roadmap and verification record](https://github.com/Luexi/diskpie/blob/f9a9a5fe8c6c5a108fae9edadda0f674a74a63b1/ROADMAP.md)
remain available as an archive.

## How agents use these documents

- Latest owner instructions govern. [AGENTS.md](AGENTS.md) governs execution.
- This roadmap defines product scope, delivery order, and acceptance gates.
- [Architecture](docs/architecture.md) defines technical constraints;
  [traceability](docs/requirements-traceability.md) records implementation evidence.
- [UI direction](docs/research/ui-product-direction.md) governs presentation.
  Historical ADRs/research are subordinate when they conflict with current policy.
- Read the sections relevant to the task. Do not restart old phases, repeat
  finished investigations, or require a multi-agent review ritual for every edit.

## Current baseline and limits of the evidence

The connected application already scans selected roots, discovers drives,
streams partial results, cancels, renders a sunburst with a synchronized list,
navigates, switches size metrics, and persists settings. English and Spanish
strings and themes exist. Native Open/Reveal and recycle adapters exist below
the UI; those buttons and confirmation flows are not connected. Branch rescan,
Explorer settings integration, and diagnostic export also have remaining work.

Recorded on 2026-09-04: 499 tests passed, six subprocess helpers were ignored
by design, the local validation gate passed, and a static-CRT release executable
measured 10.0 MB. A Windows 11 build 26200 smoke check is recorded. These are
historical observations, not a fresh build or proof of Windows 10 compatibility.
Check current source and CI before claiming a feature is ready.

Still pending: clean Windows 10/11 checks, Recycle Bin provider/directory
fixtures, OneDrive hydration checks, Narrator, DPI, and measured performance.
See traceability for each requirement. No stable release was recorded at the
baseline. Linux CI checks portable logic; Linux is not a product target.

## Non-negotiable cleanup behavior

- DiskPie's file-cleanup operation is **move to Recycle Bin only**. No permanent
  delete, empty-bin feature, or “delete instead” fallback belongs in the UI,
  API workflow, suggestions, duplicate cleanup, or leftover cleanup.
- If an item is too large, recycling is unsupported, the bin is disabled/full,
  or recoverability cannot be established, stop or skip that item and explain
  the result. Do not offer permanent deletion as a substitute.
- Existing permanent-delete/empty-bin code is dormant legacy implementation,
  not permission to expose it. Its controlled removal may be a bounded task.
- Review the exact selected paths before execution; revalidate identity, report
  outcomes per item, and refresh affected results. Synthetic chart groups are
  not filesystem targets. Do not advertise guaranteed recycling until the
  Windows adapter and provider-specific fixtures verify this new contract.
- Application management is a distinct user action: open Windows Installed Apps
  or a registered official uninstaller with explicit user intent. Explain that
  the external uninstaller governs removal and is not a recoverable Recycle Bin
  operation. DiskPie must not erase application folders itself. Attributable
  leftovers remain reviewable and recyclable only.

## Shared product structure and full feature inventory

One scan result feeds the views, selection, inspector, and cleanup basket.
Changing visualizations must not trigger a fresh scan. Keep filesystem/Shell
work off the frame loop and retain bounded workers, cancellation, native path
identity, logical/allocated size distinctions, and link/cloud boundaries.
Simplify incrementally for a demonstrated feature or maintenance benefit;
retain the working engine and avoid speculative abstraction or new infrastructure.

| Capability | Complete-product outcome | Initial state |
| --- | --- | --- |
| Scan and navigate | Folders, drives, multi-drive summary, progressive results, cancel, full/branch rescan | Connected; branch rescan missing |
| Folder view | Clear navigable folder list with sorting and shared selection | Largest-children companion exists; full view pending |
| Sunburst | Interactive proportional rings | Connected; visual polish pending |
| Top items | Ranked largest items with scope and exact metrics | Companion list exists; dedicated experience pending |
| Treemap | Proportional rectangles using the same snapshot | New |
| Age | Group by last-modified date; never equate age with disuse | New |
| Flame, bubbles, mindmap | Three further views with consistent navigation | New |
| Inspector and Shell | Size, allocation, dates, counts, native path, Open, Reveal, copy path, supported previews | Partial; Shell wiring and previews pending |
| Cleanup basket | Multi-item review, estimates, recycle confirmation and per-item outcomes | Single-item logic/adapters exist; integration pending |
| Duplicates | Compare content; choose copies to keep before recycling | New |
| Quick cleanup candidates | Explain reviewable downloads, caches, temporary/build files | New; no automatic deletion |
| Applications and leftovers | Footprint, explicit official uninstall handoff, attributable leftovers | Installed Apps adapter exists; module pending |
| Snapshots | Save scans and compare growth, additions, removals | New |
| System monitor | CPU, memory, network, storage activity, relevant processes | New |
| Usability/distribution | English/Spanish, themes, keyboard, DPI, accessibility, local operation, portable download | Partial; verification/publication pending |

The eight views are folders, sunburst, top items, treemap, age, flame, bubbles,
and mindmap. The [public reference inventory](https://diskbuddy.com/mac-disk-space-analyzer)
is inspiration; this table and the traceability matrix control our scope.

## Delivery sequence

### 1. A measurable, usable exploration baseline

Connect existing Open and Reveal adapters to selected real items. Check current
source/build/artifact sizes read-only if needed; keep development directories,
distributed executable size, and runtime memory separate. Establish a
representative scan fixture and record startup, first-result, completion,
cancellation, and memory measurements. Do not invent a 1 MB target.

Use a representative main screen to establish the modern design in egui:
navigation, scan state, visualization, inspector, and cleanup entry point.
Resolve concrete usability obstacles before adding more views or infrastructure.

**Gate:** a Windows user can choose a location, understand progress, explore it,
open an item and reveal it in Explorer. Record checks and remaining platform
limitations; publish a useful analysis release when its supported flows pass.

### 2. Clear exploration and recycle-only cleanup

Polish folders, sunburst and top items; add treemap. Connect the shared inspector
and selection to a cleanup basket with exact-path review, duplicate selection
normalization, per-item results, and refresh after attempts. Complete branch
rescan where needed. Verify no-fallback recycling before enabling each provider
class, including the directory callback behavior.

**Gate:** users can find large items and recycle supported selections end to end.
Unsupported/oversized/disabled-bin cases preserve items and explain why. Test
only disposable fixtures, never a developer's real bin. Public release notes
state supported providers and unresolved limitations accurately.

### 3. Explain which items deserve review

Add content-based duplicate discovery, quick cleanup candidates, and the age
view. Reuse the same inspector/basket. Compare content before labeling files as
duplicates; account for hard links and avoid claiming guaranteed recovered bytes.
Candidates must explain their reason; names such as `target` or an old modified
date never authorize deletion. Keep expensive reads cancellable and bounded.

**Gate:** each result has understandable evidence, a keep/recycle choice, and
no automatic mutation; duplicate and candidate fixtures cover wrong matches.

### 4. Complete the remaining product options

Deliver flame, bubbles, mindmap, snapshots/comparison, applications/leftovers,
previews, and the system monitor as separate usable increments. Define supported
preview formats when implementing that module. Restrict leftover suggestions to
attributable paths and keep official uninstall handoff distinct from recycling.
Snapshot comparisons must identify incomplete scans and changed root semantics.

**Gate:** every module has a complete user flow and focused verification. Reuse
existing data and controls; measure new scan/content-read/background overhead.
Publish progressive releases rather than holding all completed modules back.

### 5. Complete-product release and maintenance

Close remaining Windows 10/11 clean-machine, network/removable/cloud,
accessibility, keyboard, DPI, theme, locale and performance checks. Finish any
remaining supported Explorer integration, settings and diagnostic-export work.
Verify packaging, licenses/advisories, SHA-256 and documented limitations using
the existing release workflow. A complete-product claim requires all in-scope
rows to pass; earlier useful releases need only their advertised supported flows.

## Definition of done and next assignment

A task describes an observable result, bounded scope, and relevant verification.
Use [AGENTS.md](AGENTS.md) for required checks and autonomy boundaries; report
checks not run without implying they passed. Update traceability only when new
evidence supports the claim. No feature is complete merely because a backend
adapter exists. Compare footprint when adding significant dependencies or work.

**Next assignment:** wire Open and Reveal from the existing adapters, verify
selection/state/path handling and a Windows smoke flow, and record the result.
Inspect current sizes read-only where useful; do not make cleanup of the
owner's development machine a prerequisite. Routine implementation decisions
are autonomous; changes of product scope, stack, external cost, major rewrites,
or irreversible user-data operations require the owner's decision.
