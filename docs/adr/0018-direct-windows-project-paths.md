# ADR 0018: Resolve project paths with Windows Known Folder APIs

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie needs stable locations for roaming preferences and machine-local logs
and crash markers. A portable executable describes packaging, not a promise to
write beside itself. The executable directory may be read-only or removable,
and current-directory, environment-variable, and temporary fallbacks are
surprising or lossy.

The Windows baseline already uses windows 0.62.2. The `directories` 6.0.0
repository is archived/read-only, while `directories-next` 2.0.0 is older.
Neither adds enough value before DiskPie supports a second desktop OS.

Candidate evidence and Microsoft API contracts are recorded in
[app infrastructure research](../research/app-infrastructure.md#project-paths).

## Decision drivers

- Windows-recommended per-user locations with standard-user access.
- Lossless native path handling and exact HRESULT preservation.
- No write beside the executable, current-directory guess, or ephemeral
  fallback.
- Roaming preferences separated from non-roaming diagnostics.
- One existing Windows binding graph and minimal platform-only unsafe.
- A portable consumer-owned contract with deterministic fake tests.
- No abandoned or duplicate platform-directory dependency.

## Options considered

### directories 6.0.0

Its `ProjectDirs` API maps applications to appropriate cross-platform config,
data, and cache locations with a safe surface. Its GitHub repository was
archived in 2025 and moved elsewhere, and its platform dependency duplicates
capability already available through windows-rs.

### directories-next 2.0.0

This fork exposes similar locations but its current release dates to 2020. It
does not improve the maintenance or dependency case.

### Environment variables or executable-relative paths

`APPDATA`/`LOCALAPPDATA` guesses avoid FFI but can be missing, relative, or
modified. Executable/current-directory storage fails on read-only portable
media and leaks state into an unexpected folder.

### `SHGetKnownFolderPath` through windows 0.62.2

Microsoft recommends the Known Folder API for new code. Direct use returns the
exact native per-user path, preserves HRESULT detail, and reuses the accepted
target-gated bindings.

## Decision

DiskPie will provide `ProjectPaths` with direct Windows Known Folder APIs
through exact windows 0.62.2.

1. The Windows platform adapter calls `SHGetKnownFolderPath` for
   `FOLDERID_RoamingAppData` and `FOLDERID_LocalAppData`. It uses no
   environment-variable or executable-directory substitute.
2. Append the stable application identifier `io.github.luexi.diskpie`.
   Preferences use
   `%APPDATA%/io.github.luexi.diskpie/data/app.ron`; diagnostics use
   `%LOCALAPPDATA%/io.github.luexi.diskpie/logs` and
   `%LOCALAPPDATA%/io.github.luexi.diskpie/crashes`.
3. The slash notation above is descriptive. Implementation joins native path
   components with `PathBuf` and never constructs or parses a display string.
4. The Microsoft contract requires freeing the returned allocation whether
   the API succeeds or fails. Because the generated windows 0.62.2
   `Result<PWSTR>` convenience function exposes the output only on success,
   use one minimal target-gated raw ABI call with windows types/binding support
   so an RAII `CoTaskMemFree` guard exists before the HRESULT is examined. Do
   not add windows-sys solely for this call or assume a failure pointer is
   null.
5. Copy a successful `PWSTR` losslessly into `OsString::from_wide`, including
   native UTF-16 that is not valid Unicode. Never convert through `String` or a
   display form.
6. Preserve the exact HRESULT/category internally and expose a sanitized plain
   `ProjectPathError` across the adapter boundary. Do not log the resolved
   absolute path.
7. A consumer-owned `ProjectPaths` contract returns owned `PathBuf` values.
   Portable core/scanner crates do not import Windows types or choose storage
   locations.
8. Create only the needed directories with standard-user filesystem APIs.
   Resolution or creation failure disables the affected persistence/log/crash
   facility nonfatally and produces a user-visible warning.
9. There is no fallback to current directory, executable directory, guessed
   environment variables, temp, drive root, or a lossy path. Diagnostics and
   settings never silently migrate between locations.
10. Pass the settings file to eframe through
   `NativeOptions::persistence_path`. ADRs 0016 and 0017 receive the local log
   and crash directories through the same adapter.
11. Reject directories and directories-next for the Windows baseline. Add a
    separate adapter or reconsider a maintained cross-platform crate only when
    another desktop target is funded.

## Consequences

Positive outcomes:

- Preferences roam according to Windows policy while logs and crash markers
  remain machine-local.
- Single-file distribution works from read-only/removable locations without
  trying to mutate the package directory.
- Native path identity, allocation ownership, and OS errors are explicit and
  auditable.
- No new directories/dirs-sys binding graph or archived dependency is added.
- Portable services can use fake paths and failure outcomes in tests.

Accepted tradeoffs:

- The adapter contains a small Windows FFI/native-string ownership boundary.
  It needs target-gated unsafe review and RAII tests.
- A future macOS or Linux frontend must implement its own project-path adapter
  or justify a then-current shared crate.
- Disabling persistence/diagnostics on failure loses that optional capability,
  but is safer than writing to a surprising location.
- The stable application identifier and directory layout become migration
  concerns after release and must not change casually.

## Validation

- Windows tests compare returned roots to direct Known Folder results for a
  standard user and assert the exact stable child layout.
- Allocation instrumentation/error injection proves `CoTaskMemFree` runs for
  success and failure and HRESULT categories survive mapping.
- Native-unit fixtures validate the UTF-16-to-`OsString` conversion without a
  `String` or lossy display round trip.
- Read-only executable media, changed current directory, absent/misleading
  environment variables, and denied AppData tests prove no fallback file is
  created.
- Independent failures for settings, logs, and crashes prove the remaining
  facilities and scanning continue where possible with a user-visible warning.
- Dependency checks assert directories/directories-next are absent and
  portable crates contain no Windows imports.
- Diagnostic export tests assert resolved absolute project paths are not
  included by default.

## Revisit triggers

- A second desktop OS is approved and a maintained project-directory crate
  demonstrably removes more adapter code than it adds.
- Microsoft changes or deprecates Known Folder behavior on a supported Windows
  baseline.
- The application identifier must change; define an explicit migration before
  writing to the new layout.
- Enterprise policy requires a configurable storage root; add an explicit,
  validated setting or deployment mechanism, not an implicit environment
  fallback.
