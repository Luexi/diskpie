# Requirements traceability

This matrix records factual implementation status for the current Windows-only
product contract in [ROADMAP.md](../ROADMAP.md). Existing identifiers and evidence
are retained from the Scanner-derived baseline; that historical analysis no
longer defines product scope. New rows describe the expanded product target.

Policy updated 2026-09-11 against source baseline
`f9a9a5fe8c6c5a108fae9edadda0f674a74a63b1`. Evidence updated 2026-09-13 for the
integrated tree (Scanner renovado, review remediation and connected actions,
ADRs 0020–0022). A feature is complete only when its user-reachable flow and
relevant automated/manual checks are evidenced. Historical checks do not
establish readiness after later code changes.

## Status vocabulary

Status distinguishes existing code, reachable behavior, and current scope:

- **Not started**: no implementation exists.
- **Logic only**: portable policy or pure logic exists with tests, but no
  native adapter or user-reachable path.
- **Adapter, unwired**: the native adapter and its tests exist, but the running
  executable does not call it yet.
- **Wired**: reachable from the executable; the evidence column says whether
  it is verified automatically, manually, or both. `Wired (partial)` names the
  missing piece.
- **Out of scope; legacy code exists**: implementation remains in the repository
  but must not be exposed or treated as a delivery requirement.

“New target unverified” means the current evidence does not prove the revised
behavior, even if an adapter already exists. “Not started” is the documented
baseline for expanded modules; inspect current source before starting work.

Historical manual Windows checks recorded here used Windows 11 build 26200
on 2026-09-04 unless stated otherwise. The redesign's automated, native visual
and performance evidence is in [the redesign validation record](research/scanner-renovado-validation.md);
the later fixes are in [the review remediation record](research/review-remediation-validation.md).
On 2026-09-06 four gated native action fixtures passed for file/directory
recycle and (legacy) permanent file/link deletion, each on a test-created
temporary fixture; no real bin-empty operation ran. Clean Windows 10/11 VM
checks remain pending and are called out per row.

## Functional requirements

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| F-01 | Select and scan a folder or drive | Native folder dialog on the Shell STA, drive list from `discover_volumes`, resolver worker, `RuntimeController::request_scan`; `crates/diskpie-platform/tests/scan_integration.rs` drives a real NTFS scan end to end; headless `ScanFlow` tests in `crates/diskpie/src/scan_ui.rs`; release-exe smoke run (folder pick, drive pick, CLI path). | Wired |
| F-02 | Present a summary of multiple drives | "Scan all drives" launches valid roots even if another fails resolution; typed issues/omissions remain visible. `Show summary` and synthetic-root session tests; resolver tests preserve valid roots. | Wired |
| F-03 | Render a proportional sunburst | Committed `SunburstLayout` uses positive-weight effective depth before hiding/grouping, a 0.28 center radius and the configured depth ceiling (default eight). Layout/property and angular-conservation tests cover geometry; presentation tests cover the 8-byte family index and native-path color stability through zoom, metric, reordering and branch replacement. Native appearance evidence is separate. | Wired |
| F-04 | Show path, selected size, both sizes, and file count on hover | Localized tooltip and optional collapsible details read the committed snapshot and honour `show_both_sizes`. The map center describes the current view root rather than hover or selection; incomplete/unknown size remains explicit. Renderer hit tests and shell state tests cover these surfaces. | Wired |
| F-05 | Zoom by clicking a segment | Click activates a real folder or selects a file without opening it; center goes to Parent, Enter activates, Space selects. Repeated double-click activation is suppressed. Renderer tests and `focused_folder_row_space_selects_and_enter_navigates` cover keyboard requests; generation/revision seals and `deferred_navigation_never_rebinds_a_reused_node_to_a_new_snapshot` prevent stale targets. | Wired |
| F-06 | Navigate back and to the parent folder | Back and Parent commands with `CommandAvailability`; Backspace, Alt+Left, Alt+Up shortcuts; navigation history tests. | Wired |
| F-07 | Hide and restore chart branches | Context-menu Hide/Restore/Restore all through `NavigationAction`; hidden-branch bound tests in `navigation.rs`. | Wired |
| F-08 | Rescan an entire selection or one branch | Full rescan re-resolves roots; branch intent retains the settled snapshot and resolves only its native target. `branch_rescan.rs` and runtime tests cover atomic replacement, cross-branch hard links, cancellation/failure retention, and stale intents/generations. | Wired |
| F-09 | Stream partial results and useful progress | Cost-paced progressive snapshots; pending directories remain incomplete and completed siblings retain their state. Session cancellation/omission tests and `progressive_frames_are_prebuilt_throttled_and_terminal_is_not_starved`; benchmark evidence tracked in Q-03. | Wired |
| F-10 | Cancel promptly without freezing the UI | Cancel command and Esc call `cancel_active`; `repeated_cancel_and_rescan_neither_leaks_nor_drifts` and the scan-crate cancellation tests; cancellation latency benchmark pending (Q-03). | Wired |
| F-11 | Open files and folders with the platform shell | Item context/inspector actions submit snapshot validation off-thread then typed Open/Reveal to the STA. Native marshalling tests and action UI routing tests; new native interaction check pending. | Wired (partial: native UI check) |
| F-12 | Recycle confirmed items; never fall back to permanent deletion | Connected exact-path review with Cancel focused, support-worker revalidation, single-use `Confirmed<RecycleRequest>`, STA late identity validation, recycle evidence, per-attempt result and refresh obligations, and shutdown receipts. Enabled only for a proven fixed local NTFS provider; gated native fixtures (`DISKPIE_DESTRUCTIVE_FIXTURES=1`) and UI policy tests. Existing tests do **not** establish the new no-fallback policy: oversized, disabled/full-bin, unsupported-provider and uncertain-recoverability cases must skip/stop while preserving the item, and the disposable-VM provider/directory matrix remains. | Wired (partial: no-fallback policy and provider matrix unverified) |
| F-13 | Permanent deletion — excluded by current policy | The 2026-09-04..07 integration connected a typed `DELETE` flow, `StronglyConfirmed<DeleteRequest>` and post-action refresh to item actions before the 2026-09-11 decision. That control must be removed from the interface as the next bounded task; the dormant adapter may stay until a separate maintenance change removes it. Never use it as a recycle fallback. | Out of scope; legacy UI control pending removal |
| F-14 | Empty Recycle Bin — excluded by current policy | The same integration connected a Tools bin query and typed `EMPTY` flow; it never ran against a real bin. The empty-bin control must be removed from the interface as the next bounded task. Read-only bin information may still support recycle eligibility; it does not authorize emptying the bin. | Out of scope; legacy UI control pending removal |
| F-15 | Explicit handoff to Windows Installed Apps | Tools submits the fixed `ms-settings:appsfeatures` request on the STA; marshalling/routing tests, no `appwiz.cpl` fallback. This is separate from DiskPie file cleanup: the external uninstall flow governs its own removal and must not be described as recyclable. Native UI check pending. | Wired (partial: native UI check) |
| F-16 | Accept an initial path through the CLI | `diskpie [--scan-path PATH \| [--] PATH]`; parser tests in `cli.rs`; startup path resolved off-thread and scanned; release exe prints `--help`, `--version`, and usage errors through the attached parent console (verified with redirected handles, exit codes 0/0/2). | Wired |
| F-17 | Write logs and export local diagnostics | Tools builds exact paginated preview with per-export path consent, native UTC-dated Save As, retained real-transport preflight and single-use create-only commit. Platform tests cover existing/late targets, final-interval hard-link alias, source guards, bounded log tails; support token/preview and UI pagination tests. ADR 0020 refines ADR 0016 overwrite behavior. | Wired (partial: native UI check) |
| F-18 | Add/remove optional Explorer integration | Tools preferences asynchronously inspect actual HKCU state and offer ownership-safe install/repair/remove. Existing pure and isolated registry tests plus UI status mapping; failed post-apply refresh preserves its mutation report. Windows 10/11 Explorer VM checks pending. | Wired (partial: VM check) |
| F-19 | Persist preferences | Version-1 RON settings retain existing values and add defaulted `show_item_list=false` and `item_list_width_points=300` (validated to 260–400). System theme and Allocated metric remain defaults. Old-document, numeric-bound, explicit-save and `optional_table_and_appearance_preferences_are_persisted_together` tests complement retained-handle storage tests. Sort/query/navigation remain transient; manual restart evidence is tracked separately. | Wired (partial: manual restart check) |
| F-20 | Provide original English and Spanish strings | 188 typed messages in both bundles, including partial/unverified Explorer states; completeness, argument and independence tests in `i18n.rs`; runtime locale switch covers the optional list and connected workflows. Native text/layout evidence belongs to the redesign validation record. | Wired |

## Expanded product requirements

These rows are targets, not claims of implemented DiskBuddy parity. All file
cleanup reuses F-12; no module may bypass it. A recorded adapter or companion
list does not prove the complete new user experience.

| ID | Requirement | Evidence / remaining acceptance | Status |
| --- | --- | --- | --- |
| F-21 | Folder visualization with sorting and shared selection | Show list opens the complete, searchable, virtualized immediate-child list with Name/Size ordering, shared selection and keyboard access (see UX-03). A dedicated folder view with column sorting beyond name/size and per-view navigation remains to design. | Wired (partial: list panel only) |
| F-22 | Dedicated top-items visualization | The size-ordered list panel ranks immediate children of the current view; a whole-scan ranked view with explicit scope, counts and percentages remains. | Wired (partial: per-folder list only) |
| F-23 | Treemap visualization | Render rectangles from the shared snapshot; share navigation, inspector, selection and basket without rescanning. No implementation evidence recorded. | Not started |
| F-24 | Age visualization | Group by last-modified date with unknown dates explicit; age is not proof of disuse or a reason to delete automatically. | Not started |
| F-25 | Flame visualization | Shared scan data, meaningful labels, navigation, selection and keyboard companion. | Not started |
| F-26 | Bubbles visualization | Shared scan data, proportional values, understandable grouping and consistent selection. | Not started |
| F-27 | Mindmap visualization | Shared hierarchy, bounded rendering, readable navigation and consistent selection. | Not started |
| F-28 | Content-based duplicates | Confirm content matches, handle hard-link identity, allow choosing copies to keep, and send reviewed removals through F-12. Reads must be bounded/cancellable; estimated recovery must account for allocation semantics. | Not started |
| F-29 | Multi-item cleanup basket | Single-item recycle review, capability binding, per-attempt result and refresh exist (F-12). Add/remove selections, normalize overlapping targets, show exact paths and estimated space, confirm once, report each outcome, refresh results. No automatic cleanup. | Not started (single-item flow exists) |
| F-30 | Quick cleanup candidates | Explain candidate downloads, caches, temporary/build files and uncertainty. Folder names and age alone do not establish safety. All selected removals use the reviewed basket and F-12. | Not started |
| F-31 | Application footprints and attributable leftovers | F-15 handoff is connected in Tools. Add footprint/evidence, explicit Windows Installed Apps or registered official uninstall handoff, and separate leftover review. Explain external uninstall semantics; never erase app directories directly, and recycle leftovers only through F-12. | Not started (F-15 handoff exists) |
| F-32 | Saved snapshots and scan comparisons | Save local scan metadata and compare growth/additions/removals; preserve root, size-basis and incomplete-scan context so comparisons do not imply false changes. | Not started |
| F-33 | System monitor | CPU, memory, network, storage activity and relevant processes; bounded sampling that does not impair scans or idle use. | Not started |
| F-34 | Copy native path from the inspector | Preserve native path semantics; represent unsupported clipboard text honestly. Exact user flow and clipboard checks pending. | Not started |
| F-35 | File preview and fuller inspector | Collapsible details show path, logical/allocated sizes, counts, omissions and share; contextual Open/Reveal are connected. Add modification dates and supported preview formats, define format coverage when implementing, keep reads off the UI thread and respect cloud boundaries. | Wired (partial: details and Open/Reveal only) |

F-03 plus F-21 through F-27 define the **eight views**: sunburst, folders, top
items, treemap, age, flame, bubbles, mindmap. Their selection, inspector and
cleanup basket must remain consistent across views.

## Filesystem semantics

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| FS-01 | Switch between logical and allocated size | Metric toggle issues `NavigationAction::SetSizeBasis`, which re-weights the layout; aggregation tests in `diskpie-core`. | Wired |
| FS-02 | Show both sizes in details | Optional collapsible details and tooltip show logical and allocated bytes; `show_both_sizes` setting honoured. The center and list use the active metric with explicit partial/unknown formatting. | Wired |
| FS-03 | Define and enforce hard-link counting semantics | ADR 0002; real NTFS identity/allocation tests and UI precision note. Observed allocation survives deduplication; branch merge tests transfer ownership correctly among surviving aliases and refresh cross-branch observations. | Wired |
| FS-04 | Avoid link/reparse loops and tree escape by default | ADR 0003; junction and symlink loop fixtures on real NTFS; fake-provider test proving a directory reparse point is never enqueued. | Wired |
| FS-05 | Handle sparse and compressed allocation | 5 GiB sparse and NTFS-compressed fixtures; `FILE_STANDARD_INFO.AllocationSize` matched the `GetCompressedFileSizeW` oracle (evidence recorded in ADR 0002). | Wired |
| FS-06 | Avoid unintended cloud-placeholder hydration | Cloud Files placeholder classification in `filesystem.rs` with unit tests; manual OneDrive Files On-Demand check pending. | Wired (partial: manual check) |
| FS-07 | Continue after inaccessible entries | DACL-denied native directory fixture proves omission with complete siblings; session pending/cancelled state tests and partial multi-root resolver tests keep omissions visible without globally failing useful work. | Wired |
| FS-08 | Define mount-point and volume-crossing policy | ADR 0003; directory reparse points (including mount points) are boundaries in `filesystem.rs` tests; per-volume hard-link identity. | Wired |

## User experience

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| UX-01 | Resize freely and remain per-monitor DPI aware | Embedded manifest declares `PerMonitorV2` and `longPathAware`; resizable map workspace with an optional 260–400-point list, switching map/list below 800 points. `renewed_shell_fits_minimum_viewport_and_keeps_toolbar_and_status_fixed` checks headless bounds. Native and mixed-DPI evidence are separate in the redesign validation record. | Wired (partial: manual DPI check) |
| UX-02 | Follow system theme and allow light/dark override | Appearance preferences retain System/Dark/Light and locale settings. Segoe UI is preferred when locally available; embedded/multilingual fallbacks remain. Settings and theme tests cover policy; earlier manual theme checks are historical and current native evidence is recorded separately. | Wired |
| UX-03 | Offer keyboard navigation and a textual companion view | Show list exposes all immediate children independent of chart budget. Name/Size, ascending/descending (default Size descending), case-insensitive search and filtering run off-thread. Immutable keys reject stale generation/revision/metric/query/sort; shared ordering gives O(log n) selected-row lookup. 640-row sorting/filter tests, the 600-row real-focus keyboard test and Space/Enter regression cover access beyond item 200. Narrator pass remains separate. | Wired (partial: screen-reader pass) |
| UX-04 | Polished modern, understandable interface | Follow `docs/research/ui-product-direction.md`; representative main-screen design and Windows usability checks are pending. Existing shell is a foundation, not acceptance of the new design target. | Wired (partial: new design target unverified) |

## Quality and release

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| Q-01 | Keep format, clippy, and tests clean | 2026-09-13: fmt, clippy `-D warnings`, 589 workspace tests, rustdoc `-D warnings` and all CI jobs (Windows, Linux portable, MSRV) plus the security workflow passed on the integrated tree. Exact per-delivery results are in the validation records; prior results are not proof for later edits. | Wired |
| Q-02 | Cover critical logic with unit/integration/property tests | Workspace test suite (see the latest CI run for the count), real-NTFS integration tests, subprocess panic tests, registry tests under a unique root, property tests in `diskpie-core`. | Wired |
| Q-03 | Benchmark scan throughput, first result, total time, memory, cancellation, layout/render, and binary size | Isolated `tools/diskpie-bench` harness covers synthetic/native scenarios, sampled process resources, cancellation/layout timings and the differential batch prototype. Redesign runs compare the pre-redesign working tree with deliveries 1–3 against map/list variants in seven interleaved runs, using compiled source/binary manifests and rejecting stale builds or partial-result overwrite. Its README and validation record identify available reports and limits; headless frame duration is not native FPS. Production retains the conventional provider. | Wired (partial: full measurement acceptance) |
| Q-04 | Audit dependency security and licenses | `security.yml`: cargo-deny, cargo-audit, and a THIRD-PARTY-NOTICES drift check on pull requests, pushes to `main`, and weekly. | Wired |
| R-01 | Produce a portable Windows x86-64 executable | Static CRT (ADR 0011 addendum), manifest, icon and version resources; local release build imports no VCRUNTIME/MSVCP/UCRT-API DLL; `release.yml` builds twice from different paths and gates the import table. Clean-machine launch checks pending. | Wired (partial: clean-VM launch) |
| R-02 | Publish a public repository with green CI | Public repository exists; CI passed on the integrated tree on 2026-09-13. CI must stay green on the pushed head. | Wired |
| R-03 | Publish a stable release with SHA-256 and verified limitations | `release.yml` produces a draft release with ZIP, SHA256SUMS, SBOM, and provenance on a `v*` tag; no tag or release has been created yet. | Not started |

## Release interpretation

Windows 10/11 x86-64 is the sole product target. Portable delivery comes first;
an installer requires a demonstrated need. Keep the existing free/open-source
`MIT OR Apache-2.0` license. Existing platform-neutral code/CI does not create a
Linux or macOS product requirement.

Early public releases may deliver a verified subset with clear release notes.
R-03 represents the eventual complete stable release; it does not block useful
progressive releases. Every advertised feature must work end to end, and no
release may weaken F-12 or conceal unverified Windows/provider behavior. F-13
and F-14 are excluded from completion calculations. The old “75%” estimate does
not apply to this expanded scope.
