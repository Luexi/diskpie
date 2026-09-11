# Requirements traceability

This matrix records factual implementation status for the current Windows-only
product contract in [ROADMAP.md](../ROADMAP.md). Existing identifiers and evidence
are retained from the Scanner-derived baseline; that historical analysis no
longer defines product scope. New rows describe the expanded product target.

Policy updated 2026-09-11. This documentation change implements no features and
adds no runtime verification. Source baseline:
`f9a9a5fe8c6c5a108fae9edadda0f674a74a63b1`. A feature is complete only when its
user-reachable flow and relevant automated/manual checks are evidenced. Historical
checks do not establish readiness after later code changes.

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
| F-12 | Recycle confirmed items; never fall back to permanent deletion | Existing ADR 0008 flow, single-use `Confirmed<RecycleRequest>`, `IFileOperation`, progress-sink evidence, late identity validation and gated fixture (`DISKPIE_DESTRUCTIVE_FIXTURES=1`). UI binding and provider/directory matrix remain. Existing tests do **not** establish the new no-fallback policy: oversized, disabled/full-bin, unsupported-provider and uncertain-recoverability cases must skip/stop while preserving the item. | Adapter, unwired; new target unverified |
| F-13 | Permanent deletion — excluded by current policy | Legacy `DELETE`, `StronglyConfirmed<DeleteRequest>` and post-operation observation remain; a historical gated fixture ran locally. This documentation change has not removed the code. Do not connect, expose or use it as a recycle fallback; controlled removal may be a separate task. | Out of scope; legacy code exists |
| F-14 | Empty Recycle Bin — excluded by current policy | Legacy `EMPTY`, drive scope, `SHQueryRecycleBinW` and `SHEmptyRecycleBinW` remain; never executed against a real bin. Do not expose empty-bin behavior. Read-only bin information may support recycle eligibility; it does not authorize emptying the bin. | Out of scope; legacy code exists |
| F-15 | Explicit handoff to Windows Installed Apps | Fixed `ms-settings:appsfeatures` request on the STA with marshalling test; no `appwiz.cpl` fallback. UI binding pending. This is separate from DiskPie file cleanup: any external uninstall flow governs its own removal and must not be described as recyclable. | Adapter, unwired |
| F-16 | Accept an initial path through the CLI | `diskpie [--scan-path PATH \| [--] PATH]`; parser tests in `cli.rs`; startup path resolved off-thread and scanned; release exe prints `--help`, `--version`, and usage errors through the attached parent console (verified with redirected handles, exit codes 0/0/2). | Wired |
| F-17 | Write logs and export local diagnostics | Bounded daily logs with retention and redaction, typed path-free events for startup, settings, scans, and Shell actions; export policy (`build_export`) and native atomic export exist with tests, but no UI action triggers an export yet. | Wired (partial: export action) |
| F-18 | Add/remove optional Explorer integration | Per-user HKCU verb policy with ownership, staging, rollback, idempotence, stray cleanup (22 pure tests) and a real-registry adapter under a unique test root (9 tests); `--scan-path` verb argument. Settings toggle in the UI pending; Windows 10/11 VM verification pending. | Adapter, unwired |
| F-19 | Persist preferences | Versioned RON document read before the window and published after the event loop through the retained-handle adapter (`WindowsSettingsFile`, 12 native tests); theme, locale, metric, scale, layout knobs, and workers are read by the shell; round trips are covered by the settings and storage tests. A manual change-and-restart check in the UI is still pending. | Wired (partial: manual restart check) |
| F-20 | Provide original English and Spanish strings | 113 typed messages in both bundles; completeness, argument, and independence tests in `i18n.rs`; runtime locale switch. | Wired |

## Expanded product requirements

These rows are targets, not claims of implemented DiskBuddy parity. All file
cleanup reuses F-12; no module may bypass it. A recorded adapter or companion
list does not prove the complete new user experience.

| ID | Requirement | Evidence / remaining acceptance | Status |
| --- | --- | --- | --- |
| F-21 | Folder visualization with sorting and shared selection | Existing synchronized largest-children list provides a starting point; a complete folder-view flow remains to implement/verify. | Wired (partial: companion list only) |
| F-22 | Dedicated top-items visualization | Existing largest-children list is reachable; full ranked view with explicit scope, sorting and consistent metrics remains. | Wired (partial: companion list only) |
| F-23 | Treemap visualization | Render rectangles from the shared snapshot; share navigation, inspector, selection and basket without rescanning. No implementation evidence recorded. | Not started |
| F-24 | Age visualization | Group by last-modified date with unknown dates explicit; age is not proof of disuse or a reason to delete automatically. | Not started |
| F-25 | Flame visualization | Shared scan data, meaningful labels, navigation, selection and keyboard companion. | Not started |
| F-26 | Bubbles visualization | Shared scan data, proportional values, understandable grouping and consistent selection. | Not started |
| F-27 | Mindmap visualization | Shared hierarchy, bounded rendering, readable navigation and consistent selection. | Not started |
| F-28 | Content-based duplicates | Confirm content matches, handle hard-link identity, allow choosing copies to keep, and send reviewed removals through F-12. Reads must be bounded/cancellable; estimated recovery must account for allocation semantics. | Not started |
| F-29 | Multi-item cleanup basket | Existing single-item action logic is only a starting point. Add/remove selections, normalize overlapping targets, show exact paths and estimated space, confirm, report each outcome, refresh results. No automatic cleanup. | Not started (single-item foundation exists) |
| F-30 | Quick cleanup candidates | Explain candidate downloads, caches, temporary/build files and uncertainty. Folder names and age alone do not establish safety. All selected removals use the reviewed basket and F-12. | Not started |
| F-31 | Application footprints and attributable leftovers | F-15 provides only a settings handoff. Add footprint/evidence, explicit Windows Installed Apps or registered official uninstall handoff, and separate leftover review. Explain external uninstall semantics; never erase app directories directly, and recycle leftovers only through F-12. | Not started (F-15 adapter exists) |
| F-32 | Saved snapshots and scan comparisons | Save local scan metadata and compare growth/additions/removals; preserve root, size-basis and incomplete-scan context so comparisons do not imply false changes. | Not started |
| F-33 | System monitor | CPU, memory, network, storage activity and relevant processes; bounded sampling that does not impair scans or idle use. | Not started |
| F-34 | Copy native path from the inspector | Preserve native path semantics; represent unsupported clipboard text honestly. Exact user flow and clipboard checks pending. | Not started |
| F-35 | File preview and fuller inspector | Existing F-04/FS-02 cover basic details. Add dates and supported preview formats, define format coverage when implementing, keep reads off the UI thread and respect cloud boundaries. | Wired (partial: basic inspector only) |

F-03 plus F-21 through F-27 define the **eight views**: sunburst, folders, top
items, treemap, age, flame, bubbles, mindmap. Their selection, inspector and
cleanup basket must remain consistent across views.

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
| UX-03 | Offer keyboard navigation and a textual companion view | Virtualized largest-children list synchronized with the chart, accessible labels, shortcuts; headless tests for shortcuts and availability. Screen-reader (Narrator) pass pending; expanded views also require keyboard equivalence. | Wired (partial: screen-reader pass and expanded views) |
| UX-04 | Polished modern, understandable interface | Follow `docs/research/ui-product-direction.md`; representative main-screen design and Windows usability checks are pending. Existing shell is a foundation, not acceptance of the new design target. | Wired (partial: new design target unverified) |

## Quality and release

| ID | Requirement | Evidence | Status |
| --- | --- | --- | --- |
| Q-01 | Keep format, clippy, and tests clean | Local gate green on 2026-09-04 (fmt, clippy `-D warnings`, workspace tests, rustdoc `-D warnings`); `ci.yml` runs the same commands plus the release build. | Wired |
| Q-02 | Cover critical logic with unit/integration/property tests | Workspace test suite (see the latest CI run for the count), real-NTFS integration tests, subprocess panic tests, registry tests under a unique root, property tests in `diskpie-core`. | Wired |
| Q-03 | Benchmark scan throughput, first result, total time, memory, cancellation, layout/render, and binary size | No benchmark harness/report recorded. Historical 2026-09-04 release executable: 10.0 MB; workflow measures binary size. Distinguish build/cache directories, distributable size and runtime memory; establish a reproducible baseline and compare material additions. | Not started (historical binary-size measurement exists) |
| Q-04 | Audit dependency security and licenses | `security.yml`: cargo-deny, cargo-audit, and a THIRD-PARTY-NOTICES drift check on pull requests, pushes to `main`, and weekly. | Wired |
| R-01 | Produce a portable Windows x86-64 executable | Static CRT (ADR 0011 addendum), manifest, icon and version resources; local release build imports no VCRUNTIME/MSVCP/UCRT-API DLL; `release.yml` builds twice from different paths and gates the import table. Clean-machine launch checks pending. | Wired (partial: clean-VM launch) |
| R-02 | Publish a public repository with green CI | Public repository exists; CI must be green on the pushed head. | Wired |
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
