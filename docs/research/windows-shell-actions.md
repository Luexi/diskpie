# Windows shell actions research

- Research date: 2026-08-02
- Target: Windows 10/11 x86-64, portable executable, standard user
- Scope: open/reveal, recycle and permanent deletion, empty Recycle Bin,
  Installed Apps, cancellation/results, and reversible Explorer integration
- Outcome: use `windows` 0.62.2 directly from one message-pumping STA service;
  use `windows-registry` 0.6.1 narrowly for per-user static Explorer verbs.

## Recommendation

Place every Shell/COM operation behind a consumer-owned `PlatformActions`
interface in `diskpie-platform`. A single dedicated single-threaded apartment
(STA) serializes requests, pumps Windows messages, and owns all COM interface
pointers and PIDLs. The UI submits bounded requests and receives plain-data
events; it never waits for Shell I/O.

Use `SHCreateItemFromParsingName`, `SHGetIDListFromObject`,
`ShellExecuteExW`, and `SHOpenFolderAndSelectItems` for open/reveal actions.
Use `IFileOperation` with an `IFileOperationProgressSink` for recycle and
permanent deletion. Use `SHQueryRecycleBinW` and `SHEmptyRecycleBinW` for the
Recycle Bin. Launch only the fixed `ms-settings:appsfeatures` URI for modern
Installed Apps settings.

Register optional classic Explorer verbs under `HKCU\Software\Classes` with
an ownership marker and exact command. This remains a classic context-menu
integration: on Windows 11 it is normally reached through **Show more
options**. A modern top-level Windows 11 command requires package identity and
a native `IExplorerCommand` COM DLL, which is incompatible with the portable
single-EXE distribution boundary and is deferred.

Do not add `trash`, `open`, `opener`, `showfile`, `winreg`, `registry`, or
`winsafe` to the initial implementation. The direct APIs provide materially
stronger path fidelity, COM ownership, confirmation, cancellation, and result
reporting without duplicate Windows binding families.

This research supports
[ADR 0007](../adr/0007-windows-shell-sta-service.md),
[ADR 0008](../adr/0008-destructive-confirmation-contract.md), and
[ADR 0009](../adr/0009-explorer-integration-boundary.md).

## Constraints and trust boundary

- DiskPie normally runs as a standard user and has an `asInvoker`,
  `uiAccess=false` manifest.
- Destructive actions are explicit, exact-path operations against real scan
  nodes. Synthetic chart groups, summary nodes, and stale scan generations are
  never valid targets.
- Recycle and permanent deletion always require confirmation. Permanent
  deletion and emptying the Recycle Bin use stronger confirmation than
  recycling.
- Project-authored unsafe remains confined to reviewed, target-gated Windows
  adapter modules. Portable application and domain crates see no Win32, COM,
  HWND, PIDL, or HRESULT types.
- Paths remain `PathBuf`/`OsString` and raw UTF-16 at the adapter. Lossy UTF-8
  is display-only and never a target identity.
- No path or argument is interpolated into `cmd.exe`, PowerShell, or another
  command interpreter.
- Tests may delete only uniquely named fixtures they created. Recycle-bin
  destruction tests run only in a disposable VM/Sandbox user profile.

## STA service design

### Lifetime and message pumping

Start one dedicated `ShellActionService` thread and call:

```text
CoInitializeEx(NULL, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
```

Treat `RPC_E_CHANGED_MODE` as a startup error. Both `S_OK` and `S_FALSE`
establish an initialization that must be balanced by `CoUninitialize` on the
same thread. Do not initialize and uninitialize COM per request.

An STA must retrieve and dispatch window messages. A practical implementation
uses a bounded request channel plus a Win32 event and waits with
`MsgWaitForMultipleObjectsEx`; it drains queued requests when the event is
signaled and pumps messages when the queue reports input. A message-only
window plus `PostMessage` is another valid implementation. Do not block the
STA indefinitely in `recv`, a condition variable, or a thread join without
message-aware waiting.

`PerformOperations` is synchronous and can occupy the service thread. A
request-local atomic cancellation flag must therefore remain visible to the
progress sink while the call is in progress. The sink can stop before an item
by returning `HRESULT_FROM_WIN32(ERROR_CANCELLED)` from `PreDeleteItem`.
Cancellation during one recursive top-level item is not guaranteed to be
prompt because the Shell controls callback granularity.

Create, use, release, and drop every `IShellItem`, `IFileOperation`, progress
sink, and PIDL on this STA. Do not pass COM pointers to the egui thread or a
worker. This avoids accidental cross-apartment use and explicit marshaling.

Microsoft requires COM initialization for `SHOpenFolderAndSelectItems` and
recommends an STA for `ShellExecuteEx` because Shell extensions can activate
COM objects. See
[`CoInitializeEx`](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-coinitializeex),
[Single-Threaded Apartments](https://learn.microsoft.com/en-us/windows/win32/com/single-threaded-apartments),
[Processes, Threads, and Apartments](https://learn.microsoft.com/en-us/windows/win32/com/processes--threads--and-apartments),
and
[`ShellExecuteExW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw).

### Proposed portable-facing API

The exact module layout may evolve, but the boundary should preserve these
properties:

```rust
trait PlatformActions {
    fn submit(&self, request: ActionRequest) -> Result<ActionId, QueueError>;
    fn cancel(&self, id: ActionId);
    fn try_recv(&self) -> Option<ActionEvent>;
}

enum ActionRequest {
    Open(FilesystemTarget),
    Reveal(FilesystemTarget),
    Recycle(Confirmed<RecycleRequest>),
    DeletePermanently(StronglyConfirmed<DeleteRequest>),
    EmptyRecycleBin(StronglyConfirmed<EmptyBinRequest>),
    OpenInstalledApps,
    SetExplorerIntegration(Confirmed<IntegrationRequest>),
}

struct FilesystemTarget {
    path: PathBuf,
    scan_generation: u64,
    kind: TargetKind,
    fingerprint: Option<(VolumeKey, FileId128)>,
}
```

The queue is bounded and `submit` does not block the UI. Destructive requests
cannot be constructed without a confirmation capability created by the
application reducer. A capability is single-use and short-lived and binds the
action kind, raw native path, scan generation, and stable fingerprint. It is a
code invariant, not a cryptographic authorization token.

### Outcome contract

Do not reduce all native results to `bool` or assume that a successful method
call means that a mutation completed:

```rust
enum ActionOutcome {
    Dispatched { api: &'static str },
    Completed { items: Vec<ItemResult> },
    CancelledBeforeMutation,
    Partial { committed: usize, failed: usize, aborted: bool },
    Failed { stage: ActionStage, code: i32 },
    UnknownMayHaveMutated { stage: ActionStage, code: i32 },
    SafetyViolationUnexpectedPermanentDelete,
}

struct ItemResult {
    path: PathBuf,
    code: i32,
    effect: ItemEffect,
}

enum ItemEffect {
    Recycled { receipt: RecycleReceipt },
    PermanentlyDeleted,
    Unchanged,
    Unknown,
}

struct RecycleReceipt {
    display_name: Option<String>,
}
```

Preserve raw HRESULT or Win32 values for diagnostics and attach a formatted
message for the UI. Open, reveal, and Settings return `Dispatched`; success
only means Windows accepted the activation. After any destructive attempt,
rescan the parent even when the reported result is success.

## Open, reveal, and Installed Apps

### Open

1. Validate an absolute filesystem target from the current scan generation.
2. Create `IShellItem` with
   [`SHCreateItemFromParsingName`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-shcreateitemfromparsingname).
3. Obtain its absolute PIDL through
   [`SHGetIDListFromObject`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-shgetidlistfromobject).
4. Call `ShellExecuteExW` on the STA with the fixed verb `open`, the owner HWND,
   and `SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI`.
5. Free the returned PIDL with `CoTaskMemFree`.

Never accept a caller-provided verb, pass a filesystem path as an arbitrary
URI, or construct an Explorer/command-interpreter command line. Opening a
selected executable, script, shortcut, or associated document can execute
code and can trigger that target's own UAC behavior. The UI must label this as
an explicit Open action and never invoke it from hover, selection, startup
restoration, or automatic rescan.

### Reveal in Explorer

Create the Shell item and PIDL as above, then call:

```text
SHOpenFolderAndSelectItems(item_pidl, 0, NULL, 0)
```

Microsoft documents that when `cidl` is zero, the fully specified PIDL names
one item; Explorer opens its parent and selects it. A volume or share root can
fall back to opening that root. Do not use `explorer.exe /select,` because
command-line parsing, quoting, and option-like names create avoidable
ambiguity. See
[`SHOpenFolderAndSelectItems`](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shopenfolderandselectitems).

### Modern Installed Apps settings

Dispatch only the fixed Microsoft Settings URI:

```text
ms-settings:appsfeatures
```

Use the same STA `ShellExecuteExW` path. This request has a distinct trusted
URI type rather than sharing the filesystem-target constructor. Report launch
failure and do not silently substitute legacy `appwiz.cpl`. Microsoft lists
the URI in
[Launch the Windows Settings app](https://learn.microsoft.com/en-us/windows/apps/develop/launch/launch-settings).

## `IFileOperation` lifecycle

Create one `IFileOperation` per top-level destructive request. Initially do
not combine multiple user selections into one transaction-like operation.

1. `CoCreateInstance` the File Operation object on the STA.
2. Call
   [`SetOwnerWindow`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-setownerwindow)
   with DiskPie's HWND.
3. Call `SetOperationFlags` explicitly. If it is omitted, Windows defaults to
   `FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR`, which is not an adequate explicit
   contract.
4. Create one `IFileOperationProgressSink`, call
   [`Advise`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-advise),
   and retain its cookie in an RAII guard.
5. `DeleteItem` queues the operation; it does not perform deletion.
6. Call `PerformOperations`.
7. Call `GetAnyOperationsAborted` even when `PerformOperations` succeeded.
8. Record `PostDeleteItem` for actual per-item results and
   `FinishOperations` for the final result.
9. Always
   [`Unadvise`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-unadvise)
   before releasing the operation.

The relevant contracts are
[`IFileOperation`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-ifileoperation),
[`DeleteItem`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-deleteitem),
[`PerformOperations`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-performoperations),
[`GetAnyOperationsAborted`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-getanyoperationsaborted),
and
[`IFileOperationProgressSink`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-ifileoperationprogresssink).

### Operation flags

Use these common flags for app-owned progress and error UI:

| Flag | Reason |
| --- | --- |
| `FOF_NOCONFIRMATION` | DiskPie has already performed the required application confirmation. |
| `FOF_NOERRORUI` | Native errors are returned as structured results rather than modal Shell dialogs. |
| `FOF_SILENT` | Suppress Shell progress UI; DiskPie owns progress state. |
| `FOF_NO_CONNECTED_ELEMENTS` | Operate only on the specified item, not connected HTML/folder groups. |
| `FOFX_EARLYFAILURE` | With `FOF_NOERRORUI`, stop the operation set at the first error. |

Do not set:

- `FOFX_SHOWELEVATIONPROMPT` or `FOFX_REQUIREELEVATION`; protected targets
  fail without asking DiskPie to elevate;
- `FOFX_NOSKIPJUNCTIONS`; Windows documents that Shell namespace junctions
  are skipped by default; or
- `FOFX_NOCOPYHOOKS` by default; system or enterprise copy hooks may enforce a
  meaningful veto.

The authoritative flag descriptions are in
[`IFileOperation::SetOperationFlags`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-setoperationflags).

### Recycle

Add:

```text
FOFX_RECYCLEONDELETE
FOFX_ADDUNDORECORD
FOF_WANTNUKEWARNING
```

`FOFX_ADDUNDORECORD` is the Windows 8+ preferred user-invoked undo flag.
`FOF_WANTNUKEWARNING` intentionally permits a last-resort Shell warning if an
item would be destroyed instead of recycled; Microsoft documents that it
partially overrides `FOF_NOCONFIRMATION`.

`PostDeleteItem.hrDelete` is the actual delete result, unlike the queuing
result from `DeleteItem`. `psiNewlyCreated` points to the item in the Recycle
Bin and is null when the item was fully deleted. See
[`PostDeleteItem`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-postdeleteitem).

There is no separate documented API that proves in advance that a given item
will be recyclable. Lower-level transfer flags describe
`TSF_DELETE_RECYCLE_IF_POSSIBLE`, so DiskPie must not assume every provider or
volume supports the same semantics. Instrument `PreDeleteItem`/`PostDeleteItem`
flags in the Windows test matrix. An observed recycle transfer flag may be
used as extra fail-closed hardening after behavior is demonstrated, but an
undocumented mapping between flag families is not the sole safety guarantee.
See
[`TRANSFER_SOURCE_FLAGS`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-_transfer_source_flags).

The release gate is:

- use `FOFX_RECYCLEONDELETE` and the nuke warning;
- process one top-level target at a time;
- classify success only when `hrDelete` succeeded and `psiNewlyCreated` is
  non-null;
- classify a successful recycle request with null `psiNewlyCreated` as
  `SafetyViolationUnexpectedPermanentDelete`, stop subsequent work, and
  rescan; and
- disable Recycle for a provider/volume class where disposable-VM tests cannot
  establish fail-closed behavior. Never silently substitute permanent delete.

### Permanent deletion

Permanent deletion uses only the common flags after a strongly confirmed
request; it omits all recycle, undo, and nuke-warning flags. Revalidate target
identity immediately before queuing. A recursively deleted directory can be
partially modified before a child error or cancellation, so report the
committed/failed/unknown state rather than pretending that the operation
rolled back.

### Cancellation and result reporting

- Queued but undispatched requests can be canceled reliably.
- `PreDeleteItem` checks a request-local atomic flag and can return
  `HRESULT_FROM_WIN32(ERROR_CANCELLED)` before a top-level item. Microsoft says
  a callback failure cancels subsequent pending operations.
- No claim is made that cancellation interrupts every descendant of one
  recursive directory operation.
- `PerformOperations` success is never sufficient; combine its HRESULT,
  `GetAnyOperationsAborted`, every `PostDeleteItem.hrDelete` and
  `psiNewlyCreated`, and `FinishOperations.hrResult`.
- Contradictory or incomplete callback sequences produce
  `UnknownMayHaveMutated`, not success.

See
[`PreDeleteItem`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-predeleteitem),
[`PostDeleteItem`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-postdeleteitem),
and
[`FinishOperations`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-finishoperations).

## Empty Recycle Bin

Call
[`SHQueryRecycleBinW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shqueryrecyclebinw)
before confirmation to obtain an advisory item/byte estimate. The query is
racy and is not an authorization or guarantee.

The request distinguishes an exact drive root from all drives. Microsoft
documents that a null or empty root passed to
[`SHEmptyRecycleBinW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shemptyrecyclebinw)
empties all Recycle Bins on all drives. After strong confirmation call:

```text
SHEmptyRecycleBinW(
    owner_hwnd,
    selected_root_or_null,
    SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
)
```

The API exposes no documented mid-call cancellation. Cancellation is reliable
only before dispatch. Combine the HRESULT with a post-operation query and
report `Partial` or `UnknownMayHaveMutated` on error because some bins may
already have changed. Automated tests run only in a disposable VM/Sandbox
profile with deliberately seeded data.

## Confirmation boundaries

| Action | Required boundary | Result wording |
| --- | --- | --- |
| Open | Explicit button/menu command; no modal | `Dispatched`; opening may execute the selected item. |
| Reveal | Explicit button/menu command; no modal | `Dispatched`; Explorer accepted the request. |
| Installed Apps | Explicit command; no modal | `Dispatched`; Settings accepted the URI. |
| Recycle | Modal with exact native path, type, logical/allocated size, and default Cancel | Recycled only after per-item callback evidence. |
| Permanent delete | Strong destructive styling, exact path, irreversible warning, typed `DELETE`, final button | Completed, Partial, Failed, or Unknown; never “recycled.” |
| Empty Recycle Bin | Exact drive/all-drives scope, advisory count/bytes, typed `EMPTY`, final button | Completed or potentially partial; no post-dispatch cancellation promise. |
| Explorer integration | Explicit reversible toggle with current EXE path preview | Installed, repaired, removed, conflict, or failed. |

When a native path cannot be represented losslessly as UTF-8, the confirmation
shows a friendly lossy display plus an escaped raw UTF-16 representation. The
user types the action word, not the path; the internal confirmation remains
bound to the exact raw path and fingerprint.

## Native path and identity policy

- Accept only absolute DOS/UNC filesystem paths originating from a current
  scan node. Reject arbitrary Shell monikers, URLs, and device namespaces.
- Use wide APIs and preserve UTF-16 code units. Reject embedded NUL and do not
  normalize Unicode.
- Do not strip `\\?\` blindly. Extended syntax can preserve trailing dots or
  spaces whose semantics change when converted to a normal DOS path.
- Shell UI and long-path-aware filesystem APIs do not have identical path
  capabilities. Shell-item creation is a runtime capability; failure becomes
  `ShellPathUnsupported`, with no fallback through legacy
  `SHFileOperation`, Explorer command lines, or lossy normalization.
- Before destructive dispatch, open the target itself with metadata access,
  all share modes, `OPEN_EXISTING`, `FILE_FLAG_OPEN_REPARSE_POINT`, and
  `FILE_FLAG_BACKUP_SEMANTICS` when needed. Compare `(volume, FILE_ID_128)` to
  the confirmed fingerprint. Missing or changed identity yields
  `TargetChanged` and requires a new confirmation.
- Retain default no-follow semantics for symlinks, junctions, mount points, and
  other reparse points. The operation targets the link object, not its target.
- Block drive roots, volume roots, UNC share roots, current executable, and
  synthetic/summary nodes from permanent deletion. Also block a directory
  known to contain the running executable.

Because `IFileOperation` is path/Shell-item based, late identity validation
reduces but cannot eliminate the final TOCTOU interval. This residual risk and
the rescan-after-mutation rule are part of the product contract. Microsoft
documents filesystem/Shell path differences in
[Maximum Path Length Limitation](https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation)
and general extension risk in
[Security Considerations: Microsoft Windows Shell](https://learn.microsoft.com/en-us/windows/win32/shell/sec-shell).

## No-elevation behavior

- Embed `requestedExecutionLevel="asInvoker"` and `uiAccess="false"` in the
  application manifest.
- Never use the `runas` verb.
- Never set `FOFX_SHOWELEVATIONPROMPT` or `FOFX_REQUIREELEVATION`.
- Use `FOF_NOERRORUI` so protected destructive targets return an error rather
  than an IFileOperation elevation prompt.
- Write Explorer integration only below HKCU.
- Scanning and all ordinary application behavior remain usable as a standard
  user. Access-denied targets are reported rather than retried elevated.
- An explicit Open action can activate a separate target whose own manifest
  requests UAC. DiskPie itself still remains unelevated.

See
[Application Manifests](https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests).

## Reversible Explorer integration

### Portable classic verb

Register only selected folders and drives:

```text
HKCU\Software\Classes\Directory\shell\DiskPie.Scan
HKCU\Software\Classes\Drive\shell\DiskPie.Scan
```

Use these owned values:

```text
MUIVerb = "Scan with DiskPie"
Icon = "<exact current diskpie.exe path>"
MultiSelectModel = "Single"
DiskPieOwner = "<stable DiskPie ownership GUID>"
DiskPieSchema = "1"
DiskPieExecutable = "<exact current diskpie.exe path>"

command\(Default) = "\"C:\\...\\diskpie.exe\" --scan-path \"%1\""
```

Use `REG_SZ`, not `REG_EXPAND_SZ`. The executable is always quoted; `%1` is
always quoted; no command interpreter is involved; and startup parsing uses
`args_os`. Windows filenames cannot contain a double quote, while spaces,
percent signs, ampersands, carets, emoji, and option-like names remain ordinary
argument content when no shell interpreter is introduced.

`HKEY_CLASSES_ROOT` is a merged HKCU/HKLM view, so direct writes under
`HKCU\Software\Classes` require no administrator privileges. See
[Merged View of HKEY_CLASSES_ROOT](https://learn.microsoft.com/en-us/windows/win32/sysinfo/merged-view-of-hkey-classes-root),
[Creating Shortcut Menu Handlers](https://learn.microsoft.com/en-us/windows/win32/shell/context-menu-handlers),
and
[Shortcut Menu Best Practices](https://learn.microsoft.com/en-us/windows/win32/shell/verbs-best-practices).

### Install, repair, and removal algorithm

1. Inspect the exact final key. If it exists without DiskPie's marker and
   expected schema, report a conflict and do not overwrite it.
2. Create a uniquely named staging sibling, write every value and command,
   read them back, then rename the staging key to the final name. On error,
   remove only the staging key created by this request.
3. Treat an already matching final key as idempotent success.
4. A moved executable produces an owned-but-stale state. Show the stored and
   current paths and offer Repair or Remove; do not silently mutate startup
   state.
5. Remove only an exact subtree whose ownership marker, schema, and stored
   command values match DiskPie's record. Never delete the parent `shell`,
   `Directory`, `Drive`, or `Software\Classes` keys. A mismatched owned key is
   a conflict requiring an explicit repair/removal decision.
6. After successful install, repair, or removal, call
   [`SHChangeNotify`](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shchangenotify)
   as `SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, NULL, NULL)` so
   Explorer refreshes associations.

`windows-registry` supplies safe RAII `Key` operations for this exact narrow
surface. Wrap it in `IntegrationRegistry` so ownership validation and exact
subtree deletion remain application policy rather than general-purpose
registry access. Do not use registry transactions/TXF for this reversible
setting.

### Windows 11 modern-menu boundary

The portable static verbs appear in the Windows 10/classic context menu and
normally under **Show more options** on Windows 11. Do not advertise them as a
top-level Windows 11 command.

Microsoft's current guidance, updated 2026-07-17, requires a native COM DLL
implementing `IExplorerCommand`, registered with package identity using
`windows.comServer` and `windows.fileExplorerContextMenus`. An unpackaged app
must install a sparse package, and the DLL architecture must match Explorer.
File Explorer loads the extension on a latency-sensitive path, so its menu
methods must remain fast.

That distribution and in-process/surrogate extension surface conflicts with
DiskPie's initial portable single-EXE promise. Defer it to an optional future
sparse/MSIX distribution with a separately reviewed native DLL, signing,
install/uninstall lifecycle, multi-architecture release plan, crash isolation,
and performance/security tests. The portable release retains only the
reversible HKCU classic verb. See
[Add a File Explorer context menu command to a packaged desktop app](https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/integrate-packaged-app-with-file-explorer).

## Dependency and reuse evaluation

The snapshot below was verified from crates.io, current upstream tagged
source, Microsoft documentation, and RustSec on 2026-08-02. “No direct
advisory found” is not a guarantee; the resolved lockfile still requires
`cargo audit` and transitive review.

| Candidate | License, maintenance, and activity | Unsafe/security and platform | Build impact and value | Lock-in/fallback | Decision |
| --- | --- | --- | --- | --- | --- |
| [`windows` 0.62.2](https://crates.io/crates/windows/0.62.2) | `MIT OR Apache-2.0`; Microsoft windows-rs; published 2025-10-06; active; Rust 1.82. | Safe generated COM projections and `#[implement]` support over generated FFI; project unsafe can stay in a small adapter. Windows target only. [`RUSTSEC-2022-0008`](https://rustsec.org/advisories/RUSTSEC-2022-0008.html) affects versions before 0.32, not this baseline. | The source package and generated graph are nontrivial, but features can be restricted to Foundation, COM, Shell, WindowsMessaging, and any exact handle APIs. It avoids handwritten COM vtables/refcounts and is already the selected binding family. | Microsoft API naming creates platform coupling, contained by `PlatformActions`. `windows-sys` remains a whole-adapter alternative if measured binary/build cost justifies a rewrite. | **Adopt 0.62.2.** |
| [`windows-sys` 0.61.2](https://crates.io/crates/windows-sys/0.61.2) | `MIT OR Apache-2.0`; Microsoft; published 2025-10-06; active; Rust 1.71. | Raw FFI requires unsafe at every call and local COM vtables/refcounts/progress-sink plumbing. Windows only. No directly applicable advisory found. | Smaller projection surface and one `windows-link` support dependency, but substantially more local unsafe and testing for this COM-heavy adapter. | Mixing it with `windows` duplicates types and abstractions. It is a fallback only if the complete Windows adapter switches after measurement. | **Reject for Shell actions.** |
| [`windows-registry` 0.6.1](https://crates.io/crates/windows-registry/0.6.1) | `MIT OR Apache-2.0`; Microsoft; published 2025-10-06; active; Rust 1.82. | Safe RAII key create/open/get/set/remove/rename surface over reviewed Windows calls. Windows only. No direct RustSec advisory found. | Small focused crate with `windows-link`, `windows-result`, and `windows-strings`; materially reduces handle leaks and accidental broad deletion in the reversible registry layer. | Encapsulate behind `IntegrationRegistry`; direct `windows` registry calls are a viable fallback. Avoid unneeded transaction APIs. | **Adopt narrowly.** |
| [`trash` 5.2.6](https://crates.io/crates/trash/5.2.6) | MIT; published 2026-05-03; active cross-platform crate; Rust 1.85. | Public safe API, but its [tagged Windows implementation](https://github.com/ArturKovacs/trash/blob/v5.2.6/src/windows.rs) strips `\\?\`, uses `FOF_ALLOWUNDO`, has no owner/progress sink/per-item outcome, can panic on COM creation, and exposes only coarse abort state. Windows pulls old `windows ^0.56`. No direct advisory found. | Convenient cross-platform trash abstraction, but adds a duplicate bindings graph and cannot satisfy DiskPie's confirmation, identity, fail-closed recycle, or reporting contract. | Its portable abstraction would hide required Windows semantics; replacing it later would change result behavior. | **Reject.** |
| [`open` 5.4.0](https://crates.io/crates/open/5.4.0) | MIT; published 2026-07-12; active; Rust 1.62. | The [Windows implementation](https://github.com/Byron/open-rs/blob/v5.4.0/src/windows.rs) defaults to PowerShell `Start-Process` with Explorer fallback; an optional feature uses manual unsafe ShellExecute FFI. No direct advisory found. | Small cross-platform convenience, but adds subprocess/quoting policy, does not reveal items, and provides no STA ownership or structured dispatch result. | Easy to replace, but it weakens the explicit no-command-interpreter boundary immediately. | **Reject.** |
| [`opener` 0.8.5](https://crates.io/crates/opener/0.8.5) | `MIT OR Apache-2.0`; published 2026-06-11; active. | Uses `windows-sys`; its [open](https://github.com/Seeker14491/opener/blob/v0.8.5/opener/src/windows.rs) and [reveal](https://github.com/Seeker14491/opener/blob/v0.8.5/opener/src/windows/reveal.rs) paths normalize names, create an MTA thread for reveal, join synchronously, and use `ILCreateFromPathW`. No direct advisory found. | Useful generic open/reveal API, but duplicate bindings, path transformation, wrong apartment policy, blocking join, and coarse errors conflict with DiskPie. | Adapter replacement is possible, but behavior would already be coupled to normalization and threading choices. | **Reject.** |
| [`showfile` 0.1.1](https://crates.io/crates/showfile/0.1.1) | MIT; published 2024-01-06; very small and comparatively stale. | Its [source](https://github.com/jf2048/showfile/blob/0.1.1/src/lib.rs) strips extended prefixes, discards actionable errors, and pulls `windows ^0.52`. No direct advisory found. | Tiny convenience API but loses the exact path and result semantics DiskPie needs. | Low API lock-in, high correctness cost. | **Reject.** |
| [`winreg` 0.56.0](https://crates.io/crates/winreg/0.56.0) | MIT; published 2026-03-14; mature and active; Rust 1.60. | Safe public registry API with internal unsafe; Windows only; `windows-sys` dependency range and optional serde/chrono. No direct advisory found. | Proven and capable, but introduces a second Windows binding family where the smaller Microsoft-aligned registry crate is sufficient. | Good fallback if `windows-registry` lacks a required operation. | **Defer/reject initially.** |
| [`registry` 1.3.0](https://crates.io/crates/registry/1.3.0) | `MIT OR Apache-2.0`; published 2024-10-25; repository archived/unmaintained. | Pulls `windows 0.58`; subject to [`RUSTSEC-2025-0026`](https://rustsec.org/advisories/RUSTSEC-2025-0026.html). | Small API, but no value justifies an explicitly unmaintained dependency. | Immediate abandonment risk and duplicate binding version. | **Reject.** |
| [`winsafe` 0.0.28](https://crates.io/crates/winsafe/0.0.28) | MIT; published 2026-07-01; active; Rust 1.87. | Broad safe wrapper with internal FFI and feature-gated Shell/OLE/registry modules. No direct advisory found. | No normal dependencies, but a broad source/compile surface and a second Windows abstraction offer little benefit over the selected bindings. | Significant API-family lock-in; conversion to/from windows-rs types would add glue. | **Reject.** |

Commit `Cargo.lock`, pin deliberate baselines, inspect resolved features with
`cargo tree`, and run advisory and license checks before merge and every
release. Measure clean build time, incremental build time, release EXE size,
and symbol contribution before attributing size to a crate.

## Security and validation plan

### Pure and fake-adapter tests

- State-machine combinations: `S_OK` plus aborted, `PostDeleteItem` failure,
  missing callbacks, `FinishOperations` failure, queued cancellation, and
  contradictory results.
- Confirmation capability mismatch, replay, expiry, wrong action, wrong raw
  path, stale scan generation, and changed/missing fingerprint.
- Root, current executable, containing-directory, synthetic-node, and
  arbitrary-URI rejection.
- Exact flag assertions, including absence of elevation and
  `FOFX_NOSKIPJUNCTIONS` flags.
- Null/malformed PIDL and COM initialization/advise/unadvise failure paths.
- Registry ownership, staging rollback, collision refusal, idempotent install,
  moved-EXE state, and exact-subtree removal.

### Path property tests

- `OsString` names with spaces, emoji, combining characters, percent signs,
  ampersands, carets, commas, leading option-like text, trailing backslashes,
  and raw surrogate code units where the test environment permits them.
- Embedded-NUL rejection and absolute DOS/UNC/device/URI classification.
- Long paths over 260 UTF-16 code units and deep trees.
- Lossless escaped confirmation display whenever UTF-8 conversion is lossy.
- Registry command-line construction and `args_os` round-trip without a
  command interpreter.

### Temporary Windows filesystem fixtures

- Regular files/directories, read-only files, ACL-denied nodes, open handles,
  disappearing targets, sparse/compressed files, hard links, cloud
  placeholders, and paths over 260 code units.
- Junction and symlink to a sentinel outside the fixture. Deleting the link
  must leave the sentinel untouched.
- Replace a confirmed path with a different file before dispatch. Identity
  validation must abort.
- A directory with a deliberately denied descendant must produce Partial or
  Unknown and a parent rescan, never false atomic success.

### Disposable recycle matrix

Run only under a disposable Windows Sandbox/VM user and include:

- local NTFS;
- FAT/exFAT or removable media;
- SMB/non-recyclable provider;
- item larger than configured Recycle Bin capacity;
- disabled/full Recycle Bin;
- read-only, open, and ACL-denied targets; and
- cancellation before dispatch and during a multi-step test sequence.

For each successful recycle, require the per-item callback to identify a
Recycle Bin item and verify that the fixture can be restored. Any silent
permanent fallback blocks release for that provider class.

### Empty-bin matrix

Use a disposable profile and a dedicated seeded volume when possible. Verify
pre-query, exact confirmation scope, empty call, post-query, error/partial
reporting, and absence of a false cancellation promise. Never run this test in
a developer's normal profile or general hosted CI account.

### COM and no-elevation tests

- Assert all COM objects are created, invoked, and released on the service
  thread.
- Exercise message-pump responsiveness, reentrancy, bounded queue saturation,
  startup failure, and shutdown with queued/canceled actions.
- Verify balanced `CoUninitialize` and progress-sink `Unadvise` on every path.
- Under a standard-user/UAC VM, inspect the manifest and confirm that protected
  delete/recycle targets return access denied without a DiskPie elevation
  prompt.
- Confirm Explorer integration writes only HKCU.

### Explorer integration tests

- Windows 10 and Windows 11 x86-64 VMs.
- Install, repeated install, remove, repeated remove, foreign-key collision,
  partial staging failure, moved executable, repair, and ownership mismatch.
- Confirm the Windows 10 classic verb and the Windows 11 **Show more options**
  verb launch DiskPie with one exact native argument.
- Confirm the portable build does not install a modern top-level COM handler.
- Verify `SHChangeNotify` refresh behavior and that unrelated registry keys are
  byte-for-byte unchanged.

## Implementation checklist

1. Add the feature-minimized `windows` API surface and narrow
   `windows-registry` dependency only after checking the resolved lockfile.
2. Implement the bounded STA lifecycle and fakeable `PlatformActions` port.
3. Implement open/reveal/Settings with PIDLs and typed dispatch outcomes.
4. Implement `IFileOperationProgressSink`, exact flag profiles, and complete
   result aggregation before enabling destructive UI.
5. Implement confirmation capabilities and late fingerprint validation.
6. Gate recycle on the disposable-provider matrix; never infer support from a
   filesystem name alone.
7. Implement empty-bin scope/query/reporting and keep its destructive tests
   quarantined.
8. Implement owned HKCU static verbs and document Windows 11's classic-menu
   limitation in user-facing settings.
9. Run architecture, security, UX/accessibility, dependency/license, and
   Windows behavior reviews before release.
