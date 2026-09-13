# DiskPie

<p align="center">
  <img src="assets/brand/diskpie-icon.png" width="144" height="144" alt="DiskPie radial disk icon">
</p>

**A fast visual disk usage scanner.**

DiskPie is a clean-room, Windows-first disk space analyzer built with stable
Rust and egui. It scans a folder, a drive, or every drive and renders the result
as an interactive sunburst while the scan is still running. The map is the
initial view; Show list opens a synchronized, keyboard-operable item table.

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
- The center shows the current folder, active size and metric. Click a folder
  to enter or the center to go up. Show list opens a resizable panel with
  name/size ordering, aligned columns and collapsible details; below 800 points
  it switches between map and list.
- Zoom, go back, go to the parent, hide and restore branches, switch between
  logical and allocated size, rescan the selection or one branch, and cancel,
  by mouse or keyboard. Search the complete child list independently of the
  chart's sector budget.
- Full rescan retries every originally selected root, including roots that
  were unavailable. Cancelling during branch preparation retains prior results.
- Open or reveal an item, recycle supported local NTFS items, or permanently
  delete after exact-path confirmation and a stronger typed confirmation.
  Results are refreshed after every destructive attempt.
- Open Installed Apps, inspect and empty the Recycle Bin after confirmation,
  and install, repair, or remove the optional per-user Explorer verb in Tools.
- Preview a redacted diagnostics report, choose a native `.txt` destination,
  and save those exact bytes. Existing destinations require a new name; a
  destination resolved to a network share is identified before confirmation.
- Diagnostics identify the last settled scan and mark unmeasured fields as
  unknown. Explorer preferences distinguish partial installations and offer
  recovery without deleting their existing owned verb.
- Preferences persist between runs; logs are bounded, local, and path-free;
  an unexpected exit leaves a small local marker and nothing is sent anywhere.

## Known limitations

- Native layout captures cover light/dark themes, both languages and 100–200%
  application scaling; see the [design validation record](docs/research/scanner-renovado-validation.md).
  Native action/dialog walkthroughs, Narrator, physical mixed-DPI monitor
  transitions and Windows 10/11 VM checks remain pending.
- Recycling is enabled only for a proven fixed local NTFS provider. The real
  Empty Recycle Bin operation requires a disposable-VM validation pass and has
  not been exercised against the user's bin.
- The Windows scanner retains a directory handle for enumeration and child
  metadata. The updated native adapter has local fixture coverage; remote/cloud
  provider and clean-machine acceptance remain separate.
- Branch rescans rebuild and re-aggregate the committed tree off-thread.
  Large-tree memory and timing limits remain measurement targets; the batch
  enumeration prototype is confined to the benchmark harness.
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

The independent benchmark workspace has its own tests and Clippy gate; see
[`tools/diskpie-bench/README.md`](tools/diskpie-bench/README.md).
The [review remediation record](docs/research/review-remediation-validation.md)
tracks the latest fixes and performance evidence without replacing historical
redesign measurements.

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
