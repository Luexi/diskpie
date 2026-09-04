# ADR 0011: Keep Glow and gate portable MSVC runtime linkage

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must be a responsive Windows 10/11 x86-64 GUI distributed as a
portable executable. The workspace already uses eframe 0.35.0 with default
features disabled and the Glow renderer enabled. A Windows portable build must
also start on a clean client that has no Rust, Visual Studio, or separately
installed Visual C++ Redistributable.

Renderer and CRT choices both affect the same release properties: native
dependency surface, driver/runtime compatibility, compile time, EXE size,
security boundaries, diagnostics, and support burden. They must be selected by
the required benchmark and clean-machine matrix, not by intuition.

eframe supports both Glow and WGPU and states that enabling Glow instead of
WGPU can materially reduce binary size. Enabling both is unnecessary for the
first release. eframe 0.35.0 depends on the wgpu 29 line through egui-wgpu; the
independently latest wgpu major is not interchangeable.

Rust's MSVC target normally uses dynamic C runtime linkage. Full
`+crt-static` statically links startup, VCRuntime, and UCRT. The compatible
`static_vcruntime` 3.0.0 build dependency offers a hybrid: static startup and
VCRuntime with the UCRT dynamically supplied by Windows 10/11. Its upstream
documentation warns that some C/C++ dependencies can be incompatible and that
Microsoft recommends dynamic runtime libraries.

Supporting research, dependency comparisons, and primary sources are recorded
in [Windows packaging and release engineering](../research/windows-packaging-and-release.md).

## Decision drivers

- One portable `diskpie.exe` with no non-system renderer or CRT DLLs.
- Reliable launch on clean Windows 10 and Windows 11 x86-64 clients.
- Responsive sunburst rendering across physical GPUs, VMs, and Remote Desktop.
- Small binary and bounded release build time.
- Minimal native/unsafe dependency and GPU-driver surface.
- Reproducible unsigned output and inspectable PE imports/mitigations.
- Evidence from DiskPie's performance, size, and client matrix.
- A clean fallback when a build dependency or renderer is abandoned.

## Options considered

### Glow only

This matches the current dependency graph, adds no separate renderer DLL, and
has the smallest expected eframe renderer footprint. Its main risk is OpenGL
context/driver behavior on basic, virtualized, or remote Windows graphics.

### WGPU only with a narrowed DX12 backend

DX12 is native to modern Windows and may improve compatibility on some
machines. WGPU adds a substantially larger HAL/shader/backend graph, compile
time, binary size, and unsafe/driver boundary. A prototype must use the
eframe-compatible wgpu 29 line and avoid shipping DXC or backend DLLs.

### Compile both Glow and WGPU

This permits a runtime choice but makes every user download and expose both
backends. It also doubles failure modes and makes support/repro evidence
ambiguous. A renderer fallback does not justify this cost before measurements.

### Default dynamic MSVC runtime

This is the Microsoft-preferred servicing model and the least custom build.
It may leave a `VCRUNTIME*.dll` prerequisite that violates the clean-machine
portable contract.

### Full `+crt-static`

This removes external CRT imports with no helper crate. It can increase the
binary and statically embeds runtime code that is updated only by rebuilding.
Native dependencies must agree on linkage.

### Hybrid `static_vcruntime` plus `+crt-static`

This aims to remove the separately deployed VCRuntime while continuing to use
the UCRT that is an operating-system component on Windows 10/11. It adds a
small build-time dependency and non-default linker behavior that requires
clean-machine, native-dependency, and reproducibility proof.

## Decision

DiskPie will ship the first stable Windows build with **Glow only**. It will
not compile WGPU into the same release executable. WGPU remains a contingency
prototype behind eframe's renderer boundary. It can replace Glow only if the
supported Windows matrix finds a repeatable Glow failure or a measured WGPU
advantage that justifies its size, compile-time, unsafe-surface, and artifact
costs.

DiskPie will prototype all three CRT variants from independent clean target
directories:

1. default dynamic linkage;
2. full `+crt-static`; and
3. `static_vcruntime` 3.0.0 plus `+crt-static`.

The preferred candidate is the hybrid only if it satisfies every acceptance
criterion below. Until the prototype finishes, default linkage remains the
development baseline and no documentation may claim that the EXE has no
VCRedist dependency.

The hybrid is adopted for release only when:

- `dumpbin /DEPENDENTS` and `/IMPORTS` show only Windows inbox DLLs and no
  `VCRUNTIME*`, `MSVCP*`, `libgcc*`, DXC, or renderer DLL;
- it launches and completes smoke scans on clean required Windows clients with
  no developer tools or VC Redistributable installed;
- all current native dependencies pass tests in that linkage mode;
- the two-build unsigned reproducibility gate passes;
- PE mitigation and BinSkim checks do not regress; and
- size, startup, scan, render, and compile/link measurements have no material
  regression versus the default and full-static candidates.

DiskPie will initially keep Cargo's default release profile and unwind panic
strategy. LTO, codegen-unit, size optimization, symbol, `panic=abort`, and
`-C control-flow-guard=checks` changes are separate benchmark experiments. A
successful smaller link is not sufficient evidence to weaken diagnostics or
change cleanup semantics.

## Consequences

Positive outcomes:

- The initial renderer is singular, already integrated, and size-conscious.
- The portable-runtime claim is established by imports and client execution,
  not inferred from a compiler flag.
- The UCRT can remain an OS-serviced component if the hybrid passes.
- Renderer and CRT fallbacks are explicit and independently measurable.
- Product crates need not add unsafe code for packaging or rendering choices.

Accepted tradeoffs:

- Glow requires a wider real-machine graphics matrix than a simple CI launch.
- The hybrid CRT prototype adds a build dependency and can expose native build
  incompatibilities.
- Full static linking may be selected if it is the only clean-machine solution,
  accepting size and servicing costs.
- Shipping one renderer means a renderer-specific failure cannot be repaired
  by an in-process switch; a later release must change the chosen backend.
- Release optimization opportunities wait for representative benchmarks.

Fallback and reconsideration:

- If Glow fails on a required client, prototype eframe-compatible WGPU 29 with
  the narrowest DX12 feature set and compare the complete release matrix.
- If the hybrid CRT fails, use full `+crt-static` if it passes, or retain
  dynamic linkage and explicitly document/install the supported VCRedist. Do
  not copy random runtime DLLs beside the executable.
- If `static_vcruntime` becomes unmaintained, the build can return to either
  Rust-native linkage mode without changing application architecture.
- If a release-profile experiment has a material measured benefit, record the
  adopted settings and benchmark evidence in a superseding ADR.

## Validation

Renderer validation records:

- release EXE bytes and clean build/link time;
- warm/cold startup and normal/invalidated frame latency;
- physical Intel/AMD/NVIDIA coverage where available;
- VM/basic-display and Remote Desktop behavior;
- mixed-monitor DPI, resize, theme, and accessibility behavior; and
- absence of renderer/DXC redistributable artifacts.

CRT validation records for every candidate:

- `dumpbin /DEPENDENTS`, `dumpbin /IMPORTS`, and PE header/resource reports;
- clean Windows 10 22H2, Windows 11 24H2, and Windows 11 25H2 launches as a
  standard user with no developer runtime;
- full tests and Windows-specific shell/scanner tests;
- binary SHA-256 from two clean differently rooted builds;
- BinSkim/mitigation output; and
- binary size, startup, scanner, renderer, and build-time benchmark deltas.

Primary evidence:

- [eframe renderer feature documentation](https://docs.rs/eframe/0.35.0/eframe/#feature-flags)
- [Rust static/dynamic CRT linkage](https://doc.rust-lang.org/stable/reference/linkage.html#static-and-dynamic-c-runtimes)
- [`static_vcruntime` 3.0.0](https://docs.rs/static_vcruntime/3.0.0/static_vcruntime/)
- [Microsoft Universal CRT deployment](https://learn.microsoft.com/en-us/cpp/windows/universal-crt-deployment?view=msvc-170)
- [Cargo release profile defaults](https://doc.rust-lang.org/cargo/reference/profiles.html#release)

## Addendum 2026-09-03: release CRT linkage measurement and adoption

This addendum records the first two of the three CRT variants named in the
decision, measured from independent clean target directories on the branch
that embeds the PE identity resources (`crates/diskpie/build.rs`).

Environment:

- Host and target: `x86_64-pc-windows-msvc` on Windows 11 Pro 10.0.26200.
- Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo 1.97.1`
  (`rust-toolchain.toml`), Cargo default release profile, `--locked`.
- Inspection: `dumpbin.exe` from MSVC 14.51.36231, located with `vswhere`.
- Launch test: `diskpie.exe --version` exit code. The release binary detaches
  its console (`windows_subsystem = "windows"`), so the version text is not
  observable; exit code 0 is the criterion, and `--definitely-not-a-flag`
  exiting 2 confirms that argument parsing, not a stub, produced the 0.

Exact commands:

```text
cargo build --release -p diskpie --locked
$env:CARGO_TARGET_DIR = "<scratch>\target-crt-static"
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release -p diskpie --locked
$dumpbin = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" `
  -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
  -find 'VC/Tools/MSVC/**/bin/Hostx64/x64/dumpbin.exe'
& $dumpbin /NOLOGO /DEPENDENTS <exe> | Where-Object { $_ -match '\.dll' }
(Get-Item <exe>).Length
$p = Start-Process -FilePath <exe> -ArgumentList '--version' -PassThru
$p.WaitForExit(20000); $p.ExitCode
```

Measured rows (imports are the `dumpbin /DEPENDENTS` list, de-duplicated
case-insensitively):

| Variant | EXE bytes | SHA-256 | CRT imports | Other imports | `--version` exit |
|---|---|---|---|---|---|
| 1. Default dynamic linkage | 8,453,632 | `5B421B98…D3AE88` | `VCRUNTIME140.dll`, `api-ms-win-crt-{math,string,runtime,stdio,locale,heap}-l1-1-0.dll` | `kernel32`, `user32`, `gdi32`, `shell32`, `shlwapi`, `advapi32`, `ole32`, `oleaut32`, `combase`, `ntdll`, `bcryptprimitives`, `uiautomationcore`, `opengl32`, `imm32`, `dwmapi`, `uxtheme`, `api-ms-win-core-synch-l1-2-0` | 0 |
| 2. Full `+crt-static` | 8,631,296 (+177,664, +2.1%) | `77E5EFCE…035B454` | none | identical to row 1 | 0 |

Row 1 confirms the concern in the decision: Rust's default MSVC linkage leaves
a `VCRUNTIME140.dll` prerequisite, so it fails the portable contract on any
client without the Visual C++ Redistributable. Its exit code 0 on the
developer machine is not portability evidence because that machine has the
redistributable installed; only the import list is evidence. Row 2 removes
every `VCRUNTIME*`, `MSVCP*`, and `api-ms-win-crt-*` import, adds no
`libgcc*`, renderer, or DXC DLL, keeps the identical inbox import set, still
launches, and costs 2.1% of binary size.

Decision: **adopt full `+crt-static`** now, through
`.cargo/config.toml` under `[target.x86_64-pc-windows-msvc] rustflags`, as the
"fallback and reconsideration" clause already allows. The static build imports
no CRT DLL and launches, which were the two adoption gates for this pass.

Still pending, and required before any release notes claim "no VCRedist
needed":

- Variant 3 (`static_vcruntime` plus `+crt-static`) was not measured in this
  pass; it stays an open experiment and can still displace variant 2 if it
  passes every criterion above with a material advantage.
- Clean-VM validation as a standard user on Windows 10 22H2, Windows 11 24H2,
  and Windows 11 25H2 with no developer tools or redistributable.
- Compile/link time, startup, scan and render benchmarks, BinSkim/mitigation
  output, and the ADR 0012 two-checkout reproducibility gate under the new
  flags.

Operational note: Cargo replaces, never merges, `[target.*].rustflags` when
`RUSTFLAGS` or `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS` is set. The
release workflow that adds `--remap-path-prefix` (ADR 0012) must therefore
re-include `-C target-feature=+crt-static` explicitly, and the release
import-list inspection remains the backstop that catches a silent regression
to dynamic linkage.
