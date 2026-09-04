# DiskPie roadmap and continuation handoff

> Updated on 2026-09-04 after the integration cycle that connected the product.
> This document is the operational handoff for the next maintainer or agent.
> It describes the committed tree as it exists now; it is not a claim that the
> product is complete. The status of every execution phase is at the end.

## Mission and completion boundary

DiskPie is a clean-room, Windows-first disk usage analyzer written in stable
Rust with egui/eframe. The required first release is a portable Windows 10/11
x86-64 executable published from the public GitHub repository.

The project must not stop at a scaffold, demo, or MVP. Completion means that the
full product contract in [the legacy analysis](docs/legacy-scanner-analysis.md)
is implemented, verified, documented, and published as a stable release.

Read these sources before changing code, in this order:

1. [Legacy scanner analysis and product contract](docs/legacy-scanner-analysis.md)
2. [Architecture](docs/architecture.md)
3. [Requirements traceability](docs/requirements-traceability.md)
4. [Architecture decisions](docs/adr/README.md)
5. [Research index](docs/research/README.md)
6. This roadmap

The original proprietary Scanner reference files in the parent directory must
never be modified, moved, copied into this repository, committed, or published.

## Repository state

| Item | Value |
| --- | --- |
| Local repository | `C:\Users\Luis\Desktop\scan\diskpie` |
| Public repository | <https://github.com/Luexi/diskpie> |
| Integration branch | `main` (the `feat/initial-release` work was fast-forwarded into it on 2026-09-04) |
| Release toolchain / MSRV | Rust 1.97.1 pinned by `rust-toolchain.toml`; `rust-version = "1.92"` checked by a dedicated CI job |
| Product version | `0.1.0` development version; no tag or release exists yet |

The working tree is clean. Every line of the previously uncommitted work is
committed, so a fresh clone builds and CI exercises the whole tree. Use
`git status --short` before editing and keep commits small and coherent.

## Current delivery estimate

Roughly 75% of the literal release contract is implemented and reachable by a
user; the connected application scans, renders, navigates, persists
preferences, logs, and recovers from a crash marker. This is an orientation
estimate, not an acceptance metric. What remains is concentrated in binding
the destructive actions and the Explorer toggle to the interface, the
benchmark report, clean-machine validation, and publishing the release.
[`docs/requirements-traceability.md`](docs/requirements-traceability.md)
carries the per-requirement evidence.

### Status legend

- **Wired**: reachable from the executable, with the named automated or manual
  evidence. `Wired (partial)` names the missing piece.
- **Adapter, unwired**: the native adapter and its tests exist, but the
  executable does not call it yet.
- **Logic only**: portable policy or pure logic exists without a native
  adapter or user-reachable path.
- **Not started**: required product behavior is absent.

## What exists today

| Area | State | Evidence and caveats |
| --- | --- | --- |
| Compact tree, exact metrics, aggregation | Wired | `diskpie-core`; unit and property coverage; real-NTFS sparse, compressed, and hard-link fixtures confirm the size model. |
| Portable bounded scanner | Wired | `diskpie-scan`; fixed workers, per-volume queues, bounded channels, generation cancellation, partial batches, omissions; abandoned cancellations report `Cancelled`. |
| Windows conventional filesystem adapter | Wired | Long/native paths, allocated-size metadata, file identity, reparse/cloud boundaries; junction, symlink, denied-access, and long-path fixtures. |
| Windows volumes and selected-root resolution | Wired | Volume discovery feeds the drive list; selected and CLI paths are resolved off the frame thread. |
| Sunburst layout and hit testing | Wired | `SunburstView` renders the committed layout with localized tooltips; the zero-weight list is bounded by the sector budget. |
| Presentation, navigation, hide/restore, rescan intent | Wired | Every command goes through `navigation_command`/`execute_navigation`; branch rescan is shown as unavailable because the engine has no path for it. |
| Runtime orchestration | Wired | Bounded scans, cost-paced progressive snapshots, off-thread layout, cancellation/replacement, retirement reaper, ordered shutdown with typed receipts. |
| Folder picker | Wired | `IFileOpenDialog` on the Shell STA with nonblocking `request_shutdown`, bounded `finish`, and a process-wide reaper; a dismissed dialog keeps the previous scan state. |
| CLI initial path | Wired | `[--scan-path PATH \| [--] PATH]` parsed path-preservingly and scanned at startup; release builds print help, version, and usage errors. |
| Settings | Wired | Versioned RON policy read before the window and published after the event loop through the retained-handle `app.ron` adapter. |
| Typed diagnostics and local log lifecycle | Wired | Bounded daily logs, redaction, retention, startup/settings/scan/Shell events; export from the interface is not connected yet. |
| Panic marker | Wired | Native retained-handle session, process hook, next-start notice, and clean-shutdown reconciliation that requires diagnostics, runtime, Shell, resolver, and bridge receipts. |
| English and Spanish interfaces | Wired | 113 typed messages in both bundles with completeness, argument, and independence tests; runtime locale switch. |
| Visual shell | Wired | Connected product with the required scan states, telemetry, inspector, synchronized largest-items list, keyboard parity, and system fallback fonts. |
| Open/recycle/delete/empty recycle bin/Installed Apps | Adapter, unwired | ADR 0008 confirmation flow, single-use capabilities, STA requests/outcomes, late identity validation, post-operation observation, and cancellation exist; the interface binding and the disposable-VM provider matrix remain. |
| Explorer integration | Adapter, unwired | Portable policy, HKCU adapter under unique test roots, and the `--scan-path` verb exist; the settings toggle and the Windows 10/11 VM checks remain. |
| Packaging | Wired (partial) | Manifest, icon, version resources, static CRT, import-table gate, and a reproducible release workflow exist; clean-machine launches are pending. |
| Benchmarks and release evidence | Not started | No benchmark harness or report exists; binary size is measured only by the release workflow. |
| Stable GitHub release | Not started | The release workflow can publish a draft from a `v*` tag; no tag has been created. |

## Latest verification evidence (2026-09-04)

- The full local gate passed on the integrated tree: `cargo fmt --check`,
  `cargo clippy --workspace --all-targets --all-features --locked -D warnings`,
  `cargo test --workspace --all-features --locked` (499 passed, 6 subprocess
  helpers ignored by design), `cargo doc` with `-D warnings`, and
  `cargo build --release -p diskpie --locked`.
- The release executable (10.0 MB, static CRT) carries the manifest, icon,
  and version resources, imports no Visual C++ runtime DLL, prints `--help`
  and `--version` and returns exit code 2 for an unknown option through
  redirected handles, and completed a smoke scan of a 40-file fixture with
  exact counts and a clean exit code 0.
- Every workstream merged in this cycle was implemented and then reviewed by
  an independent adversarial pass before merging; findings rated high or
  blocking were fixed before integration.
- Not yet run: clean Windows 10/11 virtual-machine launches, the disposable
  Recycle Bin matrix, Narrator/accessibility review, benchmarks, and the
  remote CI run on the pushed head (check the Actions tab).

## Critical contracts that must not regress

### Runtime ownership and retirement

- The UI frame loop must never wait for scanning, layout, provider disposal,
  filesystem resolution, or shutdown.
- Rejected scan launch returns the caller-owned provider, roots, and cancel
  token through `ScanLaunchRejected::into_parts`.
- The process reaper has 64 ordinary retirement slots plus one reserved teardown
  slot. Capacity includes the payload currently being disposed, queued payloads,
  and the reserved slot. Do not release active capacity before disposal returns.
- Runtime teardown may hand ownership to bounded background retirement, but it
  must not use `mem::forget`, create one thread per drop, or wait while holding a
  mutex.
- Debug/error values crossing diagnostics must not expose selected paths.

### Hidden-branch state

- Persistent hidden-state deltas have a maximum depth of 64 and lookup work of
  at most 65 nodes.
- Rebase is O(1) on the caller by swapping to an already cached `Arc` base. If a
  cached rebase is unavailable, return typed `HiddenStateBusy`; never materialize
  an unbounded set in an egui callback.
- `RestoreAllBranches` replaces the plan with an empty base.
- Snapshot replacement may perform off-frame compaction while preserving
  identity-first anchors.

### Filesystem semantics

- Native paths are identity; lossy strings are display-only.
- Do not follow reparse points, junctions, symlinks, mount points, or cloud
  boundaries by default when they may loop, hydrate, cross a volume, or escape
  the selected tree.
- Logical and allocated sizes remain distinct. Unknown allocation is not zero.
- Hard-link deduplication is per volume and based on file identity; imprecise
  identity must remain visible in diagnostics/presentation semantics.
- Access-denied entries become omissions and do not abort the entire scan.

### Settings, diagnostics, and crash data

- No filesystem I/O may occur from an egui callback.
- Settings are limited to 64 KiB and use an application-owned `app.ron`; there
  is no fallback to eframe persistence.
- Future Windows settings and panic-marker writers must prepare and retain local
  non-reparse parent/target handles, use bounded fixed sibling names, commit by
  handle-relative rename, and classify results from post-operation identities.
  Never infer commit solely from a Win32 return code.
- Diagnostics `Drop` is atomic/Arc-only. Bounded `finish` is the explicit flush
  point; stalled providers are handed to a process-wide bounded reaper.
- Diagnostics and panic errors/debug output are path-free.
- A clean-shutdown proof may only be created after every owned provider and
  worker has actually joined. It must not be publicly forgeable.

### Destructive actions

- All destructive mutations stay behind a separate fakeable interface.
- Recycling and permanent deletion require explicit exact-path confirmation;
  permanent deletion uses a stronger confirmation.
- Tests may delete only fixtures they created inside their own temporary roots.
- Do not construct commands for `cmd.exe` or PowerShell from user paths.

## Execution roadmap

The phases below are ordered by dependency. Do not start release work while a
blocking contract in an earlier phase remains open.

### Phase 1 - Stabilize and commit the paused working tree

1. Re-read this roadmap, `docs/architecture.md`, the active ADRs, and all dirty
   diffs. Preserve user/agent changes.
2. Run formatting and a no-run compile first:

   ```powershell
   cargo fmt --all -- --check
   cargo test --workspace --all-features --no-run
   ```

3. Repeat the independent read-only runtime review. In particular, prove the
   active+queued+available+reserved capacity invariant under stalled and
   panicking disposal and the hidden-state depth/lookup bound over repeated
   cycles.
4. Recheck the accepted diagnostic lifecycle without redesigning it.
5. Review the pure panic-marker contract, but do not add `mod panic_marker` to
   the executable until native transport and composition-root lifecycle are
   ready.
6. Run the complete local gate:

   ```powershell
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo test --workspace --all-features --locked
   cargo test --workspace --doc --locked
   git diff --check
   ```

7. Update `docs/requirements-traceability.md`; it currently understates already
   implemented infrastructure and must not remain entirely `Planned`.
8. Make small coherent commits, then push and wait for both GitHub Actions
   workflows. Do not bundle unrelated runtime, diagnostics, UI, and native-file
   changes into one commit.

Exit criteria: the current infrastructure has accepted reviews, a clean full
gate, coherent commits, green remote CI, and no temporary placeholder presented
as a working feature.

### Phase 2 - Native settings and crash transport

Implement one shared retained-handle pattern in `diskpie-platform`, with separate
policy-facing adapters for settings and panic markers.

Required settings behavior:

- prepare exact `app.ron` under the application-owned settings directory;
- reject ADS, reparse targets, directories, hard-linked targets, or non-local
  replacements;
- read at most 64 KiB and distinguish missing, document, oversized, and
  unavailable states without logging paths;
- preserve whether the file was initially missing;
- use a fixed bounded set of sibling candidates;
- use `FileRenameInformationEx` relative to the retained parent;
- replace existing targets with replace + POSIX flags; create an initially
  missing target without replace semantics;
- inspect source and target identity after every call and return
  `NotCommitted`, `Committed`, or `CommittedButUnverified`; and
- clean up only a proven-owned sibling through its handle.

Required panic behavior:

- install the hook only after crash paths and native writer capabilities exist;
- retain the previous marker snapshot without reopening by path;
- write only prebuilt bounded/path-free data from the panic hook;
- do not chain the previous hook in release mode;
- preserve the previous marker across a clean no-panic run;
- restore/remove the current-session marker only after a real quiescence proof;
  and
- add deterministic race-injection and native NTFS tests.

Record the exact Windows API evidence in ADR 0019 and ADR 0017. There is no
rename-by-path or ordinary `std::fs::rename` fallback.

Exit criteria: native adapters, deterministic tests, Windows fixture tests,
composition-root integration, settings load/save across runs, and crash-marker
reconciliation all pass.

### Phase 3 - Harden native service lifecycles

`ShellService::Drop` currently joins and may block indefinitely while a modal
dialog or provider stalls. Replace this with:

- nonblocking `request_shutdown`;
- explicit bounded `finish(timeout)` outside the UI loop;
- a process-wide, precreated, bounded reaper for a timed-out `JoinHandle` and
  its singleton-instance claim;
- a fail-closed terminal state if reaper registration becomes unavailable; and
- no per-drop thread, no unbounded queue, and no `mem::forget`.

The singleton claim must remain held until the old STA actually exits so a
second Shell STA cannot start concurrently.

Exit criteria: injected stalled/panicking worker tests prove bounded drop,
bounded finish, single ownership, and eventual restart only after real exit.

### Phase 4 - Connect the actual application UI

Replace the calibration/demo shell with the real application flow while keeping
the visual direction in [UI product direction](docs/research/ui-product-direction.md).

Implementation order:

1. Start platform services and `RuntimeController` in the composition root.
2. Obtain the native owner HWND safely from eframe/raw-window-handle and submit
   folder selection to `ShellService`.
3. Resolve selected/startup paths off the UI thread with bounded ownership.
4. Create `WindowsFileSystem` with the same cancel token and launch real roots.
5. Poll runtime and shell events in the per-frame logic phase using bounded
   budgets and elapsed `Duration`; request repaint only while work exists.
6. Render the committed `SunburstLayout` through `SunburstView` and keep a
   synchronized, keyboard-operable largest-children list.
7. Wire hover details: exact/display path, logical bytes, allocated bytes, file
   count, omissions/unknown state, percentage, and scan phase.
8. Wire click/select/double-click zoom, Back, Parent, summary, hide/restore,
   full rescan, branch rescan, cancel, and metric switching through typed
   navigation commands.
9. Add a multi-volume selector and summary view with partial-discovery errors.
10. Synchronize theme, locale, scale, metric, and other preferences to the
    settings session without doing I/O in a frame callback.

Required UI states: empty, choosing, resolving, scanning, partial, cancelling,
complete, cancelled-with-results, failed, and optional-services-unavailable.

Exit criteria: headless/state tests and Windows smoke tests prove the complete
selection-to-sunburst flow, responsive cancellation, keyboard parity, wide and
narrow layouts, themes, locales, and per-monitor DPI behavior.

### Phase 5 - Shell actions and confirmations

Extend the existing Shell STA with typed requests and outcomes for:

- open/reveal a file or folder;
- recycle a confirmed item;
- permanently delete a strongly confirmed item;
- empty the recycle bin after confirmation; and
- open modern Installed Apps settings.

Implement the UI-independent confirmation state machine from ADR 0008 first.
Bind a confirmation to immutable path identity and the current generation.
Revalidate immediately before mutation and reject stale/synthetic/hidden nodes.
Use modern Shell APIs described in `docs/research/windows-shell-actions.md`.

Exit criteria: fake-adapter unit tests, native temporary-fixture tests, privacy
tests, cancellation/error outcomes, and manual Windows confirmation evidence.

### Phase 6 - Optional Explorer integration

Implement reversible, idempotent, per-user Windows Explorer verbs for folders
and drives. Never require administrator privileges or write HKCR directly.

- Use the exact executable path safely and quote through structured registry
  values, not a shell-built command.
- Add and remove only DiskPie-owned keys.
- Detect foreign/conflicting keys and fail without overwriting them.
- Expose status and opt-in/opt-out controls in settings.

Exit criteria: add/remove/idempotence/foreign-key tests plus Windows 10 and 11
manual verification for classic and modern context-menu paths.

### Phase 7 - Performance, robustness, and audits

Create reproducible benchmarks and reports for:

- files per second;
- time to first visible partial result;
- total scan time;
- memory per million entries;
- cancellation latency;
- layout time and rendered frame cost;
- hidden-state and navigation stress;
- native shutdown latency; and
- portable binary size.

Add or complete fixtures for Unicode, emoji, non-scalar UTF-16, long paths,
deep trees, files over 4 GiB, hard links, sparse/compressed files, denied access,
junction loops, cloud placeholders, removable media, slow shares, and repeated
cancel/rescan.

Run separate agent reviews for architecture, security, performance, UX, and
accessibility. Run dependency advisory and license audits using the pinned lock
file. Prototype NTFS/MFT acceleration only after the conventional backend has a
baseline; integrate it only if benchmarks show a material benefit and it falls
back cleanly without elevation.

Exit criteria: benchmark report, audit reports, traceability evidence, and no
unresolved blocking/high review finding.

### Phase 8 - Portable release

1. Build the release with the Windows MSVC target and validate the runtime is
   portable on clean Windows 10 and Windows 11 x86-64 environments.
2. Verify manifest, long-path awareness, DPI awareness, icon/version metadata,
   mitigations, no unexpected DLL/runtime dependency, and binary size.
3. Capture original light/dark screenshots for the README.
4. Finish architecture, user, troubleshooting, contribution, and future-release
   documentation.
5. Make CI and security workflows green on the release commit.
6. Tag the stable version, create a public GitHub release, attach
   `diskpie.exe`, attach its SHA-256 file, and publish verified release notes and
   limitations.

Exit criteria: a new user can download one executable, verify its checksum, run
it without administrator privileges or an installer, complete the main product
flow, and follow public documentation. Only then mark the project complete.

## Recommended agent allocation on resume

Use subagents continuously, with non-overlapping ownership:

1. **Reviewer** - read-only revalidation of runtime/navigation and diagnostics.
2. **Native file worker** - `settings_file.rs` plus narrow platform exports and
   native tests; no UI or composition-root edits.
3. **Panic transport reviewer/worker** - pure marker contract and retained-handle
   marker adapter; no settings-policy redesign.
4. **Primary agent** - full-gate integration, commits, shell lifecycle, and UI
   composition.

Before every new nontrivial subsystem, assign a bounded research task comparing
maintenance, license, unsafe surface, platform support, size/compile impact,
benefit versus local code, and abandonment/lock-in risk. Record actionable
decisions under `docs/research` or `docs/adr`.

Workers must be told that they are not alone in the repository, own exact files,
must preserve other edits, and must not commit unless explicitly assigned.

## First 30 minutes tomorrow

1. Confirm the user wants to resume.
2. Read the six documents listed at the top.
3. Run `git status --short` and `git diff --check`.
4. Inspect the runtime, diagnostics, panic-marker, storage, and main diffs.
5. Run `cargo fmt --all -- --check` and the full no-run compile.
6. Resume the interrupted independent runtime review.
7. Update this roadmap if the actual tree differs from the frozen state.

Do not begin by deleting untracked files, resetting to the pushed commit, or
rewriting the architecture. The fastest safe continuation is to validate and
commit the substantial work already present.


## Status of the execution roadmap (2026-09-04)

The phases above are kept as the plan of record. Their state after the
integration cycle:

| Phase | State | What remains |
| --- | --- | --- |
| 1 - Stabilize and commit | Done | Nothing. The tree is committed, the gate is green locally, and CI runs the same commands. |
| 2 - Native settings and crash transport | Done | Nothing blocking. The retained-handle rename goes through `NtSetInformationFile` (ADR 0019 addendum); clean-VM checks are part of phase 8. |
| 3 - Harden native service lifecycles | Done | Nothing. `request_shutdown`, bounded `finish`, and the process-wide reaper are in place with stalled- and panicking-worker tests. |
| 4 - Connect the actual application UI | Done, with two gaps | Rescanning one branch has no engine path and is shown as unavailable; a Narrator pass and wide/narrow headless frame tests are still open. |
| 5 - Shell actions and confirmations | Adapter done, interface pending | Bind the ADR 0008 flow and the STA requests to the inspector buttons and confirmation dialogs; run the disposable Recycle Bin matrix; verify directory recycling callback shape. |
| 6 - Optional Explorer integration | Adapter done, interface pending | Add the settings toggle with status, Repair, and Remove; verify on Windows 10 and 11 machines. |
| 7 - Performance, robustness, audits | Partly done | Fixture coverage exists; the benchmark harness and report, the manual OneDrive check, and the accessibility review remain. |
| 8 - Portable release | Workflow ready, release pending | Confirm CI is green on `main`, launch the exe on clean Windows 10 and 11 machines, capture screenshots, tag `v0.1.0`, and publish the draft the workflow produces. |

Next steps, in order: bind the destructive actions and the Explorer toggle to
the interface, add the benchmark harness and report, run the clean-machine and
accessibility checks, then tag the release.
