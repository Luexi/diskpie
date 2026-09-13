# Requirements traceability

Current refinement (2026-09-07): [independent-review remediation](research/review-remediation-validation.md)
records the new regression checks and measurements. Earlier evidence below
remains scoped to its source/date and is not a certification of later changes.

| Requirements | Current refinement and regression |
| --- | --- |
| F-02, F-08, FS-07 | Resolver retains the complete native selection; UI F5 retries a failed root twice and includes it after recovery. |
| F-08, F-10 | Accepted cancellation at merge, presentation and final publication keeps the old snapshot; actual provider failure stays Failed. |
| FS-04, FS-06, FS-08 | ADR 0022 anchors directory enumeration and child metadata to the same retained handle; concurrent replacement, native-name and parser/fallback fixtures. Provider/cloud acceptance remains separate. |
| F-03, UX-03, Q-03 | Depth is computed per metric during aggregation and roots are indexed; cancellation covers large sorts/grouping. Existing geometry/color/list contracts remain. New measurements are reported separately. |
| F-17 | Schema 2 distinguishes last-scan observations from unmeasured values; exact suffix/reference tests and bounded retained-handle tail reads. |
| F-18, F-19 | Partial Explorer mutations preserve outcomes and re-inspect; unverified state is invalidated. Owned partial installations expose safe completion. |
| Q-02 | Partial startup failure has no clean-shutdown proof without complete receipts. |
| Q-01 | Both Cargo workspaces have explicit format, Clippy and test checks; final results are recorded in the linked remediation report. |

This living matrix translates the acceptance criteria in
`legacy-scanner-analysis.md` into verifiable product work. A feature is only
complete when its evidence column names an automated check, a documented manual
check, or both.

## Status vocabulary

The single word `Planned` hid the difference between code that exists and code
a user can reach, so every row now uses one of four states:

- **Not started**: no implementation exists.
- **Logic only**: portable policy or pure logic exists with tests, but no
  native adapter or user-reachable path.
- **Adapter, unwired**: the native adapter and its tests exist, but the running
  executable does not call it yet.
- **Wired**: reachable from the executable; the evidence column says whether
  it is verified automatically, manually, or both. `Wired (partial)` names the
  missing piece.

Historical manual Windows checks recorded here used Windows 11 build 26200
on 2026-09-04 unless stated otherwise. The Scanner renovado implementation and
its current automated, native visual and performance evidence are tracked in
[the redesign validation record](research/scanner-renovado-validation.md).
Historical checks do not certify the redesigned surfaces. Action, integration,
export and list flows also have dedicated automated coverage.
On 2026-09-06 four gated native action fixtures passed for file/directory
recycle and permanent file/link deletion; each mutation targeted a test-created
temporary fixture. No real bin-empty operation ran. Clean Windows 10/11 VM
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
| F-12 | Recycle a confirmed item | Exact-path review with Cancel focus, support-worker revalidation, single-use capability, STA late validation and recycle evidence; result/refresh obligations and shutdown receipts. Only proven fixed local NTFS enabled. Native gated fixture tests and UI policy tests; wider disposable-VM provider matrix pending. | Wired (partial: provider/manual matrix) |
| F-13 | Permanently delete only after strong confirmation | Connected exact-path review plus typed `DELETE`, single-use strong capability, late target validation, and post-action branch/full refresh. Pure flow/UI tests and gated native temporary fixtures; new native confirmation check pending. | Wired (partial: native UI check) |
| F-14 | Empty the recycle bin after confirmation | Tools bin query/estimate, explicit all-drive scope, strong typed `EMPTY`, no-mid-operation-cancel explanation, outcome and requery obligation. Fake/marshalling and confirmation tests; real empty-bin execution is reserved for a disposable VM and has not run against the user bin. | Wired (partial: disposable-VM check) |
| F-15 | Open modern Installed Apps settings | Tools submits fixed `ms-settings:appsfeatures` on the STA; marshalling/routing tests, no `appwiz.cpl` fallback. Native UI check pending. | Wired (partial: native UI check) |
| F-16 | Accept an initial path through the CLI | `diskpie [--scan-path PATH \| [--] PATH]`; parser tests in `cli.rs`; startup path resolved off-thread and scanned; release exe prints `--help`, `--version`, and usage errors through the attached parent console (verified with redirected handles, exit codes 0/0/2). | Wired |
| F-17 | Write logs and export local diagnostics | Tools builds exact paginated preview with per-export path consent, native UTC-dated Save As, retained real-transport preflight and single-use create-only commit. Platform tests cover existing/late targets, final-interval hard-link alias, source guards, bounded log tails; support token/preview and UI pagination tests. ADR 0020 refines ADR 0016 overwrite behavior. | Wired (partial: native UI check) |
| F-18 | Add/remove optional Explorer integration | Tools preferences asynchronously inspect actual HKCU state and offer ownership-safe install/repair/remove. Existing pure and isolated registry tests plus UI status mapping; failed post-apply refresh preserves its mutation report. Windows 10/11 Explorer VM checks pending. | Wired (partial: VM check) |
| F-19 | Persist preferences | Version-1 RON settings retain existing values and add defaulted `show_item_list=false` and `item_list_width_points=300` (validated to 260–400). System theme and Allocated metric remain defaults. Old-document, numeric-bound, explicit-save and `optional_table_and_appearance_preferences_are_persisted_together` tests complement retained-handle storage tests. Sort/query/navigation remain transient; manual restart evidence is tracked separately. | Wired (partial: manual restart check) |
| F-20 | Provide original English and Spanish strings | 188 typed messages in both bundles, including partial/unverified Explorer states; completeness, argument and independence tests in `i18n.rs`; runtime locale switch covers the optional list and connected workflows. Native text/layout evidence belongs to the redesign validation record. | Wired |

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

## Quality and release

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| Q-01 | Keep format, clippy, and tests clean | The deliveries 1–3 gate history is in ROADMAP.md. Exact format, Clippy, test, rustdoc and build results for Scanner renovado belong to [its validation record](research/scanner-renovado-validation.md); prior results are not proof for later edits. `ci.yml` remains unchanged. | Wired |
| Q-02 | Cover critical logic with unit/integration/property tests | Workspace test suite (see the latest CI run for the count), real-NTFS integration tests, subprocess panic tests, registry tests under a unique root, property tests in `diskpie-core`. | Wired |
| Q-03 | Benchmark scan throughput, first result, total time, memory, cancellation, layout/render, and binary size | Isolated `tools/diskpie-bench` harness covers synthetic/native scenarios, sampled process resources, cancellation/layout timings and the differential batch prototype. Redesign runs compare the pre-redesign working tree with deliveries 1–3 against map/list variants in seven interleaved runs, using compiled source/binary manifests and rejecting stale builds or partial-result overwrite. Its README and validation record identify available reports and limits; headless frame duration is not native FPS. Production retains the conventional provider. | Wired (partial: full measurement acceptance) |
| Q-04 | Audit dependency security and licenses | `security.yml`: cargo-deny, cargo-audit, and a THIRD-PARTY-NOTICES drift check on pull requests, pushes to `main`, and weekly. | Wired |
| R-01 | Produce a portable Windows x86-64 executable | Static CRT (ADR 0011 addendum), manifest, icon and version resources; local release build imports no VCRUNTIME/MSVCP/UCRT-API DLL; `release.yml` builds twice from different paths and gates the import table. Clean-machine launch checks pending. | Wired (partial: clean-VM launch) |
| R-02 | Publish a public repository with green CI | Public repository exists; CI must be green on the pushed head. | Wired |
| R-03 | Publish a stable release with SHA-256 and verified limitations | `release.yml` produces a draft release with ZIP, SHA256SUMS, SBOM, and provenance on a `v*` tag; no tag or release has been created yet. | Not started |
