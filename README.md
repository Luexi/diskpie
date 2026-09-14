# DiskPie

<p align="center">
  <img src="assets/brand/diskpie-icon.png" width="144" height="144" alt="DiskPie radial disk icon">
</p>

**Understand your disk space, explore it visually, and review what to recycle.**

DiskPie is a free, open-source disk space analyzer for **Windows 10 and 11
x86-64**. Windows is the only product platform. It uses Rust and egui/eframe,
with a portable executable as the first distribution format.

Development is in progress; there is no published release yet. Scanning, the
map-first exploration, contextual Open/Reveal and single-item recycling work,
while the broader feature set below is still being built. See
[feature status and evidence](docs/requirements-traceability.md).

## Available in the current implementation

- Select a folder, drive or all drives; start from the native dialog or CLI.
- Watch the sunburst grow while scanning. The center names the current folder
  and its size; click a folder to enter it or the center to go up.
- Open Show list for a searchable, keyboard-operable table of every child
  with name/size ordering and collapsible details (paths, logical/allocated
  sizes, counts, omissions and share of the view).
- Zoom, go back/parent, hide/restore branches, change size basis, rescan the
  selection or one branch, and cancel while retaining available results.
- Open or reveal an item in Explorer, and recycle a supported fixed local NTFS
  item after reviewing its exact path.
- Use Tools to open Windows Installed Apps, manage the optional per-user
  Explorer context-menu verb, and save a redacted diagnostics report.
- Use English/Spanish, system/light/dark themes and persistent preferences.
- Keep scans and bounded diagnostics local; there is no scan-upload service.

## Product direction

The approved direction prioritizes a polished, understandable interface and
usable staged releases. The complete planned feature set includes eight views
(folders, sunburst, flame, bubbles, mind map, top sizes, age map and treemap),
an inspector and previews, reviewed recycling, content duplicates, cleanup
candidates, app footprints/leftovers, scan history/comparison and a system
monitor. These are goals, not claims that every feature is shipped.

File cleanup is **Recycle Bin only**. If Windows cannot recycle an item,
including an oversized file or an unsupported volume, DiskPie must explain
the refusal without offering permanent deletion. Emptying the Recycle Bin is
excluded. Windows' official app-uninstall interface is a separate explicit
user flow; DiskPie does not erase application directories itself.

DiskBuddy's [public features](https://diskbuddy.com/mac-disk-space-analyzer)
inform the scope; implementation, artwork and text are original. See the
[roadmap](ROADMAP.md) for delivery order and approved constraints.

## Current limitations

- Recycling is enabled only for proven fixed local NTFS. The recycle-only
  guarantee (no permanent-delete fallback for oversized items, disabled bins or
  unsupported volumes) still needs end-to-end Windows verification.
- A permanent-delete control and an Empty Recycle Bin control were connected
  before the recycle-only decision; they are scheduled for removal from the
  interface and are outside product scope.
- Treemap and the other additional views, the multi-item cleanup basket,
  duplicates, cleanup suggestions, app footprints, snapshots/comparisons,
  previews and monitoring are planned.
- Clean-machine Windows 10/11 checks, performance measurements and accessibility
  verification remain pending. Command-line help is English only.

## Build and contribute

On Windows with the pinned Rust MSVC toolchain and build prerequisites:

```powershell
cargo run -p diskpie --locked
cargo build --release -p diskpie --locked
```

The executable accepts `diskpie.exe [--scan-path PATH | [--] PATH]`.
Build output under target is not the distribution package. A historical
2026-09-04 verification recorded a 10.0 MB release executable; measure the
current build before treating that value as current. Development-folder size,
executable size and runtime RAM are separate measurements.

Agents start with [AGENTS.md](AGENTS.md). See [CONTRIBUTING.md](CONTRIBUTING.md)
for setup/checks and [architecture](docs/architecture.md) for code boundaries.

## Original work and license

DiskPie began as an original implementation inspired by Scanner by Steffen
Gerlach. No proprietary Scanner executable, translations, manuals, registry
files, artwork or source are distributed here. The
[legacy analysis](docs/legacy-scanner-analysis.md) remains historical behavioral
evidence; it no longer governs product scope.

DiskPie is available under either Apache License 2.0 or MIT License, at your
option. See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
