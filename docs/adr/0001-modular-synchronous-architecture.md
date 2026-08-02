# ADR 0001: Modular synchronous application architecture

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie is a Windows-first, portable disk analyzer that must scan millions of
entries without freezing its egui interface. It must stream coherent partial
results, cancel and rescan safely, preserve arbitrary native paths, support
destructive shell actions behind exact-path confirmation, and retain a core
that can compile without eframe or Windows APIs.

Filesystem enumeration is blocking. The initial product has no network service,
distributed deployment, or asynchronous application domain. An async runtime,
unbounded event bus, UI-owned filesystem traversal, or task-per-file design
would add scheduling and memory complexity without satisfying a product need.

Supporting evaluation is recorded in
[Rust, egui, and application architecture research](../research/rust-egui-architecture.md).
Filesystem-specific policies remain in ADRs 0002-0005, and sunburst geometry is
specified in ADR 0006.

## Decision drivers

- Bounded memory, handles, queues, and thread counts for arbitrarily large
  directory trees.
- A responsive renderer that never waits for filesystem or shell I/O.
- Coherent partial results, deterministic final aggregation, and stale-event
  rejection across cancellation and rescan.
- Lossless Unicode/native path handling, including long paths and names that
  are not valid UTF-8.
- A portable, safe domain model and scan protocol.
- Windows 10/11 x86-64 support and a small portable-executable footprint.
- Minimal supply-chain, unsafe-code, compile-time, and framework lock-in.
- Measured escalation to more complex crates only when a benchmark or required
  platform test demonstrates the need.

## Options considered

### Single eframe crate with UI-owned scanning

This minimizes initial files, but couples the model and filesystem lifecycle to
eframe, makes headless tests difficult, encourages I/O during UI callbacks, and
spreads Windows and native-dialog types through product logic. It cannot enforce
the portable-core requirement.

### Async runtime and task-per-directory/file

Tokio channels and cancellation compose well for async I/O, but Windows
directory enumeration and metadata queries are blocking. Per-entry tasks add
allocation, scheduler overhead, cancellation complexity, and poor device-aware
backpressure. The async runtime and `tokio-util` graph increase binary and
architectural lock-in without making filesystem calls cancellable.

### Fixed workers with a third-party MPMC channel

Flume and crossbeam-channel offer cloned receivers and selection. They are
reasonable measured alternatives, but MPMC receivers are not required when the
coordinator assigns work through one channel per worker. Crossbeam introduces
additional unsafe/security surface, while Flume introduces features and a
maintainer dependency that the initial topology does not need.

### Fixed workers with bounded standard-library channels

Per-worker work channels avoid the standard receiver-cloning limitation. A
cloned bounded `SyncSender` provides the required many-producer worker-result
path. This gives explicit backpressure, assignment, and capacity with no new
runtime dependency. A small cancellation-aware `try_send` loop is required.

### Generic generational arena and UTF-8 path wrappers

Slot maps make deletion and stale-key detection convenient, while UTF-8 path
wrappers simplify display. DiskPie initially replaces a scan tree as a coherent
revision, so in-place deletion does not justify larger keys. Valid Windows
paths need not be UTF-8, so UTF-8 wrappers cannot be the identity layer.

### Five-crate modular monolith

Five workspace crates create compile-time boundaries for domain, scanning,
application state, platform implementations, and presentation without the
operational or abstraction overhead of services, plugins, or a generalized
hexagonal framework.

## Decision

### 1. Use an acyclic five-crate workspace

An arrow means "depends on":

```text
diskpie-scan     -> diskpie-core
diskpie-app      -> diskpie-core, diskpie-scan
diskpie-platform -> diskpie-core, diskpie-scan, diskpie-app
diskpie          -> diskpie-core, diskpie-scan, diskpie-app, diskpie-platform
```

No other workspace dependency direction is permitted without a superseding
ADR. The responsibilities are:

- `diskpie-core`: compact model, exact sizes, aggregation, stable IDs,
  sunburst layout, and hit testing;
- `diskpie-scan`: portable filesystem-consumer interface, bounded coordinator,
  fixed blocking workers, cancellation, batches, omissions, and generations;
- `diskpie-app`: UI-independent reducer/state, navigation, TreeBuilder,
  settings schema, typed localization IDs, diagnostics, and external ports;
- `diskpie-platform`: target-gated implementations for scanning metadata,
  volumes, shell, recycle/delete, settings paths, and context-menu operations;
  and
- `diskpie`: composition root, eframe lifecycle, UI, mesh adapter, startup CLI,
  repaint policy, and native dialog adapter.

Portable consumers own their narrow traits; the outward platform crate depends
inward to implement them. `diskpie-core`, `diskpie-scan`, and `diskpie-app` do
not name egui, eframe, rfd, windows-sys, Win32, or COM types.

### 2. Use fixed blocking workers and bounded std channels

The initial channel mechanism is `std::sync::mpsc::sync_channel`, arranged as:

```text
                         cloned SyncSender<WorkerBatch>
worker 0 -- result ---\
worker 1 -- result ----+--> bounded MPSC result channel --> coordinator
worker N -- result ---/
     ^                         |
     |                         v
capacity-1                coherent bounded
work channel              ScanEvent channel
     |                         |
     +------ coordinator       v
                         diskpie-app/eframe drain
```

- The coordinator owns one capacity-1 `SyncSender<WorkItem>` per worker. Each
  worker owns exactly one non-clonable work receiver.
- All workers clone the sender of one bounded many-producer/single-consumer
  result channel. The coordinator owns its receiver. Initial result capacity is
  8-16 batches, subject to benchmarks.
- The background coordinator alone owns pending-directory queues, outstanding
  work, aggregation order, and per-volume permits. Workers return discovered
  children in batches; workers never recursively schedule them.
- The coordinator emits coherent application events over a separate bounded
  standard channel. The eframe thread drains it using `try_recv` within a
  per-frame batch/time budget and never waits for completion.
- Essential batches use a cancellation-aware `try_send` retry loop rather than
  an indefinitely blocking `send`. Progress-only events may be coalesced.

For the channel mechanism, this decision is authoritative and ADR 0004 uses the
same topology. ADR 0004 remains authoritative for coordinator behavior, bounded
scheduling, device permits, batch sizing, cancellation, and validation
semantics. `flume` 0.12.0 and `crossbeam-channel` 0.5.16 remain
benchmark-triggered alternatives only; neither is an initial dependency.

### 3. Use local generation-aware cancellation

`CancelToken` wraps `Arc<AtomicBool>`. Every work item, worker batch, and
application event carries a monotonically increasing scan generation.
Cancellation invalidates the generation immediately and stops new dispatch;
late events from old generations cannot mutate the current tree.

Workers check cancellation before directory API calls, after metadata calls,
periodically during enumeration, and while result backpressure exists. A
best-effort Windows cancellation call against a known dedicated worker may be
added behind the platform adapter, but never substitutes for RAII cleanup and
never uses asynchronous thread termination.

### 4. Use a local compact tree and lossless native names

The baseline is `Vec<NodeRecord>` indexed by checked `NodeId(u32)`. A whole-tree
revision protects application/UI IDs across replacement and rescan. Hot records
hold compact indices and numeric aggregates; names live separately, and full
paths are reconstructed lazily into reusable buffers.

Start with safe native `OsStr`/`OsString` storage. Add a local contiguous
UTF-16/Unix-byte name arena only after memory profiling demonstrates the need.
Do not use a UTF-8-only path crate as filesystem identity and do not persist
`OsStr::as_encoded_bytes`.

### 5. Keep geometry and application state UI-independent

The sunburst engine outputs backend-neutral indexed sector records. The
executable converts an immutable layout snapshot into one or a few cached
`egui::Mesh` values. Hit testing remains in `diskpie-core`. The application
coordinator alone applies worker batches to TreeBuilder; painting does not hold
model locks or call the filesystem.

Use eframe/egui 0.35.0 with default features disabled and enable only Glow,
AccessKit, default fonts, and persistence. Use egui_extras 0.35.0 with defaults
disabled for the virtualized textual view. Do not enable both Glow and WGPU.

### 6. Adopt focused boundary dependencies

Use the exact researched baselines documented in the supporting research:

- Serde 1.0.229 plus eframe Storage behind `SettingsStore`;
- tracing 0.1.44, tracing-subscriber 0.3.23, and tracing-appender 0.2.5;
- lexopt 0.3.2 for path-preserving startup arguments;
- fluent-bundle 0.16.0, fluent-langneg 0.14.2, and unic-langid 0.9.6 behind
  typed localization IDs;
- rfd 0.17.2 behind `DialogPort`; and
- thiserror 2.0.19 for typed errors carrying stable codes and sources.

Use proptest 1.11.0, Criterion 0.8.2, and tempfile 3.27.0 only as development
dependencies. Commit `Cargo.lock`; use compatible semver requirements in
manifests and deliberate lockfile upgrades.

### 7. Enforce the unsafe and platform boundary

`diskpie-core`, `diskpie-scan`, and `diskpie-app` use
`#![forbid(unsafe_code)]`. Project-authored unsafe is allowed only in
target-gated modules under `diskpie-platform/src/windows/`. Every unsafe block
must be minimal, state a `// SAFETY:` invariant, acquire raw handles directly
into RAII ownership, and preserve the original OS error.

The `diskpie` executable contains no project-authored unsafe. Native unsafe from
winit, glutin, rfd, windows-sys, and transitive support crates is accepted only
at the reviewed platform/UI dependency edge. CI audits licenses, advisories,
resolved features, and runtime and development dependency graphs.

## Consequences

### Positive

- Workspace edges enforce a portable core, deterministic ownership, and UI/OS
  isolation at compile time.
- Fixed threads and bounded queues provide explicit memory, handle, and
  backpressure limits.
- The std channel topology satisfies per-worker dispatch and many-producer
  results without a third-party concurrency dependency.
- Generation IDs make cancellation, rescan, stale UI state, and late worker
  results explicit and testable.
- A local four-byte node ID and lossless native name representation minimize
  memory without sacrificing Windows correctness.
- Eframe, native dialogs, Fluent, and tracing are confined to seams where their
  functionality materially exceeds a small local implementation.

### Negative and accepted trade-offs

- Per-worker work channels require a small explicit dispatcher rather than a
  cloned MPMC receiver.
- `SyncSender` has no cancellation-aware blocking send, so essential-result
  backpressure requires a carefully tested `try_send` loop.
- Egui/eframe remains a high-footprint, fast-moving UI dependency. The UI edge
  will require deliberate upgrades.
- Glow may behave differently under old drivers, RDP, or virtual machines.
- The local arena and scheduler require invariants and tests that a generic
  crate would otherwise provide.
- Direct Fluent and tracing add moderate build/binary size, accepted for
  correct localization and actionable concurrent diagnostics.

### Mitigations

- Keep channel and platform operations behind small consumer-owned interfaces,
  but do not add generalized repository/event-bus abstractions.
- Commit property tests for interval/tree invariants and deterministic fake
  scanner tests for all completion orders and backpressure states.
- Test Glow on required Windows 10/11, DPI, RDP, VM, and multi-GPU profiles.
- Commit the lockfile, deny incompatible licenses/features, and run advisory
  review for runtime and dev graphs.
- Record performance baselines before replacing std channels, the local arena,
  or Glow.

## Validation

The decision remains valid when all of the following hold:

- `cargo fmt --all -- --check`, workspace clippy with warnings denied, and all
  tests pass on stable Rust;
- dependency-direction checks show the exact acyclic graph above and portable
  crates compile without Windows/UI dependencies;
- stress fixtures demonstrate bounded threads, work, results, handles, and
  resident memory for million-entry, deep, wide, denied-access, disappearing
  volume, and slow-provider scenarios;
- cancellation tests cover pre-dispatch, mid-directory, metadata call, full
  result channel, rapid cancel/rescan, and late generations without stale tree
  mutation;
- deterministic fake workers produce identical final trees under randomized
  completion/error ordering;
- property tests cover arena IDs, aggregation, angular containment/sums, and
  hit-test boundaries;
- benchmarks record files/second, first-visible-batch time, total time, memory
  per million entries, blocked-send time, queue depths, cancellation publication
  and cleanup latency, layout/frame time, and portable binary size; and
- Windows integration tests prove native-path, long-path, Unicode, handle
  cleanup, dialog, settings, and shell-adapter behavior.

## Revisit triggers

- Benchmark flume 0.12.0 and crossbeam-channel 0.5.16 when std-channel
  contention is material, cloned receivers/select are required, or the local
  retry loop misses cancellation/throughput targets. Adoption requires a
  separate dependency and unsafe/security review.
- Enable WGPU instead of Glow when a required Windows/RDP/VM configuration
  cannot render correctly or GPU/render profiling demonstrates a material win.
- Replace boxed native names with a local contiguous arena when name storage
  dominates measured memory per million entries.
- Reconsider generational storage only when required in-place deletion cannot
  be modeled safely with a whole-tree revision.
- Add clap when the command line gains nested subcommands, shell completions,
  or generated documentation.
- Extract an eframe-independent settings backend when a real headless or second
  UI frontend exists.

Any trigger changes this accepted baseline only through a new or superseding
ADR with measurements and the complete dependency evaluation rubric.
