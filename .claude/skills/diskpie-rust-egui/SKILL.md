---
name: diskpie-rust-egui
description: Implement or review Rust changes in DiskPie across crate boundaries, the egui frame loop, bounded scanning, and Windows adapters. Use only in the DiskPie repository.
---

# DiskPie Rust and egui engineering

Use this skill for DiskPie implementation and review. Preserve the explicit
task scope and load only the canon relevant to the affected area.

## Route context

- Always read `AGENTS.md` first.
- For product behavior, read `docs/legacy-scanner-analysis.md`.
- For architecture or crate placement, read `docs/architecture.md`.
- For egui interaction and visual behavior, read
  `docs/research/ui-product-direction.md`.
- For deep runtime or dependency decisions, read
  `docs/research/rust-egui-architecture.md`.
- Record a deliberate architectural deviation in an ADR. Follow the
  dependency-research and documentation rules in `AGENTS.md`.

## Keep crate boundaries intact

- `diskpie-core`: portable domain model and layout only; no egui, eframe,
  Windows APIs, or filesystem traversal.
- `diskpie-scan`: portable bounded blocking scan orchestration, with one
  coordinator, fixed workers, bounded channels, batching, cancellation, and
  generation identity.
- `diskpie-app`: UI-independent policy, runtime, navigation, settings, and
  diagnostics; it may own traits, but not egui, eframe, COM, Win32, or HWND.
- `diskpie-platform`: target-gated OS adapters and the only normal home for
  project-authored `unsafe`.
- `diskpie`: composition and egui UI only; keep it free of `unsafe`.

Do not introduce a reverse dependency or move policy into an adapter merely
for convenience.

## Preserve runtime and safety invariants

- Never block an egui callback or renderer on filesystem, shell, COM, layout,
  settings, diagnostics, or thread joins. Submit work off-thread, use bounded
  nonblocking poll/drain, request repaint only while needed, and render a
  coherent immutable committed revision.
- Keep scanning bounded: one coordinator, fixed/bounded workers and channels,
  batched events, cancellation, and generation checks. Never spawn one task per
  file.
- Preserve path identity with `Path`, `PathBuf`, `OsStr`, and `OsString`.
  Display strings must never become action paths, and synthetic sunburst
  sectors must never become filesystem targets.
- Put Windows behavior behind `diskpie-platform` adapters. Keep each `unsafe`
  block minimal, document its `SAFETY` invariant, and convert raw resources to
  RAII immediately.
- Put open, recycle, permanent-delete, and recycle-bin operations behind the
  testable actions interface. Confirm the exact native path before recycle or
  delete, require stronger confirmation for permanent deletion, and never
  build `cmd.exe` commands from untrusted paths.
- Destructive tests may act only on exact temporary fixture paths created by
  that test. Never target user data, repository fixtures, or legacy Scanner
  artifacts.

## Verify proportionally

Run the smallest focused tests for the owning crate, then the workspace gates
required by `AGENTS.md` when the task permits. Report every skipped or blocked
check. Update requirements, traceability, ADRs, changelog, or translations only
when the corresponding contract changed.
