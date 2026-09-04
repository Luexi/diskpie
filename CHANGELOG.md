# Changelog

All notable changes to DiskPie will be documented in this file.

The format is based on Keep a Changelog, and the project follows Semantic
Versioning after its first stable release.

## [Unreleased]

### Added

- Initial clean-room repository, licensing, contribution policy, and traceable
  product requirements.
- Portable domain model with exact logical and allocated aggregation, hard-link
  identity, reparse and cloud-placeholder boundaries, and the bounded sunburst
  layout engine.
- Bounded, cancellable scan coordinator with fixed workers, per-volume
  permits, batched events, and generation identity.
- Windows adapters: conventional filesystem enumeration with native paths,
  volume discovery and root resolution, a message-pumping Shell STA with a
  bounded shutdown, retained-handle settings and diagnostic files, and
  project-path resolution.
- Connected application: native folder dialog and drive list, live sunburst
  with hover details, keyboard-operable largest-items list, zoom, back,
  parent, hide and restore, full rescan, cancel, metric switch, telemetry,
  and the required scan states.
- Persistent preferences in an application-owned `app.ron`, local bounded
  daily logs with redaction, and a private local panic marker with
  clean-shutdown reconciliation.
- English and Spanish user interfaces with a typed message table.
- Destructive-action safety layer below the UI: typed single-use
  confirmations, late identity validation, recycle evidence, and Shell
  requests for open, reveal, recycle, permanent delete, empty recycle bin, and
  Installed Apps. The interface does not expose these actions yet.
- Optional per-user Explorer verb adapter with staged, ownership-aware
  install, repair, and removal, plus the `--scan-path` startup argument.
- Portable executable identity: embedded manifest (per-monitor DPI, long
  paths, as-invoker), icon and version resources, static CRT linkage, and a
  reproducible release workflow that publishes a draft release with
  checksums, an SBOM, and provenance.
- Third-party license notices generated with cargo-about and checked in CI.

### Changed

- Partial snapshots are paced by the cost of rebuilding them, so very large
  scans no longer spend the supervisor's time materializing snapshots.
- Pending scan directories are queued per volume, avoiding a quadratic search
  when a volume's permit cap is saturated.

### Fixed

- Atomic file replacement used a Win32 call that rejects handle-relative
  renames; it now goes through `NtSetInformationFile`.
- A cancelled scan whose worker abandoned its final message was reported as a
  channel failure instead of cancelled.
- Release builds printed nothing for `--help`, `--version`, and usage errors;
  the parent console is now attached.

[Unreleased]: https://github.com/Luexi/diskpie/commits/main
