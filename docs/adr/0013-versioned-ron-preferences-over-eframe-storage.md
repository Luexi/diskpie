# ADR 0013: Store versioned RON preferences through eframe Storage

- Status: Superseded by ADR 0019
- Date: 2026-08-02
- Owners: DiskPie maintainers

ADR 0019 preserves the versioned RON schema and storage-neutral application
port, but replaces eframe's native storage backend. Inspection of the pinned
eframe 0.35.0 implementation showed that a missing `persistence_path` selects
eframe's default platform path instead of disabling persistence. That behavior
conflicts with ADR 0018's no-fallback rule.

## Context

DiskPie needs durable, user-local preferences and restored window state without
making the portable scanner depend on eframe. The persisted data will evolve,
so defaults alone do not cover migrations, a settings file written by a newer
version, malformed data, or privacy-sensitive fields.

eframe 0.35.0 already provides a string key/value `Storage` lifecycle. Its
`persistence` feature already depends on Serde and RON, saves the framework's
outer map in `app.ron`, and accepts an application-selected
`NativeOptions::persistence_path`. Its current native save is asynchronous but
uses `File::create`, not atomic file replacement.

Candidate and source analysis is recorded in
[app infrastructure research](../research/app-infrastructure.md#persistent-preferences).

## Decision drivers

- Explicit schema evolution and protection from newer unknown data.
- Safe defaults without destroying the only recoverable value.
- No persisted scan-path history, arguments, or secrets by default.
- One already-paid serialization format and a small dependency graph.
- No eframe type in the core scanner or application policy.
- Bounded input and deterministic validation/migration tests.

## Options considered

### Versioned Serde/RON value in eframe Storage

This reuses the framework lifecycle and RON dependency while giving DiskPie a
single application-owned schema and migration boundary. A consumer-owned
settings port keeps eframe outside portable code.

### Serde JSON in eframe Storage or a separate file

JSON is maintained and interoperable, but DiskPie has no public interchange or
framework-independent settings requirement. It would add a second format
without fixing eframe's outer-file behavior.

### Application-owned atomic settings backend now

A separate backend can use atomic replacement and support other frontends, but
it duplicates lifecycle and window-state storage before either need is proven.

### One unversioned eframe key per preference

This looks simple initially but scatters defaults and makes cross-field
migration, newer-version protection, bounded validation, and reset behavior
ambiguous.

## Decision

DiskPie will store one namespaced, versioned settings envelope as RON through
eframe `Storage`.

1. Pin Serde 1.0.229 with derive support. Reuse RON 0.12.2 selected by eframe
   0.35.0's `persistence` feature; do not add serde_json for this frontend.
2. A consumer-owned `SettingsStore` port exposes plain settings values and
   load/save outcomes. Only the eframe adapter sees `eframe::Storage`.
3. The value uses a stable key such as `diskpie.settings` and contains an
   explicit `schema_version` plus the versioned settings payload.
4. Serialized input and output are limited to 64 KiB. Every load validates and
   clamps ranges, finite numbers, enums, scale, thresholds, and retention.
5. Missing data loads defaults. Supported older versions migrate through
   deterministic, pure, stepwise functions. The current version validates
   directly.
6. A malformed, oversized, or newer unknown value starts with safe
   session-only defaults, raises a nonfatal UI warning and stable diagnostic
   code, and remains untouched until the user explicitly changes or resets
   settings.
7. Serde defaults may handle additive fields inside a supported version, but
   do not replace the version number. Unknown fields are tolerated; do not use
   `deny_unknown_fields`.
8. The persisted payload excludes the last scan root, recent paths, file
   names, CLI arguments, credentials, secrets, and filesystem history by
   default.
9. `ProjectPaths` from ADR 0018 supplies eframe's explicit persistence path.
   Failure to resolve or create it disables persistence nonfatally; DiskPie
   does not fall back beside the executable or into the current directory.

## Consequences

Positive outcomes:

- Settings evolution and corruption behavior are explicit and testable.
- The UI framework's existing serialization and save lifecycle are reused.
- Portable services can use a fake port without eframe or filesystem access.
- A future version cannot be silently downgraded and overwritten.
- Default persistence does not become an implicit scan-history database.

Accepted tradeoffs:

- The outer `app.ron` map remains an eframe implementation detail, not a
  DiskPie public format.
- In eframe 0.35.0, malformed outer RON loads as an empty map and saves are not
  atomically replaced. Preferences are nonessential, so this is accepted with
  a corruption-monitoring revisit trigger.
- RON is less broadly interoperable than JSON; no current consumer requires
  interchange.
- A warning/default session can surprise a user, but preserving incompatible
  bytes is safer than destructive recovery.

## Validation

- Fake-Storage unit tests cover missing, current, every supported old, newer,
  malformed, oversized, and invalid-range values.
- Golden fixtures prove every migration is deterministic and that newer raw
  data survives startup/shutdown until explicit reset.
- Restart tests prove DiskPie settings coexist with eframe window state and
  that scan roots and CLI paths are absent from the persisted value.
- Read-only, missing, and denied AppData tests prove startup and scanning
  continue with persistence disabled and no surprise fallback file.
- Dependency checks prove portable crates do not import eframe, RON, or
  filesystem storage types.

## Revisit triggers

- Reproducible preference loss attributable to eframe's non-atomic outer save.
- A second frontend or headless mode needs a framework-independent store.
- Transactional or multi-process settings become a product requirement.
- Persisting a recent path is proposed; that requires an explicit privacy and
  opt-in decision rather than an additive field alone.
