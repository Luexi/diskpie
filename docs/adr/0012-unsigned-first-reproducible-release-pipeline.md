# ADR 0012: Use an unsigned-first reproducible release pipeline

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must publish a portable Windows EXE, SHA-256 checksums, release notes,
and a usable public release. The project does not currently have a code-signing
certificate or signing service, and it must not assume one. GitHub-hosted
Windows images and SDK tools change over time, Authenticode changes PE bytes,
and an RFC 3161 timestamp is intentionally time-dependent.

A release pipeline that compiles while holding signing credentials allows
project code and dependency build scripts into the highest-trust boundary. A
pipeline that signs before comparing builds can no longer tell compiler
nondeterminism from expected signature differences. Checksums alone detect
changes but do not establish builder identity; GitHub attestations establish a
workflow identity but are not Windows publisher signatures.

DiskPie also distributes dual-license text and dependency notices. A bare EXE
without an embedded legal view cannot be the only artifact. GitHub immutable
releases lock the release tag and assets after publication, so all evidence
must be complete before the draft is published.

Supporting research, exact tool/action versions, and primary sources are
recorded in
[Windows packaging and release engineering](../research/windows-packaging-and-release.md).

## Decision drivers

- Release even when no trustworthy Authenticode credential is available.
- Isolate signing credentials from source compilation and dependency scripts.
- Prove byte identity before intentional signing mutations.
- Bind final downloads to source/workflow identity and a dependency SBOM.
- Carry project licenses and third-party notices with the portable binary.
- Use least-privilege, SHA-pinned GitHub Actions.
- Permit a future cloud-HSM or hardware-backed signer without redesigning the
  build.
- Make failure, unsigned status, and Windows lifecycle limitations explicit.

## Options considered

### Compile and sign in one privileged job

This is operationally short, but grants untrusted build scripts access to the
signing boundary and obscures the unsigned reproducibility result. It creates
the greatest impact if a dependency or workflow step is compromised.

### Publish only an unsigned EXE and checksum

This satisfies basic portability and integrity checking but does not provide a
builder identity, dependency evidence, or complete license bundle. It also
leaves no clean insertion point for later signing.

### Unsigned-first build, isolated optional signing, then package and attest

The unprivileged job proves and records canonical unsigned bytes. A protected
stage signs only that artifact if a trustworthy provider exists. The final
stage packages licenses/notices, hashes final bytes, and attests provenance and
the SBOM. The unsigned path remains a first-class, honest result.

### Adopt `cargo-dist` as the release owner

`cargo-dist` is maintained and license-compatible, but its generated workflow
owns more of the release boundary than needed for one Windows bundle and makes
the custom double-build, PE, optional signing, and client-approval gates less
direct. It remains a future multi-platform option.

## Decision

DiskPie will use an **unsigned-first** release pipeline with five trust stages:

1. Pull-request CI runs format, clippy, tests, dependency/license policy, and
   an ordinary Windows build with read-only repository permission and no
   release or signing credentials.
2. A protected version tag triggers an unprivileged release build. It checks
   version/tag agreement, uses the exact tag commit, pinned Rust and lockfile,
   builds from two clean differently rooted checkouts, and requires identical
   unsigned EXE and canonical unsigned ZIP SHA-256 values.
3. An optional protected signing stage downloads only the canonical unsigned
   EXE, verifies its expected digest, name, PE resources, commit/run identity,
   and file count, then signs through a provider-neutral interface. It neither
   checks out source nor executes Cargo or project build scripts.
4. A package/attestation stage constructs the final deterministic ZIP around
   the signed or explicitly unsigned EXE, creates final SHA-256 records,
   attaches a target/feature-specific CycloneDX SBOM, and generates GitHub
   build-provenance and SBOM attestations.
5. A protected environment approval publishes the fully populated draft only
   after required Windows client smoke tests. Repository release immutability
   is enabled before the first stable publication.

The reproducibility claim applies first to unsigned artifacts built with the
same declared environment. The workflow records the source commit and tag,
Rust/Cargo/LLVM, target and feature graph, runner image, Windows build, MSVC
linker, Windows SDK resource/signing tools, `SOURCE_DATE_EPOCH`, and archive
method. `CARGO_INCREMENTAL=0`, independent target directories, source path
remapping, and normalized archive metadata are required. Final byte equality
is the gate; flags alone are not proof.

The canonical release asset is
`diskpie-vX.Y.Z-windows-x86_64.zip`, containing `diskpie.exe`, run
instructions, `LICENSE-MIT`, `LICENSE-APACHE`, and generated third-party
notices. The raw EXE may be attached additionally only when the app embeds an
accessible third-party legal view. A target/feature-specific CycloneDX JSON,
build-info JSON, `SHA256SUMS`, and optional separate symbols archive accompany
it.

Signing is optional and has exactly three recognized states:

- clearly disclosed unsigned release;
- public-trust cloud/HSM signing through short-lived authentication; or
- controlled hardware-backed signing.

A PFX/private key in repository secrets and a self-signed production
certificate are prohibited. If signing occurs, use SHA-256 for the file digest
and RFC 3161 timestamp digest, run `signtool verify /pa /all /tw /v`, retain
the unsigned digest, and recompute all final digests. GitHub attestations do not
replace Authenticode, and Authenticode does not replace build provenance.

Every external GitHub Action is pinned to a reviewed full commit SHA. Use
first-party checkout/artifact/attestation actions and the GitHub CLI for draft
release operations. Permissions are job-scoped: build jobs have no OIDC or
write token; attestation receives only `id-token: write`,
`attestations: write`, `artifact-metadata: write`, and required repository
access; publication alone receives `contents: write`.

## Consequences

Positive outcomes:

- DiskPie can publish a verifiable release before the publisher obtains a
  trusted code-signing identity.
- Signing compromise cannot directly execute the project build graph in the
  signing job.
- Compiler/resource/archive nondeterminism is detected before signing.
- Consumers receive hashes, workflow provenance, SBOM evidence, licenses, and
  notices with one canonical download.
- A future signer can be inserted without changing the compiler/build stages.
- Immutable release assets and tags prevent silent replacement after publish.

Accepted tradeoffs:

- Unsigned builds can trigger SmartScreen or publisher warnings; release notes
  must say so plainly.
- Two clean builds and client validation increase release time and runner cost.
- GitHub-hosted `windows-2022` is recorded but not hermetic; matching one run
  does not guarantee a future hosted image will reproduce it.
- Authenticode timestamps make signed bytes intentionally different; both
  unsigned and signed identities must be retained.
- Draft publication needs a human approval after client tests.
- Tool/action version maintenance remains a maintainer responsibility.

Fallback and reconsideration:

- If two builds differ, keep the release blocked, compare PE sections,
  resources, imports, embedded paths, and archive entries, then fix or document
  the nondeterministic input. Never silently weaken the equality gate.
- If no signing provider is available or eligible, publish the explicitly
  unsigned branch with checksum and attestations.
- If a signing service becomes available, add it only to the isolated stage and
  re-check provider eligibility, action SHA, private-key custody, and cost.
- If GitHub immutable releases or attestations are unavailable, preserve local
  Sigstore/checksum evidence and delay any claim that depends on those GitHub
  controls.
- Reconsider `cargo-dist` when multiple operating systems or installer formats
  make the maintained generator materially simpler than the custom pipeline.

## Validation

The release is blocked unless evidence proves:

- both clean checkouts resolve the same lockfile/feature graph and produce the
  same unsigned EXE and unsigned canonical ZIP SHA-256;
- the PE has the expected icon, semantic version fields, `asInvoker`, Windows
  10/11 compatibility, Per-Monitor V2, and long-path manifest;
- import and BinSkim reports meet the portable/security baseline;
- `cargo deny`, `cargo audit`, `cargo audit bin` when enabled, license notice,
  and CycloneDX generation pass;
- the final signed or unsigned status is intentional and matches release notes;
- SHA-256 records verify every final asset;
- `gh attestation verify` validates the final canonical ZIP against the
  expected repository, tag, commit, and signer workflow;
- the draft contains the complete asset set before publication; and
- standard-user smoke tests pass on Windows 10 22H2, Windows 11 24H2, and
  Windows 11 25H2 clean-client images.

Primary evidence:

- [GitHub secure use and full-SHA pinning](https://docs.github.com/en/actions/reference/security/secure-use)
- [GitHub artifact attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations)
- [`actions/attest`](https://github.com/actions/attest)
- [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
- [Microsoft SignTool](https://learn.microsoft.com/en-us/windows/win32/seccrypto/signtool)
- [CA/B Forum Code Signing Baseline Requirements](https://cabforum.org/working-groups/code-signing/requirements/)
- [Microsoft deterministic PE debug entry](https://learn.microsoft.com/en-us/dotnet/fundamentals/runtime-libraries/system-reflection-portableexecutable-debugdirectoryentrytype)
