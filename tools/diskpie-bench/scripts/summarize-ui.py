"""Validate all seven UI runs and report per-run frame distributions."""
import csv
import math
import statistics
from collections import defaultdict
from pathlib import Path

root = Path(__file__).resolve().parents[1]
results = root / "results/ui"
groups = defaultdict(list)
for path in results.glob("*.frames.csv"):
    with path.open(encoding="utf-8-sig", newline="") as stream:
        for row in csv.DictReader(stream):
            if int(row["nodes"]) != int(row["entries"]) + 1:
                raise SystemExit(f"Incomplete snapshot in {path}")
            if (row["viewport_width"], row["viewport_height"]) != ("1280", "800"):
                raise SystemExit(f"Unexpected viewport in {path}")
            key = row["variant"], int(row["entries"]), int(row["run"]), row["stage"]
            groups[key].append(float(row["frame_us"]))

expected_stages = {"snapshot_first": 1, "snapshot_transition": 200, "warm": 200,
                   "metric_first": 1, "metric_transition": 200, "metric_warm": 200}
summary = []
for variant in ("baseline", "current"):
    for entries in (100_000, 1_000_000):
        for run in range(1, 8):
            for stage, count in expected_stages.items():
                samples = sorted(groups[(variant, entries, run, stage)])
                if len(samples) != count:
                    raise SystemExit(f"Incomplete UI group: {variant}/{entries}/{run}/{stage}")
                summary.append(dict(variant=variant, entries=entries, run=run, stage=stage,
                    frames=count, median_us=statistics.median(samples),
                    p95_us=samples[math.ceil(len(samples) * .95) - 1],
                    p99_us=samples[math.ceil(len(samples) * .99) - 1], max_us=max(samples)))
with (results / "summary.csv").open("w", encoding="utf-8", newline="") as stream:
    writer = csv.DictWriter(stream, fieldnames=list(summary[0]))
    writer.writeheader()
    writer.writerows(summary)

def metric(variant, entries, stage, field):
    return statistics.median(row[field] for row in summary
        if (row["variant"], row["entries"], row["stage"]) == (variant, entries, stage)) / 1000

with (results / "processes.csv").open(encoding="utf-8-sig", newline="") as stream:
    processes = list(csv.DictReader(stream))
for variant in ("baseline", "current"):
    for entries in (100_000, 1_000_000):
        runs = [int(row["run"]) for row in processes if row["variant"] == variant and int(row["entries"]) == entries]
        if sorted(runs) != list(range(1, 8)):
            raise SystemExit(f"Incomplete process group: {variant}/{entries}")

lines = ["# Headless UI measurements", "", "Seven alternating runs per revision and size, fresh process each. The same included Rust test body drives the actual shell with a final flat synthetic snapshot. A 1280×800 logical-pixel viewport and bundled fonts are used. Each run records two first frames and 200 frames in each transition/warm stage. Values below are medians of the seven per-run statistics, in milliseconds; raw distributions remain in `results/ui/summary.csv`.", "", "| Files | Stage | Statistic | Original ms | Current ms |", "|---|---|---|---:|---:|"]
for entries in (100_000, 1_000_000):
    for stage, field in (("snapshot_first", "median_us"), ("snapshot_transition", "max_us"), ("warm", "p95_us"), ("metric_first", "median_us"), ("metric_transition", "max_us"), ("metric_warm", "p95_us")):
        lines.append(f"| {entries:,} | {stage} | {field.removesuffix('_us')} | {metric('baseline',entries,stage,field):.3f} | {metric('current',entries,stage,field):.3f} |")
lines += ["", "| Files | Revision | Whole-process CPU ms | Peak working MiB | Peak private MiB | Peak handles |", "|---|---|---:|---:|---:|---:|"]
for entries in (100_000, 1_000_000):
    for variant in ("baseline", "current"):
        rows = [row for row in processes if row["variant"] == variant and int(row["entries"]) == entries]
        def med(field, divisor=1):
            return statistics.median(float(row[field]) for row in rows) / divisor
        lines.append(f"| {entries:,} | {variant} | {med('sampled_cpu_ms'):.2f} | {med('sampled_peak_working_bytes',1024**2):.2f} | {med('sampled_peak_private_bytes',1024**2):.2f} | {med('sampled_peak_handles'):.0f} |")
lines += ["", "Empty UI gets 20 warm-up frames before scanning. The scan is allowed to publish its final snapshot without refreshing the list, then `snapshot_first` includes the first ranking/submission cost. Metric dispatch occurs inside `metric_first`. Transition frames retain late asynchronous layout/list completion spikes and sleep 10 ms between frames, outside the measured interval. Warm frames run without sleeps. Baseline exposes 200 ranked rows; current indexes all N rows and virtualizes visible rows, so the current version delivers more functionality for the same flat input.", "", "Frame timings are wall-clock CPU-side `egui::Context::run_ui` durations including shell logic, cache refresh, layout of widgets and shape emission. They exclude OS window integration, GPU tessellation/rasterization, compositing, display and vsync; this is not an end-to-end FPS claim. Whole-process CPU/memory/handles include initialization, scanning, background workers, transitions and shutdown and are sampled by .NET every 10 ms. These local measurements do not replace interactive verification or hardware-specific graphics profiling."]
(root / "UI_REPORT.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
print(f"Validated {sum(map(len, groups.values()))} frames across 28 processes")
