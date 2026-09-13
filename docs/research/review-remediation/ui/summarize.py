import csv
import json
import pathlib
import statistics

base = pathlib.Path(__file__).resolve().parent
out = base / 'results'
layouts = list(csv.DictReader((out / 'layouts.csv').open(encoding='utf-8-sig')))
processes = list(csv.DictReader((out / 'processes.csv').open(encoding='utf-8-sig')))
assert len(layouts) == 28 and len(processes) == 42
assert len({row['sample'] for row in processes}) == 42
for scenario in ['balanced', 'real']:
    for run in range(1, 8):
        pair = [row for row in layouts if row['scenario'] == scenario and row['run'] == str(run)]
        assert len(pair) == 2
        assert {row['variant'] for row in pair} == {'before', 'after'}
        for key in ['nodes', 'logical_bytes', 'allocated_bytes', 'sectors', 'node_record_bytes']:
            assert pair[0][key] == pair[1][key], (scenario, run, key, pair)

summary = {}
for scenario in ['balanced', 'real']:
    for variant in ['before', 'after']:
        rows = [row for row in layouts if (row['scenario'], row['variant']) == (scenario, variant)]
        fields = ['scan_reduce_ms', 'snapshot_ms', 'layout_ms', 'first_batch_ms', 'cancel_join_ms']
        result = {key: statistics.median(float(row[key]) for row in rows) for key in fields}
        result['snapshot_plus_layout_ms'] = statistics.median(float(row['snapshot_ms']) + float(row['layout_ms']) for row in rows)
        summary[f'{scenario}-{variant}'] = result

for variant in ['before', 'after']:
    for entries in ['100000', '1000000']:
        for scenario in ['rescan_availability', 'flat_layout', 'flat_max_checkpoint_gap']:
            per_run = []
            for run in range(1, 8):
                rows = list(csv.DictReader((out / f'micro-{variant}-{run}.stdout.csv').open(encoding='utf-8-sig')))
                values = [float(row['milliseconds']) for row in rows if (row['entries'], row['scenario']) == (entries, scenario) and (scenario != 'rescan_availability' or row['sample'] != '0')]
                assert len(values) == (40 if scenario == 'rescan_availability' else 3)
                per_run.append(statistics.median(values))
            summary[f'micro-{variant}-{entries}-{scenario}'] = {
                'median_of_run_medians_ms': statistics.median(per_run),
                'min_run_median_ms': min(per_run),
                'max_run_median_ms': max(per_run),
            }

for scenario in ['micro', 'balanced', 'real']:
    for variant in ['before', 'after']:
        rows = [row for row in processes if row['sample'].startswith(f'{scenario}-{variant}-')]
        fields = ['sampled_peak_working_bytes', 'sampled_peak_private_bytes', 'sampled_peak_handles', 'sampled_cpu_ms']
        summary[f'process-{scenario}-{variant}'] = {key: statistics.median(float(row[key]) for row in rows) for key in fields}

def micro(variant, entries, scenario):
    return summary[f'micro-{variant}-{entries}-{scenario}']['median_of_run_medians_ms']

def delta(before, after):
    return (after / before - 1) * 100

lines = [
    '# Correcciones de DiskPie: comparación de rendimiento v2',
    '',
    'Medición local del 2026-09-07, Windows/MSVC y perfil release. Siete rondas alternadas, proceso nuevo por variante. No había builds ni tests simultáneos. La referencia de estas correcciones es el binario posterior al rediseño conservado, no la versión previa a las entregas 1–3.',
    '',
    '## Microbenchmark de API y layout plano',
    '',
    'Milisegundos; mediana de siete medianas por proceso. Availability contiene 40 consultas calientes después de una inicial; cada proceso mide tres layouts con los mismos nombres y pesos pseudoaleatorios. El gap es el mayor intervalo entre comprobaciones de cancelación de cada layout; no mide cancelación→join. La instrumentación de checkpoints forma parte de estos tiempos de layout.',
    '',
    '| Caso | Antes | Después | Cambio |',
    '| --- | ---: | ---: | ---: |',
]
for entries in [100000, 1000000]:
    for scenario, label in [('rescan_availability', 'Disponibilidad RescanAll'), ('flat_layout', 'Layout plano instrumentado'), ('flat_max_checkpoint_gap', 'Mayor gap de checkpoints')]:
        before, after = [micro(variant, entries, scenario) for variant in ['before', 'after']]
        lines.append(f'| {label}, {entries:,} archivos | {before:.4f} | {after:.4f} | {delta(before, after):+.1f}% |')
lines += [
    '',
    'El recorrido por todos los nodos para habilitar RescanAll desaparece; sólo se enumeran raíces indexadas. La ordenación directa por bloques conserva el orden estable y permite abandonar trabajo durante las mezclas. El primer intento de ordenar índices, descartado por su coste, permanece íntegro bajo `target/review-ui-perf-after-20260907-implementation`: el plano de un millón pasó allí de 391,5412 a 587,5433 ms (+50,1%).',
    '',
    '## Árbol equilibrado y fixture NTFS',
    '',
    'Milisegundos; mediana de siete procesos. Se separan escaneo/reducción, snapshot y layout. La suma snapshot+layout es la mediana de las sumas por ejecución, no la suma de dos medianas.',
    '',
    '| Caso | Medida | Antes | Después | Cambio |',
    '| --- | --- | ---: | ---: | ---: |',
]
for scenario, label in [('balanced', 'Equilibrado 1M'), ('real', 'NTFS 10k')]:
    for key in ['scan_reduce_ms', 'snapshot_ms', 'layout_ms', 'snapshot_plus_layout_ms', 'first_batch_ms', 'cancel_join_ms']:
        before, after = [summary[f'{scenario}-{variant}'][key] for variant in ['before', 'after']]
        lines.append(f'| {label} | {key} | {before:.3f} | {after:.3f} | {delta(before, after):+.1f}% |')
lines += [
    '',
    'La referencia histórica del rediseño sigue siendo 0,662→32,075 ms para layout equilibrado 1M (`DESIGN_REPORT.md`, 2026-09-06). La tabla anterior es una comparación nueva con el mismo binario posterior al rediseño vuelto a medir hoy; no deben mezclarse los valores de días distintos para calcular un porcentaje de mejora.',
    '',
    'El índice de profundidad se calcula dentro de la agregación; elimina el prepass de layout pero añade trabajo a materialización. Debe juzgarse junto con snapshot+layout, no sólo el layout aislado. La fixture nativa conserva 10.000 archivos de 64 bytes (640.000 bytes lógicos y asignados) y sólo se leyó; es un caso local concreto, no una promesa general de velocidad del proveedor.',
    '',
    '## Memoria y handles',
    '',
    'El probe compilado de los tipos exactos devuelve `AggregateSize=32`, `NodeRecord=288`, `ChildCandidate=32`. La profundidad ocupa padding existente; los índices de raíces añaden entradas NodeId de cuatro bytes sólo por raíz (una raíz real top-level participa en ambos índices). El mergesort directo usa un scratch de 32 bytes por candidato; el intento de índices usaba dos índices usize, 16 bytes por candidato. No se atribuye ahorro causal a máximos de proceso.',
    '',
    '| Proceso | Variante | Working set pico muestreado MiB | Privado pico muestreado MiB | Handles pico muestreado |',
    '| --- | --- | ---: | ---: | ---: |',
]
for scenario in ['micro', 'balanced', 'real']:
    for variant in ['before', 'after']:
        row = summary[f'process-{scenario}-{variant}']
        lines.append(f"| {scenario} | {variant} | {row['sampled_peak_working_bytes']/2**20:.2f} | {row['sampled_peak_private_bytes']/2**20:.2f} | {row['sampled_peak_handles']:.0f} |")
lines += [
    '',
    'Estos máximos se muestrean cada 10 ms e incluyen inicialización, construcción, escaneo, layout y cierre. Pueden omitir picos; no son un perfil del allocator. El benchmark de API/layout tampoco representa GPU, compositor, vsync, FPS ni latencia completa de una interacción nativa.',
    '',
    '## Integridad y procedencia',
    '',
    '- 28 resultados de layout/fixture y 42 procesos, sin faltantes ni duplicados. En los siete pares coinciden nodos, bytes, sectores y tamaño de NodeRecord.',
    '- Equilibrado: 1.004.369 nodos, 529 sectores, 500.500.000 bytes lógicos y 4.096.000.000 asignados. NTFS: 10.001 nodos, dos sectores, 640.000 bytes en ambas métricas.',
    '- Baseline harness: `target/bench-build-current/release/diskpie-bench.exe`, SHA-256 validado contra `tools/diskpie-bench/results/design/build-manifest.json`, variante histórica `after`.',
    '- Baseline micro: `target/review-ui-perf-20260907-215950-locked/build/release/diskpie-review-ui-perf.exe`, validado contra los hashes de la revisión anterior.',
    '- Los nuevos binarios están en `target/review-ui-perf-after-20260907-v2/build/release/`; se compilaron offline con `--locked`. El micro conserva versiones/checksums del lock raíz; el harness conserva su lock independiente.',
    '- Inputs: `sources-before-build.json`, `compiled-identities.json`, `src/main.rs`, ambos Cargo.lock y `type-probe/`. Binaries antes/después de medir: `results/binaries.json`. Raw: `results/*.stdout.csv`, `results/*.stderr.txt`, `layouts.csv`, `processes.csv`; estadísticas: `summary.json`.',
    '',
    'Las pruebas y gates del proyecto se registran por separado. Estos datos verifican los casos descritos; no certifican otros proveedores, tamaños, idiomas, accesibilidad, interacciones de Windows ni ausencia de todas las carreras.',
]
(out / 'summary.json').write_text(json.dumps(summary, indent=2), encoding='utf-8')
(base / 'SUMMARY.md').write_text('\n'.join(lines) + '\n', encoding='utf-8')
print(json.dumps(summary, indent=2))
print('All data validated. SUMMARY.md written.')
