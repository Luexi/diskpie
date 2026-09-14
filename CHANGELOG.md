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
  parent, hide and restore, full and branch rescan, cancel, metric switch, telemetry,
  and the required scan states.
- Persistent preferences in an application-owned `app.ron`, local bounded
  daily logs with redaction, and a private local panic marker with
  clean-shutdown reconciliation.
- English and Spanish user interfaces with a typed message table.
- Connected action UI: exact-path review with Cancel focused by default, typed single-use
  confirmations, late identity validation, recycle evidence, and Shell
  requests for open, reveal, recycle, permanent delete, empty recycle bin, and
  Installed Apps. Every destructive attempt creates a result and a refresh
  obligation; unexpected permanent deletion blocks later destructive actions.
  The permanent-delete and empty-bin controls predate the 2026-09-11
  recycle-only decision and are scheduled for removal from the interface.
- Optional per-user Explorer verb adapter with staged, ownership-aware
  install, repair, and removal, plus the `--scan-path` startup argument and a
  settings surface that inspects the actual registry state asynchronously.
- Diagnostic preview and native Save As flow, with per-export path consent,
  bounded log collection, retained destination preflight, actual transport
  disclosure, and create-only publication that cannot overwrite a late alias.
- Background ranking and filtering of the complete child list, with virtualized
  rows, generation/revision checks, bounded requests, and explicit shutdown.
- Isolated reproducible benchmark harness and an experimental batch-enumeration
  adapter used only by that harness; production keeps the conventional provider.
- Portable executable identity: embedded manifest (per-monitor DPI, long
  paths, as-invoker), icon and version resources, static CRT linkage, and a
  reproducible release workflow that publishes a draft release with
  checksums, an SBOM, and provenance.
- Third-party license notices generated with cargo-about and checked in CI.

### Changed

- Documentation direction updated on 2026-09-11: permanent Windows-only scope,
  free/open-source distribution, Rust/egui, portable staged deliveries and the
  expanded visual disk-management feature plan. Planned features are not
  represented as implemented.
- Agent guidance now favors complete scoped tasks, incremental simplification
  and proportional verification. Legacy scope and process requirements are
  marked as historical.
- Product cleanup policy is now Recycle Bin only, with refusal when recycling
  is unavailable. Permanent-delete and empty-bin features are excluded. This
  is a documentation change: dormant legacy APIs remain and the new complete
  recycling flow still requires implementation/Windows verification.

- Compute positive depth during aggregation and index snapshot roots, removing
  the depth prepass and the inherited full-tree rescan-availability query.
  Large layout sorts, grouping and model preparation check cancellation.
- Export schema 2 describes the last settled scan and marks unmeasured metrics
  unknown. Export selection retains bounded suffix data; log collection reads
  only verified bounded tails. Open/Reveal avoids recycle-only provider probes.

- Map-first Scanner renovado composition, with one compact toolbar, informative
  center, contextual status and an optional persistent item-list panel. Theme
  and language controls live in Preferences; file actions remain contextual.
- Adaptive sunburst rings fill the available radius for shallow trees; stable
  ancestry colors retain their identity through navigation and rescans.
- Complete list sorting by name or active size shares a comparator with
  keyboard selection lookup. New panel settings default compatibly for old RON.
- Neutral light/dark surfaces, stronger text contrast and primary Segoe UI
  when available, preserving embedded and multilingual fallbacks.
- Partial snapshots are paced by the cost of rebuilding them, so very large
  scans no longer spend the supervisor's time materializing snapshots.
- Pending scan directories are queued per volume, avoiding a quadratic search
  when a volume's permit cap is saturated.
- Branch replacement waits for the scan coordinator to join, rebuilds and
  re-aggregates off-thread, and preserves the previous frame on cancellation or
  failure. Original hard-link allocation observations survive deduplication.
- Layout and list computation check for superseding requests while doing work.

### Fixed

- Windows enumeration and child metadata now share a retained directory handle,
  preventing concurrent pathname replacement from redirecting observations.
- Cancelling during branch merge/presentation or immediately before publication
  preserves the previous frame; provider failures remain classified as failures.
- Full rescan retains and retries roots that failed initial resolution.
- Explorer partial removal preserves its outcome and refreshes observed state;
  unverified state is invalidated and recoverable partial installs expose repair.
- Partial startup failure cannot claim no runtime was composed or mint a clean
  shutdown proof without actual receipts.
- The independent benchmark prototype passes the same warning-denying Clippy
  policy as the production workspace.

- Branch readiness now observes queued, reserved, active and undrained work
  under one lock, preventing a pending rescan from appearing settled during
  supervisor handoff. Navigation and context targets retain their original
  generation/revision, and displayed metric labels follow the committed frame.
- Atomic file replacement used a Win32 call that rejects handle-relative
  renames; it now goes through `NtSetInformationFile`.
- A cancelled scan whose worker abandoned its final message was reported as a
  channel failure instead of cancelled.
- Release builds printed nothing for `--help`, `--version`, and usage errors;
  the parent console is now attached.
- A failed root resolution in a multi-drive request no longer discards the
  other roots; failures remain visible as omissions.
- Directories stay incomplete while enumeration is pending, and terminal
  cancellation does not misrepresent unfinished descendants as complete.

[Unreleased]: https://github.com/Luexi/diskpie/commits/main
