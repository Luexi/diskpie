# ADR 0009: Keep portable Explorer integration per-user and classic

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie needs optional, reversible Explorer integration for starting a scan on
a selected folder or drive. The primary distribution is one portable Windows
executable that runs without administrator privileges or an installer.

Windows supports static registry verbs in the classic context menu. Windows
11's modern top-level menu has a different current contract: a native
`IExplorerCommand` COM DLL registered through package identity with
`windows.comServer` and `windows.fileExplorerContextMenus`. An unpackaged app
needs a sparse package, and the extension DLL architecture must match
Explorer. This introduces installation, signing, native DLL, performance, and
crash-surface requirements that do not fit the initial portable single-EXE
promise.

Registry mutation is reversible but still security-sensitive. DiskPie must
not overwrite another program's verb, delete a broad registry tree, construct
a command through `cmd.exe`, or silently retain a stale executable path after
the portable file moves.

Supporting current sources, dependency evaluation, and the Windows 10/11 test
matrix are recorded in
[Windows shell actions research](../research/windows-shell-actions.md).

## Decision drivers

- Optional setup and complete removal without administrator privileges.
- One portable EXE with no required installer, package identity, or companion
  DLL.
- Exact native folder/drive argument delivery without command injection.
- Ownership-aware, idempotent, narrow registry mutation and rollback.
- Honest Windows 11 UX documentation.
- Low Explorer crash/latency risk and no in-process extension in portable v1.
- Small dependency/unsafe surface and a clean future packaging boundary.

## Options considered

### Machine-wide static verbs under HKLM/HKCR

Machine-wide registration can serve all users but normally requires elevation
and complicates portable removal. Writing the merged HKCR view also obscures
whether state belongs to HKCU or HKLM.

### Per-user static verbs under HKCU

Static `Directory` and `Drive` verbs require no native DLL, remain reversible,
and launch the portable executable directly with `%1`. They work in the
Windows 10/classic menu and through **Show more options** on Windows 11.

### Legacy in-process `IContextMenu` handler

An in-process extension adds bitness, lifetime, crash, security, and Explorer
latency risks while still not meeting the documented package-identity path for
the modern Windows 11 top-level menu.

### Modern `IExplorerCommand` with MSIX or sparse package

This is the documented modern Windows 11 solution and can appear in the
top-level context menu. It requires a native COM DLL, package manifest and
identity, architecture-specific artifacts, install/uninstall lifecycle, and
additional signing, latency, compatibility, and crash testing. It is suitable
only for an optional packaged distribution.

## Decision

The portable DiskPie release will provide only an optional per-user classic
Explorer verb.

1. Register exactly these vendor-qualified keys:

   ```text
   HKCU\Software\Classes\Directory\shell\DiskPie.Scan
   HKCU\Software\Classes\Drive\shell\DiskPie.Scan
   ```

2. Store `MUIVerb`, `Icon`, `MultiSelectModel=Single`, a stable
   `DiskPieOwner` marker, schema version, and exact executable path as
   `REG_SZ`. The child `command` default is structurally equivalent to:

   ```text
   "C:\...\diskpie.exe" --scan-path "%1"
   ```

   Both executable and selected path are quoted. No `cmd.exe`, PowerShell,
   `rundll32`, environment expansion, or caller-provided command fragment is
   involved. DiskPie parses startup arguments with `args_os`.

3. Install is ownership-aware and staged. If the final key is foreign or
   ambiguous, report Conflict and do not overwrite it. Otherwise write a
   uniquely named sibling staging key, read back and verify all values, then
   rename it to the final key. Rollback removes only the staging key created by
   the current request.
4. Matching installation is idempotent. Removal verifies DiskPie's ownership
   marker, schema, and expected stored command values and deletes only the two
   exact owned subtrees. It never removes `shell`, `Directory`, `Drive`, or
   `Software\Classes` parents.
5. When the portable executable moves, DiskPie reports an owned-but-stale
   integration and offers explicit Repair or Remove with both paths shown. It
   does not silently change registry state.
6. Successful install, repair, or removal calls `SHChangeNotify` with
   association-change semantics to refresh Explorer.
7. Use `windows-registry` 0.6.1 narrowly behind an `IntegrationRegistry` port.
   Its safe RAII operations reduce handle and broad-deletion risk; ownership
   checks remain local policy. Direct windows-rs registry calls are the fallback
   if a required operation is missing.
8. The settings UI and user documentation state that Windows 11 normally
   exposes this legacy verb under **Show more options**. The portable release
   does not claim a modern top-level menu command.
9. A future top-level Windows 11 integration is a separate optional sparse or
   MSIX distribution. It must implement a native `IExplorerCommand` COM DLL,
   register `windows.comServer` and `windows.fileExplorerContextMenus`, match
   Explorer architecture, keep menu-construction methods fast, and pass a new
   security/performance/signing/install lifecycle review. It must not become a
   prerequisite for the portable EXE.

## Consequences

Positive outcomes:

- Portable users can add and remove integration without elevation or an
  installer.
- Registry scope and ownership are narrow, auditable, and recoverable.
- Direct process activation preserves spaces, Unicode, emoji, percent signs,
  and option-like paths without command-interpreter injection.
- The portable executable does not load DiskPie code inside Explorer or add a
  companion crash surface.
- Windows 11 behavior is honest and leaves a clean path for an independently
  packaged edition.

Accepted tradeoffs:

- Windows 11 users normally choose **Show more options** before the DiskPie
  verb; the command is not first-level in the modern menu.
- Moving the portable executable makes the stored command stale until the user
  chooses Repair or Remove.
- Static verbs support one selected folder or drive initially; multi-selection
  requires a separately designed activation transport and UX.
- Staging/ownership metadata adds registry values and conflict states.
- A future modern integration will require additional distribution artifacts,
  package identity, native code, signing, and architecture-specific testing.

## Validation

- Pure tests verify exact key paths, `REG_SZ` values, command quoting,
  `MultiSelectModel=Single`, ownership markers, collision refusal, idempotence,
  and exact-subtree deletion.
- Property tests round-trip startup arguments containing spaces, emoji,
  combining characters, `%`, `&`, `^`, commas, leading dashes/slashes, and
  trailing backslashes through the direct command template and `args_os`.
- Failure-injection tests cover every staging write/read/rename step and prove
  rollback never touches a pre-existing or parent key.
- Windows 10 and Windows 11 standard-user VMs cover install, repeated install,
  invocation on folder and drive, moved EXE, Repair, Remove, repeated Remove,
  foreign collision, and ownership/value mismatch.
- Windows 10 verifies the classic context-menu entry. Windows 11 verifies the
  entry under **Show more options** and verifies that no modern COM handler or
  package is installed by the portable build.
- Tests snapshot adjacent registry keys before and after the operation and
  prove they remain unchanged. All fixtures use a unique test-only verb name
  and clean up only a verified owned subtree.
- Standard-user tests confirm all writes are below HKCU, no UAC prompt occurs,
  and `SHChangeNotify` refreshes Explorer association state.

## Revisit triggers

- Produce the optional modern Windows 11 integration only when there is a
  supported sparse/MSIX packaging plan, signing/release infrastructure, a
  native COM DLL security model, architecture coverage, Explorer latency and
  crash-isolation tests, and reliable uninstall/rollback.
- Multi-selection requires explicit path transport, bounded startup behavior,
  native-path round-trip tests, and a superseding/amending decision.
- A Microsoft-supported top-level mechanism that preserves a single portable
  EXE without package identity or in-process code can justify reevaluation.
- A `windows-registry` limitation can trigger replacement behind
  `IntegrationRegistry`; it does not justify widening registry scope or
  weakening ownership checks.
