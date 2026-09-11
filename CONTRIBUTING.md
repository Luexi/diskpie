# Contributing to DiskPie

DiskPie is a free, open-source Windows application. Start with [AGENTS.md](AGENTS.md)
and the relevant task in [ROADMAP.md](ROADMAP.md). Older Scanner analysis and
research explain history; they do not define current product scope.

## Development setup

Use Windows with MSVC build tools and the Rust toolchain pinned by
rust-toolchain.toml. Do not change the user's global default toolchain as a
setup step. From the repository root:

```powershell
cargo build -p diskpie --locked
cargo run -p diskpie --locked
```

The product target is Windows 10/11 x86-64. Existing Linux checks exercise
neutral logic, not a Linux application release.

## Scope and workflow

- Preserve unrelated changes. Use focused branches/commits for pull requests,
  or the integration workflow explicitly assigned by the owner.
- Reuse Rust/egui and existing adapters. Ordinary edits do not need a new issue,
  research report or ADR. Explain lasting decisions and significant dependency
  tradeoffs in the relevant document or review description.
- Define an outcome and acceptance evidence. An adapter alone is not a finished
  feature; connect the interface when that is part of the assignment.
- Simplify incrementally for demonstrated benefits. Consult the owner before
  migration, broad rewrites, scope changes or new spending.
- Keep meaningful tests. Automated mutations target only temporary fixtures
  created by the test, never personal files.
- Update traceability, user docs, translations or the changelog only when their
  corresponding contract or status changes.

## Verification

Start with focused checks. For stable code integration, use the applicable
existing gates in .github/workflows/ci.yml:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --locked
$env:RUSTDOCFLAGS = "-D warnings"; cargo doc --workspace --no-deps --locked
cargo build --release -p diskpie --locked
```

Native behavior needs Windows fixture/UI verification. Releases also need the
pending clean-machine, accessibility and performance evidence recorded in the
roadmap and traceability matrix. Report actual checks and material checks not
run, with a reason; old evidence is not a new runtime pass.

Documentation-only edits need link, consistency, status and mirrored-skill
checks, not a local Rust rebuild. Remote workflows may run automatically.

## Recycling and privacy

Cleanup moves reviewed items to the Recycle Bin only. Too-large items,
unsupported providers or an inability to ensure recycling must produce a
refusal with an explanation. Do not connect legacy permanent-delete/empty-bin
commands or offer their native fallback prompts. Preserve native target
identity and keep I/O outside the UI thread. App management uses the official
Windows interface; leftover files follow the same recycle-only contract.

Use [SECURITY.md](SECURITY.md) for vulnerability reports. Scans and diagnostics
stay local; do not introduce telemetry or uploads without an owner decision.

## Third-party notices and original work

Do not copy proprietary Scanner/DiskBuddy code, text or assets. Preserve the
project's MIT OR Apache-2.0 terms and CODE_OF_CONDUCT.md.
THIRD-PARTY-NOTICES.md is generated from the locked Windows dependency graph
and ships in release ZIPs. When Cargo.lock, about.toml or about.hbs changes:

```powershell
cargo install --locked --version 0.9.1 --features cli cargo-about
cargo about generate --locked --fail about.hbs -o THIRD-PARTY-NOTICES.md
```

The security workflow checks notices drift. Documentation edits alone do not
require regenerating dependency notices.
