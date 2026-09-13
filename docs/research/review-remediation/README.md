# Evidence retained with the remediation record

Date: 2026-09-07. The parent [validation report](../review-remediation-validation.md)
explains scope, results and limitations. This directory preserves small raw
outputs and provenance records; binaries, build trees and large fixtures stay
in their original `target/` directories. The historical redesign dataset is
unchanged.

- `checks/`: final workspace and independent-harness command results. Console
  logs are exact copies renamed from `.log` to `.txt` so the repository's
  application-log exclusion does not hide them. The native fixture output is
  the earlier applicable run identified in the parent report.
- [Final source/release verification](checks/final-source-release-verification.json)
  confirms all 73 measured inputs still match and identifies the executable.
- `export/`: 28 raw measurements, the original validation/provenance report,
  console output and the exact helper/script used. Its original evidence root
  is `target/actions-fixes-20260907-aux01/`; references to source snapshots,
  libraries, binaries and focused-test files in that copied report refer to
  that root, not to additional files shipped here.
- `ui/`: the v2 comparison, raw stdout/stderr CSVs, process samples, source and
  binary manifests, the microbenchmark source/lock and experiment scripts.
  The original root is `target/review-ui-perf-after-20260907-v2/`; compiled
  binaries and the full type-probe source tree remain there. The failed first
  sorting experiment remains under
  `target/review-ui-perf-after-20260907-implementation/`.
  The [comparison report](ui/SUMMARY.md), [worker results](ui/worker-probe/summary.json)
  and [equally warmed NTFS measurements](ui/native-warmed/measurements.csv)
  distinguish checkpoint intervals, actual worker handover and cache effects.

Scripts are preserved as experiment records. They assume their original
directory layouts and frozen binaries. Reproduction requires a fresh output
directory and checking the documented source and binary identities first;
running a script over this archive is neither necessary nor supported.

Neither sampled process peaks nor headless CPU timings establish GPU/display
FPS or causal memory savings. Local paths identify evidence on the measurement
machine; they are not portable download URLs.

`files-sha256.json` inventories the retained files by relative path and SHA-256,
excluding the inventory itself.
