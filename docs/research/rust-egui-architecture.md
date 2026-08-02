# Rust, egui, and application architecture research

- Research date: 2026-08-02
- Toolchain snapshot: current stable Rust 1.97.1; workspace MSRV 1.92
- Baseline: Windows 10/11 x86-64, portable executable
- UI baseline: `egui`/`eframe`
- License target: `MIT OR Apache-2.0`
- Decision record:
  [ADR 0001](../adr/0001-modular-synchronous-architecture.md)

## Recommendation

Build DiskPie as a five-crate modular monolith with an acyclic dependency
graph. Keep the model, scan protocol, scheduling rules, and sunburst geometry
independent of egui and Windows. Use fixed blocking workers and bounded
standard-library channels rather than an async runtime or a third-party
channel crate. Keep unsafe Windows code in one platform crate and third-party
native UI code at the executable boundary.

Adopt a deliberately small application dependency set:

- `eframe`/`egui` 0.35.0 with Glow, AccessKit, default fonts, and persistence;
- `egui_extras` 0.35.0 with default features disabled for the virtualized
  accessible table;
- Serde 1.0.229 for the settings value;
- tracing 0.1.44, tracing-subscriber 0.3.23, and tracing-appender 0.2.5 for
  structured diagnostics;
- lexopt 0.3.2 for the small, path-aware command line;
- the Project Fluent crates for English and Spanish localization;
- rfd 0.17.2 behind an application-owned folder/save-dialog interface;
- thiserror 2.0.19 for typed product errors; and
- proptest, Criterion, and tempfile as development-only dependencies.

Commit `Cargo.lock`, review resolved features with `cargo tree -e features`,
and treat versions in this note as the researched baseline rather than a
request for automatic major/minor upgrades.

Rust 1.97.1 was the current stable patch on the research date according to the
[official Rust release notes](https://doc.rust-lang.org/stable/releases.html).
The lower workspace `rust-version` follows eframe 0.35.0's declared MSRV so
portable crates do not accidentally require the release builder's exact patch.

## Constraints and decision drivers

DiskPie must remain responsive while scanning millions of entries, publish
coherent partial results, cancel promptly, preserve arbitrary native paths,
and ship as a Windows 10/11 x86-64 portable executable. It must also leave a
portable, testable core rather than embedding filesystem or UI types in the
model. The renderer must never wait for filesystem or shell I/O.

The dependency evaluation rubric is the one required by the repository:

1. exact version/API baseline and license;
2. recent maintenance and release activity;
3. advisories, unsafe-code footprint, and security boundary;
4. Windows 10/11 and portable-core compatibility;
5. compile-time and portable-binary impact;
6. measurable benefit over a small local implementation;
7. abandonment and lock-in risk; and
8. an explicit adopt, defer, benchmark, or reject outcome.

## Acyclic five-crate workspace

An arrow means "depends on." These are the only permitted workspace edges:

```text
diskpie-scan     -> diskpie-core
diskpie-app      -> diskpie-core, diskpie-scan
diskpie-platform -> diskpie-core, diskpie-scan, diskpie-app
diskpie          -> diskpie-core, diskpie-scan, diskpie-app, diskpie-platform
```

There is no reverse edge and no cyclic feature configuration. In particular,
`diskpie-scan` does not depend on `diskpie-platform`, and `diskpie-app` does
not depend on `diskpie-platform` or the `diskpie` executable. Traits live with
the portable consumer that needs them; the platform crate depends inward to
implement those traits. The executable is the composition root.

### `diskpie-core`

Owns compact tree records, `NodeId`, exact logical/allocated-size values,
aggregation, deterministic ordering, pruning, sunburst layout, and polar hit
testing. Its public data uses no egui, eframe, rfd, tracing-subscriber, or
Windows types. The intended initial dependency is `thiserror` only.

Use `NodeId(u32)` into `Vec<NodeRecord>`, with a checked maximum and a whole
tree `TreeRevision`/scan generation. Keep parent/first-child/next-sibling
indices and numeric aggregates in the hot record. Store exact native names
separately and reconstruct full `PathBuf` values lazily. Do not store a full
path per node.

Start with safe `Box<OsStr>`/`OsString` name storage. If the memory-per-million
benchmark shows names dominate, replace it behind a local name-store API with
contiguous UTF-16 units on Windows and bytes on Unix. Do not persist or
exchange [`OsStr::as_encoded_bytes`](https://doc.rust-lang.org/std/ffi/os_str/struct.OsStr.html):
the encoding is unspecified outside the same Rust version and target.

### `diskpie-scan`

Owns the portable `ScanFs` consumer interface, scan generation, cancellation,
fixed worker protocol, coordinator, directory work items, worker batches,
omissions, and coherent application events. Traversal is iterative. A work
item is one directory, never one file and never an unbounded recursive task.

The coordinator is the sole owner of pending-directory queues, outstanding
work, aggregation order, and per-volume permits. Workers enumerate and return
bounded batches; they do not schedule children or mutate application state.

### `diskpie-app`

Owns the UI-independent reducer/state machine, commands, navigation history,
selection, scan lifecycle, settings schema, typed localization message IDs,
diagnostics state, and TreeBuilder. It declares narrow `SettingsStore`,
`ShellActions`, and dialog ports where a real external seam exists. It does
not declare generic repository, service-locator, or UI-widget abstractions.

### `diskpie-platform`

Implements `ScanFs`, settings-path policy, volumes, shell open, Recycle Bin,
permanent deletion, context-menu registration, and other OS adapters. Windows
implementations are target-gated. A portable adapter supplies ordinary
directory enumeration on other hosts, while unsupported shell operations
return typed errors.

### `diskpie`

Is the composition root and eframe executable. It owns startup parsing,
dependency construction, eframe lifecycle, widgets, virtual table, sunburst
mesh conversion, native dialog adapter, and repaint scheduling. Painting only
reads application snapshots and never performs filesystem or shell I/O.

## Bounded standard-library channel topology

The initial implementation uses only `std::sync::mpsc::sync_channel`:

1. The coordinator creates one bounded work channel per worker. Each worker
   owns its non-clonable `Receiver<WorkItem>`; the coordinator owns all
   `SyncSender<WorkItem>` values. Initial capacity is one, permitting at most
   one queued directory behind the directory currently being processed.
2. The coordinator creates one bounded worker-result channel. Every worker
   receives a clone of its `SyncSender<WorkerBatch>`, making this the required
   many-producer/single-consumer channel; the coordinator owns the sole
   receiver. Start at 8-16 batches and tune by benchmark.
3. The background coordinator publishes coherent `ScanEvent` values to the
   application through a separate bounded `sync_channel`. The eframe thread
   uses `try_recv` and drains a time- and batch-bounded amount per frame.
4. A worker never performs an indefinitely blocking `send`. It loops on
   `try_send`, checks the atomic cancellation token and generation between
   attempts, and yields or briefly parks. Essential data batches are retained;
   progress-only notifications may be coalesced.
5. Every work item, worker batch, and application event carries a monotonically
   increasing generation. Cancellation invalidates the generation immediately,
   and the application ignores all late events from older generations.

This topology works around the standard receiver not being clonable while
retaining bounded memory, explicit per-worker assignment, and many-producer
results. It also makes per-volume permits observable and testable. `flume` and
`crossbeam-channel` are benchmark-triggered alternatives, not initial
dependencies.

Cancellation is a local `CancelToken(Arc<AtomicBool>)`. Workers check it before
each directory API call, after metadata calls, periodically during enumeration,
and while a result channel is full. The coordinator stops dispatch immediately.
Best-effort Windows cancellation of a known dedicated worker may complement
the token, but must not replace RAII cleanup or terminate a thread.

## Runtime dependency evaluation

### UI and renderer

| Candidate | Evaluation | Outcome |
|---|---|---|
| [`eframe`/`egui` 0.35.0](https://docs.rs/eframe/0.35.0/eframe/) | MIT OR Apache-2.0; released 2026-06-25 and actively developed; eframe MSRV 1.92. Native presentation transitively includes unsafe/FFI in winit, glutin, OpenGL, and Windows bindings, so it is not allowed in portable crates. It directly provides DPI-aware Windows lifecycle, input, theme, AccessKit integration, persistence, and indexed meshes; reproducing those locally is unjustified. Build/binary cost is high and breaking API churn creates medium-high UI lock-in, mitigated by keeping egui types in the executable and committing the lockfile. | Adopt at the UI edge. |
| Glow backend in eframe 0.35.0 | Same license/activity boundary as eframe. The [official manifest](https://raw.githubusercontent.com/emilk/egui/0.35.0/crates/eframe/Cargo.toml) notes that switching from default WGPU to Glow can significantly reduce binary size. It is sufficient for DiskPie's two-dimensional indexed mesh. Native unsafe remains isolated in the UI graph. | Adopt Glow only; test Win10/11, RDP, VMs, and multiple GPUs. |
| WGPU backend in eframe 0.35.0 | Actively maintained and portable, but substantially increases compile time and binary/dependency footprint. It offers no measured benefit for the initial analytic 2D mesh and enabling both renderers compounds support cost. | Reject initially; revisit after a required Glow configuration fails or GPU profiling justifies it. |
| [`egui_extras` 0.35.0](https://docs.rs/egui_extras/latest/src/egui_extras/table.rs.html) | MIT OR Apache-2.0 and released with egui. Unsafe/platform exposure is inherited from the UI graph. With defaults disabled, its medium incremental cost earns virtualized `body.rows` for thousands of accessible rows; implementing a correct table locally is higher risk. Extra loader/image/SVG/syntax features add unrelated dependencies. Lock-in is limited to one UI adapter. | Adopt with `default-features = false`. |

The [`eframe::App`](https://docs.rs/eframe/latest/eframe/trait.App.html)
baseline is the 0.35 `ui` callback; older examples using `update` must not shape
the architecture. Use `egui::ThemePreference` for System/Light/Dark. Egui's
default fonts cover limited scripts; the product must later bundle a
permissively licensed fallback font for arbitrary path display.

### Channels, cancellation, arena, strings, and paths

| Candidate | Evaluation | Outcome |
|---|---|---|
| `std::sync::mpsc::sync_channel` and local atomic token | Standard-library maintenance/license baseline; no extra advisory, unsafe, build, binary, abandonment, or supply-chain surface. It supports Windows and the portable core. Per-worker receivers solve the non-clonable receiver limitation, and a cloned bounded result sender supplies MPSC. The small local backpressure loop is testable. Lock-in is negligible. | Adopt. |
| [`flume` 0.12.0](https://docs.rs/crate/flume/latest) | MIT OR Apache-2.0; released 2025-12-08 with upstream activity observed in 2026. Pure Rust and advertised as no unsafe. It supports bounded MPMC, cloned receivers, select, and sync/async APIs; default features also add select/async/random-fairness machinery. Windows/core portable; low-medium compile/binary cost. It becomes valuable only if MPMC/select materially simplifies a measured bottleneck. Maintainer concentration creates medium abandonment risk, mitigated by a channel adapter. | Do not add initially; benchmark with defaults disabled when triggered. |
| [`crossbeam-channel` 0.5.16](https://docs.rs/crate/crossbeam-channel/latest) | MIT OR Apache-2.0; released 2026-07-06 and mature. It is portable and fast but contains unsafe; its [changelog](https://docs.rs/crate/crossbeam-channel/latest/source/CHANGELOG.md) records historical double-free and uninitialized-memory fixes and yanked releases. Compile/binary cost is low-medium and its MPMC/select benefit is not required by the chosen topology. Ecosystem abandonment risk is low, but dependency/security surface exceeds the local solution. | Benchmark-triggered alternative only; require at least 0.5.16 and an audit. |
| `kanal` 0.1.1 | MIT; last release 2025-03-23. Performance-centric channel internals add unsafe/newer-project risk without a product requirement. Windows support is plausible and footprint is modest, but maintenance maturity and local benefit are weaker than std/flume/crossbeam. | Reject initially. |
| `tokio-util` 0.7.19 `CancellationToken` | MIT and released 2026-07-21. It supplies useful async cancellation trees, but imports Tokio concepts and unsafe future internals into a blocking filesystem design. Compile/binary and architectural lock-in are high relative to an atomic flag. | Reject. |
| Local `Vec<NodeRecord>` plus `NodeId(u32)` | Project license; safe portable Rust; no supply-chain/build cost. A four-byte ID and append/build model fit immutable scan revisions. The implementation cost is small but requires overflow, generation, ordering, and invalid-ID tests. | Adopt. |
| `slotmap` 1.1.1 | Zlib; released 2025-12-06. Portable but uses unsafe and eight-byte generational keys; release history includes memory-safety fixes. Deletion/stable-key behavior does not benefit an arena replaced by revision. Low compile cost but needless memory and representation lock-in. | Reject unless in-place deletion becomes a measured requirement. |
| `thunderdome` 0.6.1 / `generational-arena` 0.2.9 | Respectively MIT/Apache with last release 2023, and MPL-2.0 with last release 2023. Both use larger keys than the local ID; maintenance and, for the latter, license-policy risk exceed the benefit. | Reject. |
| `string-interner` 0.20.0 / `lasso` 0.7.3 | Permissive and portable, with string-interner updated 2026-04-30. Filenames are usually unique, so hash/symbol overhead rarely amortizes. Both are UTF-8 string-oriented and cannot be the lossless native-path representation. | Reject. |
| `compact_str` 0.10.0 / `smol_str` 0.3.6 | Permissive and current in 2026; compact string layouts reduce allocation for UTF-8 text but compact_str uses unsafe representation code. Neither preserves arbitrary Windows names. Local native storage gives a measurable path to lower memory without encoding loss. | Reject for filesystem identity; ordinary UI labels may use `String`. |
| [`camino` 1.2.5](https://docs.rs/crate/camino/latest) | MIT OR Apache-2.0 and released 2026-07-28. Maintained, safe, and low-cost, but intentionally UTF-8-only; conversion can fail for valid native paths. It would lock core correctness to a narrower path model. | Reject. |

### Settings, diagnostics, CLI, localization, dialogs, and errors

| Candidate | Evaluation | Outcome |
|---|---|---|
| Serde 1.0.229 plus [eframe Storage](https://docs.rs/eframe/latest/eframe/trait.Storage.html) | MIT OR Apache-2.0; Serde released 2026-07-18 and is foundational. Derive adds compile-time proc-macro cost but little runtime/binary overhead and no new native boundary. Eframe persistence is already paid for in the UI graph. A versioned `Settings` value behind `SettingsStore` limits format/framework lock-in and supports an explicit portable path through [`NativeOptions`](https://docs.rs/eframe/latest/eframe/struct.NativeOptions.html). | Adopt. |
| `confy` 2.0.0 / separate directories plus TOML or JSON | Confy is MIT and was released in 2025, but its [own documentation](https://docs.rs/confy/latest/confy/index.html) says it is not standard-compliant or maintained. A second path/format stack adds build and migration surface without initial benefit. | Reject; add an eframe-independent store only when a real headless frontend exists. |
| [`tracing` 0.1.44](https://docs.rs/tracing/0.1.44/tracing/), `tracing-subscriber` 0.3.23, `tracing-appender` 0.2.5 | MIT; releases span 2025-12 through 2026-04 and the ecosystem is active. Structured spans materially improve scan-generation, queue, latency, omission, and OS-error diagnostics versus a local concurrent logger. Compile/binary cost is medium. The non-blocking writer has a fixed queue and transitive concurrent unsafe; hold its `WorkerGuard`, expose dropped-line counts, and audit the resolved graph. Framework lock-in is medium but call sites remain ordinary events/spans. | Adopt with narrowly selected `std`, `fmt`, `env-filter`, and `registry` features. |
| `log` 0.4.33 plus `flexi_logger` 0.31.9 | Both are current and permissive; flexi_logger has useful rotation and retention. It loses structured scan/span fields or requires a bridge, duplicating the logging stack. Build cost is similar enough that the local benefit favors tracing. | Reject for the product baseline. |
| [`lexopt` 0.3.2](https://docs.rs/lexopt/latest/src/lexopt/lib.rs.html) | MIT; released 2026-02-28; source forbids unsafe. It is portable, preserves `OsString`/`PathBuf`, has minimal compile/binary cost, and a short parser loop is cheaper than local edge-case handling. Low lock-in because the parsed application options are owned locally. | Adopt. |
| `clap` 4.6.5 / `pico-args` 0.5.0 | Clap is active, MIT/Apache, and robust but derive/default help, suggestions, and styling are excessive for a handful of options; compile/binary cost is materially higher. Pico-args is MIT but last released in 2022. | Reject initially; revisit clap for nested commands, completions, or generated documentation. |
| [`fluent-bundle` 0.16.0](https://docs.rs/fluent-bundle/latest/fluent_bundle/bundle/struct.FluentBundle.html), `fluent-langneg` 0.14.2, `unic-langid` 0.9.6 | MIT OR Apache-2.0; releases in 2025 and a mature localization standard. Pure Rust with moderate parser/plural compile and binary cost. Plurals, selectors, arguments, negotiation, and fallback exceed a small map's correctness. Typed local message IDs and embedded FTL resources limit lock-in and permit backend replacement. | Adopt directly for embedded `en-US` and `es-MX`. |
| `i18n-embed` 0.16.0 / `rust-i18n` 4.2.1 | Permissive and maintained through 2025/2026. i18n-embed adds rust-embed, arc-swap, parking_lot, and macro/asset machinery; rust-i18n adds global macro/custom mapping lock-in. Both are portable, but neither provides enough additional product value over the direct Fluent facade to justify footprint. | Reject initially. |
| [`rfd` 0.17.2](https://docs.rs/rfd/latest/rfd/) | MIT; released 2026-01-12. Windows folder/save selection uses windows-sys/COM, so unsafe native FFI is significant but isolated in the executable/platform edge. With defaults disabled for a Windows build, compile/binary cost is low-medium and no external runtime is required. Native dialog behavior saves substantial platform code. API stability is marked provisional, creating medium lock-in/abandonment risk; a tiny `DialogPort` is the fallback seam. | Adopt for explicit folder/save actions only. |
| `native-dialog` 0.9.7 / C dialog wrappers | MIT and released 2026-05-30, but cross-platform behavior relies on external Linux dialog programs and its broad message-dialog surface is unnecessary. C wrappers add avoidable FFI/security/packaging exposure. | Reject. |
| [`thiserror` 2.0.19](https://docs.rs/thiserror/latest/thiserror/derive.Error.html) | MIT OR Apache-2.0; released 2026-07-18. The derive is compile-time only and generates standard `Error` implementations without runtime/native unsafe. It removes repetitive conversions while preserving typed, localizable error codes and source chains. Lock-in is low because public behavior remains `std::error::Error`. | Adopt in product crates. |
| `anyhow` 1.0.104 / `miette` 7.6.0 | Permissive and maintained, but type erasure or terminal-oriented reports obscure application error codes, exact paths, recoverability, and localization. They add binary/compile surface without benefit in a GUI product boundary. | Reject in product code; reconsider anyhow only for one-shot developer tooling. |

The application error shape should carry a stable `ErrorCode`, operation,
optional exact `PathBuf`, raw OS code, and source. Access failures become
reportable omissions rather than global scan failures. The UI maps error codes
to Fluent messages while diagnostics retain the full source chain. Never log
every filename at info level.

The tracing file layer should be non-ANSI plain text. Because
tracing-appender's rolling writer is time-based rather than retention-capped,
perform small, safe startup cleanup by retaining a configured number/total
size of DiskPie-created log files only.

## Development dependency evaluation

| Candidate | Evaluation | Outcome |
|---|---|---|
| [`proptest` 1.11.0](https://docs.rs/crate/proptest/latest/features) | MIT OR Apache-2.0; released 2026-03-24. Pure test dependency; disable defaults and enable only `std` to avoid fork/timeout/rusty-fork baggage on Windows. Strategy/shrinking and committed failure persistence materially improve interval, aggregation, ordering, ID, and cancellation-state testing. Medium test compile cost, zero release-binary cost, low lock-in through ordinary assertions. | Adopt dev-only. |
| [`criterion` 0.8.2](https://docs.rs/crate/criterion/latest) | MIT OR Apache-2.0; released 2026-02-04. Statistical baselines and reports provide measurable gates for queue, arena, scan, layout, and mesh choices. Dev-only compile cost is high but release impact is zero. Benchmark API lock-in is low. | Adopt dev-only with `harness = false`. |
| `tempfile` 3.27.0 | MIT OR Apache-2.0; released 2026-03-11 and actively maintained. It contains platform filesystem/unsafe internals but remains test-only. Its robust cleanup and Windows behavior are safer than hand-rolled temporary-path deletion. Tests may remove only descendants of the `TempDir` they created. | Adopt dev-only. |
| `quickcheck` 1.1.0 / `divan` 0.1.21 / `assert_fs` 1.1.4 | Permissive. Quickcheck is smaller but has less strategy composition and persistence; Divan is lightweight but lacks Criterion's established baselines/reporting; assert_fs duplicates small fixture helpers. All are dev-only, but extra graphs still require audit and compile time. | Reject initially. |

Fuzzing may use cargo-fuzz/libFuzzer as external CI tooling; it is not a
runtime dependency and does not change the stable product toolchain.

## Proposed dependency baseline

```toml
[workspace.package]
edition = "2024"
rust-version = "1.92"
license = "MIT OR Apache-2.0"

# diskpie executable/UI edge
eframe = { version = "0.35.0", default-features = false, features = [
  "glow", "accesskit", "default_fonts", "persistence"
] }
egui_extras = { version = "0.35.0", default-features = false }
rfd = { version = "0.17.2", default-features = false }

# focused product dependencies
serde = { version = "1.0.229", features = ["derive"] }
thiserror = "2.0.19"
lexopt = "0.3.2"
fluent-bundle = "0.16.0"
fluent-langneg = "0.14.2"
unic-langid = "0.9.6"
tracing = { version = "0.1.44", default-features = false, features = ["std"] }
tracing-subscriber = { version = "0.3.23", default-features = false,
  features = ["std", "fmt", "env-filter", "registry"] }
tracing-appender = "0.2.5"

# dev-only
proptest = { version = "1.11.0", default-features = false, features = ["std"] }
criterion = "0.8.2"
tempfile = "3.27.0"
```

Do not put every dependency in `[workspace.dependencies]` merely because it is
listed here. Each crate declares only the dependencies it uses. The executable
may depend directly on `egui` 0.35.0 if it imports egui rather than using the
eframe re-export.

## Unsafe, security, and platform boundaries

- Add `#![forbid(unsafe_code)]` to `diskpie-core`, `diskpie-scan`, and
  `diskpie-app`.
- Project-authored Windows unsafe belongs only under
  `diskpie-platform/src/windows/`. Each block is minimal, has a `// SAFETY:`
  invariant, converts raw handles immediately into RAII ownership, and
  preserves the original OS error code.
- The `diskpie` executable should contain no project-authored unsafe. Native
  unsafe from winit/glutin/rfd/windows-sys remains a reviewed dependency edge,
  not part of domain code.
- Run `cargo deny`, `cargo audit`, the compatible-license allow-list, and
  `cargo tree -e features` in CI. Audit development dependencies as well.
- Never enable dependency features by convenience (`all`, all loaders, both
  renderers). Feature additions require the same evaluation rubric.
- No GPL, AGPL, SSPL, proprietary Scanner resource, or copied Filelight code
  may enter the repository.

## Validation and escalation gates

Record files/second, first-visible-batch time, total scan time, peak memory per
million entries, cancellation publication and cleanup latency, blocked-send
time, queue depth, layout time, frame time, and portable executable size.

Escalate dependencies only when a reproducible test demonstrates the trigger:

- benchmark std channels against flume and crossbeam only when queue
  contention, MPMC receiver cloning, or select is a demonstrated constraint;
- try WGPU when Glow fails a required Windows/RDP/VM configuration or rendering
  misses its measured budget;
- implement a contiguous native-name arena when names dominate memory;
- reconsider generational storage only for required in-place deletion that a
  whole-tree revision cannot safely model;
- adopt clap when the CLI becomes nested or needs completions/generated docs;
  and
- add a separate settings backend when an actual non-eframe frontend exists.

The architecture remains intentionally reversible at its external seams while
keeping the common path small, synchronous, bounded, and testable.
