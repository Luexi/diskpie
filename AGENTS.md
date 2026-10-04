# DiskPie agent instructions

Owner decisions updated 2026-09-11. Apply these rules to the task actually
assigned; the roadmap is not authorization to execute every phase.

## Current contract and context

- DiskPie is permanently **Windows-only**, targeting Windows 10/11 x86-64.
  Neutral modules and existing Linux unit checks are implementation tools,
  not commitments to Linux, macOS or ARM64 products.
- It is free and open source under the existing MIT OR Apache-2.0 license.
  Deliver a portable executable first. Prioritize clear exploration, a polished
  interface and reviewed recycling, with reasonable size and resource usage.
- Keep Rust and egui/eframe. A migration, broad rewrite or product-scope change
  requires an owner decision before implementation.
- [ROADMAP.md](ROADMAP.md) owns product scope, priorities and delivery order.
  [Traceability](docs/requirements-traceability.md) owns current status/evidence.
  [Architecture](docs/architecture.md) owns existing implementation boundaries.
  [UI direction](docs/research/ui-product-direction.md) owns visual guidance.

Start here, then read only the relevant task, status rows and source files.
Use the repository Rust/egui skill for relevant implementation or review. Its
canonical text is .agents/skills/diskpie-rust-egui/SKILL.md, mirrored verbatim
at .claude/skills/diskpie-rust-egui/SKILL.md.

Latest explicit owner instructions take precedence. This file governs agent
workflow. Historical Scanner analysis, research and ADRs explain past work;
they cannot override the current contract. Read them to answer a concrete
question, not as a mandatory tour of every document.

## Deliver one usable change

1. Check the working tree and preserve unrelated edits. State the outcome,
   likely files and acceptance check briefly.
2. Reuse existing code and adapters. Complete the user-facing flow when
   applicable; an internal adapter alone is not a finished feature.
3. Verify proportionally, update only affected documentation/status, and report
   actual evidence, remaining gaps and the next bounded task.

Proceed with routine, reversible decisions inside the assigned scope. Consult
the owner for scope/technology changes, new spending, broad rewrites, or
irreversible/external actions not already authorized. Do not re-request approval
already given. Make a proposal concrete and reviewable before seeking a decision.

Simplify in small steps tied to a feature, bug, measured bottleneck or maintenance
obstacle. Do not expand crash reporting, diagnostics, persistence, generic
frameworks or future-platform support without such a need. Existing safeguards
are not permission for blind deletion or redesign.

Research the unresolved question only. Prefer maintained libraries where they
reduce work; explain a significant dependency's benefit, license compatibility
and material costs briefly. Create an ADR for a lasting architectural decision,
not ordinary edits. Use parallel agents or independent review when bounded work
or risk warrants them, not as a compulsory ritual.

## File actions: recycle only

- Cleanup means reviewing selected native paths and moving them to the Windows
  Recycle Bin. Validate identity and reject stale, synthetic or changed targets.
  Never derive action paths from display text.
- If an item is too large, a volume/provider does not support recycling, or safe
  recycling cannot be established, skip/fail it and explain why. Do not offer
  or accept permanent deletion as a fallback, including through a native Shell
  prompt. Confirmation cannot override this restriction.
- Permanent-delete controls, shortcuts and commands, and emptying the Recycle
  Bin are out of scope. Dormant legacy implementations remain; do not connect
  them. Removal may be a separate scoped maintenance change.
- App management may open Windows' official uninstall interface as a separate,
  explicit user-controlled flow. Do not silently uninstall or erase application
  directories. Leftover file cleanup follows the same recycle-only contract.
- Metadata scans must not download cloud-only contents. Content analysis must
  make cloud-only/unsupported candidates explicit. Old files and generically
  named build folders are not automatically safe to remove.
- File-mutation tests operate only on temporary fixtures created by that test.
  No agent assignment authorizes cleaning the owner's computer or user data.

## Preserve useful engineering boundaries

Keep filesystem/Shell I/O, layout work and blocking joins outside the egui frame
loop. Preserve bounded workers/queues, cancellation, native path identity,
hard-link accounting and reparse-point boundaries. Keep project-authored unsafe
code minimal, documented and isolated in Windows adapters. Never construct
shell commands by concatenating untrusted paths. Use the five existing crate
boundaries in architecture; do not add speculative layers.

## Evidence and checks

For code changes, start with focused tests, then run applicable existing CI
gates before stable integration: fmt, clippy, workspace tests and release build
as defined in .github/workflows/ci.yml. Windows behavior requires Windows
evidence; portable tests alone cannot prove it. Recycling needs disposable
fixtures that verify refusal without permanent deletion. Commands and release
checks are in [CONTRIBUTING.md](CONTRIBUTING.md).
Before testing a piece in isolation, first write down the concrete ways it
could fail; each test must catch at least one. Do not write tests after the
code that merely restate the implementation.

For documentation-only edits, check consistency, relative links, factual status
and mirrored-skill equality. A local Rust rebuild is not required for docs;
existing remote CI may still run. Report material checks not run and why.
Measure development-folder size, release size and runtime RAM separately.
Do not clean build output or optimize blindly from a folder total.

## Original implementation and licenses

Create DiskPie code, artwork and text independently. Do not copy, modify, stage,
package or publish proprietary Scanner artifacts, including Scanner.exe,
translations/manuals and context-menu registry files. DiskBuddy is a feature
reference, not reusable proprietary assets. Preserve license notices and
evaluate compatibility with the project's existing license.
