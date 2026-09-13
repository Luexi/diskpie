"""Summarize the complete alternating A/B run using only Python's stdlib."""
import csv
import json
import statistics
from collections import defaultdict
from pathlib import Path

root = Path(__file__).resolve().parents[1]
with (root / "results/samples.csv").open(encoding="utf-8-sig", newline="") as stream:
    rows = list(csv.DictReader(stream))
groups = defaultdict(list)
for row in rows:
    groups[(row["scenario"], int(row["requested_entries"]), row["variant"])].append(row)

metrics = ["scan_reduce_ms", "snapshot_ms", "layout_ms", "first_batch_ms", "cancel_join_ms", "sampled_peak_working_bytes", "sampled_peak_private_bytes", "sampled_peak_handles", "sampled_cpu_ms"]
summary = []
for (scenario, entries, variant), samples in sorted(groups.items()):
    if len(samples) != 7 or {int(sample["run"]) for sample in samples} != set(range(1, 8)):
        raise SystemExit(f"Incomplete seven-run group: {scenario}/{entries}/{variant}")
    other = groups[(scenario, entries, "current" if variant == "baseline" else "baseline")]
    # Model totals are exact evidence, independent from nondeterministic timing.
    for field in ["nodes", "logical_bytes", "allocated_bytes", "sectors"]:
        values = {sample[field] for sample in samples + other}
        if len(values) != 1:
            raise SystemExit(f"Correctness changed for {scenario}/{entries}: {field}={values}")
    record = {"scenario": scenario, "entries": entries, "variant": variant, "runs": len(samples)}
    for metric in metrics:
        values = sorted(float(sample[metric]) for sample in samples)
        record[metric + "_median"] = statistics.median(values)
        record[metric + "_min"] = values[0]
        record[metric + "_max"] = values[-1]
    summary.append(record)

with (root / "results/summary.csv").open("w", newline="", encoding="utf-8") as stream:
    writer = csv.DictWriter(stream, fieldnames=list(summary[0]))
    writer.writeheader()
    writer.writerows(summary)

lookup = {(row["scenario"], row["entries"], row["variant"]): row for row in summary}
lines = ["# DiskPie performance measurements", "", "Seven alternating baseline/current samples per scenario; medians below. Raw ranges, CPU, private memory and handle samples are retained in `results/summary.csv` and all samples in `results/samples.csv`.", "", "Baseline: archived commit `f9a9a5fe8c6c5a108fae9edadda0f674a74a63b1`. Current source SHA-256 and hardware/toolchain are recorded in `results/environment.json`. Both binaries use identical harness source and dependency lock, optimized builds and a fresh process per sample. Filesystem cache is warm; these are local measurements, not guarantees for other machines.", "", "| Scenario | Entries | Scan+reduce ms A → B | Snapshot ms A → B | Layout ms A → B | Peak working MiB A → B |", "|---|---:|---:|---:|---:|---:|"]
for scenario, entries in sorted({(row["scenario"], row["entries"]) for row in summary}):
    before = lookup[(scenario, entries, "baseline")]
    after = lookup[(scenario, entries, "current")]
    def pair(metric, divisor=1):
        return f"{before[metric + '_median']/divisor:.2f} → {after[metric + '_median']/divisor:.2f}"
    lines.append(f"| {scenario} | {entries:,} | {pair('scan_reduce_ms')} | {pair('snapshot_ms')} | {pair('layout_ms')} | {pair('sampled_peak_working_bytes', 1024**2)} |")
lines += ["", "`flat` has all files in one directory. `wide` has N/10 sibling directories, nine files per directory and a 192-character shared parent prefix; N=1,000,000 therefore creates 100,000 pending-capable directories, not one million tasks. `balanced` has branching factor 16, three directory levels and N distributed files. `deep` has 128 directory levels with N files distributed along the chain. `omissions` denies one entry in 100. `slow` pauses one millisecond per 512 entries. Synthetic node counts include directories and root, and therefore differ from requested file counts in balanced/deep scenarios.", "", "Scan time includes event reduction but excludes snapshot/layout. First-result timing is the first data batch, not an egui frame. Cancellation starts after the first batch of a second scan and ends when its coordinator/workers have joined. CPU and memory/handle metrics are sampled through .NET every ten milliseconds and may miss a short peak; they include fixture generation inside the synthetic provider and the second cancellation scan. The separate headless UI report covers frame responsiveness.", "", "Every A/B group must retain identical node counts, logical/allocation totals and sector counts; report generation fails otherwise. The production scanner backend remains conventional. `batch` uses the isolated FileIdExtdDirectoryInfo prototype; it rejects reparse/cloud boundaries and is not production feature coverage. `results/differential-10000.txt` verifies names, kinds, sizes and file identities against the production provider for the real fixture. Additional automated fixtures cover native UTF-16 names, emoji, hard links and directories. Sparse/compressed, UNC, cloud, denied-access and all provider classes still need their own differential evidence before any production promotion.", "", "Interpretation: compare every scenario and retain regressions. The additional original-allocation field is a correctness cost needed for branch rescans; removing the temporary snapshot map and sharing pending path prefixes are the local optimizations. This measurement does not justify a persistent-tree rewrite or an NTFS/MFT backend by itself."]
(root / "REPORT.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
print(f"Validated {len(rows)} samples; wrote results/summary.csv and REPORT.md")
