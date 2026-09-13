# Correcciones de DiskPie: comparación de rendimiento v2

Medición local del 2026-09-07, Windows/MSVC y perfil release. Siete rondas alternadas, proceso nuevo por variante. No había builds ni tests simultáneos. La referencia de estas correcciones es el binario posterior al rediseño conservado, no la versión previa a las entregas 1–3.

## Microbenchmark de API y layout plano

Milisegundos; mediana de siete medianas por proceso. Availability contiene 40 consultas calientes después de una inicial; cada proceso mide tres layouts con los mismos nombres y pesos pseudoaleatorios. El gap es el mayor intervalo entre comprobaciones de cancelación de cada layout; no mide cancelación→join. La instrumentación de checkpoints forma parte de estos tiempos de layout.

| Caso | Antes | Después | Cambio |
| --- | ---: | ---: | ---: |
| Disponibilidad RescanAll, 100,000 archivos | 0.5335 | 0.0014 | -99.7% |
| Layout plano instrumentado, 100,000 archivos | 23.7639 | 24.5911 | +3.5% |
| Mayor gap de checkpoints, 100,000 archivos | 17.1953 | 0.5474 | -96.8% |
| Disponibilidad RescanAll, 1,000,000 archivos | 5.8304 | 0.0023 | -100.0% |
| Layout plano instrumentado, 1,000,000 archivos | 536.4771 | 512.5303 | -4.5% |
| Mayor gap de checkpoints, 1,000,000 archivos | 472.0354 | 5.0070 | -98.9% |

El recorrido por todos los nodos para habilitar RescanAll desaparece; sólo se enumeran raíces indexadas. La ordenación directa por bloques conserva el orden estable y permite abandonar trabajo durante las mezclas. El primer intento de ordenar índices, descartado por su coste, permanece íntegro bajo `target/review-ui-perf-after-20260907-implementation`: el plano de un millón pasó allí de 391,5412 a 587,5433 ms (+50,1%).

## Árbol equilibrado y fixture NTFS

Milisegundos; mediana de siete procesos. Se separan escaneo/reducción, snapshot y layout. La suma snapshot+layout es la mediana de las sumas por ejecución, no la suma de dos medianas.

| Caso | Medida | Antes | Después | Cambio |
| --- | --- | ---: | ---: | ---: |
| Equilibrado 1M | scan_reduce_ms | 6779.054 | 6977.072 | +2.9% |
| Equilibrado 1M | snapshot_ms | 1806.887 | 1658.636 | -8.2% |
| Equilibrado 1M | layout_ms | 31.234 | 0.605 | -98.1% |
| Equilibrado 1M | snapshot_plus_layout_ms | 1840.945 | 1659.365 | -9.9% |
| Equilibrado 1M | first_batch_ms | 0.718 | 0.532 | -25.9% |
| Equilibrado 1M | cancel_join_ms | 0.589 | 0.492 | -16.5% |
| NTFS 10k | scan_reduce_ms | 296.344 | 466.344 | +57.4% |
| NTFS 10k | snapshot_ms | 5.678 | 7.003 | +23.3% |
| NTFS 10k | layout_ms | 0.706 | 0.629 | -10.9% |
| NTFS 10k | snapshot_plus_layout_ms | 6.384 | 7.632 | +19.5% |
| NTFS 10k | first_batch_ms | 16.827 | 22.688 | +34.8% |
| NTFS 10k | cancel_join_ms | 0.418 | 0.367 | -12.2% |

La referencia histórica del rediseño sigue siendo 0,662→32,075 ms para layout equilibrado 1M (`DESIGN_REPORT.md`, 2026-09-06). La tabla anterior es una comparación nueva con el mismo binario posterior al rediseño vuelto a medir hoy; no deben mezclarse los valores de días distintos para calcular un porcentaje de mejora.

El índice de profundidad se calcula dentro de la agregación; elimina el prepass de layout pero añade trabajo a materialización. Debe juzgarse junto con snapshot+layout, no sólo el layout aislado. La fixture nativa conserva 10.000 archivos de 64 bytes (640.000 bytes lógicos y asignados) y sólo se leyó; es un caso local concreto, no una promesa general de velocidad del proveedor.

## Memoria y handles

El probe compilado de los tipos exactos devuelve `AggregateSize=32`, `NodeRecord=288`, `ChildCandidate=32`. La profundidad ocupa padding existente; los índices de raíces añaden entradas NodeId de cuatro bytes sólo por raíz (una raíz real top-level participa en ambos índices). El mergesort directo usa un scratch de 32 bytes por candidato; el intento de índices usaba dos índices usize, 16 bytes por candidato. No se atribuye ahorro causal a máximos de proceso.

| Proceso | Variante | Working set pico muestreado MiB | Privado pico muestreado MiB | Handles pico muestreado |
| --- | --- | ---: | ---: | ---: |
| micro | before | 356.89 | 474.92 | 87 |
| micro | after | 372.92 | 474.91 | 87 |
| balanced | before | 1115.35 | 1136.83 | 134 |
| balanced | after | 1119.18 | 1136.61 | 134 |
| real | before | 12.14 | 9.27 | 136 |
| real | after | 12.36 | 9.80 | 136 |

Estos máximos se muestrean cada 10 ms e incluyen inicialización, construcción, escaneo, layout y cierre. Pueden omitir picos; no son un perfil del allocator. El benchmark de API/layout tampoco representa GPU, compositor, vsync, FPS ni latencia completa de una interacción nativa.

## Integridad y procedencia

- 28 resultados de layout/fixture y 42 procesos, sin faltantes ni duplicados. En los siete pares coinciden nodos, bytes, sectores y tamaño de NodeRecord.
- Equilibrado: 1.004.369 nodos, 529 sectores, 500.500.000 bytes lógicos y 4.096.000.000 asignados. NTFS: 10.001 nodos, dos sectores, 640.000 bytes en ambas métricas.
- Baseline harness: `target/bench-build-current/release/diskpie-bench.exe`, SHA-256 validado contra `tools/diskpie-bench/results/design/build-manifest.json`, variante histórica `after`.
- Baseline micro: `target/review-ui-perf-20260907-215950-locked/build/release/diskpie-review-ui-perf.exe`, validado contra los hashes de la revisión anterior.
- Los nuevos binarios están en `target/review-ui-perf-after-20260907-v2/build/release/`; se compilaron offline con `--locked`. El micro conserva versiones/checksums del lock raíz; el harness conserva su lock independiente.
- Inputs: `sources-before-build.json`, `compiled-identities.json`, `src/main.rs`, ambos Cargo.lock y `type-probe/`. Binaries antes/después de medir: `results/binaries.json`. Raw: `results/*.stdout.csv`, `results/*.stderr.txt`, `layouts.csv`, `processes.csv`; estadísticas: `summary.json`.

Las pruebas y gates del proyecto se registran por separado. Estos datos verifican los casos descritos; no certifican otros proveedores, tamaños, idiomas, accesibilidad, interacciones de Windows ni ausencia de todas las carreras.

## Controles adicionales: worker real y NTFS con calentamiento

La matriz v2 tiene alta variabilidad: la mediana por ronda del layout plano baseline de 1M varía entre 364,6385 y 878,4316 ms. En sus siete pares NTFS siempre fue más rápida la segunda ejecución, independientemente de variante; por tanto el +57,4% observado en aquella tabla tiene un efecto de orden/cache y no se interpreta como coste causal de la corrección. La matriz anterior v1 había observado +8,5%. Ambas se conservan.

### Relevo y cierre del worker real

Un mismo programa usa LayoutService original y corregido, enlazado a las rlibs conservadas de cada versión. Retiene un snapshot de un millón de archivos, inicia un layout grande y espera 200 ms antes de supersederlo por una vista pequeña o ejecutar Drop. Los siete casos por variante seguían ocupados. No hay barrera dentro de producción. La espera fija no garantiza entrar en la fase peor del algoritmo; el snapshot permanece retenido, por lo que no se incluye su destrucción.

| Medida (ms, mediana de siete) | Antes | Después | Rango después |
| --- | ---: | ---: | ---: |
| Submit siguiente → completion pequeña | 255.7001 | 3.3937 | 3.0223–4.7648 |
| Drop (request stop) → join real | 264.2042 | 2.7465 | 2.4531–4.0477 |

La primera medida incluye admisión, cancelación cooperativa del trabajo anterior, cálculo pequeño y polling cada 1 ms. La segunda usa el Drop que solicita stop y hace join efectivo. Son medidas del servicio, distintas del gap entre checkpoints; ninguna es un frame completo, FPS, click→píxel ni cota universal de cancelación.

### Control nativo con calentamiento equivalente

Antes de CADA proceso medido se ejecuta un proceso completo del mismo binario baseline sobre la fixture NTFS existente (escaneo, materialización, layout y escaneo cancelado). Ese calentamiento termina fuera de todos los timers de la siguiente muestra. Las variantes se alternan durante siete rondas. No se modificó la fixture, no hubo builds/tests simultáneos y coinciden nodos, bytes, sectores y NodeRecord en los 14 resultados.

| Medida (ms) | Antes | Después |
| --- | ---: | ---: |
| scan_reduce_ms | 285.009 | 287.016 |
| snapshot_ms | 5.892 | 6.349 |
| layout_ms | 0.687 | 0.615 |
| first_batch_ms | 16.510 | 17.029 |
| cancel_join_ms | 0.820 | 0.539 |

El cambio observado de escaneo/reducción con este control es +0.7%. Sigue siendo un caso de 10.000 archivos de 64 bytes con caché caliente: no demuestra rendimiento universal de NTFS, red ni cloud, ni aísla cada llamada Windows.

Inputs/binarios del probe real: `worker-probe/{main.rs,identities.json,before.exe,after.exe}`; datos y resumen: `worker-probe/{before-*.csv,after-*.csv,summary.json}`. Control nativo: `run-native-warmed.ps1`, `native-warmed/{method.json,binaries.json,measurements.csv,summary.json}`, más stdout/stderr de cada calentamiento y muestra. Los hashes de binarios y rlibs se verificaron después de las mediciones. La disponibilidad sigue siendo una API aislada; no se midió el frame completo con esta corrección.
