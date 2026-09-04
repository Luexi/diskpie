# ADR 0016: Keep diagnostics local, bounded, structured, and explicit

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie needs enough diagnostic context to investigate scan and platform
failures without freezing the renderer, filling a disk, retaining filesystem
history, or sending data to a service. Dependencies also emit through the
`log` facade. A GUI distributed as a portable executable cannot rely on a
terminal, environment-configured logger, or writable executable directory.

Diagnostic export must be useful for support while remaining a deliberate,
reviewable user action. Candidate versions, feature costs, retention behavior,
and privacy analysis are recorded in
[app infrastructure research](../research/app-infrastructure.md#local-diagnostics-and-export).

## Decision drivers

- Local-only operation with no telemetry or background diagnostic network
  client.
- Structured, allowlisted fields instead of path-rich free-form messages.
- Nonblocking renderer/scanner behavior and explicit queue bounds.
- Bounded retention, line size, export size, and panic-independent flush
  behavior.
- Compatibility with upstream `log` events without adopting stderr as the
  backend.
- Explicit preview and consent before creating a shareable artifact.
- Narrow features, current maintenance, acceptable licensing, and an audited
  dependency graph.

## Options considered

### tracing, tracing-subscriber, and tracing-appender rolling files

This stack supplies structured spans/events, `log` compatibility, a bounded
nonblocking writer with drop accounting, and daily rolling. Its rolling writer
opens a joined path with `std::fs`, however, so it cannot prove that the root
and active file are local, non-reparse, single-link objects before the first
write. Its built-in retention also follows metadata and accepts names outside
DiskPie's exact namespace. Use the queue, not its rolling filesystem sink.

### `log` with env_logger

The facade and backend are maintained and familiar, but env_logger is aimed at
stderr and environment filters. It does not provide spans, bounded async file
I/O, rolling retention, or DiskPie's export policy.

### Application-owned native file writer

A narrow Win32 writer can bind the active directory and file by handle,
append without following a reparse point, and enumerate retention from that
same directory handle. Keeping queue saturation and drop accounting in the
pinned nonblocking adapter limits the custom surface to the filesystem
boundary. This is selected because the generic rolling sink cannot enforce the
local-only and object-identity contract.

### Network observability SDK

A hosted SDK adds transport, identity/context, data-governance, and opt-in
surface that no requirement needs. It conflicts with the local-only boundary.

## Decision

DiskPie will use a local structured tracing pipeline and an explicit,
redacted diagnostic export.

1. Pin `tracing = 0.1.44`, `tracing-subscriber = 0.3.23`, and
   `tracing-appender = 0.2.5`. Disable default features. Enable only `std` on
   tracing and `fmt`, `std`, and `tracing-log` on tracing-subscriber. Leave
   tracing-appender's optional `parking_lot` feature off. Use
   tracing-appender only for its bounded nonblocking queue and `WorkerGuard`,
   never for rolling-file creation or retention.
2. Do not enable tracing attributes, ANSI, `env-filter`, regex, JSON,
   local-time, chrono, or user-provided filter expressions without a measured
   need and dependency review. File formatting explicitly disables ANSI.
3. A validated setting chooses from fixed maximum levels, with INFO as the
   production default. Successful per-file scan events are prohibited at
   normal levels.
4. Production events use stable codes and an allowlist: app version, target,
   coarse OS version, in-process generation/request IDs, counts, durations,
   queue depths, allocation classes, and policy enums.
5. Events must not contain paths or basenames, filenames, CLI arguments,
   current directory, environment, usernames, hostnames, URLs, file contents,
   volume labels, credentials, secrets, Shell payloads, or serialized
   settings. Upstream targets default to WARN/ERROR and are scrubbed again on
   export.
6. Write only beneath ADR 0018's dedicated Local AppData `logs` directory.
   An application-owned Win32 writer opens and retains a local, non-reparse
   directory handle, opens each UTC-dated `diskpie.YYYY-MM-DD.log` with append
   access plus `FILE_FLAG_OPEN_REPARSE_POINT`, and verifies a regular direct
   child with one link before writing. It retains that handle until UTC
   rollover, then repeats the complete validation. Root, leaf, hard-link,
   sharing, or transport ambiguity disables the sink without fallback.
   Before each append, verify that the complete record keeps the active file
   at or below 16 MiB; reject the whole record and increment a path-free sink
   health counter rather than partially crossing the budget.
7. Retention runs in the writer thread against the retained directory handle,
   never through `fs::read_dir` or a reopened raw path. It uses bounded native
   handle enumeration, linear work, an explicit entry budget, and protects the
   actual active handle/identity. It retains at most eight exact UTC names in
   the managed namespace, always including the active identity. An exact
   syntactically valid future date is part of that reserved namespace and does
   not escape the cap. The pass never deletes directories, reparse points,
   multiply linked files, identity-changed entries, or unrelated names.
8. Configure the nonblocking writer with a 4,096-line queue and lossy mode.
   Queue pressure drops diagnostics rather than application work. Own its
   `WorkerGuard` until services stop, and expose the appender error/drop count
   in an export.
9. Logging initialization or write failure is nonfatal and user-visible. Do
   not fall back to the executable/current directory or an unbounded stderr
   capture.
10. Diagnostic export is an explicit user action that writes a plain UTF-8
   `.txt` file to a user-selected native path. It has an 8 MiB total cap and a
   16 KiB input-line cap, prefers newest complete lines, and records omitted
   file/line counts plus appender drops.
11. Export includes only a fixed header, app/target/coarse OS versions,
    diagnostics-policy version, explicitly allowlisted non-path settings,
    aggregate metrics, sanitized recent logs, and presence of the minimal ADR
    0017 panic marker.
12. A second conservative pass redacts drive, UNC, and extended paths; URLs
    and query strings; environment-like assignments; token/password/
    authorization/cookie patterns; and control characters. The UI previews
    included categories and content before save.
13. Filesystem paths are excluded by default and require explicit consent on
    each export. Secret-like values remain redacted even with that consent.
14. Preflight the selected export parent by handle and classify its final
    transport before the overwrite/network confirmation; drive-letter syntax
    alone cannot distinguish a mapped UNC destination. Write to an exclusively
    created temporary sibling and replace with a parent-handle-relative native
    operation while the inspected destination identity remains locked. Refuse
    a destination equal or hard-linked to an owned setting/log/crash file,
    clean every pre-commit temporary through an immediate RAII guard, and
    report committed state distinctly from pre-commit failure. Never modify
    the source logs during export.
15. No diagnostic action uploads data or initializes an HTTP/TLS client. A UNC
    export can use the network only because the user explicitly selected that
    filesystem destination; the confirmation UI labels this fact.

## Consequences

Positive outcomes:

- Scan and renderer progress do not depend on diagnostic disk speed.
- Structured event policy removes most sensitive data before it reaches a
  file; export redaction is defense in depth.
- Retention and export bounds prevent unlimited diagnostic growth.
- Support artifacts are inspectable without archive tooling and exist only
  after an explicit user decision.
- `log`-emitting dependencies can be captured without making their free-form
  output the primary application schema.

Accepted tradeoffs:

- Lossy mode can omit the exact event near a saturated failure. The dropped
  count and bounded application behavior are preferred to blocking.
- The native daily writer and handle enumerator add reviewed Windows-specific
  unsafe code. They keep the local-only boundary enforceable and leave queue
  concurrency in the pinned library.
- The 16 MiB daily cap can omit late-day diagnostics under abnormal volume.
  The health count and export make the omission explicit; application work
  remains preferable to unbounded diagnostic growth.
- Abrupt termination can lose queued log lines. The minimal synchronous panic
  marker records only that a panic occurred, not the lost event context.
- Conservative redaction can remove useful text. Preview and aggregate fields
  make that visible without weakening the default.

## Validation

- Saturation tests fill 4,096 lines and prove renderer/scanner progress plus a
  nonzero drop count. Shutdown tests prove the `WorkerGuard` is held until
  services stop and orderly data is flushed.
- Synthetic UTC rotation tests prove at most eight exact matching regular
  files remain and unrelated files, directories, symlinks, junctions, hard
  links, and reparse points are untouched or fail the facility closed. Exact
  future-dated regular names remain inside the same eight-file cap and cannot
  evict the active identity. Root/leaf redirection and UNC targets receive no
  log bytes.
- Retention stress proves handle-bound enumeration is linear, observes its
  explicit entry budget, runs outside startup/UI callbacks, and cannot revisit
  the full directory once per deletion batch.
- Byte-boundary tests prove one full record may reach exactly 16 MiB, a record
  that would cross it is wholly omitted and counted, and an oversized existing
  active file receives no additional bytes.
- Read-only, missing, full, and permission-denied destinations disable the
  affected facility without stopping startup or scan.
- Property and golden tests cover native path forms, URLs, assignments,
  tokens, authorization/cookies, control characters, oversized lines,
  omission notices, preview, and unchanged source files.
- Exports are asserted at or below 8 MiB, every input line is capped at
  16 KiB, owned-file destinations are rejected, and UNC intent is labeled.
- Dependency review and `cargo deny check` confirm the locked advisory/license
  state and that no HTTP/TLS client, Sentry SDK, or telemetry exporter is
  reachable from diagnostics.
- Release stress tests measure INFO log bytes per hour/day; a failed disk
  budget gate blocks release or invokes the byte-bounded-writer revisit.

## Revisit triggers

- Measured INFO output routinely reaches the 16 MiB daily cap or support needs
  justify a different fixed total budget.
- Queue compile time, binary size, or advisory risk exceeds an agreed release
  gate; prototype a smaller bounded channel without weakening the native sink.
- Richer filters or JSON are requested; add only the exact feature after
  measuring and preserving the event/privacy contract.
- Any automatic upload, remote observability, or richer data collection needs
  a new privacy ADR and explicit opt-in UX; it is not an implementation detail
  of this pipeline.
