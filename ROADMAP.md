# DiskPie roadmap and continuation handoff

> Paused on 2026-08-02 at the user's request. This document is the operational
> handoff for the next maintainer or Codex agent. It describes the dirty working
> tree as it exists now; it is not a claim that the product is complete.

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

## Frozen repository state

| Item | Value |
| --- | --- |
| Local repository | `C:\Users\Luis\Desktop\scan\diskpie` |
| Public repository | <https://github.com/Luexi/diskpie> |
| Working branch | `feat/initial-release` |
| Last pushed commit | `a1ba3d0dc5a35cce5c515eb574f10ac7f55d0fcd` |
| Draft pull request | <https://github.com/Luexi/diskpie/pull/1> |
| Last green pushed CI | <https://github.com/Luexi/diskpie/actions/runs/30775009484> and <https://github.com/Luexi/diskpie/actions/runs/30775009490> |
| Stable Rust baseline | Rust 1.92, edition 2024 |
| Product version | `0.1.0` development version |

The working tree is intentionally dirty and contains substantial uncommitted
work. Do not reset, checkout, clean, or broadly reformat it. Existing changes
belong to the current implementation effort and must be reviewed in place.

Uncommitted areas include:

- application runtime, reducer, navigation, and layout-service ownership;
- typed diagnostics, local diagnostic lifecycle, and Windows diagnostic files;
- settings-policy and explicit native-settings design work;
- local panic-marker contract work that is not yet wired into the executable;
- eframe composition-root and shell scaffolding;
- ADR and research updates; and
- the UI product-direction document.

Run `git status --short` and inspect each diff before editing. There has been no
commit after `a1ba3d0`; the remote CI proves only that pushed commit, not the
current working tree.

## Current delivery estimate

Approximately 40-45% of the literal release contract is implemented or has
strong underlying infrastructure. This is an orientation estimate, not an
acceptance metric. The remaining 55-60% is concentrated in product integration,
native actions, end-to-end verification, packaging, and release evidence.

### Status legend

- **Accepted**: independently reviewed with no blocking/high finding.
- **Implemented, revalidation pending**: code and focused tests exist, but the
  last independent review or full-workspace gate was interrupted by the pause.
- **Contract only**: policy or pure logic exists, but the native adapter or
  composition-root integration does not.
- **Not implemented**: required product behavior is absent.

## What exists today

| Area | State | Evidence and caveats |
| --- | --- | --- |
| Compact tree, exact metrics, aggregation | Accepted baseline | `diskpie-core`; unit and property coverage already exists. |
| Portable bounded scanner | Accepted baseline | `diskpie-scan`; fixed workers, bounded channels, generation cancellation, partial batches, omissions. |
| Windows conventional filesystem adapter | Accepted baseline | Long/native paths, allocated-size metadata, file identity, reparse/cloud boundaries. |
| Windows volumes and selected-root resolution | Accepted baseline | Volume discovery, stable keys, device hints, capacity, DOS/UNC resolution. |
| Sunburst layout and hit testing | Accepted baseline | UI-independent layout plus egui mesh/hit-test adapter. |
| Presentation, navigation, hide/restore, rescan intent | Implemented, revalidation pending | Latest focused runtime/navigation stress checks passed; final independent review was interrupted. |
| Runtime orchestration | Implemented, revalidation pending | Bounded scans, progressive snapshots, off-thread layout, cancellation/replacement, retirement reaper. |
| Folder picker | Implemented | Direct `IFileOpenDialog` on one message-pumping Shell STA. Its shutdown path still needs hardening. |
| CLI initial path | Implemented at parser boundary | Path-preserving parser exists; it is not yet connected to a real UI scan. |
| Settings schema/policy | Implemented | Versioned, bounded RON policy exists. Native Windows retained-handle file transport is absent. |
| Typed diagnostics and local log lifecycle | Accepted | Independent review accepted the latest bounded/nonblocking lifecycle. Full workspace rerun still required. |
| Panic marker | Contract only | Pure contract and standalone tests exist; module is not declared or integrated, and native retained-handle transport is absent. |
| English and Spanish infrastructure | Partially implemented | Typed Fluent IDs and both bundles exist; final connected UI strings and completeness checks remain. |
| Visual shell | Scaffold only | It renders an original responsive instrument shell, but still shows calibration/demo data and disabled actions. |
| Open/recycle/delete/empty recycle bin/Installed Apps | Implemented below the UI | ADR 0008 confirmation flow, single-use capabilities, STA requests/outcomes, late identity validation, and cancellation exist; the egui binding and the disposable-VM provider matrix remain. |
| Explorer integration | Partially implemented | Portable policy, HKCU adapter with unique test roots, and the `--scan-path` verb argument exist; the settings UI remains. |
| Benchmarks and release evidence | Not implemented as a complete gate | Some lower-level tests exist; required benchmark suite/report and portable binary measurements remain. |
| Stable GitHub release | Not implemented | Repository and draft PR exist; no release or portable asset exists. |

## Last known verification evidence

Do not combine these results into a claim that the current tree is fully green:

- A prior full-workspace run, before the newest runtime/diagnostic changes,
  passed 264 unit/integration tests plus one doc test.
- The latest runtime/navigation remediation passed seven focused tests, twenty
  repeated ceiling/stall stress cycles, strict all-target Clippy, and a format
  check for `diskpie-app`.
- The diagnostic lifecycle passed fourteen focused tests, crate checks, strict
  Clippy, and formatting; an independent reviewer returned **ACCEPT**.
- The panic-marker file passed a standalone test harness: 22 tests passed and
  four subprocess helper tests were intentionally ignored by direct execution.
  This does not prove executable integration.
- The current combined dirty tree was **not** rerun through the full workspace
  gate after the pause. That is the first resumption task.

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

