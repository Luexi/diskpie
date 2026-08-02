# Contributing to DiskPie

Thank you for helping make disk usage understandable, fast, and safe.

## Before starting

1. Read `AGENTS.md` and `docs/legacy-scanner-analysis.md`.
2. Search existing issues and open a focused issue for substantial behavior or
   architecture changes.
3. Keep proprietary Scanner artifacts outside this repository. Do not submit
   recovered code, copied strings, images, translations, or bundled binaries.
4. For a nontrivial dependency, add or update a research note using the rubric
   in `docs/research/README.md` before integrating it.

## Local setup

Use stable Rust with the MSVC target on Windows:

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --workspace
```

Portable core work should remain testable without Windows UI or shell APIs.
Windows-specific behavior still requires a Windows 10 or 11 verification pass.

## Change workflow

- Branch from `main` using a short name such as `feat/scan-progress` or
  `fix/reparse-loop`.
- Keep commits focused and use conventional prefixes such as `feat`, `fix`,
  `perf`, `test`, `docs`, `build`, and `ci`.
- Add tests at the smallest useful level. Filesystem-action tests may operate
  only on temporary fixtures created by the test itself.
- Record user-visible changes under `Unreleased` in `CHANGELOG.md`.
- Update requirements, architecture, user documentation, and translations when
  behavior changes.

## Required checks

Run before opening a pull request:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Run relevant benchmarks or Windows integration tests for changes that affect
scanning, layout, rendering, cancellation, allocation accounting, or shell
operations. State every check run and any check not run in the pull request.

## Safety and reporting

Never build a shell command by concatenating a path. Keep destructive actions
behind the confirmation policy and platform adapter. Report vulnerabilities
privately according to `SECURITY.md`.

By contributing, you agree that your contribution is licensed under the
project's `MIT OR Apache-2.0` terms and that you will follow
`CODE_OF_CONDUCT.md`.
