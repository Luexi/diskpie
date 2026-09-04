# App infrastructure research

- Research date: 2026-08-02
- Scope: preferences, startup CLI parsing, folder/drive selection, project
  paths, local diagnostics, and panic handling
- Target: Windows 10/11 x86-64, standard user, portable executable

## Executive decision

DiskPie should keep these facilities behind small consumer-owned ports and
use the platform or framework already selected when that gives a narrower
dependency and privacy boundary:

- Store versioned Serde data as one application-owned RON document. Pin Serde
  1.0.229 and RON 0.12.2; disable eframe's `persistence` feature and do not add
  `serde_json` for this frontend. This corrects the initial recommendation
  after exact-source verification described in ADR 0019.
- Parse the optional startup path with lexopt 0.3.2 from `OsString`. Never
  round-trip a filesystem argument through UTF-8.
- Implement the Windows folder picker directly with `IFileOpenDialog` through
  windows 0.62.2, on the message-pumping Shell STA accepted in ADR 0007. rfd
  0.17.2 is rejected for the Windows adapter because its selected-path
  conversion can panic on ill-formed UTF-16 and its API collapses cancellation
  and native errors.
- Resolve Roaming AppData and Local AppData directly with
  `SHGetKnownFolderPath` through the already-adopted windows crate. The
  `directories` repository is archived, and its portability is not useful
  enough to justify another platform dependency before a second OS exists.
- Use tracing 0.1.44, tracing-subscriber 0.3.23, and tracing-appender 0.2.5 for
  bounded, local-only diagnostics. The application owns the event schema,
  privacy allowlist, retention, and explicit export flow.
- Install a minimal local `std::panic` hook. Do not add color-eyre,
  human-panic, Sentry, or another crash/telemetry SDK.

The corresponding decisions are recorded in ADRs 0013 through 0018.

## Constraints and threat model

The infrastructure must preserve DiskPie's existing architecture and product
promises:

- `diskpie-core` remains platform independent. Framework and Windows types
  stay in adapters; portable-facing ports use `PathBuf`, plain enums, and
  owned data.
- Arbitrary Windows paths include valid native names that are not valid
  Unicode. A path must remain `OsString`/`PathBuf` from ingestion through the
  scanner boundary.
- The renderer must not perform blocking filesystem or COM work.
- DiskPie runs without elevation and never writes beside the executable just
  because it was distributed as one portable `.exe`.
- Preferences, logs, and crash data are local. There is no automatic upload,
  analytics, telemetry, or diagnostic network client.
- A user can intentionally scan or export to a UNC path. That is explicit
  filesystem I/O, not telemetry; the UI must make that scope visible.
- Corrupt or newer settings must fail to safe defaults without destroying the
  only recoverable copy. Logging and crash-report failures must not stop a
  scan.
- Every queue, retained file set, exported artifact, and panic record has an
  explicit bound.

## Version and maintenance snapshot

The following exact versions were current on crates.io on 2026-08-02. Release
metadata and source links are primary evidence; a lockfile and `cargo deny`
remain the reproducible-build controls.

| Candidate | Exact release | License | Maintenance and safety observation |
| --- | --- | --- | --- |
| [serde](https://crates.io/crates/serde/1.0.229) | 1.0.229 | MIT OR Apache-2.0 | Published 2026-07-18; MSRV 1.56; derive is opt-in. |
| [ron](https://crates.io/crates/ron/0.12.2) | 0.12.2 | MIT OR Apache-2.0 | Published 2026-06-22; MSRV 1.64; crate source denies unsafe code. |
| [serde_json](https://crates.io/crates/serde_json/1.0.151) | 1.0.151 | MIT OR Apache-2.0 | Published 2026-07-20; MSRV 1.71; unnecessary duplicate format here. |
| [eframe](https://crates.io/crates/eframe/0.35.0) | 0.35.0 | MIT OR Apache-2.0 | Published 2026-06-25; MSRV 1.92; already the UI framework. |
| [lexopt](https://crates.io/crates/lexopt/0.3.2) | 0.3.2 | MIT | Published 2026-02-28; zero dependencies; forbids unsafe code. |
| [pico-args](https://crates.io/crates/pico-args/0.5.0) | 0.5.0 | MIT | Published 2022-06-04; zero dependencies; repository activity stopped in 2023. |
| [clap](https://crates.io/crates/clap/4.6.5) | 4.6.5 | MIT OR Apache-2.0 | Published 2026-07-31; MSRV 1.85; active, but broader than a one-path CLI. |
| [rfd](https://crates.io/crates/rfd/0.17.2) | 0.17.2 | MIT | Published 2026-01-12; active, but its Windows path/error contract is unsuitable. |
| [windows](https://crates.io/crates/windows/0.62.2) | 0.62.2 | MIT OR Apache-2.0 | Published 2025-10-06; MSRV 1.82; already accepted for Windows adapters. |
| [directories](https://crates.io/crates/directories/6.0.0) | 6.0.0 | MIT OR Apache-2.0 | Published 2025-01-12; its GitHub repository is archived/read-only. |
| [directories-next](https://crates.io/crates/directories-next/2.0.0) | 2.0.0 | MIT OR Apache-2.0 | Published 2020-10-22; stale fork baseline. |
| [tracing](https://crates.io/crates/tracing/0.1.44) | 0.1.44 | MIT | Published 2025-12-18; MSRV 1.65; active structured facade. |
| [tracing-subscriber](https://crates.io/crates/tracing-subscriber/0.3.23) | 0.3.23 | MIT | Published 2026-03-13; MSRV 1.65; newer than the patched advisory floor. |
| [tracing-appender](https://crates.io/crates/tracing-appender/0.2.5) | 0.2.5 | MIT | Published 2026-04-17; MSRV 1.63; supplies bounded nonblocking I/O and retention. |
| [log](https://crates.io/crates/log/0.4.33) | 0.4.33 | MIT OR Apache-2.0 | Published 2026-06-20; useful compatibility facade, not a complete backend. |
| [env_logger](https://crates.io/crates/env_logger/0.11.11) | 0.11.11 | MIT OR Apache-2.0 | Published 2026-06-25; stderr/environment oriented and has no retention/export. |
| [color-eyre](https://crates.io/crates/color-eyre/0.6.5) | 0.6.5 | MIT OR Apache-2.0 | Published 2025-05-30; terminal report ergonomics do not fit a private GUI crash record. |
| [human-panic](https://crates.io/crates/human-panic/2.0.8) | 2.0.8 | MIT OR Apache-2.0 | Published 2026-04-02; writes richer panic/backtrace/system reports than policy permits. |
| [sentry](https://crates.io/crates/sentry/0.49.0) | 0.49.0 | MIT | Published 2026-07-29; default SDK graph is explicitly network/telemetry oriented. |

The only directly relevant RustSec result found in the primary advisory
database was
[RUSTSEC-2025-0055](https://rustsec.org/advisories/RUSTSEC-2025-0055.html), an
ANSI escape injection in tracing-subscriber fixed in 0.3.20. The selected
0.3.23 is patched, and DiskPie also disables ANSI in file logs. This search is
not a substitute for running `cargo deny check advisories` against the final
lockfile.

### Dependency, unsafe, and portability assessment

No trustworthy whole-application compile-time or binary-size numbers exist
before implementation and release-profile measurement. The comparisons below
describe incremental shape; release gates must record clean/incremental build
time, compressed/uncompressed executable size, and `cargo tree -e features`
for the final Windows target rather than substitute crate download size.

| Choice | Incremental dependency and build shape | Unsafe and portability boundary |
| --- | --- | --- |
| Application-owned Serde/RON document | Serde derive and RON are direct, pinned dependencies. Disabling eframe persistence removes its path/storage behavior; JSON would add another serializer and its code paths. | Serde/RON deny unsafe in their public source; native file ownership and atomic replacement remain in the reviewed platform adapter. |
| lexopt | One direct crate, zero transitives, no derive macro. pico-args is similarly small; clap adds clap-builder and optional derive/formatting/filter facilities. | lexopt forbids unsafe and preserves `OsString`; all three are portable, but only lexopt matches the current complexity/maintenance balance. |
| Direct Common Item Dialog | Reuses windows 0.62.2. Dialog/Known Folder code needs narrow `Win32_System_Com` and `Win32_UI_Shell` features; the existing Shell STA also needs its message-window feature set. rfd instead brings its own raw windows-sys backend and cross-platform facade. | Direct code concentrates native allocation/COM invariants in one target-gated adapter. rfd's Windows backend contains extensive raw unsafe and broad raw-handle `Send`/`Sync`; its safe facade does not repair the path/error semantics. |
| Direct Known Folder paths | Reuses the same `Win32_UI_Shell` projection. directories adds directories/dirs-sys platform logic; directories-next adds an older parallel family. | The consumer port is portable, while one small Windows conversion/free boundary is platform-only. directories is a safe convenience surface but its archived upstream and platform internals weaken the maintenance case. |
| Narrow tracing stack | Adds subscriber registry/formatting plus appender channel, time, symlink, and error helpers. Disabling ANSI, env-filter, regex, JSON, chrono, local-time, attributes, and parking_lot avoids their optional graphs. | The tracing/appender source presents a safe API; channel/subscriber transitives contain normal low-level unsafe. No unsafe or tracing type enters the portable domain model. |
| `log`/env_logger | The facade alone is small, but env_logger's formatting, environment filtering, and terminal support still do not supply retention/export. An application writer adds no crates but adds authored concurrency and deletion code. | A local writer could be safe Rust yet carry a larger logical race/deletion audit. env_logger is portable but terminal-oriented. |
| Minimal panic hook | Adds no crate. color-eyre adds span/backtrace presentation; human-panic adds report/system/backtrace dependencies; Sentry's defaults include reqwest/native TLS plus context, panic, logs, metrics, debug images, and release health. | The standard hook is safe application code with a fixed data budget. Sentry crosses the network/trust boundary; richer local reporters cross the approved data boundary even without upload. |

## Persistent preferences

### Framework behavior

> **Post-decision correction:** exact inspection of the locked eframe 0.35.0
> native renderer paths found that `persistence_path: None` calls
> `create_storage(app_id)`, which selects eframe's default directory. It does
> not disable persistence. Because Known Folder resolution can fail at
> runtime, this violates the no-fallback contract. ADR 0019 therefore
> supersedes the adoption conclusion below and disables the feature.

eframe's [`Storage` trait](https://docs.rs/eframe/0.35.0/eframe/trait.Storage.html)
is a small string key/value interface. Its
[`get_value`/`set_value` helpers](https://docs.rs/eframe/0.35.0/eframe/fn.set_value.html)
use Serde and RON when the `persistence` feature is enabled. On Windows,
eframe's current
[`FileStorage` implementation](https://docs.rs/crate/eframe/0.35.0/source/src/native/file_storage.rs)
stores the outer key/value map in `APP_ID/data/app.ron` beneath Roaming
AppData. `NativeOptions::persistence_path` permits an application-selected
file.

This reuses an already-paid dependency and keeps window placement plus app
preferences in one framework-owned store. It also has limitations that must
be explicit:

- the outer file is an implementation detail, not DiskPie's stable public
  format;
- malformed outer RON currently loads as an empty map;
- `flush` clones the map and saves on an `eframe_persist` thread, then waits
  for the previous save;
- the current save uses `File::create`, so the outer file replacement is not
  atomic.

DiskPie retains the schema analysis but does not use eframe's outer map. The
application owns the entire bounded document and delegates explicit native
read/replacement to the platform adapter. UI callbacks operate only on the
in-memory settings session.

The Windows adapter holds a local, non-reparse parent and the inspected
single-link target by handle. A save creates and flushes an exclusive sibling,
then calls `NtSetInformationFile(FileRenameInformationEx)` with a plain name
relative to the retained parent plus replace/POSIX flags. The Win32 wrapper
`SetFileInformationByHandle` rejects any non-null `RootDirectory` with
`ERROR_INVALID_PARAMETER`, so it cannot express this handle-relative commit
(see ADR 0019 for the measured evidence). The target handle grants delete
sharing because POSIX replacement requires it from every opener; identity is
protected by comparing the retained handle's file identity immediately before
and after the rename, not by a sharing lock. Windows builds/filesystems
without this extended operation get session-only settings, never a path-based
fallback.

### Schema contract

Use a shape equivalent to:

```rust
struct SettingsEnvelope {
    schema_version: u32,
    settings: SettingsV1,
}
```

The exact type belongs in the app/application boundary, not in the scanner.
Under ADR 0019 this envelope is the entire `app.ron` document rather than a
framework map entry. Serialized input and output are capped at 64 KiB before
parsing or saving.

Load behavior is deterministic:

1. Missing value loads defaults.
2. A supported older version passes through pure, stepwise migrations, then
   validation and clamping. Saving writes only the current version.
3. The current version is validated and clamped before use.
4. A newer unknown version starts a safe, session-only default configuration,
   shows a nonfatal warning, and does not overwrite the value unless the user
   explicitly resets it.
5. Malformed or oversized data follows the same preserve-and-warn rule. A
   stable diagnostic code is allowed; raw settings text is not logged.

Serde field defaults help with additive fields inside one supported version,
but they do not replace the schema number. Do not use `deny_unknown_fields`:
ignoring a future additive field is safer than rejecting the entire known
version. Validate enums, finite numeric values, UI scale, thresholds, and
retention bounds after deserialization.

The default persisted set excludes last scan roots, recent paths, command-line
arguments, file names, secrets, and filesystem history. A future "remember
last folder" feature needs a separate privacy decision and explicit opt-in.

### Candidate comparison

- **Application-owned Serde + RON document — adopt (corrected).** Typed,
  bounded schema with an exact no-fallback path and atomic native replacement.
- **Serde JSON — defer.** JSON is useful for interchange or a framework-free
  settings service, but adding a second format provides no present benefit.
- **eframe Storage — reject after exact-source verification.** A missing
  explicit path activates its default platform path, and the outer file save
  is not atomic. There is no supported runtime disable switch.
- **Unversioned eframe key per field — reject.** It makes cross-field
  migration, newer-version protection, reset, and bounded validation harder.

## Startup CLI and native path preservation

The initial contract is intentionally narrow:

```text
diskpie.exe [--scan-path PATH | [--] PATH]
```

Zero paths opens the normal selector; one path, positional or through the
`--scan-path` option that ADR 0009 writes into the Explorer verb, schedules it
as the initial scan root. More than one path in any combination and every
unknown option are usage errors. `--` allows a path whose first component looks like an option. Help
and version output may be supported, but there are no subcommands, shell
completion, environment expansion, globbing, trimming, canonicalization, or
prefix rewriting.

lexopt's [`Parser::from_env`](https://docs.rs/lexopt/0.3.2/lexopt/struct.Parser.html#method.from_env)
reads `std::env::args_os`, and
[`Arg::Value`](https://docs.rs/lexopt/0.3.2/lexopt/enum.Arg.html) carries an
`OsString`. DiskPie can move it directly into `PathBuf`; it must never use
`to_str`, `to_string_lossy`, or serialize the value just to cross a layer.
Option names are ASCII protocol tokens; filesystem values are native strings.

Candidate assessment:

- **lexopt 0.3.2 — adopt.** Zero dependencies, unsafe forbidden, direct
  `OsString` values, simple enough that the grammar remains visible in one
  function.
- **pico-args 0.5.0 — reject for the current baseline.** It can preserve
  `OsString` through its OS-string callbacks and is similarly small, but its
  2022 release and inactive repository offer no compensating advantage.
- **clap 4.6.5 — defer.** It correctly supports OS-native arguments and is
  actively maintained. Its builder/derive/help/completion surface is useful
  only after DiskPie gains real subcommands or generated CLI documentation.
  If adopted later, start with `default-features = false` and only measured
  `std`, help, usage, and error-context features.
- **Manual `std::env::args_os` parser — prototype fallback only.** The grammar
  is small, but reproducing option termination, missing-value, help, and
  consistent diagnostics correctly saves less code than it first appears.

Parsing occurs before UI startup and produces a plain application request or
a structured usage error. Logs may record only an error code and argument
count, never the raw argument. Tests must include a `Parser::from_iter`
equivalent with spaces, emoji, UNC roots, extended-length syntax, trailing
separators, option-like names, and `OsStringExt::from_wide` containing an
unpaired surrogate. Equality is native-unit equality, not display-string
equality.

## Folder and drive selection

### Why rfd is not the Windows adapter

rfd is maintained, MIT licensed, cross-platform, and convenient. Those
qualities do not overcome three contract failures in its 0.17.2 Windows
backend:

1. Its selected-path conversion scans a `PWSTR` and calls
   [`String::from_utf16(...).unwrap()`](https://github.com/PolyMeilex/rfd/blob/0.17.2/src/backend/win_cid/file_dialog/com.rs#L25-L34).
   An ill-formed but native Windows path can panic, and converting through
   `String` violates exact path identity.
2. Its synchronous API returns an
   [`Option<PathBuf>`](https://docs.rs/rfd/0.17.2/rfd/struct.FileDialog.html),
   while the
   [Windows implementation maps COM setup/show failures through `.ok()`](https://github.com/PolyMeilex/rfd/blob/0.17.2/src/backend/win_cid/file_dialog.rs#L79-L99)
   into the same `None` as cancellation. DiskPie needs `Cancelled` distinct
   from a diagnostic HRESULT.
3. Its Windows backend sets `FOS_PICKFOLDERS` rather than reading and
   [augmenting the dialog's existing options](https://github.com/PolyMeilex/rfd/blob/0.17.2/src/backend/win_cid/file_dialog/dialog_ffi.rs#L312-L334),
   and does not enforce `FOS_FORCEFILESYSTEM`. Its source also contains broad
   [unsafe `Send`/`Sync` assertions for raw window handles](https://github.com/PolyMeilex/rfd/blob/0.17.2/src/file_dialog.rs#L20-L33).

These are correctness and auditability issues, not merely binary-size
preferences. ADR 0015 supersedes only ADR 0001 section 6's preliminary rfd
selection. It does not supersede ADR 0001's modular boundary or windows-rs
baseline.

### Direct Common Item Dialog contract

The Microsoft
[Common Item Dialog](https://learn.microsoft.com/en-us/windows/win32/shell/common-file-dialog)
is available throughout the supported Windows baseline. The folder picker is
an `IFileOpenDialog` owned and invoked on ADR 0007's message-pumping Shell
STA, whose apartment is initialized under the documented
[`CoInitializeEx`](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-coinitializeex)
contract:

1. Create `FileOpenDialog`; call
   [`IFileDialog::GetOptions`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-ifiledialog),
   then OR and set `FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM |
   FOS_PATHMUSTEXIST | FOS_DONTADDTORECENT` from the documented
   [`FILEOPENDIALOGOPTIONS`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-_fileopendialogoptions).
2. Set the owner HWND and call
   [`Show`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-imodalwindow-show).
   Map the documented cancellation HRESULT to `Cancelled`; preserve all other
   HRESULTs as sanitized adapter errors.
3. Obtain the
   [`SIGDN_FILESYSPATH`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-sigdn)
   display name from the resulting `IShellItem`. Copy the returned UTF-16
   units into `OsString::from_wide` and release the `CoTaskMemFree` allocation
   through RAII. Never convert through `String`.
4. Give the dialog a stable client GUID, call
   [`ClearClientData`](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifiledialog-clearclientdata)
   after use, and keep `FOS_DONTADDTORECENT` so the diagnostic/privacy
   subsystem does not create an implicit folder history.
5. Permit one outstanding modal request. The UI submits a bounded request to
   the Shell service and remains responsive while the native modal loop
   pumps messages.

The portable-facing outcome is conceptually
`Result<DialogOutcome<PathBuf>, DialogError>`, where `DialogOutcome` has
`Selected` and `Cancelled` variants. Drive roots are folders and must be
selectable. The application's initial drive list still comes from the volume
adapter; the dialog does not replace it.

The implementation should first be completed as a Windows prototype and pass
Windows 10/11 owner-window, DPI, keyboard, accessibility, cancellation,
drive-root, UNC, extended-length, and exact UTF-16 tests before the UI depends
on it. A fake `DialogPort` covers application tests on every host.

The incremental windows-rs features are `Win32_System_Com` and
`Win32_UI_Shell`; ADR 0007's service also carries its existing
`Win32_UI_WindowsAndMessaging` feature. Do not enable the whole Windows API
surface or add windows-sys solely for this dialog.

## Project paths

`directories` 6.0 describes reasonable platform locations in its
[`ProjectDirs`](https://docs.rs/directories/6.0.0/directories/struct.ProjectDirs.html)
API, but the
[upstream repository is archived](https://github.com/dirs-dev/directories-rs)
and moved away from GitHub. `directories-next` 2.0.0 is older still. Either
also adds a path-specific platform dependency while DiskPie already carries
windows-rs for its supported OS.

Use
[`SHGetKnownFolderPath`](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shgetknownfolderpath),
which Microsoft recommends for new Known Folder code, through windows 0.62.2:

- settings file: `FOLDERID_RoamingAppData` plus
  `io.github.luexi.diskpie/data/app.ron`;
- local diagnostic root: `FOLDERID_LocalAppData` plus
  `io.github.luexi.diskpie`, containing dedicated `logs` and `crashes`
  directories.

The exact identifier should be treated as persistent after release. A
`ProjectPaths` adapter returns native `PathBuf` values; it converts the
allocated `PWSTR` losslessly and frees it with `CoTaskMemFree` on every path,
including failure. The application-owned adapter from ADR 0019 receives the
settings file; eframe never receives or derives a persistence path. Portable
crates do not import Windows types.

There is one subtle unsafe obligation. The
[Microsoft contract](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shgetknownfolderpath)
requires freeing the output allocation whether the call succeeds or fails,
while the generated windows 0.62.2 convenience function returns
[`Result<PWSTR>` and exposes the output only on success](https://github.com/microsoft/windows-rs/blob/32c3144490c016fe496a0aed769bce60987a2e9d/crates/libs/windows/src/Windows/Win32/UI/Shell/mod.rs#L3301-L3306).
The adapter must use a minimal target-gated raw call with windows types/binding
support so it can install an output guard before examining the HRESULT. It
must not add a broad second binding crate or assume the failure pointer is
null. This is the one project-authored ABI/unsafe detail in `ProjectPaths` and
needs failure-injection coverage.

Failure to resolve or create a location disables that facility and produces a
user-visible, nonfatal warning. There is deliberately no fallback to the
current directory, executable directory, guessed environment variables, or a
temporary directory. Such fallbacks either fail on read-only media, leak data
into a surprising location, or silently lose it. Absolute project paths are
not themselves logged or added to an export.

This direct adapter is adopted for the Windows baseline. Reconsider a
maintained cross-platform path crate only when a second desktop target is
funded and its adapter would remove more code than it adds.

## Local diagnostics and export

### Logging stack and feature budget

Use exact direct versions with narrow features:

```toml
tracing = { version = "=0.1.44", default-features = false, features = ["std"] }
tracing-subscriber = { version = "=0.3.23", default-features = false, features = ["fmt", "std", "tracing-log"] }
tracing-appender = "=0.2.5"
```

Do not enable `env-filter`, regex, JSON, ANSI, local-time, chrono, or
`tracing`'s attributes feature until a measured use requires them. The
`tracing-log` bridge captures dependencies that emit through the `log` facade.
A validated application setting chooses a fixed maximum level; arbitrary
`RUST_LOG` expressions are not part of the production GUI contract.

The formatter uses `with_ansi(false)`. Stable event names and structured
fields make scan generation, service, request ID, outcome code, item counts,
latency, and queue pressure useful without raw file names. Per-file successful
events are prohibited at normal levels.

Do not use tracing-appender's
[`rolling::Builder`](https://docs.rs/tracing-appender/0.2.5/tracing_appender/rolling/struct.Builder.html).
The pinned implementation joins a raw path and opens it through `std::fs`
before DiskPie can verify the directory or leaf by handle. A junction or
symlink can therefore redirect the active sink, including to UNC, and an
existing hard link can receive log bytes. Its retention separately follows
metadata and lets a Windows creation timestamp admit names outside DiskPie's
exact namespace. A later prune cannot make an already-open unsafe sink safe.

Use tracing-appender only for its bounded nonblocking queue. Its consumer is a
small application-owned Win32 daily writer. The writer retains a local,
non-reparse directory handle, opens the UTC active name with append access and
`FILE_FLAG_OPEN_REPARSE_POINT`, verifies a regular direct child with one link,
and retains that file handle until UTC rollover. Every rollover repeats the
full object and parent validation; no failure selects another path. The handle
denies competing writers, so its current size can enforce a 16 MiB daily cap:
append only a complete record that fits and count a whole-record omission when
the next write would cross the limit.

Retention runs on that same writer thread and directory handle. Enumerate with
`GetFileInformationByHandleEx` and `FileIdExtdDirectoryInfo`, whose resumable
records include the 128-bit file ID and reparse tag, rather than reopening a
raw path with `fs::read_dir`. Parse the variable records defensively, enforce
an explicit total-entry budget, perform linear work, and protect the actual
active handle/identity. Only exact ASCII `diskpie.YYYY-MM-DD.log` regular
single-link children belong to the eight-file namespace. The active identity
is always one retained member; syntactically valid future dates remain managed
inside the same cap rather than being preserved without limit. Ambiguity fails
closed and is counted. Eight managed files plus the per-file cap place the
application-owned log payload below 128 MiB. Release tests still measure
normal INFO volume and verify that cap pressure is exceptional and visible.

Use
[`NonBlockingBuilder`](https://docs.rs/tracing-appender/0.2.5/tracing_appender/non_blocking/struct.NonBlockingBuilder.html)
with an explicit 4,096-line queue and lossy mode. Renderer and scan workers
must not block on a slow or full diagnostic sink. Retain its `WorkerGuard`
until all application services stop, expose its error counter in an export,
and record dropped diagnostics as a count. Abrupt process termination can
lose queued lines; the separate synchronous panic marker covers only the
minimal crash signal.

The retained directory must contain only DiskPie logs; retention deletion is
constrained to the owned prefix/suffix and must not follow symlinks. Directory
or writer failure disables file logging without preventing startup or scan.

Alternatives:

- **`log` + `env_logger` — reject as the backend.** The facade is common and
  maintained, but env_logger targets stderr and environment filtering. It
  supplies neither structured spans, rolling retention, bounded async I/O,
  nor export policy.
- **Use tracing-appender's rolling writer — reject.** It cannot enforce the
  root/leaf reparse, hard-link, transport, or retained-identity contract before
  its first write. Its nonblocking queue remains useful because it keeps
  concurrency, saturation, and drop accounting out of the native writer.
- **A network observability SDK — reject.** It conflicts with the product's
  local-only diagnostic boundary and provides no required capability.

### Privacy allowlist

Production events are defined by an allowlist. They may include:

- stable event and error codes;
- app version, target architecture, and coarse OS version;
- anonymous in-process generation/request IDs;
- counts, durations, queue depths, allocation classes, and selected policy
  enum values.

They must not include raw or display paths, path basenames, filenames,
command-line arguments, current directory, environment variables, usernames,
hostnames, URLs, file contents, volume labels, secrets, authentication values,
Shell payloads, or settings serialization. Upstream targets default to WARN or
ERROR and receive an additional export scrub because their message formats are
not controlled by DiskPie.

### Explicit diagnostic export

Export is an explicit user action and creates a plain UTF-8 `.txt` artifact at
a user-selected destination. Plain text avoids another archive dependency and
supports an inspect-before-share workflow. The export contains:

- a fixed schema/header, app and target versions, coarse OS build, and
  diagnostics-policy version;
- settings values only from an explicit non-path allowlist;
- aggregate metrics, omitted-line/file notices, appender drop count, and the
  presence of a minimal panic marker;
- the newest complete log lines that fit an 8 MiB total cap, with each input
  line capped at 16 KiB.

Before writing, run a second conservative scrub for Windows drive/UNC/extended
paths, URLs and query strings, environment-like assignments, token/password/
authorization/cookie patterns, and control characters. Show a preview and the
exact included categories. "Include filesystem paths" is off by default and
requires explicit consent for each export; even then, secret-like values stay
redacted. Export never modifies or deletes the original logs.

Write to a temporary sibling and rename into the chosen destination. Refuse a
destination equal to the owned settings, log, or crash files, and do not
overwrite without the native dialog's explicit confirmation. A UNC
destination can transmit the artifact because the user selected it; label
that fact before confirmation. There is no share/upload button that initiates
network traffic.

## Crash and panic behavior

### Rejected report frameworks

- [color-eyre 0.6.5](https://docs.rs/color-eyre/0.6.5/color_eyre/) improves
  terminal error reports and adds span/backtrace presentation. DiskPie needs a
  fixed private GUI record, not rich ANSI console context.
- [human-panic 2.0.8 source](https://docs.rs/crate/human-panic/2.0.8/source/src/report.rs)
  writes a TOML report containing panic cause, source location, backtrace, and
  system metadata. That is intentionally richer than DiskPie's collection
  policy and can expose paths or payload secrets even though it does not
  automatically upload them.
- [Sentry 0.49.0](https://docs.rs/sentry/0.49.0/sentry/) is an observability SDK
  whose default features include transport and panic/context collection. An
  opt-out configuration still adds a network-oriented SDK and policy surface
  for no local requirement.

### Minimal local hook

Install one application-owned `std::panic::set_hook` after `ProjectPaths` is
resolved but before UI or worker threads start. Prepare the target directory
and fixed path in advance. The release hook writes or replaces
`crashes/last-panic.txt` best-effort with a fixed schema capped at 4 KiB. It
does not call tracing, acquire application locks, or panic on any I/O failure.

Allowed fields are deliberately sparse: marker schema, Unix timestamp, app
version, target OS/architecture, fixed `uncaught_panic` code, and a static
application-assigned thread role or `unknown`. The hook must not serialize the
panic payload, raw thread name, source file or line, backtrace, arguments,
environment, current directory, settings, scan paths, memory, or file
contents. Debug builds may chain the previous console hook after the local
write; the release GUI must not depend on stderr.

On the next start, the app offers a plain "DiskPie ended unexpectedly" notice
with preview, export, and delete actions. It does not infer a cause from the
marker and never sends it anywhere. A last-marker overwrite bounds storage
and avoids an unreviewed crash archive.

The diagnostic privacy promise is therefore precise:

> DiskPie has no telemetry, analytics, crash upload, or background diagnostic
> network client. Preferences, logs, and crash markers stay on this device.
> Diagnostics never initiate network traffic and nothing is uploaded
> automatically. A user-selected UNC scan root or export destination is
> explicit filesystem I/O and can use the network; the UI labels that scope.
> Exports occur only after destination choice and inclusion review; paths are
> off by default, and secrets are never intentionally included.

## Decision matrix

| Area | Candidate | Status | Reason and revisit trigger |
| --- | --- | --- | --- |
| Preferences | Serde 1.0.229 + RON 0.12.2 application-owned document | **Adopt** | Typed and versioned; exact Known Folder path, bounded decode, and native atomic replacement enforce ADR 0019. |
| Preferences | serde_json 1.0.151 | **Defer** | No interchange or independent-backend requirement. Revisit for a second frontend or public config format. |
| CLI | lexopt 0.3.2 | **Adopt** | Native `OsString`, zero dependencies, visible grammar. |
| CLI | pico-args 0.5.0 | **Reject** | Correctly small but stale with no advantage over lexopt. |
| CLI | clap 4.6.5 | **Defer** | Active and capable but oversized for zero/one positional path. Revisit with subcommands/completions. |
| Folder dialog | Direct `IFileOpenDialog` through windows 0.62.2 | **Prototype** | Required lossless/error contract; gate UI adoption on Windows 10/11 conformance tests. |
| Folder dialog | rfd 0.17.2 on Windows | **Reject** | UTF-16 conversion can panic; cancellation and COM errors collapse; flags are insufficient. |
| Project paths | Direct Known Folder APIs through windows 0.62.2 | **Adopt** | Small target-gated code, no new crate, exact native path. |
| Project paths | directories 6.0.0 / directories-next 2.0.0 | **Reject** | Archived or stale dependency for portability not currently shipped. |
| Logging | tracing 0.1.44 + subscriber 0.3.23 + appender 0.2.5 | **Adopt** | Structured allowlist, bounded nonblocking queue, count retention, local-only sink. |
| Logging | `log` + env_logger 0.11.11 | **Reject** | Console/env focus; no retention, structured context, or export contract. |
| Logging | Application-owned byte-rotating writer | **Defer** | Additional concurrency/security code. Prototype only if volume or size gates fail. |
| Panic | Minimal local `std::panic` hook | **Adopt** | No new dependency or network; fixed private bounded marker. |
| Panic | color-eyre 0.6.5 / human-panic 2.0.8 | **Reject** | Rich console/backtrace reports exceed the privacy data budget. |
| Panic | Sentry 0.49.0 | **Reject** | Network/telemetry SDK conflicts with the local-only boundary. |

## Validation plan

### Preferences

- Pure document-store unit tests cover missing, current, every supported old, newer,
  malformed, oversized, and clamped values.
- Golden fixtures prove migrations are deterministic and preserve a newer raw
  value until explicit reset.
- Restart tests prove application settings and bounded window state round-trip
  without persisting a scan path by default.
- Read-only/missing AppData tests prove persistence disables nonfatally and
  never falls back beside the executable.

### CLI and dialog

- Native-unit path tests include unpaired UTF-16, emoji, spaces, `%`, `&`,
  option-like names, UNC, drive roots, extended prefixes, and trailing
  separators.
- Grammar tests cover zero/one/multiple values, `--`, unknown options,
  help/version, and sanitized error reporting.
- Windows integration tests verify COM STA ownership, existing-option
  preservation, exact added flags, owner HWND, cancel-versus-HRESULT mapping,
  non-filesystem rejection, and allocation release.
- Windows 10 and 11 manual/automated checks cover keyboard-only selection,
  screen-reader labels supplied by the native dialog, high DPI, drive roots,
  network roots, and one-request bounding.

### Diagnostics and panic

- Saturate the 4,096-line queue and verify application progress plus a
  nonzero dropped count. Verify `WorkerGuard` flush during orderly shutdown.
- Rotate across synthetic dates; prove only owned pattern matches are removed,
  at most eight log files remain, and no symlink or unrelated file is touched.
- Exercise denied, read-only, missing, and full destinations without blocking
  startup or scan.
- Property/fuzz tests feed path, URL, assignment, token, control-character,
  and oversized-line fixtures through export redaction. Assert the 8 MiB cap,
  16 KiB line cap, omission notices, preview, and unchanged source logs.
- Subprocess panic tests assert a sensitive payload and sensitive full path do
  not appear byte-for-byte, the marker is at most 4 KiB, unwritable paths do
  not recurse, and next-start preview/delete works.
- Inspect the final runtime dependency graph and run `cargo deny check`;
  specifically ensure no HTTP/TLS client, Sentry SDK, or telemetry exporter is
  reachable from diagnostics.

## Revisit triggers

- Reconsider the application-owned store only if a second frontend or
  transactional cross-process settings require a broader shared service. Do
  not re-enable an implicit framework path.
- Reconsider clap when the CLI gains subcommands, generated documentation, or
  completions; preserve `OsString` at the positional boundary.
- Reconsider rfd only for a future non-Windows adapter and only after its
  native path conversion and structured-error behavior meet the same tests.
- Replace count-based rolling with a byte-bounded local writer if release
  stress tests exceed the diagnostic disk budget.
- Consider a maintained cross-platform project-directory crate when a second
  desktop OS is actually supported.
- Any proposal for richer crash collection or upload requires a new privacy
  ADR, explicit opt-in UX, data inventory, retention policy, and network threat
  model. It is not an incremental change to this design.
