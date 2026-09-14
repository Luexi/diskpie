# DiskPie reproducible benchmarks

This independent Cargo workspace uses the existing Rust/Windows dependencies,
pinned by its checked-in lockfile. It does not add a production dependency or
select a new filesystem backend. Run from the repository root on Windows.

Before changing production code, preserve its comparison source:

```powershell
New-Item -ItemType Directory -Force target/bench-baseline
git rev-parse HEAD | Set-Content target/bench-baseline/SOURCE_COMMIT.txt
git archive --format=tar --output=target/bench-baseline/source.tar HEAD
tar -xf target/bench-baseline/source.tar -C target/bench-baseline
```

Then run the complete alternating A/B matrix while other builds/tests are idle:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools/diskpie-bench/scripts/benchmark.ps1
python tools/diskpie-bench/scripts/summarize.py
```

The script adds only the identical harness and isolated prototype to the
archived tree, builds both versions in separate target directories, and runs
seven fresh processes per variant/scenario. It retains stdout/stderr and raw
CSV results. `-Include100kReal` adds a 100,000-file real fixture; the default
creates 10,000 files below `target/bench-fixture-10000`. Fixture creation is
excluded from timings and no cleanup command deletes user data. `-SkipBuild`
is only appropriate when both measured binaries already match the sources.
`-Runs` is available for smoke diagnosis; the report generator accepts only
complete seven-run comparisons.

Synthetic coverage is flat, wide, balanced, deep, omission-heavy and delayed
provider work at 100,000 and 1,000,000 requested entries. Exact node counts are
in CSV. The depth scenario is 128 levels with the requested files spread along
the chain; it is not one million directory levels. The wide scenario is N/10
sibling directories containing nine files each, under a long shared parent
prefix. Flat separately tests one million files in a single directory.

Metrics separate scan/event reduction, snapshot materialization, layout,
first data batch and cancellation/join. The PowerShell runner samples working
set, private memory, handle count and CPU through .NET every 10 ms. These are
sampled process metrics, not per-frame timings or a complete allocator profile.
Run the application's independent headless UI benchmark for frame metrics.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools/diskpie-bench/scripts/ui-benchmark.ps1
python tools/diskpie-bench/scripts/summarize-ui.py
```

This second script includes the same `ui_frames.inc.rs` test body in the
archived shell test module and the current shell, then builds both release test
binaries. It measures the actual shell through `egui::Context::run_ui` with a
1280-by-800 viewport and 100,000/1,000,000 files. The first snapshot/metric frames
and 200 frames each of transition/warm states are retained individually. CPU,
memory and handle samples are in `results/ui/processes.csv`; per-run frame
distributions and method limitations appear in `UI_REPORT.md`. This measures
CPU-side widget construction and shape emission, not native GPU/display FPS.
Both benchmark scripts must run sequentially while other builds/tests are idle.

The batch prototype lives in the platform source directory but is compiled
only via this harness's `#[path]` module. Its Win32 handles use `File` RAII;
its 64-KiB buffer parser validates offsets/lengths before reading fields. It
rejects reparse/cloud boundaries. Native names never become action paths.
The baseline implementation remains the sole production provider.

```powershell
cargo test --locked --manifest-path tools/diskpie-bench/Cargo.toml --target-dir target/bench-tests
target/bench-build-current/release/diskpie-bench.exe --scenario differential --root target/bench-fixture-10000
```

This is a separate workspace: the main workspace Clippy/format gate does not
cover its binary or the prototype it includes. Run these checks explicitly:

```powershell
cargo fmt --manifest-path tools/diskpie-bench/Cargo.toml --all -- --check
cargo clippy --manifest-path tools/diskpie-bench/Cargo.toml --all-targets --locked --target-dir target/bench-tests -- -D warnings
```

The isolated tests cover malformed records and differential native UTF-16
names, emoji, hard links and directories. The real-fixture comparison checks
every name, kind, logical/allocation size and file identity. Further provider
matrices are necessary before considering production use.

API source: Microsoft's [FILE_ID_EXTD_DIR_INFO contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_extd_dir_info).

## Scanner renovado comparison

The redesign uses a separate baseline: the working tree including deliveries
1–3 saved under `target/design-before` before the design changes. Do not use the
older `bench-baseline` source to attribute redesign performance. Use an isolated
build directory for this baseline to avoid reusing artifacts from another copy.

```powershell
powershell -File tools/diskpie-bench/scripts/design-benchmark.ps1 -BuildOnly
powershell -File tools/diskpie-bench/scripts/design-benchmark.ps1 -SkipBuild
python tools/diskpie-bench/scripts/summarize-design.py
```

This compares the previous inspector, the new default map and the optional
list, using seven rotated runs and the same CPU-frame test body. A test-only
environment hook selects the new list's visibility; production ignores it.
Layout samples cover flat, balanced and deep trees at 100,000 and 1,000,000
entries. Raw CSVs, hardware/toolchain and hashes go to `results/design/` and the
validated report to `DESIGN_REPORT.md`. Run measurements after other builds and
tests finish. Native screenshot evidence is separate from these CPU timings.

`BuildOnly` records the compiled source and executable identities in
`target/design-build-manifest.json`. `SkipBuild` rejects changed sources,
executables or toolchains, and refuses to overwrite partial measurement files.
Archive a previous result directory explicitly before starting another run.

The Windows-only ignored test `shell::tests::visual_matrix::native_screenshot_matrix`
captures the real eframe/Glow UI using the in-memory filesystem in
`visual_matrix.inc.rs`. Set `DISKPIE_VISUAL_OUTPUT` to an evidence directory;
optional `DISKPIE_VISUAL_FILTER` matches case names. Run the compiled test
executable with `--exact`, `--ignored` and `--test-threads=1`; PowerShell should
use `Start-Process -Wait` when running the release Windows-subsystem executable.
Use a copy outside Cargo's output directory if other builds will run concurrently.
The test writes PPM screenshots and a CSV with requested/actual dimensions and
observed states, and verifies service shutdown. See
`docs/research/scanner-renovado-validation.md` for coverage and limitations.

## Review remediation measurements

The [2026-09-07 remediation record](../../docs/research/review-remediation-validation.md)
tracks the implemented fixes and new measurements. Preserve the archived
pre-redesign reference and `results/design/` unchanged. UI/layout comparisons for
the fixes use separate build/output directories and the reviewed post-redesign
binary as an additional baseline, with verified source/lock/binary provenance.
Do not apply a hard-coded checkpoint index from an older layout algorithm to a
new one; use total durations and algorithm-independent maximum checkpoint gaps.
