# Windows packaging and release engineering

- Research date: 2026-08-02
- Target: Windows 10/11 x86-64, portable GUI executable, standard user
- Release target: `x86_64-pc-windows-msvc`
- Current application stack: Rust, eframe 0.35.0, Glow renderer
- Outcome: embed native PE identity, keep Glow as the initial renderer, pin the
  release compiler independently from the MSRV, prove unsigned builds before
  optional signing, and publish a deterministic portable bundle with checksums,
  dependency evidence, and GitHub attestations.

## Recommendation

Build one `diskpie.exe` with first-party artwork and an embedded version
resource, icon group, and application manifest. Use `winresource` 0.1.31 from
the binary crate's build script. The manifest must declare `asInvoker`,
`uiAccess=false`, the Windows 10/11 compatibility GUID, Per-Monitor V2 DPI
awareness with a compatible fallback, and `longPathAware=true`. A long-path
manifest does not replace DiskPie's extended-length Unicode path handling or
the machine-wide Windows long-path policy.

Keep the existing Glow-only eframe feature set for the first stable release.
It has the smallest known renderer footprint and adds no renderer DLL to the
portable bundle. WGPU is a measured fallback, not a second renderer shipped in
the same executable. eframe 0.35.0 resolves `wgpu ^29`, so any prototype must
use the lock-resolved 29.x line (29.0.4 was current in that line on the research
date), not the independently current wgpu 30 API.

Pin release builds to Rust 1.97.1. Keep `package.rust-version = "1.92"` as a
separate MSRV promise only while a dedicated 1.92 job remains green. Rust
1.97.1 is the current stable point release and fixes an LLVM miscompilation;
floating `stable` is not a reproducible release input. Keep Cargo's default
release profile until DiskPie's required benchmarks justify LTO, codegen-unit,
size-optimization, panic, debug-symbol, or Control Flow Guard changes.

Build and attest an unsigned canonical executable first. Two clean checkouts at
different absolute paths on the same pinned runner/toolchain must produce the
same SHA-256 before the project claims reproducibility. Authenticode is an
optional later stage because no certificate or signing service is assumed.
Signing changes the PE bytes, so preserve the unsigned digest and compute a new
digest for the signed executable and the bundle.

The canonical download is a deterministic ZIP containing the portable EXE,
run instructions, both project licenses, and third-party notices. A raw EXE is
an additional asset only after the same notices are available from inside the
application. Attach SHA-256 checksums, a target/feature-specific CycloneDX
SBOM, build information, and release notes. Generate GitHub build-provenance
and SBOM attestations; these complement, but do not replace, Authenticode.

This research supports:

- [ADR 0010](../adr/0010-separate-release-toolchain-from-msrv.md)
- [ADR 0011](../adr/0011-glow-baseline-and-portable-msvc-runtime-gate.md)
- [ADR 0012](../adr/0012-unsigned-first-reproducible-release-pipeline.md)

## Current repository facts

At the research date:

- the workspace declares `rust-version = "1.92"`;
- `rust-toolchain.toml` uses the floating channel `stable`;
- eframe 0.35.0 has default features disabled and enables `glow`, not `wgpu`;
- no Windows resource build script, CRT-linkage configuration, dependency
  policy, license-notice generator, or release workflow exists yet; and
- the worktree is active, so this record deliberately does not change
  manifests, source files, or workflows.

## Decision matrix

`Unsafe` below describes the relevant trust boundary. Exact transitive unsafe
counts must be measured from the final `Cargo.lock` and release feature graph;
crate-name searches and manifest metadata are not substitutes for that audit.

| Candidate or approach | Verdict | License, maintenance, and activity | Security and unsafe boundary | Platform and build/binary effect | Value, lock-in, and fallback |
|---|---|---|---|---|---|
| Rust 1.97.1 release toolchain | **Adopt** | Rust project; current stable point release on 2026-07-16 | Compiler is a critical supply-chain input; 1.97.1 fixes an LLVM miscompilation | Tier-1 MSVC target; exact compiler/LLVM identity becomes stable | Essential for repeatability; update by reviewed PR and repeat the reproducibility gate |
| `Cargo.lock` plus `--locked` | **Adopt** | Cargo-native | Binds the reviewed dependency graph; does not prove crates are safe | No runtime cost | Minimal lock-in; regenerate intentionally when updating dependencies |
| `winresource` 0.1.31 | **Adopt** | MIT; released 2026-03-16; active | Build script invokes SDK resource tools; no shipped helper DLL; audit build-script source and lock it | Windows build dependency; tiny resource-only PE growth | Replaces fragile hand-authored resource compilation while retaining an `.rc`/manifest fallback |
| eframe 0.35.0 with Glow 0.17 / glutin 0.32.3 | **Adopt** | eframe: MIT OR Apache-2.0; Glow: MIT OR Apache-2.0 OR Zlib; glutin: Apache-2.0; active upstreams | GL context, FFI, and driver code contain the renderer's unsafe boundary; application code need not add unsafe | Windows native; no renderer DLL; upstream states Glow can materially reduce size versus WGPU | Already integrated and simple; fallback is a measured WGPU build if compatibility fails |
| Cargo default `release` profile | **Adopt** | Cargo-native | Keeps target-default unwinding and avoids unmeasured optimizer/security changes | `opt-level=3`, no incremental build, 16 codegen units, no full LTO by default | Sound baseline; change only after the complete performance/size matrix |
| `cargo-deny` 0.20.2 | **Adopt** | MIT OR Apache-2.0; released 2026-07-09; active Embark project | CI-only executable with a broad dependency graph and advisory/index network trust; no product runtime code | Cross-platform; install/cache cost only | One policy for advisories, licenses, bans, and sources; readable `deny.toml` limits lock-in |
| `cargo-audit` 0.22.2 | **Adopt** | MIT OR Apache-2.0; released 2026-06-05; active RustSec project | CI-only; consumes the RustSec advisory database; binary scan is accurate when auditable metadata exists | Cross-platform; install/cache cost only | Independent release/nightly check and `cargo audit bin`; if CI is slow, keep it off ordinary PRs |
| `cargo-cyclonedx` 0.5.9 | **Adopt** | Apache-2.0; released 2026-03-19; active CycloneDX project | CI-only Cargo invocation; untrusted build scripts must never receive secrets | Cross-platform; generates data, not runtime code; supports reproducible `SOURCE_DATE_EPOCH` output | Standard CycloneDX JSON avoids a local SBOM implementation; CLI is easy to replace |
| `cargo-about` 0.9.1 with `cli` | **Adopt** | MIT OR Apache-2.0; released 2026-06-30; active Embark project | CI-only license harvesting; generated output still needs human review | Cross-platform; large tool compile/cache cost, no product runtime cost | Safer than manually tracking transitive notices; template and output remain project-owned |
| `cargo-auditable` 0.7.5 | **Prototype** | MIT OR Apache-2.0; released 2026-06-28; Rust Secure Code project | Wraps Cargo/link output and embeds a sorted compressed dependency record; re-run reproducibility and PE checks | Windows is officially supported; upstream reports normally under 4 KiB | Enables accurate post-build auditing; fallback is ordinary Cargo plus separately attested SBOM |
| `static_vcruntime` 3.0.0 with `+crt-static` | **Prototype** | MIT OR Apache-2.0 OR Zlib; released 2025-11-25 | Build/link boundary; native dependencies may not tolerate the hybrid configuration | MSVC only; static startup/VCRuntime with dynamic Windows UCRT; size delta must be measured | Promising single-EXE portability without shipping VCRuntime; fallback is full static CRT or documented VCRedist dependency |
| Full `-C target-feature=+crt-static` without `static_vcruntime` | **Prototype fallback** | Rust/MSVC mechanism | Statically linked CRT code is serviced only by rebuilding; inspect all native dependencies | MSVC target; likely larger than hybrid; removes external CRT imports | Fewer moving parts than hybrid, but Microsoft recommends dynamic runtime linkage; choose only from measurements and clean-VM tests |
| `-C control-flow-guard=checks` | **Prototype** | rustc option | Instruments indirect control flow in compiled crates; the prebuilt standard library is not automatically rebuilt with matching instrumentation | Windows-only hardening with possible size/runtime overhead | Benchmark and inspect PE load configuration before adoption; default remains off |
| `cargo-geiger` 0.13.0 | **Prototype as non-gating evidence** | Apache-2.0 OR MIT; released 2025-08-31; maintained in the geiger-rs project | CI-only source statistics; upstream explicitly says unsafe counts are not a security verdict | Cross-platform; large Cargo-internal tool graph, no product runtime cost | Gives repeatable unsafe expression/function/implementation counts for the exact feature graph; retain reports, but review invariants rather than chasing zero |
| Microsoft BinSkim 4.4.9.7 | **Prototype**, then gate | MIT; released 2026-03-30; active and recently improved Rust recognition | External binary analyzer; run without environment capture because that can leak values | Cross-platform .NET tool; CI/download and SARIF cost only | Strong PE mitigation verification; baseline/suppress only reviewed Rust-specific false positives |
| eframe WGPU renderer using wgpu 29.0.4 | **Defer** | MIT OR Apache-2.0; active | Larger HAL, shader, backend, and GPU-driver unsafe surface | DX12 can improve compatibility on modern Windows but increases compile time and binary size; avoid DXC DLL artifacts | Prototype only if Glow fails a supported-machine test or a renderer benchmark; do not ship both initially |
| `embed-resource` 3.0.11 | **Defer** | MIT; released 2026-07-02; active | Lower-level build-script/resource compiler boundary | Windows resource build dependency | More flexible than needed; use if `winresource` cannot express a verified resource contract |
| Syft 1.50.0 | **Defer** | Apache-2.0; released 2026-07-28; active Anchore project | Large external scanner and action/download trust | Cross-platform Go binary; no runtime impact | Useful scheduled cross-check, including auditable metadata, but weaker Cargo feature intent than cargo-cyclonedx |
| `cargo-dist` 0.32.0 | **Defer** | MIT OR Apache-2.0; released 2026-05-22; active | Workflow generator owns a large part of the release trust boundary | Significant tool/workflow graph for one Windows artifact | Revisit for multi-platform installers; a short first-party workflow better fits custom signing and repro gates now |
| Authenticode provider | **Defer until credentials exist** | Provider-specific | Private key must remain in a compliant signing service/HSM or hardware token | Signing mutates the PE and can add service cost/latency | Keep a provider-neutral sign/verify stage; unsigned checksums and attestations remain valid fallback |
| Both Glow and WGPU in one release EXE | **Reject** | Licenses are compatible | Doubles renderer/driver attack and support surface | Largest compile/binary footprint and ambiguous failures | Ship one renderer selected by evidence |
| PFX/private key stored as a GitHub secret | **Reject** | Not applicable | Publicly trusted code-signing keys are subject to current CA/B Forum hardware-protection rules; a base64 secret is the wrong custody model | Hosted runner secret exposure risk | Use cloud HSM/signing service, hardware-backed controlled signing, or publish unsigned |
| Self-signed production executable | **Reject** | Not applicable | Does not establish public trust and can mislead users | Still produces Windows trust warnings | Publish honestly unsigned with SHA-256 and attestations until public-trust signing exists |
| Raw-EXE-only release without notices | **Reject** | Risks missing license notice obligations | Removes reviewable dependency evidence | Smallest download, but incomplete distribution | ZIP is canonical; raw EXE is additive only with embedded notices |
| CI run number/current clock embedded in PE or archive | **Reject** | Not applicable | Ambient build data weakens provenance and reproducibility | Guarantees byte drift | Use semantic version plus source commit; derive any required time from `SOURCE_DATE_EPOCH` |

No direct RustSec advisory was found by package-name lookup for `winresource`,
`static_vcruntime`, `eframe`, `glow`, `glutin`, or `wgpu` on the research date.
That is not a clean bill of health: transitive dependencies dominate these
graphs, advisories can be target/feature-specific, and only the final locked
graph plus actual binary can be release evidence.

## PE identity and manifest contract

### Version and icon resources

The version resource must set at least:

- `FileDescription`: the binary crate's `package.description`, embedded from
  `CARGO_PKG_DESCRIPTION` so the PE string and the crate metadata cannot drift
  (currently `A fast visual disk usage scanner`);
- `ProductName`: `DiskPie`;
- `InternalName`: `diskpie`;
- `OriginalFilename`: `diskpie.exe`;
- `FileVersion` and `ProductVersion`: the release semantic version; and
- a truthful copyright notice.

Do not invent a company name. Add `CompanyName` only when it names the actual
publisher and, once signing exists, agrees with the validated publisher
identity. Encode the fixed PE version as four unsigned 16-bit components,
`major.minor.patch.0`; keep prerelease/build text in the string version. Do not
use the CI run number as the fourth field.

Embed one independently created multi-image ICO as an `RT_GROUP_ICON` resource.
It should contain crisp small sizes and a 256-pixel image so Explorer can select
an appropriate representation rather than scale one bitmap. The same original
DiskPie artwork may be used for the runtime window icon, but no Scanner assets
or derivatives may enter the repository or binary.

`winresource` uses the Windows SDK `rc.exe` on the MSVC target and directs Cargo
to link the resulting resource. The build script must be target-gated so the
portable core and non-Windows checks do not require a Windows SDK. If resource
generation ever becomes nondeterministic or insufficient, retain the manifest
and `.rc` source as the stable contract and switch to a direct SDK invocation.

### Required manifest behavior

Use an embedded resource ID 1 manifest. The semantic content is:

```xml
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1"
          manifestVersion="1.0"
          xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity type="win32" name="DiskPie.DiskPie"
                    version="0.1.0.0" processorArchitecture="amd64" />
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}" />
    </application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2,PerMonitor</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
```

The Windows compatibility GUID above covers Windows 10 and Windows 11. Do not
add older compatibility GUIDs unless DiskPie expands its supported baseline.
Microsoft recommends declaring process DPI awareness in the manifest. The old
`dpiAware` element provides the per-monitor fallback, while Windows 10 1607+
recognizes `dpiAwareness` and Windows 10 1703+ recognizes Per-Monitor V2.

`longPathAware` is an opt-in for affected Win32 APIs, not a complete long-path
solution. Microsoft documents a second condition: the machine policy
`LongPathsEnabled`. DiskPie must continue to use Unicode APIs and extended
paths where required and must test with that policy both enabled and disabled.
The manifest must never request elevation merely to scan.

### Resource compiler dependency record (2026-09-03)

This record satisfies the `AGENTS.md` dependency rule for the build-script
crate that embeds the manifest, icon group, and `VERSIONINFO` above. Evidence
basis, stated exactly:

- `winresource` 0.1.31: crates.io metadata and the registry-vendored source
  (`lib.rs`, one file) were inspected on 2026-09-03. Zero `unsafe`
  occurrences; `rc.exe` is located through `RC_PATH` (`lib.rs:733`), then the
  toolkit path, then `reg query` of
  `HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots` (`lib.rs:797`);
  the manifest line is written as `<FILETYPE> 24 "<file>"` (`lib.rs:578-587`).
- `embed-resource` 3.0.11: crates.io metadata and the registry-vendored source
  were inspected on 2026-09-03 after fetching the crate into a scratch probe
  crate (`cargo fetch` with `embed-resource = "=3.0.11"` as the only
  build-dependency). The MSVC resolver is `src/windows_msvc.rs`: `RC_<target>`
  or `RC` overrides, then `winreg` lookups of `KitsRoot10`/`KitsRoot81`/
  `KitsRoot`, then `vswhom`. `embed-resource` itself has zero `unsafe`;
  `vswhom` 0.1.0 has three `unsafe` sites and `vswhom-sys` 0.1.3 compiles
  `ext/vswhom.cpp` with `cc` on every clean MSVC build; `winreg` 0.55.0 has
  `unsafe` in five source files. `cargo tree -e build,normal` on the probe
  resolves 22 packages on `x86_64-pc-windows-msvc`.
- Release dates and cadence come from the crates.io API on 2026-09-03.

Both candidates are MIT, and neither has a pure-Rust resource compiler: on the
MSVC target both invoke the Windows SDK `rc.exe`, which is preinstalled on the
GitHub `windows-2022` image and on any machine with the Visual Studio C++
workload. The requirement "no external tool" is therefore unsatisfiable for a
full resource set; the only pure-Rust option found, `embed-manifest`, is
documented as writing a manifest resource but no icon group or version block
(crates.io description only; its source was not audited because it cannot
deliver this contract alone).

| Criterion | `winresource` 0.1.31 | `embed-resource` 3.0.11 |
|---|---|---|
| License | MIT | MIT |
| Maintenance | Released 2026-03-16; six releases in the preceding year | Released 2026-07-02; five releases in the preceding year |
| Unsafe footprint | Zero `unsafe` in the crate; one runtime dependency (`version_check`, already in `Cargo.lock`) once the `toml` feature is disabled | Zero `unsafe` in the crate; MSVC-target dependencies `vswhom` (FFI `unsafe`, C++ compiled by `cc`) and `winreg` (registry FFI `unsafe`), plus `cc`, `rustc_version`, and `toml` on every target |
| External tools on MSVC | Windows SDK `rc.exe`, located through `RC_PATH`, then `reg query` of `HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots` (both inbox on `windows-2022`) | Windows SDK `rc.exe`, located through `RC_<target>`/`RC`, then `winreg` Kits-root lookups, then `vswhom`; cross-compilation uses `llvm-rc` with a C preprocessor step through `cc` |
| Platform support | Windows MSVC (`rc.exe`), Windows GNU (`windres`), and Linux/macOS cross builds; the crate compiles on every host so a plain `[build-dependencies]` entry keeps `cargo check` portable | Same platforms; also compiles on every host |
| Build and binary impact | One extra build-script crate; `Cargo.lock` grows by exactly one package; the PE grows only by the resource section | Seven packages new to `Cargo.lock` (`embed-resource`, `toml`, `toml_writer`, `serde_spanned`, `vswhom`, `vswhom-sys`, `winreg`), one C++ compile, and registry/COM toolchain discovery on each clean MSVC build; identical PE growth |
| Local-alternative cost | Hand-writing a `.res` emitter in `build.rs` (~300 lines of PE resource layout) is possible but duplicates a tested crate | Same |
| Lock-in and fallback | The crate only emits `resource.rc` plus `cargo:rustc-link-arg`; the manifest and property list stay project-owned, so a switch to `embed-resource` or a direct `rc.exe` call keeps every input | Lower-level API; equally replaceable |

Decision: adopt `winresource = "=0.1.31"` with `default-features = false`.
All version strings are set programmatically from `CARGO_PKG_*`, so the
optional `toml` feature (which reads `[package.metadata.winresource]`) is not
needed and its dependency tree stays out of the lock. `embed-resource` remains
the documented fallback if `winresource` is abandoned or if a resource type it
cannot express becomes necessary.

Implementation notes recorded from the first embedding:

- `crates/diskpie/build.rs` returns early unless `CARGO_CFG_TARGET_OS` is
  `windows`, so the Linux portable-core job never needs an SDK.
- `cargo:rerun-if-changed` covers `build.rs`, `diskpie.manifest`, and
  `assets/brand/diskpie-icon.ico`; `cargo:rerun-if-env-changed=RC_PATH` covers
  the resource-compiler override because `winresource` emits no rerun
  directive of its own.
- The manifest is embedded as resource ID 1 (`RT_MANIFEST`) verbatim from
  `crates/diskpie/diskpie.manifest`; `mt.exe -validate_manifest` on the
  extracted resource reports "Parsing of manifest successful" and the only
  differences from the source file are `mt.exe`'s canonical re-serialization
  of self-closing tags.
- `winresource` quirk: the `RT_MANIFEST` resource ID is the numeric value of
  `VersionInfo::FILETYPE` (`lib.rs:578-587`). The manifest lands on ID 1 only
  because `FILETYPE` defaults to `VFT_APP` (`0x1`). Never call
  `set_version_info(VersionInfo::FILETYPE, ...)` in `build.rs`; doing so moves
  the manifest to another ID and breaks the resource ID 1 contract that the
  release-inspection gate checks with `mt.exe -inputresource:diskpie.exe;#1`.
- `winresource` derives the fixed PE version as `major.minor.patch.0` from
  `CARGO_PKG_VERSION_*`, matching the contract above.
- The manifest `assemblyIdentity` version is a literal in the template file,
  not a build-time substitution. `build.rs` reads the manifest and fails the
  build unless it contains `version="<major>.<minor>.<patch>.0"` for the
  current `CARGO_PKG_VERSION`, so a workspace version bump must edit
  `crates/diskpie/diskpie.manifest` in the same commit; the guard turns a
  silent identity mismatch into a build error.
- `CompanyName` is populated from `CARGO_PKG_AUTHORS`, which currently names
  the individual publisher recorded in `LICENSE-MIT`; re-check it against the
  validated publisher identity before any signing stage is enabled.
- Reproducibility note for the ADR 0012 two-checkout gate: the generated
  `OUT_DIR/resource.rc` names the icon and manifest by absolute worktree path
  and `winresource` passes `/I<CARGO_MANIFEST_DIR>` to `rc.exe`. Only the icon
  and manifest bytes enter the `.res`, so the PE is expected to be path-free,
  but this is exactly the "build scripts may introduce absolute paths" case
  above and must be confirmed by the byte-compare of two differently rooted
  builds rather than assumed.

## Portable runtime and renderer gate

The portable contract is stronger than "there is one `.exe` in `target`": a
clean supported client with no developer tools and no separately installed VC
Redistributable must launch as a standard user. System DLLs and the Windows 10+
UCRT are allowed. Release inspection must reject unexpected imports such as
`VCRUNTIME*.dll`, `MSVCP*.dll`, `libgcc*.dll`, a bundled GPU backend, or DXC.

Compare three MSVC runtime variants from clean target directories:

1. Rust's default dynamic CRT linkage;
2. full `+crt-static`; and
3. `static_vcruntime` plus `+crt-static`, which statically links the startup and
   VCRuntime pieces while dynamically using the inbox UCRT.

For each variant record EXE bytes, compile/link time, imports, startup, scan and
render benchmarks, BinSkim output, and launch results on clean Windows 10/11.
The hybrid wins only if it removes the VCRedist prerequisite, keeps only inbox
imports, passes all native dependency tests, remains reproducible, and has no
material regression. A successful link alone is insufficient; the crate warns
that some C/C++ dependencies may not work in this configuration.

Keep Glow unless the supported-machine matrix shows a repeatable context or
driver failure, or WGPU has a measured material advantage after accounting for
binary size and build time. Test Glow on Intel, AMD, and NVIDIA hardware where
available; Microsoft Basic Display/VM graphics; Remote Desktop; and mixed-DPI
monitors. A WGPU prototype should enable only the Windows backend needed by the
release and must prove that it does not add distributable DXC or renderer DLLs.

## Release profile experiment, not folklore

Cargo documents these release defaults: optimization level 3, no debug info,
no stripping, no debug assertions or overflow checks, local ThinLTO behavior
rather than full LTO, unwind panic strategy, no incremental compilation, and
16 codegen units. Keep that baseline. Test alternatives in named profiles or
CI-only environment overrides so one result cannot silently redefine release.

At minimum compare:

- default release;
- ThinLTO and one codegen unit;
- `opt-level="s"` and `opt-level="z"`;
- Control Flow Guard checks; and
- `panic="abort"` only after confirming diagnostics, cleanup, and application
  recovery expectations.

Measure compile/link time, EXE bytes, startup, files/second, time to first
visible result, total scan time, memory per million entries, cancellation
latency, layout/render frame time, and the clean-machine launch matrix. Adopt a
profile change only when the improvement exceeds benchmark noise and no
correctness, diagnostics, compatibility, or mitigation check regresses. Do not
strip symbols blindly; if later diagnostics require PDBs, publish them as a
separate diagnostics artifact, not in the portable ZIP.

## Reproducibility contract

The first release claim is deliberately bounded: the unsigned EXE and unsigned
canonical ZIP must be bit-identical across two clean checkouts at different
absolute paths in one pinned GitHub-hosted Windows image. GitHub's
`windows-2022` label is not a hermetic image and changes over time, so record the
runner image version and every relevant tool version. Cross-date rebuilds are
"reproducible" only after they independently match.

Inputs to bind or normalize:

- exact source commit and annotated release tag;
- Rust 1.97.1, Cargo, LLVM host, target, and components;
- committed `Cargo.lock` and `--locked` on every build/tool invocation;
- GitHub runner image name/version, Windows build, Visual Studio installation,
  MSVC `link.exe`, SDK `rc.exe`/`mt.exe`/`signtool.exe`, and BinSkim versions;
- release feature graph from `cargo tree -e features`;
- `CARGO_INCREMENTAL=0` and independent target directories;
- `SOURCE_DATE_EPOCH` set to the source commit time;
- source/build path remapping to the same virtual prefix; and
- stable file order, timestamps, separators, permissions, and compression
  settings in the ZIP.

Build scripts and generated files may still introduce absolute paths or
timestamps. rustc path remapping is best effort and does not rewrite paths
introduced independently by the MSVC linker or external generators. Therefore
the gate is the final byte comparison, not the presence of flags. Inspect the
PE debug directory for a reproducible-build entry and treat a content-derived
COFF timestamp as non-calendar data. On mismatch, compare PE headers, sections,
resources, imports, embedded strings, and archive entries before release.

Authenticode and RFC 3161 timestamps intentionally change the final bytes.
Preserve the attested unsigned SHA-256, sign exactly one canonical unsigned
EXE, verify it, rebuild the ZIP around the signed EXE, then hash and attest the
final assets. Never compare a signed artifact to the unsigned reproducibility
candidate.

## CI and release pipeline

### Trust separation

1. **Pull request CI** has `contents: read` only. It runs formatting, clippy,
   tests, dependency policy, and an ordinary Windows build. It never receives
   signing, release, Azure, or environment secrets. Do not use
   `pull_request_target` to build contributor code.
2. **Protected tag build** checks out the exact tag commit, validates that the
   tag version and Cargo version agree, runs all gates, builds twice, records
   the environment, and uploads the canonical unsigned artifacts. This job is
   still unprivileged.
3. **Optional signing** uses a protected environment and receives only the
   already-built canonical EXE. It does not check out or build project source
   and validates the expected commit, artifact digest, PE identity, file name,
   and file count before invoking a provider. If no trusted provider is
   configured, the pipeline takes the documented unsigned branch.
4. **Package and attest** creates deterministic final assets, computes
   SHA-256, creates build-provenance and SBOM attestations, and uploads to a
   draft release. Give only this job `id-token: write`, `attestations: write`,
   `artifact-metadata: write`, and the minimum `contents` permission it needs.
5. **Publish** is protected by an environment approval after Windows client
   smoke tests. Enable immutable releases before the first stable publication;
   GitHub recommends filling a draft and then publishing it because assets and
   the tag become immutable.

Use job-specific permissions and concurrency keyed by the tag. Build jobs do
not need `id-token` or `contents: write`. A signing job does not need source.
An attestation proves which GitHub workflow identity made a claim about a
digest; it does not prove the workflow was benign, make an unsigned executable
trusted by Windows, or replace review of the builder.

### Pinned first-party actions

GitHub states that a full commit SHA is the only immutable way to reference an
action. These tag commits were verified through the repositories on the
research date and must be re-reviewed before an intentional update:

| Action | Release | Full commit SHA |
|---|---:|---|
| [`actions/checkout`](https://github.com/actions/checkout/commit/3d3c42e5aac5ba805825da76410c181273ba90b1) | v7.0.1 | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| [`actions/upload-artifact`](https://github.com/actions/upload-artifact/commit/043fb46d1a93c77aae656e7c1c64a875d1fc6a0a) | v7.0.1 | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` |
| [`actions/download-artifact`](https://github.com/actions/download-artifact/commit/3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c) | v8.0.1 | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` |
| [`actions/attest`](https://github.com/actions/attest/commit/508db95dd578ae2727ebd6217d5ba78e4fbda05d) | v4.2.1 | `508db95dd578ae2727ebd6217d5ba78e4fbda05d` |

Use the preinstalled `rustup` selected by `rust-toolchain.toml` instead of a
third-party Rust setup action. Use `gh release create --draft` and
`gh release upload` instead of a third-party release action.

### Signing without assuming a certificate

The release design must support these states explicitly:

- `unsigned`: publish with a prominent note, SHA-256, GitHub provenance/SBOM
  attestations, and expected SmartScreen behavior;
- `authenticode-cloud`: provider authenticates through short-lived OIDC and
  keeps the key in a compliant managed service; or
- `authenticode-hardware`: a controlled manual/self-hosted step uses a
  hardware-protected key and returns only the signed artifact.

Do not store a PFX or its password as ordinary repository secrets and do not
ship a self-signed production build. Current CA/B Forum requirements place
publicly trusted code-signing private keys behind hardware-protection controls.
Provider selection is a publisher/business decision, not a crate dependency.

Azure Artifact Signing is a possible future provider, but its current Public
Trust eligibility is limited to organizations in the USA, Canada, EU, and UK,
and individuals in the USA and Canada. If DiskPie's validated publisher is in
Mexico, Public Trust is not currently available through that service. Re-check
eligibility rather than designing the release around it. If it later applies,
the then-current actions must be SHA-pinned; the researched releases were
`Azure/login` v3.0.0 at
`532459ea530d8321f2fb9bb10d1e0bcf23869a43` and
`Azure/artifact-signing-action` v2.0.0 at
`c7ab2a863ab5f9a846ddb8265964877ef296ee82`.

Provider-neutral signing and verification is:

```powershell
signtool.exe sign /fd SHA256 /tr $TimestampUrl /td SHA256 <provider-options> $exe
signtool.exe verify /pa /all /tw /v $exe
Get-AuthenticodeSignature -LiteralPath $exe | Format-List *
```

`SignTool` requires an explicit file digest and timestamp digest in current
SDKs; use SHA-256 and an RFC 3161 timestamp. A zero exit status plus the verbose
verification log is release evidence.

## Dependency, SBOM, and license policy

Install release tools by exact version with locked tool dependencies, or use
upstream binaries only after verifying upstream checksums:

```powershell
cargo install --locked --version 0.20.2 cargo-deny
cargo install --locked --version 0.22.2 cargo-audit
cargo install --locked --version 0.5.9 cargo-cyclonedx
cargo install --locked --version 0.9.1 --features cli cargo-about
cargo install --locked --version 0.7.5 cargo-auditable
cargo install --locked --version 0.13.0 cargo-geiger
```

`cargo-deny` is the policy authority. Allow only reviewed licenses compatible
with DiskPie's distribution, allow crates.io as the normal external source,
deny unknown registries and unapproved Git sources, and start duplicate
versions as warnings with an explicit reduction plan. Every advisory ignore,
license exception, or source exception must include an owner, evidence,
rationale, and review/expiry date in an adjacent comment or tracking issue.
Unknown or unlicensed packages fail.

`cargo-audit` independently checks RustSec on protected releases and a schedule.
Do not run `cargo audit fix` in CI. Build with `cargo-auditable` only if its
prototype passes reproducibility; then `cargo audit bin` binds the advisory
result to the executable rather than merely to a nearby lockfile.

Generate the SBOM for the actual binary manifest, target, and release features,
not `--all-features` unless that is genuinely what ships:

```powershell
$env:SOURCE_DATE_EPOCH = git show -s --format=%ct HEAD
cargo cyclonedx --manifest-path .\crates\diskpie\Cargo.toml `
  --format json --describe binaries --spec-version 1.5 `
  --target x86_64-pc-windows-msvc --target-in-filename
cargo about generate -o .\artifacts\THIRD-PARTY-NOTICES.html .\about.hbs
```

When release features are introduced, pass exactly the same
`--no-default-features`/`--features` combination to both the build and SBOM
generation and retain `cargo tree -e features` as evidence. `cargo-about` must
target Windows x86-64 and include runtime dependencies; separately decide
whether build-tool notices belong in the distributed notice or build metadata.
Automated output is reviewed, not treated as legal advice.

The app's About/Legal view should make the generated third-party notices
available so a standalone raw EXE retains them. The ZIP also contains plain or
HTML notice text and both DiskPie license files.

## Artifact set and deterministic archive

Use versioned, lowercase, architecture-explicit names:

```text
diskpie-vX.Y.Z-windows-x86_64.zip
diskpie-vX.Y.Z-windows-x86_64.exe        # optional additive asset
diskpie-vX.Y.Z-windows-x86_64.cdx.json
diskpie-vX.Y.Z-windows-x86_64.build-info.json
diskpie-vX.Y.Z-windows-x86_64.pdb.zip    # optional diagnostics asset
SHA256SUMS
```

The canonical ZIP contains:

```text
diskpie.exe
README.txt
LICENSE-MIT
LICENSE-APACHE
THIRD-PARTY-NOTICES.html
```

Create entries in stable ordinal order with `/` separators, a fixed timestamp
derived from `SOURCE_DATE_EPOCH` but clamped to the ZIP format's supported
range, fixed compression settings, no archive comment, and normalized external
attributes. Do not rely on a convenience archive command until two clean
archives compare byte-for-byte. Hash final release bytes and write shasum-style
records (`HEX *filename`) so `actions/attest` can consume `subject-checksums`.

Create one provenance attestation for final checksummed assets and a second
SBOM attestation that supplies the CycloneDX JSON as `sbom-path`. The current
`actions/attest` action accepts SPDX or CycloneDX JSON and documents a 16 MiB
SBOM limit. GitHub artifact attestations provide SLSA Build Level 2 by
themselves. If immutable releases are enabled, GitHub additionally creates a
release attestation binding the tag, commit, and assets.

## Copy-ready validation commands

The exact SDK tool path comes from a Visual Studio developer shell. Store every
output under the release evidence directory.

```powershell
rustc -vV
cargo -vV
rustup show active-toolchain
cargo metadata --locked --format-version 1 > artifacts\cargo-metadata.json
cargo tree --locked --target x86_64-pc-windows-msvc -e features > artifacts\cargo-tree.txt
cargo geiger --locked --target x86_64-pc-windows-msvc `
  --manifest-path .\crates\diskpie\Cargo.toml > artifacts\cargo-geiger.txt

cargo deny check
cargo audit --deny warnings
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked

$env:CARGO_INCREMENTAL = '0'
$env:SOURCE_DATE_EPOCH = git show -s --format=%ct HEAD
cargo auditable build --locked --release --target x86_64-pc-windows-msvc --package diskpie
cargo audit bin .\target\x86_64-pc-windows-msvc\release\diskpie.exe
```

Run the resource/import/security inspection against the canonical unsigned EXE
and repeat signature/hash checks after optional signing:

```powershell
$exe = (Resolve-Path '.\target\x86_64-pc-windows-msvc\release\diskpie.exe').Path
(Get-Item -LiteralPath $exe).VersionInfo |
  Format-List FileVersion,ProductVersion,ProductName,FileDescription,OriginalFilename,LegalCopyright

mt.exe -nologo "-inputresource:$exe;#1" "-out:artifacts\manifest.extracted.xml"
mt.exe -nologo -manifest artifacts\manifest.extracted.xml -validate_manifest
dumpbin.exe /DEPENDENTS $exe | Tee-Object artifacts\dependents.txt
dumpbin.exe /HEADERS $exe | Tee-Object artifacts\headers.txt
dumpbin.exe /IMPORTS $exe | Tee-Object artifacts\imports.txt
llvm-readobj.exe --coff-imports --coff-resources --coff-debug-directory $exe |
  Set-Content artifacts\pe-readobj.txt

BinSkim.exe analyze $exe --output artifacts\binskim.sarif
Get-AuthenticodeSignature -LiteralPath $exe | Format-List *
Get-FileHash -Algorithm SHA256 -LiteralPath $exe
```

If `llvm-readobj` is retained, install and pin Rust's `llvm-tools` component or
record the exact external LLVM package; do not silently use whichever binary is
on `PATH`. Inspect `DllCharacteristics` for high-entropy VA, dynamic base, and
NX compatibility. Require CFG only after its prototype is adopted and the
binary contains real CFG metadata/checks.

Consumer verification examples:

```powershell
Get-FileHash -Algorithm SHA256 .\diskpie-vX.Y.Z-windows-x86_64.zip
gh attestation verify .\diskpie-vX.Y.Z-windows-x86_64.zip --repo OWNER/diskpie `
  --source-ref refs/tags/vX.Y.Z --signer-workflow OWNER/diskpie/.github/workflows/release.yml
gh release verify vX.Y.Z --repo OWNER/diskpie
gh release verify-asset vX.Y.Z .\diskpie-vX.Y.Z-windows-x86_64.zip --repo OWNER/diskpie
```

## Windows client validation matrix

Windows 10 version 22H2 is the final Windows 10 feature release and mainstream
Home/Pro/Enterprise support ended on 2025-10-14. DiskPie nevertheless keeps a
Windows 10 22H2 x64 compatibility test because the product requirement is
explicit. Use a fully patched ESU or isolated test image and disclose this OS
lifecycle fact; do not imply DiskPie can make an unsupported OS secure.

Required release candidates:

- Windows 10 22H2 x64 compatibility image;
- Windows 11 24H2 x64;
- Windows 11 25H2 x64; and
- Windows 11 26H1 on matching new hardware when available, as an opportunistic
  test rather than the broad-deployment baseline. Microsoft identifies 24H2
  and 25H2 as the recommended enterprise releases and 26H1 as a specialized,
  preinstalled hardware release.

For each required image test:

- standard user, no Visual Studio/Rust/VC Redistributable, no elevation prompt;
- launch from a writable folder and a read-only executable directory;
- clean profile and existing settings profile;
- logical and allocated scans, partial results, cancellation, and rescan;
- ASCII, accented, CJK, RTL, emoji, reserved-looking, long, and deeply nested
  paths, with long-path policy both enabled and disabled;
- access denied, reparse points, sparse/compressed files, removable media, and
  a slow or disconnected path as covered by the product test plan;
- 100%, 150%, 200%, and mixed-monitor DPI, light/dark/system theme, resize,
  keyboard, and accessibility tree;
- Glow on physical GPU, VM/basic display, and RDP paths available to the test
  lab;
- open/reveal/recycle/delete confirmation boundaries as a standard user;
- extracted manifest, version fields, icon, imports, mitigations, and binary
  audit; and
- signed or explicitly unsigned Windows properties, SHA-256, ZIP contents, and
  GitHub attestation verification.

The stable release is blocked by a required-image failure. A 26H1-only failure
is documented and triaged unless it also reproduces on 24H2/25H2 or violates a
promised hardware baseline.

## Primary sources

### Rust and Cargo

- [Rust 1.97.1 announcement](https://blog.rust-lang.org/2026/07/16/Rust-1.97.1/)
- [Cargo `rust-version`](https://doc.rust-lang.org/stable/cargo/reference/rust-version.html)
- [rustup toolchain files](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file)
- [Cargo profiles and release defaults](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [Rust linkage and `crt-static`](https://doc.rust-lang.org/stable/reference/linkage.html#static-and-dynamic-c-runtimes)
- [rustc path remapping](https://doc.rust-lang.org/rustc/remap-source-paths.html)
- [rustc Control Flow Guard option](https://doc.rust-lang.org/rustc/codegen-options/#control-flow-guard)

### Windows executable contract

- [Microsoft application manifests](https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests)
- [Microsoft process DPI-awareness guidance](https://learn.microsoft.com/en-us/windows/win32/hidpi/setting-the-default-dpi-awareness-for-a-process)
- [Microsoft maximum path length limitation](https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation)
- [Microsoft version information](https://learn.microsoft.com/en-us/windows/win32/menurc/version-information)
- [Microsoft icon resources](https://learn.microsoft.com/en-us/windows/win32/menurc/icons)
- [Microsoft PE format](https://learn.microsoft.com/en-us/windows/win32/debug/pe-format)
- [Microsoft deterministic PE debug entry](https://learn.microsoft.com/en-us/dotnet/fundamentals/runtime-libraries/system-reflection-portableexecutable-debugdirectoryentrytype)
- [Microsoft Universal CRT deployment](https://learn.microsoft.com/en-us/cpp/windows/universal-crt-deployment?view=msvc-170)
- [Microsoft SignTool](https://learn.microsoft.com/en-us/windows/win32/seccrypto/signtool)
- [Microsoft BinSkim guidance](https://learn.microsoft.com/en-us/windows-hardware/drivers/driversecurity/binskim-check-binaries)
- [Windows 10 22H2 lifecycle](https://learn.microsoft.com/en-us/lifecycle/announcements/windows-10-22h2-end-of-support-update)
- [Windows 11 release information](https://learn.microsoft.com/en-us/windows/release-health/windows11-release-information)

### Reused release tooling

- [`winresource` 0.1.31](https://docs.rs/winresource/0.1.31/winresource/)
- [`static_vcruntime` 3.0.0](https://docs.rs/static_vcruntime/3.0.0/static_vcruntime/)
- [eframe 0.35.0 features](https://docs.rs/eframe/0.35.0/eframe/#feature-flags)
- [wgpu 29.0.4](https://docs.rs/crate/wgpu/29.0.4)
- [`embed-resource` 3.0.11](https://docs.rs/crate/embed-resource/3.0.11)
- [`cargo-deny` 0.20.2](https://docs.rs/crate/cargo-deny/0.20.2)
- [`cargo-audit` 0.22.2](https://docs.rs/crate/cargo-audit/0.22.2)
- [`cargo-cyclonedx` 0.5.9](https://docs.rs/crate/cargo-cyclonedx/0.5.9)
- [`cargo-cyclonedx` reproducible-output change](https://docs.rs/crate/cargo-cyclonedx/0.5.9/source/CHANGELOG.md)
- [`cargo-about` 0.9.1](https://docs.rs/crate/cargo-about/0.9.1) and
  [its guide](https://embarkstudios.github.io/cargo-about/)
- [`cargo-auditable` 0.7.5](https://docs.rs/crate/cargo-auditable/0.7.5) and
  [its upstream design](https://github.com/rust-secure-code/cargo-auditable)
- [`cargo-geiger` 0.13.0](https://docs.rs/crate/cargo-geiger/0.13.0)
- [Microsoft BinSkim 4.4.9.7](https://github.com/microsoft/binskim/releases/tag/v4.4.9.7)
- [Syft 1.50.0](https://github.com/anchore/syft/releases/tag/v1.50.0)
- [`cargo-dist` 0.32.0](https://docs.rs/crate/cargo-dist/0.32.0)

### GitHub supply chain and signing

- [GitHub secure use: full-SHA action pinning](https://docs.github.com/en/actions/reference/security/secure-use)
- [GitHub artifact attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations)
- [`actions/attest` modes and inputs](https://github.com/actions/attest)
- [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
- [`gh attestation verify`](https://cli.github.com/manual/gh_attestation_verify)
- [`gh release verify`](https://cli.github.com/manual/gh_release_verify)
- [Azure Artifact Signing eligibility FAQ](https://learn.microsoft.com/en-us/azure/artifact-signing/faq)
- [Azure Artifact Signing trust models](https://learn.microsoft.com/en-us/azure/artifact-signing/concept-trust-models)
- [CA/B Forum Code Signing Baseline Requirements](https://cabforum.org/working-groups/code-signing/requirements/)

Version checks above came from the primary crates.io package metadata and
upstream GitHub release/tag APIs on 2026-08-02. Re-check versions, advisories,
eligibility, and action SHAs when implementing or publishing a later release.
