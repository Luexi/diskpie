# Requirements traceability

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

Manual Windows checks recorded here were performed on Windows 11 build 26200
with the statically linked release executable on 2026-09-04 unless stated
otherwise. Clean-machine Windows 10 and Windows 11 virtual-machine checks are
still pending and are called out per row.

## Functional requirements

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| F-01 | Select and scan a folder or drive | Native folder dialog on the Shell STA, drive list from `discover_volumes`, resolver worker, `RuntimeController::request_scan`; `crates/diskpie-platform/tests/scan_integration.rs` drives a real NTFS scan end to end; headless `ScanFlow` tests in `crates/diskpie/src/scan_ui.rs`; release-exe smoke run (folder pick, drive pick, CLI path). | Wired |
| F-02 | Present a summary of multiple drives | "Scan all drives" launches a multi-root scan; `Show summary` command through `NavigationAction::ShowSummary`; session tests for the synthetic multi-root summary. | Wired |
| F-03 | Render a proportional sunburst | `SunburstView` fed by the committed `SunburstLayout`; layout unit and property tests in `diskpie-core`; angular-conservation check in `scan_integration.rs`. | Wired |
| F-04 | Show path, selected size, both sizes, and file count on hover | Localized tooltip and inspector rows from the committed snapshot; hit-test tests in `sunburst_view.rs`; inspector honours `show_both_sizes`. | Wired |
| F-05 | Zoom by clicking a segment | Click selects, double-click or Enter activates through `NavigationAction`; navigation tests in `diskpie-app`. | Wired |
| F-06 | Navigate back and to the parent folder | Back and Parent commands with `CommandAvailability`; Backspace, Alt+Left, Alt+Up shortcuts; navigation history tests. | Wired |
| F-07 | Hide and restore chart branches | Context-menu Hide/Restore/Restore all through `NavigationAction`; hidden-branch bound tests in `navigation.rs`. | Wired |
| F-08 | Rescan an entire selection or one branch | Full rescan (F5) re-resolves the roots and relaunches; branch rescan has no engine path and is shown as unavailable with a reason. | Wired (partial: branch rescan) |
| F-09 | Stream partial results and useful progress | Progressive snapshots paced by `SnapshotPolicy`; telemetry rail shows phase and counters; `progressive_frames_are_prebuilt_throttled_and_terminal_is_not_starved` runtime test. First-result benchmark still pending (Q-03). | Wired |
| F-10 | Cancel promptly without freezing the UI | Cancel command and Esc call `cancel_active`; `repeated_cancel_and_rescan_neither_leaks_nor_drifts` and the scan-crate cancellation tests; cancellation latency benchmark pending (Q-03). | Wired |
| F-11 | Open files and folders with the platform shell | `ShellRequest::Open`/`Reveal` on the STA with marshalling tests (`shell_actions.rs`); typed `OpenItem`/`RevealItem` intents in `diskpie-app::actions`. The shell UI does not expose the buttons yet. | Adapter, unwired |
| F-12 | Recycle a confirmed item | ADR 0008 confirmation flow, single-use `Confirmed<RecycleRequest>`, `IFileOperation` with progress-sink evidence and late identity validation; gated fixture test (`DISKPIE_DESTRUCTIVE_FIXTURES=1`). UI binding pending. | Adapter, unwired |
| F-13 | Permanently delete only after strong confirmation | Typed word `DELETE`, `StronglyConfirmed<DeleteRequest>`, post-operation observation; gated fixture test executed once locally. UI binding pending. | Adapter, unwired |
| F-14 | Empty the recycle bin after confirmation | Typed word `EMPTY`, exact drive scope, `SHQueryRecycleBinW` estimate and `SHEmptyRecycleBinW`; never executed against a real bin. UI binding pending. | Adapter, unwired |
| F-15 | Open modern Installed Apps settings | Fixed `ms-settings:appsfeatures` request on the STA with marshalling test; no `appwiz.cpl` fallback. UI binding pending. | Adapter, unwired |
| F-16 | Accept an initial path through the CLI | `diskpie [--scan-path PATH \| [--] PATH]`; parser tests in `cli.rs`; startup path resolved off-thread and scanned; release exe prints `--help`, `--version`, and usage errors through the attached parent console (verified with redirected handles, exit codes 0/0/2). | Wired |
| F-17 | Write logs and export local diagnostics | Bounded daily logs with retention and redaction, typed path-free events for startup, settings, scans, and Shell actions; export policy (`build_export`) and native atomic export exist with tests, but no UI action triggers an export yet. | Wired (partial: export action) |
| F-18 | Add/remove optional Explorer integration | Per-user HKCU verb policy with ownership, staging, rollback, idempotence, stray cleanup (22 pure tests) and a real-registry adapter under a unique test root (9 tests); `--scan-path` verb argument. Settings toggle in the UI pending; Windows 10/11 VM verification pending. | Adapter, unwired |
| F-19 | Persist preferences | Versioned RON document read before the window and published after the event loop through the retained-handle adapter (`WindowsSettingsFile`, 12 native tests); theme, locale, metric, scale, layout knobs, and workers are read by the shell; round trips are covered by the settings and storage tests. A manual change-and-restart check in the UI is still pending. | Wired (partial: manual restart check) |
| F-20 | Provide original English and Spanish strings | 113 typed messages in both bundles; completeness, argument, and independence tests in `i18n.rs`; runtime locale switch. | Wired |

## Filesystem semantics

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| FS-01 | Switch between logical and allocated size | Metric toggle issues `NavigationAction::SetSizeBasis`, which re-weights the layout; aggregation tests in `diskpie-core`. | Wired |
| FS-02 | Show both sizes in details | Inspector and tooltip show logical and allocated bytes; `show_both_sizes` setting honoured. | Wired |
| FS-03 | Define and enforce hard-link counting semantics | ADR 0002; `hard_links_count_paths_logically_and_identity_once_for_allocation` on real NTFS; allocation-precision note surfaced in the UI. | Wired |
| FS-04 | Avoid link/reparse loops and tree escape by default | ADR 0003; junction and symlink loop fixtures on real NTFS; fake-provider test proving a directory reparse point is never enqueued. | Wired |
| FS-05 | Handle sparse and compressed allocation | 5 GiB sparse and NTFS-compressed fixtures; `FILE_STANDARD_INFO.AllocationSize` matched the `GetCompressedFileSizeW` oracle (evidence recorded in ADR 0002). | Wired |
| FS-06 | Avoid unintended cloud-placeholder hydration | Cloud Files placeholder classification in `filesystem.rs` with unit tests; manual OneDrive Files On-Demand check pending. | Wired (partial: manual check) |
| FS-07 | Continue after inaccessible entries | DACL-denied directory fixture proves an omission with siblings complete; access-denied entries surface as omissions in the UI. | Wired |
| FS-08 | Define mount-point and volume-crossing policy | ADR 0003; directory reparse points (including mount points) are boundaries in `filesystem.rs` tests; per-volume hard-link identity. | Wired |

## User experience

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| UX-01 | Resize freely and remain per-monitor DPI aware | Embedded manifest declares `PerMonitorV2` and `longPathAware` (verified in the built exe); resizable viewport; wide/narrow layouts. Multi-monitor DPI manual check pending. | Wired (partial: manual DPI check) |
| UX-02 | Follow system theme and allow light/dark override | Theme selector (System/Dark/Light) persisted through settings; manual check performed. | Wired |
| UX-03 | Offer keyboard navigation and a textual companion view | Virtualized largest-children list synchronized with the chart, accessible labels, shortcuts; headless tests for shortcuts and availability. Screen-reader (Narrator) pass pending. | Wired (partial: screen-reader pass) |

## Quality and release

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| Q-01 | Keep format, clippy, and tests clean | Local gate green on 2026-09-04 (fmt, clippy `-D warnings`, workspace tests, rustdoc `-D warnings`); `ci.yml` runs the same commands plus the release build. | Wired |
| Q-02 | Cover critical logic with unit/integration/property tests | Workspace test suite (see the latest CI run for the count), real-NTFS integration tests, subprocess panic tests, registry tests under a unique root, property tests in `diskpie-core`. | Wired |
| Q-03 | Benchmark scan throughput, first result, total time, memory, cancellation, layout/render, and binary size | No benchmark harness or report exists yet. Binary size is measured by the release workflow only. | Not started |
| Q-04 | Audit dependency security and licenses | `security.yml`: cargo-deny, cargo-audit, and a THIRD-PARTY-NOTICES drift check on pull requests, pushes to `main`, and weekly. | Wired |
| R-01 | Produce a portable Windows x86-64 executable | Static CRT (ADR 0011 addendum), manifest, icon and version resources; local release build imports no VCRUNTIME/MSVCP/UCRT-API DLL; `release.yml` builds twice from different paths and gates the import table. Clean-machine launch checks pending. | Wired (partial: clean-VM launch) |
| R-02 | Publish a public repository with green CI | Public repository exists; CI must be green on the pushed head. | Wired |
| R-03 | Publish a stable release with SHA-256 and verified limitations | `release.yml` produces a draft release with ZIP, SHA256SUMS, SBOM, and provenance on a `v*` tag; no tag or release has been created yet. | Not started |
