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

### tracing, tracing-subscriber, and tracing-appender

This stack supplies structured spans/events, `log` compatibility, a bounded
nonblocking writer with drop accounting, daily rolling, and count-based
retention. Features can exclude ANSI, regex filtering, JSON, and local-time
support.

### `log` with env_logger

The facade and backend are maintained and familiar, but env_logger is aimed at
stderr and environment filters. It does not provide spans, bounded async file
I/O, rolling retention, or DiskPie's export policy.

### Application-owned file writer

A small writer could rotate by bytes and reduce library code, but safe
concurrency, queue saturation, retention deletion, flush, and error accounting
are substantial infrastructure. It is justified only by measured failure of
the selected stack.

### Network observability SDK

A hosted SDK adds transport, identity/context, data-governance, and opt-in
surface that no requirement needs. It conflicts with the local-only boundary.

## Decision

DiskPie will use a local structured tracing pipeline and an explicit,
redacted diagnostic export.

1. Pin `tracing = 0.1.44`, `tracing-subscriber = 0.3.23`, and
   `tracing-appender = 0.2.5`. Disable default features. Enable only `std` on
   tracing and `fmt`, `std`, and `tracing-log` on tracing-subscriber. Leave
   tracing-appender's optional `parking_lot` feature off.
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
   Use daily rotation, a unique `diskpie` prefix and `.log` suffix, and a
   DiskPie-owned retention pass capped at eight exact log names. Do not enable
   tracing-appender 0.2.5's `max_log_files`: its pruning uses following
   metadata and, when Windows creation time is available, admits any regular
   entry that merely matches the prefix/suffix. The local pass accepts only
   `diskpie.YYYY-MM-DD.log`, uses non-following metadata, rejects reparse
   points, and never deletes directories, symlinks, or unrelated files.
7. Configure the nonblocking writer with a 4,096-line queue and lossy mode.
   Queue pressure drops diagnostics rather than application work. Own its
   `WorkerGuard` until services stop, and expose the appender error/drop count
   in an export.
8. Logging initialization or write failure is nonfatal and user-visible. Do
   not fall back to the executable/current directory or an unbounded stderr
   capture.
9. Diagnostic export is an explicit user action that writes a plain UTF-8
   `.txt` file to a user-selected native path. It has an 8 MiB total cap and a
   16 KiB input-line cap, prefers newest complete lines, and records omitted
   file/line counts plus appender drops.
10. Export includes only a fixed header, app/target/coarse OS versions,
    diagnostics-policy version, explicitly allowlisted non-path settings,
    aggregate metrics, sanitized recent logs, and presence of the minimal ADR
    0017 panic marker.
11. A second conservative pass redacts drive, UNC, and extended paths; URLs
    and query strings; environment-like assignments; token/password/
    authorization/cookie patterns; and control characters. The UI previews
    included categories and content before save.
12. Filesystem paths are excluded by default and require explicit consent on
    each export. Secret-like values remain redacted even with that consent.
13. Write to a temporary sibling and rename to the chosen destination. Refuse
    a destination equal to an owned setting/log/crash file and require native
    overwrite confirmation. Never modify the source logs during export.
14. No diagnostic action uploads data or initializes an HTTP/TLS client. A UNC
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
- tracing-appender rotation is time-based while DiskPie owns the narrow
  count-retention policy. Release volume tests gate acceptance; excessive
  output triggers a byte-rotation prototype.
- A daily appender uses a background thread and transitive channel/time code,
  increasing supply-chain and binary surface over a synchronous writer.
- Abrupt termination can lose queued log lines. The minimal synchronous panic
  marker records only that a panic occurred, not the lost event context.
- Conservative redaction can remove useful text. Preview and aggregate fields
  make that visible without weakening the default.

## Validation

- Saturation tests fill 4,096 lines and prove renderer/scanner progress plus a
  nonzero drop count. Shutdown tests prove the `WorkerGuard` is held until
  services stop and orderly data is flushed.
- Synthetic-date rotation tests prove at most eight exact matching regular
  files remain and unrelated files, directories, symlinks, junctions, and
  reparse points are untouched.
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

- Measured INFO output exceeds the release disk budget under the eight-file
  policy.
- Appender/channel compile time, binary size, or advisory risk exceeds an
  agreed release gate; prototype an application-owned byte-rotating writer.
- Richer filters or JSON are requested; add only the exact feature after
  measuring and preserving the event/privacy contract.
- Any automatic upload, remote observability, or richer data collection needs
  a new privacy ADR and explicit opt-in UX; it is not an implementation detail
  of this pipeline.
