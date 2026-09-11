---
name: diskpie-rust-egui
description: Implement or review a scoped Rust, egui, scanner, or Windows integration change in the DiskPie repository under its current product contract.
---

# DiskPie Rust and egui

Read repository-root AGENTS.md for authority, workflow, the recycle-only policy
and verification rules. Load only the roadmap task, status rows, code and
architectural references relevant to the assignment. Legacy Scanner analysis
is historical evidence, not the current product specification.

## Place the change correctly

- diskpie-core: domain model, accounting and layout; no UI or I/O.
- diskpie-scan: bounded coordination, batching and cancellation.
- diskpie-app: UI-independent runtime, navigation, action policy and settings.
- diskpie-platform: Windows filesystem/Shell adapters and isolated unsafe code.
- diskpie: composition and egui interface, without project-authored unsafe code.

Reuse those boundaries. Windows 10/11 x86-64 is the only product target.
Keep Rust/egui; a migration or broad rewrite requires the owner's decision.

## Preserve relevant invariants

- Keep blocking work outside frame callbacks. Submit bounded work, consume
  coherent results and repaint only when needed.
- Preserve scan generations, bounded queues/workers and cancellation.
- Keep native paths in Path/PathBuf/OsStr/OsString. Display strings and
  synthetic sectors cannot become filesystem targets.
- Cleanup is recycle-only after review and identity validation. If Windows
  cannot recycle, including oversized items, refuse without permanent deletion
  or a native fallback prompt. Do not connect legacy permanent-delete/empty-bin
  paths. The new end-to-end refusal guarantee still needs native verification.
- Preserve hard-link, reparse and cloud-placeholder semantics. Mutating tests
  act only on temporary paths created by that test.

## Finish the slice

Connect the requested outcome through its actual flow, reusing existing
adapters. Simplify only for that task or a demonstrated problem; do not expand
diagnostics, panic handling or generic infrastructure by default. Follow the
proportional checks in CONTRIBUTING.md and report limitations honestly.
Implemented, connected and verified are distinct states in traceability.

This is the canonical skill text. Keep its .claude/skills/ counterpart
byte-identical; do not maintain different product rules for different agents.
