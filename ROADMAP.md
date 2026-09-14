# DiskPie product roadmap and agent handoff

> Product decisions updated 2026-09-11. Baseline and delivery status updated
> 2026-09-13 after the integration of the Scanner renovado redesign, the
> review remediation and the connected action flows (ADRs 0020–0022).

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

The connected application scans selected roots, discovers drives, streams
partial results, cancels, renders the map-first sunburst with an optional
searchable, virtualized list, navigates, switches size metrics, rescans one
branch or the whole selection, and persists settings. Contextual Open and
Reveal are connected, as is single-item recycling for proven fixed local NTFS
with exact-path review. Tools offers the Installed Apps handoff, Explorer
verb install/repair/remove with real registry inspection, and a redacted
diagnostics preview with a native Save As flow. English and Spanish strings
(188 messages) and themes exist. See [architecture](docs/architecture.md) and
the validation records under `docs/research/` for the evidence.

Two connected controls predate the 2026-09-11 decision and contradict it: a
typed permanent-delete flow on items and an Empty Recycle Bin flow in Tools.
Removing them from the interface is the first task of delivery 2.

Recorded on 2026-09-13: format, Clippy, rustdoc and 589 workspace tests pass
locally and in every CI job; the security workflow passes. A reproducible
benchmark harness exists under `tools/diskpie-bench` with recorded seven-run
comparisons. Native captures cover both themes, both languages and 100–200 %
scaling. These are development observations, not release readiness.

Still pending: clean Windows 10/11 checks, the no-fallback recycling policy
and its provider/directory fixtures, OneDrive hydration checks, Narrator,
physical mixed-DPI transitions and broader performance acceptance. See
traceability for each requirement. No release has been published. Linux CI
checks portable logic; Linux is not a product target.

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
| Scan and navigate | Folders, drives, multi-drive summary, progressive results, cancel, full/branch rescan | Connected, including branch rescan |
| Folder view | Clear navigable folder list with sorting and shared selection | Searchable complete child list with name/size ordering exists; dedicated view pending |
| Sunburst | Interactive proportional rings | Connected; Scanner renovado composition with adaptive depth and stable colors |
| Top items | Ranked largest items with scope and exact metrics | Per-folder size ordering exists; whole-scan ranked view pending |
| Treemap | Proportional rectangles using the same snapshot | New |
| Age | Group by last-modified date; never equate age with disuse | New |
| Flame, bubbles, mindmap | Three further views with consistent navigation | New |
| Inspector and Shell | Size, allocation, dates, counts, native path, Open, Reveal, copy path, supported previews | Details, Open and Reveal connected; dates, copy path and previews pending |
| Cleanup basket | Multi-item review, estimates, recycle confirmation and per-item outcomes | Single-item recycle flow connected; basket and no-fallback policy pending |
| Duplicates | Compare content; choose copies to keep before recycling | New |
| Quick cleanup candidates | Explain reviewable downloads, caches, temporary/build files | New; no automatic deletion |
| Applications and leftovers | Footprint, explicit official uninstall handoff, attributable leftovers | Installed Apps handoff connected in Tools; module pending |
| Snapshots | Save scans and compare growth, additions, removals | New |
| System monitor | CPU, memory, network, storage activity, relevant processes | New |
| Usability/distribution | English/Spanish, themes, keyboard, DPI, accessibility, local operation, portable download | Connected; Windows 10/11, Narrator and publication pending |

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

**Status 2026-09-13:** implemented and covered by automated checks and native
captures (ADR 0021, redesign and remediation validation records, benchmark
harness). The clean Windows 10/11 walkthrough and the analysis release remain.

### 2. Clear exploration and recycle-only cleanup

First remove the connected permanent-delete and empty-bin controls so the
interface matches the recycle-only contract. Then polish folders, sunburst
and top items; add treemap. Connect the shared inspector
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

**Next assignment (delivery 2):** remove the permanent-delete and empty-bin
controls from the interface, then add the treemap view and the multi-item
cleanup basket on the shared snapshot, selection and inspector. Verify the
no-fallback recycling policy with disposable fixtures before widening provider
support. Routine implementation decisions are autonomous; changes of product
scope, stack, external cost, major rewrites, or irreversible user-data
operations require the owner's decision.
