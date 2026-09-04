# DiskPie contributor guide

## Authority and scope

Read `docs/legacy-scanner-analysis.md` before changing product behavior. It is
the primary requirements source. Record evidence for deliberate deviations in
an ADR and keep the intent of the product intact.

DiskPie is Windows-first, but the domain model, scanner protocol, and sunburst
layout must remain independent of eframe and Windows APIs. Windows-specific
code belongs behind platform adapters.

Before implementing or reviewing Rust, egui, scanner, or Windows-platform
changes, read the project skill at `.agents/skills/diskpie-rust-egui/SKILL.md`
for Codex/agent runtimes or `.claude/skills/diskpie-rust-egui/SKILL.md` for
Claude; both copies must remain byte-identical.

## Clean-room boundary

The parent directory contains proprietary Scanner reference artifacts. Never
copy, modify, stage, package, or publish them. In particular, do not introduce
any of these files anywhere in this repository:

- `Scanner.exe`
- `Scanner.lang`
- `Scanner.en.txt`
- `Scanner.de.txt`
- `Add Scanner to Context Menu.reg`
- `Delete Scanner from Context Menu.reg`

The legacy analysis may be used as behavioral evidence. Create all DiskPie
code, icons, strings, palettes, translations, and documentation independently.

## Engineering constraints

- Use stable Rust and keep unsafe code isolated, justified, and reviewed.
- Do not make the renderer wait for filesystem or shell I/O.
- Use bounded parallelism and batched scan events; never spawn one task per file.
- Do not follow directory reparse points or links by default.
- Treat access failures as reportable omissions rather than global scan errors.
- Put open, recycle, permanent-delete, and recycle-bin operations behind a
  separate testable actions interface.
- Require an exact-path confirmation for recycle/delete. Permanent deletion
  must use a stronger confirmation.
- Tests may only delete temporary fixtures they created.
- Never construct commands by concatenating untrusted paths for `cmd.exe`.

## Dependencies and licenses

Prefer maintained MIT, Apache-2.0, BSD, ISC, or Zlib dependencies when they
provide measurable value. Before adding a nontrivial dependency, record its
license, maintenance, unsafe footprint, platform support, build/binary impact,
local-alternative cost, and abandonment/lock-in risk under `docs/research/` or
`docs/adr/`. Do not copy or link GPL, AGPL, SSPL, or otherwise incompatible code.

## Required checks

Keep these green for every stable commit:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Add focused unit, integration, property, Windows-specific, and benchmark
coverage as the affected subsystem requires. Report checks not run and why.
