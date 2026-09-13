"""Validate and summarize the seven interleaved redesign measurements."""
import csv
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path

root = Path(__file__).resolve().parents[1]
results = root / "results/design"
environment = json.loads((results / "environment.json").read_text(encoding="utf-8-sig"))
runs = environment["Runs"]
if runs != 7:
    raise SystemExit("Seven runs are required for the redesign report")
groups = defaultdict(list)
for path in results.glob("*.frames.csv"):
    with path.open(encoding="utf-8-sig", newline="") as stream:
        for row in csv.DictReader(stream):
            if int(row["nodes"]) != int(row["entries"]) + 1:
                raise SystemExit(f"Incomplete snapshot: {path}")
            if int(row["ranked_rows"]) not in (0, int(row["entries"])):
                raise SystemExit(f"Unexpected ranked count: {path}")
            groups[(row["variant"], int(row["entries"]), int(row["run"]), row["stage"])].append(float(row["frame_us"]))

stages = {"snapshot_first": 1, "snapshot_transition": 200, "warm": 200,
          "metric_first": 1, "metric_transition": 200, "metric_warm": 200}
summary = []
for variant in ("before", "after-map", "after-list"):
    for entries in (100_000, 1_000_000):
        for run in range(1, runs + 1):
            for stage, expected in stages.items():
                samples = sorted(groups[(variant, entries, run, stage)])
                if len(samples) != expected:
                    raise SystemExit(f"Incomplete frames: {variant}/{entries}/{run}/{stage}")
                summary.append(dict(variant=variant, entries=entries, run=run, stage=stage,
                    frames=len(samples), median_us=statistics.median(samples),
                    p95_us=samples[math.ceil(len(samples) * .95) - 1], max_us=max(samples)))
with (results / "summary.csv").open("w", encoding="utf-8", newline="") as stream:
    writer = csv.DictWriter(stream, fieldnames=list(summary[0]))
    writer.writeheader()
    writer.writerows(summary)
with (results / "layouts.csv").open(encoding="utf-8-sig", newline="") as stream:
    layouts = list(csv.DictReader(stream))
with (results / "processes.csv").open(encoding="utf-8-sig", newline="") as stream:
    processes = list(csv.DictReader(stream))
expected_processes = {
    f"{variant}-{entries}-{run}"
    for variant in ("before", "after-map", "after-list")
    for entries in (100_000, 1_000_000)
    for run in range(1, runs + 1)
} | {
    f"layout-{variant}-{scenario}-{entries}-{run}"
    for variant in ("before", "after")
    for scenario in ("flat", "balanced", "deep")
    for entries in (100_000, 1_000_000)
    for run in range(1, runs + 1)
}
if len(processes) != len(expected_processes) or {r["sample"] for r in processes} != expected_processes:
    raise SystemExit("Missing or duplicate process samples")
for scenario in ("flat", "balanced", "deep"):
    for entries in (100_000, 1_000_000):
        for variant in ("before", "after"):
            rows = [r for r in layouts if (r["scenario"], int(r["requested_entries"]), r["variant"]) == (scenario, entries, variant)]
            if sorted(int(r["run"]) for r in rows) != list(range(1, runs + 1)):
                raise SystemExit(f"Incomplete layouts: {scenario}/{entries}/{variant}")
            for row in rows:
                before = next(r for r in layouts if r["variant"] == "before" and
                    (r["scenario"], r["requested_entries"], r["run"]) == (scenario, str(entries), row["run"]))
                for key in ("nodes", "logical_bytes", "allocated_bytes", "sectors"):
                    if row[key] != before[key]:
                        raise SystemExit(f"Differential count failure {key}: {row}")

def frame_value(variant, entries, stage, field):
    return statistics.median(r[field] for r in summary if (r["variant"], r["entries"], r["stage"]) == (variant, entries, stage)) / 1000

def layout_value(variant, scenario, entries):
    return statistics.median(float(r["layout_ms"]) for r in layouts if
        (r["scenario"], int(r["requested_entries"]), r["variant"]) == (scenario, entries, variant))

def process_value(variant, entries, field):
    ids = {f"{variant}-{entries}-{run}" for run in range(1, runs + 1)}
    return statistics.median(float(r[field]) for r in processes if r["sample"] in ids)

lines = ["# Scanner renovado: rendimiento antes/después", "",
    "Referencia: copia del árbol de trabajo inmediatamente anterior al rediseño, incluyendo las entregas 1–3. Siete ejecuciones intercaladas por variante, con un proceso nuevo por muestra. Los hashes, configuración, hardware y toolchain están en `results/design/environment.json`.", "",
    f"Equipo: {environment['CPU']}, {environment['LogicalProcessors']} procesadores lógicos, {environment['MemoryBytes'] / 2**30:.2f} GiB de RAM. {environment['Toolchain'].splitlines()[0]}; Windows/MSVC, perfil release y dependencias bloqueadas. Se validaron {sum(map(len, groups.values())):,} frames, {len(layouts)} layouts y {len(processes)} procesos.", "",
    "Las mediciones de interfaz usan el mismo cuerpo de prueba y una ventana egui headless de 1280×800 puntos con fuentes embebidas. Un ajuste exclusivo del helper de pruebas activa la lista opcional. `before` muestra el inspector anterior; `after-map` es la vista inicial nueva; `after-list` abre la lista nueva. Ambas versiones indexan todos los elementos. Las diferencias entre variantes también incluyen diferencias deliberadas en la cantidad de contenido visible.", "",
    "## CPU por frame", "",
    "Milisegundos; mediana de siete estadísticas por ejecución. Los máximos de transición conservan el coste de las publicaciones asíncronas.", "",
    "| Archivos | Etapa | Estadística | Antes | Después: mapa | Después: lista |", "|---|---|---|---:|---:|---:|"]
for entries in (100_000, 1_000_000):
    for stage, field in (("snapshot_first", "median_us"), ("snapshot_transition", "max_us"), ("warm", "p95_us"), ("metric_first", "median_us"), ("metric_transition", "max_us"), ("metric_warm", "p95_us")):
        values = " | ".join(f"{frame_value(v, entries, stage, field):.3f}" for v in ("before", "after-map", "after-list"))
        lines.append(f"| {entries:,} | {stage} | {field.removesuffix('_us')} | {values} |")
lines += ["", "## Layout", "", "Milisegundos; mediana de siete ejecuciones. El layout nuevo calcula la profundidad efectiva antes del agrupamiento. Se verifican los mismos nodos, bytes y sectores en ambas versiones.", "", "| Árbol | Entradas solicitadas | Antes | Después | Cambio |", "|---|---:|---:|---:|---:|"]
for scenario in ("flat", "balanced", "deep"):
    for entries in (100_000, 1_000_000):
        values = [layout_value(v, scenario, entries) for v in ("before", "after")]
        percent = (values[1] / values[0] - 1) * 100
        lines.append(f"| {scenario} | {entries:,} | {values[0]:.3f} | {values[1]:.3f} | {percent:+.1f}% |")
lines += ["", "## Proceso completo: memoria y handles", "",
    "Mediana de siete ejecuciones. Working set, bytes privados y handles son máximos muestreados cada 10 ms; la CPU es el último acumulado observado. Incluyen inicialización, escaneo, servicios y cierre, además de los frames. No representan memoria incremental de un componente ni un perfil de asignaciones.", "",
    "| Archivos | Variante | Pico working set (MiB) | Pico privado (MiB) | Pico handles | CPU observada (s) |",
    "|---|---|---:|---:|---:|---:|"]
for entries in (100_000, 1_000_000):
    for variant in ("before", "after-map", "after-list"):
        working = process_value(variant, entries, "sampled_peak_working_bytes") / 2**20
        private = process_value(variant, entries, "sampled_peak_private_bytes") / 2**20
        handles = process_value(variant, entries, "sampled_peak_handles")
        cpu = process_value(variant, entries, "sampled_cpu_ms") / 1000
        lines.append(f"| {entries:,} | {variant} | {working:.1f} | {private:.1f} | {handles:.0f} | {cpu:.2f} |")
balanced_before = layout_value("before", "balanced", 1_000_000)
balanced_after = layout_value("after", "balanced", 1_000_000)
warm_delta = frame_value("after-list", 100_000, "warm", "p95_us") - frame_value("before", 100_000, "warm", "p95_us")
metric_delta = frame_value("after-list", 1_000_000, "metric_warm", "p95_us") - frame_value("before", 1_000_000, "metric_warm", "p95_us")
lines += ["", "## Interpretación y regresiones", "",
    "Los resultados no muestran una mejora uniforme de CPU por frame. La vista de mapa mantiene un coste parecido en el caso caliente de un millón de archivos. Algunas primeras publicaciones mejoran y otras transiciones o estados calientes empeoran; siete muestras locales no permiten atribuir significado estadístico a diferencias pequeñas.", "",
    f"La regresión más clara de layout aparece en el árbol equilibrado de un millón de entradas: {balanced_before:.3f} → {balanced_after:.3f} ms, es decir, +{balanced_after - balanced_before:.3f} ms. El código nuevo recorre descendientes con peso positivo para calcular la profundidad antes de agrupar, ocultar o aplicar el presupuesto; el anterior podía agrupar pronto y evitar ese recorrido. Ese mecanismo explica trabajo adicional, aunque estas mediciones no son un perfil causal por función. El cálculo ocurre en el worker de layout y conserva cancelación cooperativa; no se presenta como una mejora de rendimiento.", "",
    f"Con la lista abierta, el p95 caliente de 100 mil archivos aumenta {warm_delta:.3f} ms; después del cambio de métrica, el de un millón aumenta {metric_delta:.3f} ms. La lista nueva muestra más filas y controles que el inspector anterior, por lo que las variantes no dibujan idéntico contenido.", "",
    "La revisión estática localizó un recorrido heredado de todos los nodos al consultar la disponibilidad de reescaneo completo (`NavigationState::full_rescan_unavailable_reason` → `filesystem_roots`). Está presente en ambas versiones; el rediseño no añade otro recorrido de todo el árbol por frame. Es un candidato a optimización posterior, pero haría falta perfilar para cuantificar su contribución al tiempo medido.", "",
    "El índice de familia añade ocho bytes por nodo (unos 8 MB para un millón). Los picos de proceso menores en algunas variantes no prueban un ahorro de memoria: dependen de la temporización y de la vida de snapshots y servicios, y el muestreo puede omitir picos entre observaciones. Los handles muestreados son iguales en estas seis combinaciones."]
lines += ["", "## Alcance de la evidencia", "",
    "La duración por frame incluye lógica, widgets y emisión de shapes por egui; excluye ventana nativa, rasterización GPU, compositor y vsync. No es una medición de FPS. Cada ejecución tiene 20 frames de calentamiento vacíos, dos primeros frames y 200 frames por etapa de transición/caliente. Las pausas de 10 ms entre frames de transición quedan fuera de la duración medida.", "",
    "`processes.csv` conserva CPU de proceso, memoria y handles muestreados cada 10 ms; incluyen escaneo, servicios, inicialización y cierre. Los CSV de layout separan escaneo/reducción, publicación, layout y cancelación. Son resultados locales; la validación visual nativa se documenta por separado. No se deduce una mejora general del escáner de cambios en estos frames."]
(root / "DESIGN_REPORT.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
print(f"Validated {sum(map(len, groups.values()))} frames, {len(layouts)} layouts and {len(processes)} processes")
