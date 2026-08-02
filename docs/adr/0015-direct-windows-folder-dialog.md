# ADR 0015: Use the Common Item Dialog directly for folder selection

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers
- Supersedes: ADR 0001 section 6 only for its preliminary `rfd 0.17.2 behind
  DialogPort` selection

## Context

DiskPie needs a native Windows folder/drive picker that preserves arbitrary
native paths, has an owner window, keeps egui responsive, and distinguishes
user cancellation from platform failure. ADR 0007 already accepts one
message-pumping Windows Shell STA and windows 0.62.2.

rfd 0.17.2 is maintained and convenient, but its Windows backend converts a
returned `PWSTR` with `String::from_utf16(...).unwrap()`. This can panic for an
ill-formed native UTF-16 path and loses the exact-native-string invariant. Its
public synchronous result is `Option<PathBuf>`, collapsing cancellation and
COM failures, and its folder flags do not explicitly require a filesystem
result.

Source links, candidate comparison, and the conformance plan are recorded in
[app infrastructure research](../research/app-infrastructure.md#folder-and-drive-selection).

## Decision drivers

- Lossless UTF-16-to-`OsString` path conversion.
- Cancellation distinct from HRESULT and adapter failure.
- Owner-HWND modality, keyboard/accessibility behavior, and Windows 10/11
  support.
- A responsive renderer and correct COM apartment/message-pump ownership.
- Folder and drive-root selection limited to filesystem objects.
- No implicit recent-folder history owned by DiskPie diagnostics.
- One existing Windows binding graph and a small auditable unsafe boundary.

## Options considered

### rfd 0.17.2

rfd provides a cross-platform facade and native Windows Common Item Dialog.
Its UTF-16 conversion, error-collapsing API, option behavior, per-call thread
model, and broad raw-handle `Send`/`Sync` assertions do not meet DiskPie's
path, outcome, and platform-audit contracts.

### Direct `IFileOpenDialog` through windows 0.62.2

The already-selected Microsoft projection exposes the complete dialog,
HRESULT, owner, option, client-state, Shell item, and allocation contracts. It
can run on the existing Shell STA and return only portable owned data.

### Custom in-app folder browser

An egui browser could avoid COM but would need to recreate drive/network
navigation, accessibility, keyboard behavior, Shell namespaces, and path edge
cases. It is a large new filesystem UI and still needs a platform adapter.

## Decision

DiskPie will implement the Windows `DialogPort` with direct
`IFileOpenDialog`/Common Item Dialog APIs through exact windows 0.62.2.

1. Dialog creation, configuration, `Show`, result extraction, and COM object
   release occur on ADR 0007's message-pumping Shell STA. No dialog COM pointer
   crosses a thread or enters portable crates.
2. The UI submits a bounded request and awaits a plain event without blocking
   the renderer. At most one modal selection request is outstanding.
3. The adapter calls `GetOptions`, preserves the existing options, and ORs
   `FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST |
   FOS_DONTADDTORECENT` before `SetOptions`.
4. `Show` receives DiskPie's owner HWND. The documented cancellation HRESULT
   becomes `DialogOutcome::Cancelled`; every other failure preserves a
   sanitized HRESULT/category in `DialogError`.
5. A selected `IShellItem` is requested as `SIGDN_FILESYSPATH`. The allocated
   `PWSTR` is copied as UTF-16 units into `OsString::from_wide` and released
   with `CoTaskMemFree` through RAII on every path. It is never converted
   through `String` or a display form.
6. Set a stable application client GUID and call `ClearClientData` after use.
   Together with `FOS_DONTADDTORECENT`, this prevents DiskPie's dialog client
   state from becoming an implicit last-folder feature.
7. The portable-facing contract is equivalent to
   `Result<DialogOutcome<PathBuf>, DialogError>` with `Selected` and
   `Cancelled` distinct. Drive roots are valid selected folders.
8. Use only the windows features already required by the Shell adapter plus
   the narrow dialog interfaces. Project-authored unsafe is target-gated,
   minimal, documented with `SAFETY` invariants, and limited to native string
   ownership/conversion where the projection cannot express it safely.
9. rfd is rejected for the Windows adapter. It may be reevaluated independently
   for a future non-Windows adapter only if that target passes the same path
   and result contract.

This decision supersedes only ADR 0001 section 6's preliminary rfd dependency
choice. ADR 0001's five-crate architecture, consumer-owned `DialogPort`,
portable core rule, synchronous engine boundary, and windows-rs baseline
remain in force.

## Consequences

Positive outcomes:

- Native path identity and COM error information are not discarded by a
  cross-platform facade.
- The picker inherits Windows' familiar drive/network navigation,
  accessibility, keyboard, and DPI behavior.
- It composes with the already-required Shell STA and windows-rs graph.
- Application tests can use a deterministic fake `DialogPort`.
- Cancellation remains normal user intent, not a diagnostic failure.

Accepted tradeoffs:

- Direct COM setup and allocation ownership require more target-gated code and
  review than calling rfd.
- The modal dialog occupies the Shell STA while shown, though its native modal
  loop pumps messages and the renderer remains responsive. Other Shell work is
  serialized until the dialog closes.
- Common Item Dialog behavior is Windows-specific. The port, not a
  cross-platform dialog dependency, preserves future adapter options.
- `ClearClientData` means the picker may not reopen at the previous folder; an
  explicit remembered-folder feature would require a privacy decision.

## Validation

- First complete a target-gated prototype; make production UI wiring depend on
  passing the Windows conformance suite below.
- Unit tests with a fake port cover selected, cancelled, queue-full, service
  unavailable, and HRESULT error outcomes.
- Windows tests assert STA ownership, owner HWND, existing-option preservation,
  exact added flags, cancellation mapping, filesystem-only result handling,
  `ClearClientData`, and `CoTaskMemFree` on every exit.
- Path tests cover drive roots, UNC roots, extended-length syntax, emoji,
  spaces, reserved punctuation, and lossless unpaired-surrogate fixtures.
- Windows 10/11 x86-64 checks cover keyboard-only selection, screen-reader
  behavior, high DPI, cancellation, foreground/modal ownership, and queue
  serialization.
- Diagnostics tests prove selected paths and dialog state are not logged or
  persisted.

## Revisit triggers

- Replace direct COM only if a helper exposes lossless native paths, distinct
  cancellation/errors, owner HWND, exact option control, STA compatibility,
  and a smaller measured audited surface.
- A second desktop OS should add its own `DialogPort` adapter; it does not
  reopen the accepted Windows implementation automatically.
- Persisting or restoring the last selected folder requires a new explicit
  privacy/UX decision.
