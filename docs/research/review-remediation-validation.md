# Independent review remediation: implementation and validation

Date: 2026-09-07. Scope: seven confirmed findings, the partial-startup quiescence
risk and requested performance improvements. Rust/egui, the five crates, the
approved map/list design, native paths, explicit action confirmation and
create-only diagnostic export remain the contract.

## Behavior changes

- Windows enumeration and child metadata share a retained directory handle;
  pathname replacement cannot redirect subsequent child opens. [ADR 0022](../adr/0022-retained-directory-enumeration.md)
  records the API change.
- Branch cancellation and terminal publication share a final synchronization
  point. A retained token survives coordinator join. Merge, model freeze and
  presentation indexing check cancellation. Accepted cancellation preserves the
  old frame; actual provider failures remain failures.
- Resolution retains the complete normalized native selection separately from
  successful roots. Composition preserves it through deferred launches and
  branch rescans. F5 retries failed roots until their state is observed.
  Promotion follows the displayed generation, including its first partial
  snapshot; an unshown new launch cannot change the retained frame's scope.
- Explorer attempts preserve operation and inspection results separately.
  Partial removal is reported and notifies Explorer; failed inspection clears
  stale verified state. Partial installations expose the domain's Install/Repair
  capabilities and their own localized status.
- Diagnostic export schema 2 describes the last settled scan. Generation,
  outcome and observed items are distinct from session totals. Unmeasured bytes,
  duration and total omissions are `unknown`, never fabricated zeros.
- Missing startup/exit receipts cannot mint runtime quiescence. The obsolete
  no-runtime placeholder constructor has been removed.

## Performance changes and method

Positive descendant depth is folded into existing logical/allocated aggregates.
On the checked x64 layout it uses padding: `AggregateSize` remains 32 bytes and
`NodeRecord` 288 bytes. Layout reads this depth and applies its ceiling without
a depth prepass. Hiding, grouping and sector budgets still cannot change scale.
Compact snapshot root indexes remove the arena walk from RescanAll availability
and use O(root count) space.

Stable chunk sorting and direct merges allow candidate sorting cancellation;
ownership-bearing payloads use index merges and permutation. Grouping, budget
enforcement and member copying also check cancellation. On x64 the direct
candidate merge buffer costs 32 bytes per candidate; it avoids the random
access overhead found in the first index-only implementation. Scratch remains
O(candidate count), and abandoning work still requires releasing allocations.
Checkpoint frequency alone does not guarantee cancellation-to-join latency.

Export retains bounded suffix bytes and header-margin line metadata instead of
an object per input line. Header lengths are computed without formatting the
whole header per candidate. Log collection seeks to the tail of the initially
observed file size and reads at most 1 MiB per source through its verified
handle. Open/Reveal skips provider queries used only for recycle eligibility.

Historical [Scanner renovado measurements](../../tools/diskpie-bench/DESIGN_REPORT.md)
remain unchanged: their reference includes deliveries 1–3 and the observed
balanced-million layout change was 0.662 → 32.075 ms. Review verified source and
binary identities before remediation. New results use isolated output
directories and frozen helpers identified by hashes. UI/layout builds are also
isolated; export helpers reuse the verified workspace release libraries.
Neither experiment replaces that reference or overwrites raw evidence.
CPU headless is not FPS; sampled process peaks do not prove memory savings.

New measurements are collected only after builds and tests have stopped.

### UI, layout and native scan comparison

The final v2 comparison uses seven alternating before/after rounds, each in a
fresh process. Before is the preserved post-redesign binary, remeasured on the
same day. The API/layout microbenchmark reports medians of seven per-process
medians: 40 warm availability calls and three instrumented layouts per process.
The balanced and native harness reports medians of seven individual processes.

| Case / measured work | Before | After |
| --- | ---: | ---: |
| RescanAll availability, 1M flat files | 5.8304 ms | 0.0023 ms |
| Instrumented flat layout, 1M files | 536.4771 ms | 512.5303 ms |
| Largest checkpoint interval, 1M flat layout | 472.0354 ms | 5.0070 ms |
| Balanced 1M, snapshot materialization | 1,806.887 ms | 1,658.636 ms |
| Balanced 1M, layout | 31.234 ms | 0.605 ms |
| Balanced 1M, median of snapshot + layout per run | 1,840.945 ms | 1,659.365 ms |
| Local NTFS 10k, combined matrix (order effect; see below) | 296.344 ms | 466.344 ms |
| Local NTFS 10k, scan + reduction with equal warmup | 285.009 ms | 287.016 ms |

The layout depth prepass and full-arena availability walk are removed. The
first cancellable index-sort attempt increased the measured million-file flat
layout by 50.1% and was discarded; v2 uses direct candidate merges. At 100k,
flat layout is 23.7639 → 24.5911 ms (+3.5%), while its checkpoint interval is
17.1953 → 0.5474 ms. Checkpoint instrumentation is included in flat layout
times. These intervals are not cancellation-to-join or whole-frame latency.

Variance is substantial: the baseline million-file flat process medians range
from 364.6 to 878.4 ms. In all seven NTFS pairs of the combined matrix, the
second execution was faster, regardless of variant. Its +57.4% median
difference is confounded by that order/cache effect and does not isolate the
cost of the native calls. The earlier v1 matrix observed +8.5% with the same
native change; both original matrices remain preserved.

A separate seven-pair NTFS control runs the complete baseline harness on the
same fixture immediately before every measured process: scan, materialization,
layout and cancelled scan. The identical warmup is
outside timing, and there is no process sampler in this control. Scan/reduction
medians are 285.009 → 287.016 ms (+0.7%); ranges are 254.492–311.037 ms before
and 266.305–409.908 ms after. These are observations for this small local
fixture, not a general equivalence or speedup claim. The additional attribute
checks enforce the directory boundary; broader provider profiling remains
necessary. No overall application speedup is claimed.

All 28 balanced/native results agree across variants on nodes, logical and
allocated bytes, sectors and `NodeRecord` size. The balanced fixture has
1,004,369 nodes and 529 sectors. The native fixture contains 10,000 files of
64 bytes and was read only. Forty-two process samples include the 14 micro
processes; their 10 ms sampled memory/handle peaks are not allocator profiles.

The [full comparison](review-remediation/ui/SUMMARY.md) records phase costs,
sampled peaks, source/binary identities and raw-data locations.

### Layout worker handover

A separate helper calls the actual `LayoutService` from the preserved before
and current release libraries. Seven alternating fresh-process pairs submit a
million-file layout, wait 200 ms, verify the service is still busy, then either
supersede it with a small layout or request stop by dropping the service. The
helper retains the snapshot and polls at 1 ms; compilation and snapshot setup
are outside timing.

| Service interval | Before median | After median |
| --- | ---: | ---: |
| Submit replacement → small replacement completion | 255.7001 ms | 3.3937 ms |
| Request stop through Drop → worker join | 264.2042 ms | 2.7465 ms |

After ranges are 3.0223–4.7648 ms and 2.4531–4.0477 ms respectively. These are
real service completion/join observations at the selected interruption time,
not worst-case bounds, branch-merge timing or full native interaction latency.

### Export comparison

Seven fresh processes per variant ran in alternating before/after order. The
actual Windows collector read eight self-created fixture files and verified
their bytes before timing only `build_export`.

| Input | Before median | After median | Change |
| --- | ---: | ---: | ---: |
| 65,000 synthetic structured events, 7,930,000 bytes | 278.898 ms | 202.734 ms | -27.31% |
| Newline-only stress, 1 MiB / 1,048,576 records | 1,186.860 ms | 30.639 ms | -97.42% |

Both variants included identical recent-log bytes from all eight sources. The
new artifact is 18 bytes smaller because schema 2 changes its metadata. The
newline case is pathological, not representative of normal typed logs. The
timings exclude collection, filesystem reads, UI and GPU; they do not measure
the seek optimization or prove memory savings. The final export/reader source
hashes still match the measured implementations after the later layout changes.

See the [export validation and provenance](review-remediation/export/EXPORT_VALIDATION.md)
and [28 raw measurements](review-remediation/export/export-seven-runs.csv).

## Integrated checks

Windows 11 Pro x64 build 26200, Rust/MSVC 1.97.1, on 2026-09-07:

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed without warnings |
| `cargo test --workspace --all-features --locked` | 588 unit/integration tests + 1 rustdoc passed; 8 explicit ignored entry points |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked` | Passed |
| `cargo build --release -p diskpie --locked` | Passed |
| Independent harness format / Clippy with `-D warnings` / locked tests | Passed; 2 tests |
| Four gated native Shell fixtures, single test thread | Passed without skips; only their own temporary files/directories/links were affected |
| `git diff --check` | Passed |

The six ignored subprocess helpers run through their parent tests. The other
two ignored entry points are the frame benchmark and native screenshot matrix;
the old screenshot matrix was not rerun and is not claimed as new evidence.
The [command outcomes](review-remediation/checks/v2-checks.jsonl),
[workspace test output](review-remediation/checks/v2-test.txt) and
[native fixture output](review-remediation/checks/final-native-fixtures.txt)
are preserved with the documentation. Original `v2-*` command logs are in
`target/review-fixes-20260907-164202/`; earlier successful checks and exploratory
failures remain in separate files. The native fixture run predates the final
sort/scope adjustment; the affected action code did not change afterward.

Release executable: 11,025,920 bytes, SHA-256
`0A950653F5993AC99D5047260BD0F06A672A84675B9A015EC57E3BDF80964E98`.
The [final verification](review-remediation/checks/final-source-release-verification.json)
confirms all 73 measured source inputs still match and records this binary.
The redesign record reported 10,930,688 bytes for its binary: this integrated
change adds 95,232 bytes (+0.9%). That is total binary change, not an attribution
to one API or optimization. No binary was published.

## Regression coverage

- Cancellation at merge, presentation and immediately before publication;
  provider failure and cancellation remain distinguishable.
- Full UI/resolver sequence with a failed root, another failed retry, then
  recovery: both native roots are requested each time.
- F5 follows the visible scope when a second launch is cancelled before or
  after its first snapshot is displayed.
- Retained native directory metadata and existing filesystem scenarios on
  self-created fixtures, plus parser and fallback behavior.
- Explorer removal failures, partial-install capabilities and UI invalidation
  after an unverified refresh.
- Export golden/reference equivalence, schema/unknown metrics, bounded tails,
  append/short-read behavior and source identity replacement.
- Depth by metric, indexed roots, cancellable freeze and stable sort, alongside
  existing layout/property, color, hit-testing and list coverage.
- Partial startup failure while a previous worker is alive cannot certify a
  clean shutdown.

## Acceptance limits

Fixtures do not complete Narrator, clean Windows 10/11 VM launches, physical
mixed-DPI transitions, OneDrive or the disposable-VM provider/action matrix.
Native enumeration still observes a changing filesystem, not an atomic
snapshot; ordinary child replacement within the retained directory can be
observed. The new attribute access request can also yield unknown metrics on
restrictive ACLs. See ADR 0022 for the provider contract and acceptance limits.
No real bin-empty call, user Explorer mutation, commit, tag or publication runs.
Proprietary reference artifacts remain untouched.
