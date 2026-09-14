# Scanner renovado: rendimiento antes/después

Referencia: copia del árbol de trabajo inmediatamente anterior al rediseño, incluyendo las entregas 1–3. Siete ejecuciones intercaladas por variante, con un proceso nuevo por muestra. Los hashes, configuración, hardware y toolchain están en `results/design/environment.json`.

Equipo: 13th Gen Intel(R) Core(TM) i5-13420H, 12 procesadores lógicos, 23.67 GiB de RAM. rustc 1.97.1 (8bab26f4f 2026-07-14); Windows/MSVC, perfil release y dependencias bloqueadas. Se validaron 33,684 frames, 84 layouts y 126 procesos.

Las mediciones de interfaz usan el mismo cuerpo de prueba y una ventana egui headless de 1280×800 puntos con fuentes embebidas. Un ajuste exclusivo del helper de pruebas activa la lista opcional. `before` muestra el inspector anterior; `after-map` es la vista inicial nueva; `after-list` abre la lista nueva. Ambas versiones indexan todos los elementos. Las diferencias entre variantes también incluyen diferencias deliberadas en la cantidad de contenido visible.

## CPU por frame

Milisegundos; mediana de siete estadísticas por ejecución. Los máximos de transición conservan el coste de las publicaciones asíncronas.

| Archivos | Etapa | Estadística | Antes | Después: mapa | Después: lista |
|---|---|---|---:|---:|---:|
| 100,000 | snapshot_first | median | 3.884 | 3.038 | 2.936 |
| 100,000 | snapshot_transition | max | 2.631 | 1.852 | 2.433 |
| 100,000 | warm | p95 | 0.877 | 0.853 | 1.104 |
| 100,000 | metric_first | median | 0.635 | 0.986 | 0.964 |
| 100,000 | metric_transition | max | 3.270 | 2.317 | 3.676 |
| 100,000 | metric_warm | p95 | 1.620 | 1.449 | 1.871 |
| 1,000,000 | snapshot_first | median | 13.257 | 11.689 | 13.228 |
| 1,000,000 | snapshot_transition | max | 16.489 | 12.162 | 15.689 |
| 1,000,000 | warm | p95 | 14.207 | 14.544 | 13.579 |
| 1,000,000 | metric_first | median | 11.949 | 10.276 | 10.601 |
| 1,000,000 | metric_transition | max | 16.476 | 15.801 | 16.997 |
| 1,000,000 | metric_warm | p95 | 13.310 | 13.524 | 15.468 |

## Layout

Milisegundos; mediana de siete ejecuciones. El layout nuevo calcula la profundidad efectiva antes del agrupamiento. Se verifican los mismos nodos, bytes y sectores en ambas versiones.

| Árbol | Entradas solicitadas | Antes | Después | Cambio |
|---|---:|---:|---:|---:|
| flat | 100,000 | 19.900 | 23.075 | +16.0% |
| flat | 1,000,000 | 455.670 | 483.661 | +6.1% |
| balanced | 100,000 | 0.311 | 3.309 | +964.0% |
| balanced | 1,000,000 | 0.662 | 32.075 | +4745.2% |
| deep | 100,000 | 0.414 | 0.452 | +9.2% |
| deep | 1,000,000 | 13.944 | 13.573 | -2.7% |

## Proceso completo: memoria y handles

Mediana de siete ejecuciones. Working set, bytes privados y handles son máximos muestreados cada 10 ms; la CPU es el último acumulado observado. Incluyen inicialización, escaneo, servicios y cierre, además de los frames. No representan memoria incremental de un componente ni un perfil de asignaciones.

| Archivos | Variante | Pico working set (MiB) | Pico privado (MiB) | Pico handles | CPU observada (s) |
|---|---|---:|---:|---:|---:|
| 100,000 | before | 64.3 | 75.6 | 173 | 1.20 |
| 100,000 | after-map | 85.4 | 85.6 | 173 | 1.19 |
| 100,000 | after-list | 80.8 | 78.8 | 173 | 1.41 |
| 1,000,000 | before | 836.6 | 841.6 | 173 | 12.11 |
| 1,000,000 | after-map | 811.6 | 817.6 | 173 | 15.12 |
| 1,000,000 | after-list | 771.5 | 776.2 | 173 | 15.59 |

## Interpretación y regresiones

Los resultados no muestran una mejora uniforme de CPU por frame. La vista de mapa mantiene un coste parecido en el caso caliente de un millón de archivos. Algunas primeras publicaciones mejoran y otras transiciones o estados calientes empeoran; siete muestras locales no permiten atribuir significado estadístico a diferencias pequeñas.

La regresión más clara de layout aparece en el árbol equilibrado de un millón de entradas: 0.662 → 32.075 ms, es decir, +31.413 ms. El código nuevo recorre descendientes con peso positivo para calcular la profundidad antes de agrupar, ocultar o aplicar el presupuesto; el anterior podía agrupar pronto y evitar ese recorrido. Ese mecanismo explica trabajo adicional, aunque estas mediciones no son un perfil causal por función. El cálculo ocurre en el worker de layout y conserva cancelación cooperativa; no se presenta como una mejora de rendimiento.

Con la lista abierta, el p95 caliente de 100 mil archivos aumenta 0.227 ms; después del cambio de métrica, el de un millón aumenta 2.157 ms. La lista nueva muestra más filas y controles que el inspector anterior, por lo que las variantes no dibujan idéntico contenido.

La revisión estática localizó un recorrido heredado de todos los nodos al consultar la disponibilidad de reescaneo completo (`NavigationState::full_rescan_unavailable_reason` → `filesystem_roots`). Está presente en ambas versiones; el rediseño no añade otro recorrido de todo el árbol por frame. Es un candidato a optimización posterior, pero haría falta perfilar para cuantificar su contribución al tiempo medido.

El índice de familia añade ocho bytes por nodo (unos 8 MB para un millón). Los picos de proceso menores en algunas variantes no prueban un ahorro de memoria: dependen de la temporización y de la vida de snapshots y servicios, y el muestreo puede omitir picos entre observaciones. Los handles muestreados son iguales en estas seis combinaciones.

## Alcance de la evidencia

La duración por frame incluye lógica, widgets y emisión de shapes por egui; excluye ventana nativa, rasterización GPU, compositor y vsync. No es una medición de FPS. Cada ejecución tiene 20 frames de calentamiento vacíos, dos primeros frames y 200 frames por etapa de transición/caliente. Las pausas de 10 ms entre frames de transición quedan fuera de la duración medida.

`processes.csv` conserva CPU de proceso, memoria y handles muestreados cada 10 ms; incluyen escaneo, servicios, inicialización y cierre. Los CSV de layout separan escaneo/reducción, publicación, layout y cancelación. Son resultados locales; la validación visual nativa se documenta por separado. No se deduce una mejora general del escáner de cambios en estos frames.
