# ADR 0008: Bind destructive actions to exact confirmed targets

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

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
