# ADR 0007: Isolate Windows Shell actions in one STA service

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must open files and folders, reveal items in Explorer, recycle and
permanently delete items, empty the Recycle Bin, and launch modern Installed
Apps settings. These APIs involve Shell extensions and COM. Some extensions
require a single-threaded apartment (STA), and an STA must pump messages.

The egui thread cannot wait for native I/O or own long-running destructive
operations. COM interface pointers cannot be moved casually between
apartments. Destructive calls require richer outcomes than a boolean: queuing
an `IFileOperation` item is not deletion, `PerformOperations` can succeed when
an operation was aborted, and progress callbacks contain the actual per-item
result.

Supporting API research, dependency analysis, and test plans are recorded in
[Windows shell actions research](../research/windows-shell-actions.md).

## Decision drivers

- Correct COM apartment ownership and message pumping.
- A responsive egui renderer with bounded, nonblocking submission.
- Exact Unicode/native-path handling and explicit Shell long-path failures.
- Complete cancellation, HRESULT, abort, and per-item result reporting.
- Standard-user operation without DiskPie elevation prompts.
- Small, reviewable unsafe and platform boundaries.
- No command-interpreter construction or generic helper behavior that changes
  paths silently.
- Minimal duplicate binding, compile-time, and supply-chain cost.

## Options considered

### Run Shell calls on the egui thread

This makes HWND ownership easy but allows synchronous Shell activation,
`IFileOperation::PerformOperations`, extension loading, and Recycle Bin calls
to freeze input and rendering. It also mixes COM lifecycle and unsafe Windows
types into presentation code.

### Spawn an arbitrary worker or MTA per action

Per-action threads avoid UI blocking but do not satisfy extensions that expect
an STA. They complicate interface ownership, cancellation, shutdown, and
serialization of destructive operations. A thread that initializes COM but
does not pump messages remains an invalid STA design.

### Use generic `open` and trash crates

The evaluated helpers reduce a few calls but use subprocesses, path
normalization/prefix stripping, old duplicate bindings, coarse outcomes, or
different apartment policies. None supplies DiskPie's combined exact-path,
owner-HWND, confirmation, callback, cancellation, and fail-closed result
contract.

### Dedicated message-pumping STA with direct `windows` APIs

One target-gated service can initialize COM exactly once, own all Shell
objects, serialize destructive work, integrate a bounded request queue with a
Windows message-aware wait, and return plain portable outcomes. Direct
Microsoft projections support `IFileOperationProgressSink` without handwritten
COM vtables.

## Decision

DiskPie will use one dedicated `ShellActionService` STA on Windows.

1. The service calls `CoInitializeEx(NULL, COINIT_APARTMENTTHREADED |
   COINIT_DISABLE_OLE1DDE)`. `RPC_E_CHANGED_MODE` is fatal to service startup;
   `S_OK` and `S_FALSE` are balanced by `CoUninitialize` on the service thread.
2. The service combines a bounded request channel with a Windows event or
   message-only window and a message-aware wait. It continuously retrieves and
   dispatches STA messages and never waits indefinitely in a raw Rust blocking
   receive.
3. All `IShellItem`, PIDL, `IFileOperation`, progress-sink, and related COM
   lifetimes stay on that thread. No COM pointer crosses into egui, scan
   workers, or portable crates.
4. The UI submits requests without blocking and receives plain-data events.
   Queue saturation is an explicit `QueueError`, not an unbounded allocation.
5. Open creates an `IShellItem` with `SHCreateItemFromParsingName`, obtains a
   PIDL with `SHGetIDListFromObject`, and invokes fixed verb `open` through
   `ShellExecuteExW`. Reveal uses `SHOpenFolderAndSelectItems`. PIDLs are freed
   with `CoTaskMemFree`.
6. Installed Apps dispatches only the trusted fixed URI
   `ms-settings:appsfeatures`. Filesystem requests cannot carry arbitrary URIs
   or verbs.
7. Destructive actions use one `IFileOperation` per top-level target, set the
   owner HWND and explicit flags, and install one progress sink with an RAII
   `Advise`/`Unadvise` cookie.
8. A request-local atomic cancellation flag is visible while
   `PerformOperations` blocks. `PreDeleteItem` provides cooperative
   between-item cancellation. The API does not promise interruption within
   every recursive item.
9. Outcomes combine `PerformOperations`, `GetAnyOperationsAborted`, every
   `PostDeleteItem`, and `FinishOperations`. Open/reveal/Settings success is
   called `Dispatched`, not `Completed`.
10. Use `windows` 0.62.2 with only required features. Use direct native APIs
    rather than `trash`, `open`, `opener`, or `showfile`. The portable-facing
    `PlatformActions` trait contains Windows lock-in.
11. Project-authored unsafe remains in target-gated Windows adapter modules,
    with minimal blocks, RAII ownership, explicit `SAFETY` invariants, and raw
    OS error preservation.
12. DiskPie uses an `asInvoker`, `uiAccess=false` manifest and never requests
    elevation for Shell mutations. Protected targets fail normally.

## Consequences

Positive outcomes:

- COM and Shell extension behavior has one auditable apartment and lifetime.
- The renderer stays responsive and cannot accidentally perform destructive
  work during an egui callback.
- Cancellation and partial mutation are explicit rather than misreported as a
  transaction.
- Arbitrary native paths do not pass through PowerShell, `cmd.exe`, or
  Explorer command-line parsing.
- Direct per-item callbacks support the stronger destructive contract in ADR
  0008.
- A consumer-owned trait preserves a portable core and permits deterministic
  fake-adapter tests.

Accepted tradeoffs:

- The service requires message-aware waiting, careful shutdown, reentrancy
  tests, and more implementation than a helper crate.
- `PerformOperations` and `SHEmptyRecycleBinW` remain synchronous and can
  occupy the STA. Cancellation inside one recursive operation is limited by
  native callback behavior.
- Shell parsing can reject a path that the filesystem scanner can enumerate.
  DiskPie reports `ShellPathUnsupported`; it does not rewrite the path or use a
  lossy legacy fallback.
- Opening a user-selected executable can activate that executable's own UAC
  request, although DiskPie itself remains unelevated.
- `windows` generated bindings add compile cost; feature selection and binary
  measurement are release requirements.

## Validation

- Unit tests cover service startup failure, bounded-queue saturation, queued
  cancellation, shutdown, missing/contradictory callbacks, HRESULT mapping,
  and `S_OK` plus aborted operation state.
- Windows tests assert every COM object is created, invoked, and released on
  the STA and that `CoUninitialize`/`Unadvise` occur on every exit path.
- Reentrancy and responsiveness tests exercise the message pump while
  requests, cancellation, and Shell callbacks are active.
- Open/reveal tests use Unicode, emoji, option-like names, spaces, `%`, `&`,
  long paths, roots, missing items, and absent file associations. They verify
  no command-interpreter process is constructed.
- Standard-user/UAC VM tests verify the manifest and prove protected mutation
  returns access denied without a DiskPie elevation prompt.
- CI runs portable-crate tests without Windows APIs; Windows 10/11 x86-64 jobs
  run target-gated integration tests that do not mutate user data.
- The release build records clean/incremental build time and EXE size for the
  exact windows-rs feature set.

## Revisit triggers

- Replace the event/message integration mechanism only if reentrancy or
  shutdown tests demonstrate a defect; retain one message-pumping STA.
- Reconsider `windows-sys` only after a whole-adapter prototype demonstrates a
  material measured size/build win and passes an additional unsafe/COM review.
- Add a helper crate only if it supports the complete native-path,
  apartment/lifetime, cancellation, owner-window, and structured-result
  contract with no duplicate binding graph.
- Supersede this ADR if Windows removes or materially changes the documented
  Shell/COM contracts on a required OS version.

## Decision note 2026-09-03: bounded shutdown and singleton retirement

The original `Drop` joined the STA worker without a deadline, so a visible
modal folder dialog or a stalled Shell call could block process exit
indefinitely. The lifecycle now has three explicit stages:

1. `ShellService::request_shutdown` is nonblocking and idempotent. It moves the
   lifecycle to `Stopping`, sets the shutdown flag, and signals the wake event;
   the message-aware wait's bounded health timeout guarantees the loop observes
   the flag even if signalling fails.
2. `ShellService::finish(timeout)` is the composition root's explicit
   completion point, called outside every UI callback. It polls join-readiness
   with bounded sleeps and returns `ShellServiceFinish::Exited { panicked }`,
   `TimedOutHandedToReaper`, or `TimedOutReaperUnavailable`.
3. `Drop` performs atomic/`Arc` work only. A worker that is still running is
   handed, together with its singleton claim, to one process-wide reaper
   through a nonblocking `try_send`; it never joins.

The reaper is a single thread created at most once, precreated during
`ShellService::start` so a creation failure surfaces as
`ShellServiceStartError::ReaperUnavailable` instead of at drop. Its queue is a
fixed `sync_channel` of `SHELL_REAPER_CAPACITY` (4) entries. Each entry owns
the `JoinHandle` and the `ServiceInstanceClaim`; the reaper joins first and
releases the claim afterwards, so a second Shell STA cannot start while the
old one is still alive. If the queue is full or the reaper thread is gone the
lifecycle fails closed: the claim is retained for the rest of the process,
`start` keeps returning `AlreadyRunning`, and the detached worker exits on its
own. There is no per-drop thread, no unbounded queue, and no `mem::forget`.

The legacy blocking `shutdown` remains for callers that need a synchronous
join and repeats a recorded worker panic on every later call. Validation uses
injected worker behaviours (stalled for a fixed duration, or panicking after
the shutdown request) on private singleton registries so the tests prove
bounded `finish`, bounded `Drop`, single ownership, restart only after the
real exit, and the fail-closed outcome without showing any UI.

## Decision note 2026-09-04: reveal parses the display name

Item 5 describes obtaining the PIDL through `SHGetIDListFromObject` on an
`IShellItem`. The implementation keeps that route for Open, where the item is
needed anyway, but Reveal parses the exact path with `SHParseDisplayName`
(no bind context, no attribute request) and hands the resulting absolute PIDL
straight to `SHOpenFolderAndSelectItems`. Both routes address the item by its
exact native path rather than a display string, and both PIDLs are freed with
`CoTaskMemFree`; the marshalling of each is covered by tests behind the
internal `ShellPrimitives` seam without launching Explorer.
