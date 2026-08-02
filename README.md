# DiskPie

<p align="center">
  <img src="assets/brand/diskpie-icon.png" width="144" height="144" alt="DiskPie radial disk icon">
</p>

**A fast visual disk usage scanner.**

DiskPie is a clean-room, Windows-first disk space analyzer built with stable
Rust and egui. It turns a folder, drive, or multi-drive summary into an
interactive sunburst so large branches stand out immediately while the scan is
still running.

> Development is underway toward the first stable portable release. The
> requirements and verification status are tracked in
> [`docs/requirements-traceability.md`](docs/requirements-traceability.md).

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

## Project status

The architecture, scanner, interface, Windows adapters, verification evidence,
and release links will be documented here as they land. Until a signed-off
GitHub release is published, build artifacts are development snapshots.

## Development

Install the stable Rust MSVC toolchain on Windows, then run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for workflow, safety rules, and the
required evidence for changes.

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
