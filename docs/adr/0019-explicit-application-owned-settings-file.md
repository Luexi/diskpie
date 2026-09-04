# ADR 0019: Own the explicit settings file outside eframe persistence

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers
- Supersedes: ADR 0013; ADR 0018 decision item 10

## Context

DiskPie must persist preferences only at the exact roaming path returned by
the Windows Known Folder adapter. If that path cannot be resolved or created,
persistence must be disabled without writing beside the executable, in the
current directory, in a temporary directory, or at a framework-selected
fallback.

ADR 0013 assumed that passing `None` as
`NativeOptions::persistence_path` disabled native eframe persistence. The
pinned eframe 0.35.0 source proves otherwise. Both native renderer paths call
`create_storage_with_file` when the option is `Some`, but call
`create_storage(app_id)` when it is `None`. With the `persistence` feature,
that second function derives eframe's default platform directory and creates
storage there. `persist_window = false` suppresses window-state serialization;
it does not suppress storage construction or application saves.

The same implementation stores an outer key/value map and writes it through
`File::create`. It therefore cannot provide the application-owned whole-file
format or replacement contract needed for deterministic corruption recovery.
There is no runtime switch that keeps the feature for successful Known Folder
resolution while disabling all storage on failure.

The evidence is local source for the exact locked dependency:

- `eframe-0.35.0/src/native/glow_integration.rs::init_run_state`;
- `eframe-0.35.0/src/native/wgpu_integration.rs::init_run_state`;
- `eframe-0.35.0/src/native/epi_integration.rs::{create_storage,
  create_storage_with_file}`;
- `eframe-0.35.0/src/native/file_storage.rs`.

For the Windows commit boundary, Microsoft's
[`FILE_RENAME_INFORMATION`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information)
contract states that POSIX rename semantics allow replacement while existing
target handles remain valid, and the
[`FileRenameInformationEx`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ne-wdm-_file_information_class)
class is available from Windows 10 version 1709. This permits DiskPie to
retain the inspected target identity through commit and fail persistence
closed on older/unsupported systems. The retained target handle grants
read, write, and delete sharing: POSIX replacement requires every open handle
on the target to permit delete sharing, so a handle that denied it would block
DiskPie's own commit.

The handle-relative form of that request is only reachable through the native
system call. Measured on Windows 11 build 26200 against an NTFS volume,
`SetFileInformationByHandle` returns `ERROR_INVALID_PARAMETER` (87) for every
rename whose `RootDirectory` member is not null, with both `FileRenameInfo`
and `FileRenameInfoEx`, regardless of flags or buffer size. The same buffer
passed to `NtSetInformationFile` with `FileRenameInformationEx` (class 65)
succeeds, honours `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` with
`FILE_RENAME_FLAG_POSIX_SEMANTICS`, and returns
`STATUS_OBJECT_NAME_COLLISION` for a create-only rename whose target already
exists. The adapter therefore issues the rename through
`NtSetInformationFile` and translates the `NTSTATUS` with
`RtlNtStatusToDosError` before classification. The Win32 wrapper remains in
use only for information classes that take no directory handle.

## Decision drivers

- Enforce ADR 0018's no-fallback rule by construction.
- Preserve newer, malformed, and oversized data until an explicit user edit.
- Never expose the renderer to settings filesystem I/O.
- Use one bounded, versioned, path-free RON document.
- Atomically replace an owned regular file without following a reparse point,
  alternate data stream, or attacker-selected sibling.
- Continue startup and scanning when settings are unavailable.
- Keep eframe, native paths, and filesystem types out of portable policy.

## Options considered

### Keep eframe persistence and pass an invalid sentinel path on failure

This avoids the default-path branch, but deliberately constructing a device,
invalid, or unwritable path is not a supported disable mechanism. It can emit
repeated write errors, has platform-specific semantics, and still retains the
non-atomic outer map.

### Build two executables with and without the persistence feature

Compile-time variants could select the correct behavior, but path availability
is a runtime property. Shipping two binaries cannot choose safely after Known
Folder resolution and would double release/testing surface.

### Patch or fork eframe with a runtime storage-disable option

An upstreamable option would solve only fallback selection. DiskPie would
still inherit the outer-map format and non-atomic save, while carrying a UI
framework fork and update burden for a small adapter.

### Disable eframe persistence and own the settings file

The existing `SettingsStore` port and RON schema already isolate policy. A
small executable/platform adapter can read the bounded document before the UI,
publish only in-memory values to the UI, and write after the native event loop
or on a dedicated bounded worker. On Windows, replacement can reuse the same
reviewed handle-relative, non-reparse filesystem boundary as diagnostic
artifacts.

## Decision

DiskPie will disable eframe's `persistence` feature and own the exact settings
file selected by `ProjectPaths`.

1. Retain the versioned `SettingsEnvelope`, validation, overwrite protection,
   64 KiB input/output limit, RON 0.12.2, and storage-neutral `SettingsStore`
   policy from ADR 0013.
2. Serialize the envelope as the entire `app.ron` document. Do not wrap it in
   eframe's key/value map and do not treat that outer format as compatible.
3. Resolve and create only the ADR 0018 settings directory before reading.
   Absence or failure yields an unavailable store; it never selects a second
   path.
4. Read before creating the native window. Bound bytes before full parsing,
   parse only the schema header before rejecting a newer version, and never
   log the path or raw document.
5. Keep the live `SettingsSession` in application memory. UI changes update
   that session without filesystem I/O in an egui callback.
6. A successful save writes one validated, bounded document to an exclusively
   created sibling in the already-opened, non-reparse settings directory,
   flushes it, and atomically replaces the owned destination using
   `NtSetInformationFile` with `FileRenameInformationEx`,
   `FILE_RENAME_FLAG_REPLACE_IF_EXISTS`, and
   `FILE_RENAME_FLAG_POSIX_SEMANTICS` when an inspected destination already
   exists. An initially missing destination uses the same extended information
   class without replacement flags, so a concurrently created entry makes the
   rename fail atomically. Supply the retained parent handle plus a plain
   relative name; do not resolve a second absolute parent or pre-delete the
   destination. `SetFileInformationByHandle` cannot carry a `RootDirectory`
   and must not be used for this rename.
7. Refuse a destination that is a directory, reparse point, hard-linked file,
   alternate data stream, changed identity, or outside the resolved parent.
   Keep the inspected destination handle open through the namespace commit so
   the comparison and replacement refer to the same object. Preserve the prior
   file and report a stable sanitized error when ownership cannot be proved.
   Retain whether the destination was missing at read time; if an entry appears
   under that name before publication, treat it as a conflict and do not
   replace it. This closes the initially-missing race without inventing or
   truncating a placeholder during startup. If an inspected destination has
   instead disappeared before publication, there is no prior file left to
   protect: publish with create-only semantics, so a concurrent recreation
   still fails atomically and later publications resume replacement.
   If the extended rename class or its POSIX replacement semantics are not
   supported, fail settings publication closed; do not fall back to the racy
   `FileRenameInfo`, `ReplaceFileW`, `File::rename`, or framework storage.
8. Complete pending settings publication after the event loop exits or on a
   bounded coalescing worker. Startup, scanning, and shutdown remain usable if
   the save fails. The native adapter must distinguish `NotCommitted`,
   `Committed`, and, if a filesystem requires it, `CommittedButUnverified`;
   it must never return an ordinary pre-commit error after the namespace was
   already changed. If `NtSetInformationFile` reports failure, inspect the
   still-open source identity and the target entry before classification:
   observing the source identity at the target name is a committed outcome,
   while an unchanged original target is not committed. If the rename
   succeeded but the post-commit inspection cannot prove the result, report
   `CommittedButUnverified`, delete nothing, and re-inspect the target through
   the retained parent before the next publication instead of comparing
   against the stale identity. Never infer the outcome from an error code
   alone.
9. Store no scan root, recent item, CLI argument, filename, credential, or
   secret. Window placement may be added only as bounded numeric state with
   monitor clamping and no filesystem content.
10. On non-Windows targets, use an explicit platform adapter with equivalent
    semantics or expose settings as unavailable. Do not reintroduce eframe's
    implicit platform path to make a portable build appear persistent.

## Consequences

Positive outcomes:

- A missing Known Folder path cannot silently create a fallback file.
- The whole file has one documented application schema and recovery policy.
- Atomic replacement and object ownership can share one audited Windows
  boundary with diagnostic files.
- The renderer never waits for settings I/O.
- Removing eframe persistence also removes framework-only storage behavior and
  its implicit path dependency from the release graph.

Accepted tradeoffs:

- DiskPie owns a small amount of lifecycle and platform adapter code.
- eframe no longer restores its private widget memory automatically.
- Window geometry requires a narrow application-owned schema if restoration is
  implemented; it cannot be obtained by quietly re-enabling the feature.
- Windows 10 builds before 1709, or filesystems that reject extended POSIX
  rename semantics, run normally with session-only preferences. They do not
  receive a weaker replacement fallback.
- Other desktop targets remain explicitly nonpersistent until they implement
  an equivalent adapter.

## Validation

- Dependency and feature tests prove eframe persistence is disabled.
- A changed current directory, read-only executable directory, missing Known
  Folder, and misleading environment variables create no settings file.
- Golden tests cover missing, current, newer, malformed, oversized, truncated,
  unknown-field, invalid-range, and non-Unicode parent paths.
- Restart tests prove valid edits round-trip while untouched incompatible bytes
  survive startup and shutdown exactly.
- Windows tests cover regular create/replace, destination reparse points,
  hard links, alternate data streams, parent replacement, long paths, sharing
  violations, a target handle retained through commit, an initially missing
  target that appears before commit, unsupported `FileRenameInfoEx`, injected
  post-rename errors, interrupted publication, and orphan recovery.
- UI responsiveness tests prove no settings read, parse, encode, flush, or
  replacement occurs in the egui callback.
- Privacy tests assert the persisted document contains no startup or scan path.

## Revisit triggers

- eframe adds a supported runtime-disabled storage mode and atomic
  application-owned whole-file backend; both are required before reconsidering
  the feature.
- A second frontend needs the same store and justifies moving the executable
  adapter into a reusable non-UI crate.
- Multi-process settings become a requirement and need an explicit lock and
  conflict policy.
