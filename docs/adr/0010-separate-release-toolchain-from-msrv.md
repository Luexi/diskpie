# ADR 0010: Separate the release toolchain from the MSRV

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must ship a reviewable Windows x86-64 executable and support
contributor builds on stable Rust. At the time of this decision the workspace
declares `rust-version = "1.92"`, while `rust-toolchain.toml` selects the
floating `stable` channel. Those settings answer different questions:

- `package.rust-version` is the oldest compiler version the package claims to
  support; and
- the toolchain file selects the compiler used by contributors, CI, and
  release builds.

Leaving the release compiler as `stable` makes the compiler, LLVM, Cargo, and
standard library change without a source change. Raising `rust-version` to the
current compiler would unnecessarily abandon a stated compatibility promise.
Rust 1.97.1 was released on 2026-07-16 and fixes an LLVM miscompilation, making
it the appropriate current stable point release rather than 1.97.0.

Supporting research and primary sources are recorded in
[Windows packaging and release engineering](../research/windows-packaging-and-release.md).

## Decision drivers

- Repeatable release inputs and meaningful byte-for-byte comparison.
- Avoiding a compiler version with a known fixed miscompilation.
- An explicit, testable MSRV promise for contributors and downstream users.
- Stable Rust only; no nightly-only release dependency.
- Controlled compiler updates with reviewable benchmark and binary evidence.
- Low maintenance cost and no third-party toolchain setup action.

## Options considered

### Float on the `stable` channel

This is convenient for development and continuously receives fixes, but two
release jobs at different times can use different compilers without any
repository diff. It prevents a durable provenance statement and makes binary
differences hard to attribute.

### Pin Rust 1.97.1 and keep MSRV 1.92 separate

The toolchain file binds the production compiler while `rust-version` retains
the package compatibility declaration. A dedicated MSRV job proves the lower
bound. Compiler upgrades become ordinary reviewed changes with fresh tests,
benchmarks, dependency resolution, and reproducibility evidence.

### Set both the release toolchain and MSRV to 1.97.1

This is simple and guarantees only one tested compiler, but it raises the
consumer/contributor requirement without a product need. It confuses release
repeatability with language/library compatibility.

### Pin an older 1.92 compiler for every build

This maximizes similarity with the declared MSRV but gives production builds
none of the later stable compiler fixes and optimizations. An MSRV is a lower
compatibility test, not necessarily the safest release compiler.

## Decision

DiskPie will pin the release and default repository toolchain to Rust 1.97.1,
including the minimal profile, required components, and
`x86_64-pc-windows-msvc` target.

DiskPie will keep `package.rust-version = "1.92"` as an independent MSRV
promise. CI must test the locked workspace with Rust 1.92 in a dedicated job.
If that job fails because project code or a locked dependency genuinely
requires a newer compiler, maintainers must either restore compatibility or
raise the MSRV explicitly with changelog and contributor documentation. The
release compiler version alone is never a reason to raise the MSRV.

Release builds must:

1. use the committed `Cargo.lock` and `--locked`;
2. record `rustc -vV`, `cargo -vV`, the active toolchain, and the target;
3. record the GitHub runner, MSVC linker, and Windows SDK resource-tool
   versions because pinning Rust does not pin the hosted Windows image; and
4. repeat the unsigned binary reproducibility gate after every compiler update.

Compiler updates use an explicit reviewed pull request. Stable point releases
that fix correctness or security defects receive priority, but they still run
the same tests, benchmarks, PE inspection, and byte comparison before becoming
the release toolchain.

## Consequences

Positive outcomes:

- A release provenance statement can name an exact Rust/Cargo/LLVM toolchain.
- Compiler-caused byte changes are intentional and reviewable.
- The 1.92 compatibility promise remains meaningful and independently tested.
- No third-party GitHub Action is required to select Rust; rustup honors the
  repository toolchain file.
- A known 1.97.0 LLVM miscompilation is not present in the release baseline.

Accepted tradeoffs:

- The pinned compiler does not update automatically; maintainers own regular
  review of stable release announcements and advisories.
- `windows-2022` is still a moving hosted image. Full environment identity must
  be recorded, and cross-date reproducibility is not guaranteed merely by this
  ADR.
- CI spends time on both the MSRV and release compilers.
- Some future dependency updates may force a deliberate MSRV decision.

Fallback and reconsideration:

- If Rust 1.97.1 has a confirmed DiskPie-affecting regression, pin the nearest
  corrected stable point release or temporarily revert to a documented safe
  compiler. Do not return to a floating release channel.
- If sustaining 1.92 blocks necessary maintained dependencies, raise the MSRV
  in a dedicated compatibility decision with migration notes.
- If a future hermetic Windows builder becomes practical, it may additionally
  pin MSVC and SDK inputs without changing the MSRV separation.

## Validation

- `rustc -vV`, `cargo -vV`, and `rustup show active-toolchain` report 1.97.1 in
  the release job.
- Rust 1.92 successfully runs the workspace MSRV check against `Cargo.lock`.
- Formatting, clippy, tests, Windows tests, and benchmarks pass on 1.97.1.
- Two clean, differently rooted checkouts produce an identical unsigned EXE
  and canonical unsigned ZIP.
- A compiler update pull request includes before/after binary size, critical
  benchmark results, PE/import checks, and new build-environment records.

Primary evidence:

- [Rust 1.97.1 announcement](https://blog.rust-lang.org/2026/07/16/Rust-1.97.1/)
- [Cargo `rust-version` field](https://doc.rust-lang.org/stable/cargo/reference/rust-version.html)
- [rustup toolchain file](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file)
