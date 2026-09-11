# ADR 0008: Bind destructive actions to exact confirmed targets

- Status: Partially superseded by the 2026-09-11 owner decisions in ROADMAP.md
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Current applicability (2026-09-11)

The [current roadmap](../../ROADMAP.md) supersedes this record's permanent-delete
and empty-bin product requirements. Cleanup is recycle-only: if Windows cannot
recycle, including oversized items or unsupported providers, the application
must refuse and explain why. It must not show or accept a permanent-delete
fallback, including a native Shell dialog offering it.

Exact native paths, target identity, review, stale-target rejection and honest
outcomes still apply to recycling. The evidence and implementation notes below
describe historical code, including dormant delete/empty-bin APIs that still
exist. Do not wire those APIs or treat their historical tests as verification
of the new end-to-end recycle-only experience. Verify native refusal behavior
with disposable fixtures before enabling recycling in the UI.


## Historical context

DiskPie exposes recycling, permanent deletion, and emptying the Recycle Bin
from a disk-usage visualization. A chart selection can become stale while a
scan, rescan, external process, or filesystem rename changes the underlying
path. Windows paths are not necessarily valid UTF-8. Shell deletion is
path-based and can partially mutate a recursive directory before reporting an
error or cancellation.

The product requirements mandate explicit confirmation showing the exact path
for recycle and delete, with visibly stronger confirmation for permanent
deletion. Tests may delete only fixtures they create. DiskPie must never
silently replace a requested recycle operation with permanent deletion.

The native behavior and provider test matrix are documented in
[Windows shell actions research](../research/windows-shell-actions.md). The STA
and callback lifecycle is accepted by [ADR 0007](0007-windows-shell-sta-service.md).

## Decision drivers

- Prevent stale, synthetic, changed, or unintended targets from reaching a
  destructive adapter.
- Make recycle, permanent deletion, and empty-bin risk visibly distinct.
- Preserve exact native paths even when they cannot be rendered as UTF-8.
- Detect identity changes as late as possible before a path-based mutation.
- Fail closed when recycling is unavailable; never silently delete forever.
- Report partial and unknown mutation honestly.
- Define cancellation boundaries without implying rollback.
- Make the safety policy independently unit-testable without invoking Windows.

## Options considered

### Pass a selected path directly from the UI

This is simple but lets stale generations, synthetic sunburst sectors, lossy
display paths, and post-confirmation target swaps reach the platform adapter.
Confirmation becomes a convention rather than an enforceable boundary.

### Use one generic confirmation dialog for all destructive actions

One dialog reduces UI code but makes permanent deletion and all-drive Recycle
Bin destruction look equivalent to reversible recycling. It also encourages a
generic boolean result and unclear cancellation wording.

### Require typed paths

Typing the path appears strong but is inaccessible, error-prone, and
impossible to perform losslessly for native UTF-16 names that are not valid
Unicode scalar sequences. It does not by itself bind the action to stable
filesystem identity.

### Typed action capabilities plus late identity validation

The application reducer can issue a single-use capability only after the
correct confirmation flow. Binding it to raw path, operation, scan generation,
and stable file identity prevents reuse and stale-target substitution. A
strong action word can reinforce irreversible operations without requiring
the user to reproduce an untypeable native name.

## Decision

DiskPie will enforce destructive confirmation through typed, single-use action
capabilities and late target revalidation.

1. `Recycle` accepts only `Confirmed<RecycleRequest>`.
   `DeletePermanently` and `EmptyRecycleBin` accept only
   `StronglyConfirmed<...>`. Platform request constructors remain private to
   the reducer/action boundary.
2. Every capability binds operation kind, exact raw native path or bin scope,
   scan generation, target kind, and `(volume identity, FILE_ID_128)` when
   available. It is short-lived and consumed once. Changing action, path,
   scope, generation, or fingerprint invalidates it.
3. Only real current scan nodes can create filesystem confirmations. Synthetic
   groups, summary rows, stale nodes, drive/volume/share roots, the running
   executable, and a directory known to contain it are rejected before UI
   confirmation.
4. Recycle confirmation shows the exact path, item kind, logical size,
   allocated size, and that a reparse entry means the link object rather than
   its target. Cancel is the default focused action.
5. Permanent deletion uses visually stronger destructive styling, the exact
   path, an irreversible warning, typed word `DELETE`, and a final explicit
   button. The invariant ASCII action word is shown adjacent to the field with
   a localized explanation.
6. Empty-bin confirmation shows the exact drive root or **all drives**, an
   advisory item/byte estimate, a warning that cancellation is unavailable
   after dispatch, typed word `EMPTY`, and a final explicit button.
7. A native path is held as `PathBuf`/`OsString`. If UTF-8 rendering is lossy,
   confirmation shows both a friendly display and an escaped lossless UTF-16
   representation. The user types the action word, not the path.
8. Immediately before recycle or permanent deletion, the Windows adapter opens
   the item itself without following a reparse point and compares its current
   volume/file ID with the capability. A missing or changed target returns
   `TargetChanged` and requires new confirmation. DiskPie acknowledges the
   residual final TOCTOU interval of a path-based Shell API.
9. Recycle uses `FOFX_RECYCLEONDELETE`, `FOFX_ADDUNDORECORD`, and
   `FOF_WANTNUKEWARNING`. It is successful only when the actual per-item
   callback succeeds and identifies a newly created Recycle Bin item.
10. A successful recycle callback with no Recycle Bin item is a
    `SafetyViolationUnexpectedPermanentDelete`; DiskPie stops subsequent work,
    surfaces the incident, and rescans. If a provider/volume class cannot prove
    fail-closed behavior in disposable tests, Recycle is disabled there rather
    than replaced with permanent deletion.
11. Permanent deletion omits all recycle/undo flags and runs only after strong
    confirmation. Empty-bin execution uses the exact confirmed drive scope;
    null scope, meaning all drives, cannot be constructed accidentally from an
    empty filesystem string.
12. Queued requests can be canceled reliably. `PreDeleteItem` provides
    cooperative between-item cancellation. Cancellation during one recursive
    item or after `SHEmptyRecycleBinW` dispatch is not promised.
13. `DeleteItem` queuing and `PerformOperations` success are not completion.
    Outcomes combine actual per-item callbacks, final callback HRESULT,
    `GetAnyOperationsAborted`, and post-operation observation. Results include
    `Completed`, `CancelledBeforeMutation`, `Partial`, `Failed`,
    `UnknownMayHaveMutated`, and the recycle safety violation.
14. DiskPie rescans the affected parent or Recycle Bin scope after every
    destructive attempt, including failure and cancellation.

## Consequences

Positive outcomes:

- A destructive platform request cannot exist merely because the UI retained
  a path string.
- Permanent and all-bin actions are visibly and mechanically stronger than
  recycling.
- Lossy display conversion cannot change the actual target.
- Common stale-scan and path-replacement errors fail before mutation.
- Recycle fallback is observable and release-gated by provider behavior.
- Partial mutation and cancellation limitations are explicit in UI and logs.
- Confirmation and result state machines can be tested without touching the
  filesystem.

Accepted tradeoffs:

- Confirmation capabilities and result aggregation add types and reducer
  state.
- Stable identity may be unavailable on some providers. DiskPie must surface
  this weaker assurance and may disable destructive actions where responsible
  validation is impossible.
- Late identity validation cannot make a path-based Shell delete atomic.
- `FOF_WANTNUKEWARNING` can add a secondary native warning in a recycle edge
  case; this is accepted as a defense against unexpected permanent deletion.
- Recursive directory deletion and empty-bin operations can be partial and are
  never presented as transactions.
- Strong typed words add a small accessibility/localization burden; accessible
  labels, focus order, keyboard operation, and screen-reader announcements are
  required.

## Validation

- Pure tests prove that a capability cannot be forged through public APIs,
  reused, or applied to a different operation, raw path, fingerprint,
  generation, target kind, or bin scope.
- Property tests cover raw UTF-16 display/escaping, embedded NUL, Unicode,
  emoji, long/deep paths, UNC roots, device namespaces, and option-like names.
- Fake-adapter tests enumerate every native result combination, including
  successful method HRESULT plus abort, missing callbacks, failed per-item
  result, null recycle item, partial operation, and cancellation before/after
  mutation.
- Windows fixture tests swap identity after confirmation, delete a
  symlink/junction to an external sentinel, exercise ACL-denied descendants,
  and prove the sentinel survives and the UI reports TargetChanged/Partial.
- Disposable VM tests cover NTFS, removable/FAT-class media, SMB,
  disabled/full/undersized Recycle Bin, oversized items, read-only files, open
  handles, and access denied. Every accepted recycle produces observable
  Recycle Bin callback evidence.
- Empty-bin tests run only in a disposable user profile with seeded fixtures,
  verify exact drive versus all-drive scope, and never execute in normal CI or
  a developer profile.
- UX/accessibility review verifies default Cancel focus, destructive contrast,
  exact-path presentation, keyboard flow, localized explanation, and screen
  reader output in English and Spanish.
- Security review confirms tests and cleanup operate only under unique
  test-owned roots/registry keys and cannot expand an unresolved variable,
  wildcard, or broad directory into a delete target.

## Revisit triggers

- A documented Windows API that performs recycle/delete against a verified
  file handle can justify a superseding design that reduces TOCTOU.
- A provider class can be enabled for Recycle only after the full disposable
  matrix proves no silent permanent fallback on supported Windows versions.
- Multi-selection requires a new UX/result ADR or amendment preserving
  per-target confirmation identity, partial results, bounded requests, and
  stop-on-safety-violation behavior.
- Any request to weaken typed confirmation, allow synthetic targets, or treat
  partial mutation as success requires explicit security review and a
  superseding ADR.

## Implementation note 2026-09-04: capability flow and native actions

The application half of this ADR lives in `crates/diskpie-app/src/actions.rs`
and the native half in `crates/diskpie-platform/src/windows/shell_actions.rs`,
dispatched by the existing Shell STA in `shell_service.rs`.

Application (`diskpie_app::actions`):

- `TargetValidator` resolves a `NodeId` sealed with its `GenerationId` against
  the current `TreeSnapshot`. It rejects stale generations, unknown and
  synthetic nodes, caller-declared hidden nodes, empty paths, drive/volume/
  share roots (item 3), the running executable, and any directory that
  contains it. Two implementation decisions go slightly beyond the list in
  item 3 and fail closed: the scan root itself is refused as a destructive
  target because it has no in-snapshot parent to rescan (item 14), and a
  destructive validation without a supplied executable path is refused rather
  than skipping self-protection. Open and reveal validation accept the scan
  root and do not require the executable path. Because those checks depend on
  the purpose, `FilesystemTarget` records the `ActionPurpose` it was validated
  for, and `ConfirmationFlow::begin_recycle`/`begin_delete` refuse any target
  that was not validated for `Destructive`
  (`FlowError::NotValidatedForDestruction`); an open- or reveal-validated
  executable, its directory, the scan root, or a drive root therefore cannot
  become a capability through the public API (amendment 2026-09-04).
- `ConfirmationFlow` is the pure state machine: `Idle -> Reviewing ->
  Confirmed` for recycling and `Idle -> Reviewing -> AwaitingWord ->
  StronglyConfirmed` for permanent deletion and emptying a bin. The words are
  the invariant ASCII `DELETE` and `EMPTY`, compared exactly with no case
  folding or whitespace tolerance. `cancel` is available in every state.
- `Confirmed<RecycleRequest>`, `StronglyConfirmed<DeleteRequest>`, and
  `StronglyConfirmed<EmptyRecycleBinRequest>` have private constructors, are
  neither `Clone` nor `Copy`, and bind kind, exact `PathBuf` or
  `RecycleBinScope`, generation, `TargetKind`, and `Option<FileIdentity>`.
  `ActionBinding::verify_target`/`verify_scope` name the first mismatching
  field. `consume` yields the plain request plus a `PendingObligation`
  (`RescanParent`, `RescanAll`, or `RescanRecycleBin`) that the runtime settles
  into an `ActionReport`; `requires_halt` is true only for the safety
  violation.
- `RecycleBinScope::AllDrives` is the only value that maps to a null native
  root; `DriveRoot::parse` accepts exactly `X:\` and rejects the empty string.
- `TargetPresentation` carries the exact path, the friendly display, and an
  `escaped_utf16` form produced by `escape_utf16` only when UTF-8 rendering is
  lossy. The escape is lossless and round-trips unpaired surrogates through
  `unescape_utf16`.
- `classify_delete_evidence` implements item 13 over plain `DeleteEvidence`
  and is exercised by fake-adapter tests for every outcome combination,
  including success plus abort, missing callbacks, per-item failure, null
  recycle item, and cancellation before and after mutation.
- `DeleteEvidence::observation` carries the post-operation observation item 13
  requires. Copy-engine success codes are not uniformly "removed":
  `COPYENGINE_S_USER_IGNORED` (`0x00270005`) means skipped, and
  `COPYENGINE_S_DONT_PROCESS_CHILDREN` (`0x00270008`) was observed for a plain
  file delete. The adapter therefore re-opens the exact path (no reparse
  following) after the operation; a path that still holds the same identity
  (or, without a bound identity, the same kind) downgrades an otherwise
  complete permanent file/link delete to `Failed` and every other case to
  `UnknownMayHaveMutated` at stage `PostOperationObservation`. The observation
  never upgrades an outcome (amendment 2026-09-04).

Platform (`diskpie_platform`):

- `ShellRequest` gained `Open`, `Reveal`, `InstalledApps`, `QueryRecycleBin`,
  `Recycle`, `DeletePermanently`, and `EmptyRecycleBin`. The destructive
  variants are only constructible from a consumed capability
  (`ShellRequest::recycle`, `delete_permanently`, `empty_recycle_bin`) and the
  request type is deliberately not `Clone`. Every request ends in exactly one
  `ShellEvent`; `ShellService::cancel(id)` flips a request-local flag that skips
  a queued request and is read cooperatively in `PreDeleteItem`.
- Open uses `SHCreateItemFromParsingName`, `SHGetIDListFromObject`, and
  `ShellExecuteExW` with verb `open`, `SEE_MASK_IDLIST | SEE_MASK_NOASYNC |
  SEE_MASK_FLAG_NO_UI`, and the owner HWND. Reveal uses `SHParseDisplayName`
  and `SHOpenFolderAndSelectItems`. Installed Apps activates only
  `ms-settings:appsfeatures` and reports failure with no `appwiz.cpl`
  fallback. The raw calls sit behind an internal `ShellPrimitives` seam so
  marshalling is tested without launching Explorer or Settings.
- Recycle/delete run one `IFileOperation` per request with `SetOwnerWindow`,
  the explicit flag profile (`FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT
  | FOF_NO_CONNECTED_ELEMENTS | FOFX_EARLYFAILURE`, plus
  `FOFX_RECYCLEONDELETE | FOFX_ADDUNDORECORD | FOF_WANTNUKEWARNING` for
  recycling only), and a `#[implement]`ed `IFileOperationProgressSink` that
  records `StartOperations`, `PreDeleteItem`, `PostDeleteItem` (`hrDelete` and
  whether `psiNewlyCreated` was non-null), and `FinishOperations`. The advise
  cookie is an RAII guard. `GetAnyOperationsAborted` is always queried.
- Before validation the adapter refuses, as defence in depth, any request
  whose target kind is the scan root or whose path is a drive, volume, or share
  root (`Failed` at stage `ForbiddenTarget`), and a delete payload re-wrapped
  in the other destructive `ShellRequest` variant (`Failed` at stage
  `RequestMismatch`); neither reaches a file operation.
- Late validation (item 8) opens the exact path with zero access, full
  sharing, `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS`, checks
  the observed kind against the bound `TargetKind`, compares `FILE_ID_INFO`
  with the bound identity, and closes the handle before `DeleteItem`. A missing
  item (`ERROR_FILE_NOT_FOUND`/`ERROR_PATH_NOT_FOUND` only; an un-openable name
  is a validation failure, not a changed target) or changed kind/identity is
  `TargetChanged`. The scanner records no
  identity for directories and reparse entries, so those validations report
  `IdentityAssurance::KindOnly` in the outcome instead of `Exact`; this is the
  weaker assurance the ADR requires to be surfaced.
- Empty bin queries `SHQueryRecycleBinW` before and after
  `SHEmptyRecycleBinW(owner, root_or_null, SHERB_NOCONFIRMATION |
  SHERB_NOPROGRESSUI | SHERB_NOSOUND)`; an error with an unchanged estimate is
  `Failed`, any other error is `UnknownMayHaveMutated`.
- `#[implement]` expands to absolute `::windows_core::` paths, so
  `diskpie-platform` now names `windows-core 0.62.2` directly with the same
  feature set `windows` already resolves; the lock file gained only that edge.

Tests that run by default never mutate anything: they validate identity on
temporary fixtures, prove a swapped target yields `TargetChanged` through the
live STA with the fixture intact (submitted in permanent mode on purpose, so a
regression of late validation could only destroy the test's own fixture and
never add an item to the developer's Recycle Bin), exercise pre-cancelled and
re-wrapped requests, refuse scan/drive roots, observe gone/present/swapped
paths, and map terminal events for every request kind. Fixture-mutating tests
(recycle a temporary file, recycle a temporary directory tree, permanently
delete a file, delete a directory symlink whose sentinel must survive) run
only with `DISKPIE_DESTRUCTIVE_FIXTURES=1` and act only on files they created
under the temporary directory. On the development machine the
permanent-delete and symlink-sentinel tests passed (`hrDelete` was
`COPYENGINE_S_DONT_PROCESS_CHILDREN`, a success code other than `S_OK`); the
recycle tests were left for an explicit opt-in run because they add items to
the user's real Recycle Bin. Emptying a bin is never tested against a real bin.

Open question (2026-09-04): it is not yet verified whether the copy engine
reports a recycled directory as a single `PostDeleteItem` carrying the new
Recycle Bin item or as one callback per descendant. If descendants report
success with a null `psiNewlyCreated`, the classifier raises
`SafetyViolationUnexpectedPermanentDelete` for a folder recycle: fail-closed
(halt plus incident) but a false alarm. The gated directory-recycle fixture is
the test that settles it; until it has been run in a disposable profile,
directory recycling must not be enabled in the UI.

Still open: the egui binding of the flow and result presentation, the
disposable-VM provider matrix that gates enabling Recycle per provider class
(item 10), the directory-recycle callback shape above, and ACL-denied
descendant fixtures for the `Partial` path.
