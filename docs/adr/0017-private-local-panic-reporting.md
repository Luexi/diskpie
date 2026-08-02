# ADR 0017: Record only a minimal local panic marker

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

An uncaught panic can terminate a GUI process with little visible evidence.
DiskPie should acknowledge the unexpected exit on next launch and let the user
include that fact in diagnostics, but panic payloads, source paths, thread
names, backtraces, process environment, and memory can expose scan paths or
secrets. A hook also runs while the process may be inconsistent, so calling
the normal tracing pipeline or application locks can deadlock or panic again.

Framework comparison and the exact privacy boundary are recorded in
[app infrastructure research](../research/app-infrastructure.md#crash-and-panic-behavior).

## Decision drivers

- No telemetry, crash upload, background networking, or external service.
- A bounded, useful next-start signal with minimal sensitive content.
- No dependency on the async logger, terminal, allocator-heavy report
  formatting, or application locks.
- Best-effort behavior that cannot turn an initial panic into recursive panic.
- Explicit user preview/export/delete controls.
- No new crash-reporting dependency or network-capable runtime graph.

## Options considered

### Minimal application-owned `std::panic` hook

The standard hook can write a fixed local marker before normal panic handling
continues. DiskPie controls every field and adds no crate or transport.

### color-eyre 0.6.5

color-eyre is designed for rich terminal error context, span traces, and
backtrace presentation. Its useful data is intentionally broader than the
approved GUI marker.

### human-panic 2.0.8

human-panic creates a local TOML report rather than automatically uploading,
but includes panic cause, source location, backtrace, and system metadata.
Those can contain user paths and payload secrets and require more dependencies.

### Sentry 0.49.0

Sentry is an active observability SDK whose default graph includes transport
and panic/context collection. Even configured inactive, it introduces a
network/telemetry policy surface for no required local capability.

## Decision

DiskPie will install a minimal application-owned panic hook and reject rich or
remote crash-reporting frameworks.

1. Resolve ADR 0018 project paths and prepare the local `crashes` directory
   before starting UI, scan, or Shell threads. Then install one
   `std::panic::set_hook` for the process.
2. The hook best-effort replaces `crashes/last-panic.txt` with a fixed-format
   marker of at most 4 KiB. Keeping one marker bounds retention.
3. Allowed fields are marker schema, Unix timestamp, app version, target OS
   and architecture, a fixed `uncaught_panic` code, and an application-assigned
   static thread role or `unknown`.
4. The marker excludes the panic payload, source file and line, raw thread
   name, backtrace, arguments, environment, current directory, username,
   hostname, settings, scan paths, memory, file contents, and arbitrary error
   text. It contains no user-derived strings.
5. Hook formatting uses a fixed bounded buffer and simple standard-library
   operations. It does not call tracing, allocate an unbounded report, acquire
   application locks, unwrap, expect, or panic on directory, clock, open,
   truncate, write, or flush failure.
6. Save and replace are best effort; inability to write produces no fallback
   in the executable/current/temporary directory and does not recurse.
7. In debug builds, the previous hook may be invoked after the local write to
   aid development. Release GUI behavior does not depend on stderr or expose
   the excluded fields through the default hook.
8. At the next launch, marker presence produces only "DiskPie ended
   unexpectedly" with preview, include-in-export, and delete actions. DiskPie
   does not infer or display a cause it did not collect.
9. No panic or next-start action sends data or initializes a network client.
   Export remains the explicit, previewed ADR 0016 filesystem action.
10. color-eyre, human-panic, Sentry, and equivalent rich/remote panic reporters
    are rejected for the production application.

## Consequences

Positive outcomes:

- A user gets a truthful next-start signal without collecting panic content or
  filesystem context.
- The hook is independent of the possibly blocked asynchronous logger.
- Storage, field count, and lifetime are strictly bounded.
- No crash SDK, HTTP/TLS dependency, account, retention service, or consent
  state is introduced.
- The marker is easy to preview, export deliberately, or delete.

Accepted tradeoffs:

- The marker cannot diagnose the panic cause by itself. Normal allowlisted logs
  and reproducible tests carry that burden.
- A power loss, process abort before hook execution, stack exhaustion, or
  unwritable Local AppData can leave no marker.
- Replacing one marker loses earlier panic history. Bounded privacy is more
  important than an automatic crash archive.
- The release hook suppresses the default rich console panic output; debug
  builds retain it for developers.
- A concurrent second panic can race the same marker. Either valid bounded
  marker is sufficient; no cross-thread locking is added in the panic path.

## Validation

- Subprocess tests panic from main, UI-named, worker-named, and unnamed threads
  and verify a marker appears without deadlock.
- Sensitive panic payloads, full source paths, arguments, environment values,
  usernames, hostnames, scan paths, and raw thread names are asserted absent
  byte-for-byte.
- Every marker is valid fixed-format text at or below 4 KiB and contains only
  the documented keys and bounded values.
- Read-only, missing, full, and permission-denied crash locations prove the
  hook returns without recursive panic and creates no fallback file.
- Concurrent-panic and intentionally failing-writer subprocesses prove the
  process terminates rather than deadlocking in the hook.
- Next-start tests cover preview, explicit include in ADR 0016 export, delete,
  and no inferred-cause wording.
- Final dependency inspection proves no crash SDK or HTTP/TLS transport was
  introduced.

## Revisit triggers

- More crash detail is proposed. Any added field requires a documented data
  inventory, sensitivity analysis, size bound, preview behavior, and new
  privacy ADR.
- Automatic upload or remote crash correlation is proposed. It requires
  explicit opt-in product design, network threat modeling, retention/deletion
  policy, and legal review; it cannot amend this ADR silently.
- Evidence shows the hook itself contributes to recursive failure; simplify or
  disable the marker rather than collecting broader state.
