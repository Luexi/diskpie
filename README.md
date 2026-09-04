# DiskPie

<p align="center">
  <img src="assets/brand/diskpie-icon.png" width="144" height="144" alt="DiskPie radial disk icon">
</p>

**A fast visual disk usage scanner.**

DiskPie is a clean-room, Windows-first disk space analyzer built with stable
Rust and egui. It scans a folder, a drive, or every drive and renders the result
as an interactive sunburst while the scan is still running, with a synchronized
keyboard-operable list of the largest items beside it.

> Development is underway toward the first stable portable release. Per-feature
> status and evidence are tracked in
> [`docs/requirements-traceability.md`](docs/requirements-traceability.md).

Continuation and design context are maintained in the
[`ROADMAP.md`](ROADMAP.md) handoff and
[`docs/architecture.md`](docs/architecture.md) architecture guide.

## Product goals

- Responsive, cancelable scans with partial results.
- Logical-size and allocated-space views with documented filesystem semantics.
- Safe handling of long Unicode paths, reparse points, hard links, sparse files,
  inaccessible entries, and cloud placeholders.
- Click-to-zoom sunburst navigation plus a keyboard-friendly textual view.
- Explicitly confirmed recycle and permanent-delete operations behind a
  separate platform boundary.
- Portable Windows 10/11 x86-64 distribution that works without administrator
  privileges or an installer.
- English and Spanish user interfaces, system/light/dark themes, persistent
  preferences, logging, and local diagnostic export.

## What works today

- Choose a folder with the native dialog, pick a drive from the list, scan
  all drives, or start with a path: `diskpie.exe [--scan-path PATH | [--] PATH]`.
- Watch the sunburst grow while scanning; hover for the exact path, logical
  and allocated sizes, counts, omissions, and share of the view.
- Zoom, go back, go to the parent, hide and restore branches, switch between
  logical and allocated size, rescan, and cancel, by mouse or keyboard.
- Preferences persist between runs; logs are bounded, local, and path-free;
  an unexpected exit leaves a small local marker and nothing is sent anywhere.

## Known limitations

- Open, reveal, recycle, permanent delete, empty recycle bin, and Installed
  Apps are implemented and tested below the interface but are not exposed in
  it yet.
- The optional Explorer context-menu verb has no settings toggle yet.
- Rescanning a single branch is not available; use a full rescan.
- Exporting a diagnostics bundle from the interface is not available yet.
- No release has been published yet; clean-machine Windows 10 and 11
  verification is pending. The command-line help is English only.

## Development

Install the pinned Rust MSVC toolchain on Windows (`rust-toolchain.toml`
selects it), then run the same gate CI enforces:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
$env:RUSTDOCFLAGS = "-D warnings"; cargo doc --workspace --no-deps --locked
cargo build --release -p diskpie --locked
```

The release configuration links the C runtime statically, so the first build
after a fresh clone compiles the whole graph. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for workflow, safety rules, and the
required evidence for changes, and `THIRD-PARTY-NOTICES.md` for the licenses
of shipped dependencies.

## Clean-room provenance

DiskPie is an original implementation inspired by Scanner by Steffen Gerlach.
No Scanner executable, translations, manuals, registry files, artwork, or source
code are distributed with this project. The static behavioral analysis that
defines the compatibility target is preserved in
[`docs/legacy-scanner-analysis.md`](docs/legacy-scanner-analysis.md).

## License

DiskPie is available under either the Apache License 2.0 or the MIT License, at
your option. See [`LICENSE-APACHE`](LICENSE-APACHE) and
[`LICENSE-MIT`](LICENSE-MIT).
