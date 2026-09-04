# DiskPie UI product direction

- Decision date: 2026-08-02
- Frontend: egui/eframe 0.35
- Direction: **Forensic radial instrument**
- Dominant tone: industrial/utilitarian; secondary tone: editorial

## Purpose

The interface is an exploratory instrument for answering two questions quickly:
where did the space go, and what exact filesystem object is safe to act on?
Scanning, navigation, comparison, and cancellation are primary. Destructive
actions remain visibly separate and require their own confirmation flow.

## Design feasibility and impact

| Dimension | Score | Rationale |
| --- | ---: | --- |
| Aesthetic impact | 4 | A live radial map and instrument-like telemetry are recognizable without branding. |
| Context fit | 5 | The visual language suits inspection, measurement, and filesystem uncertainty. |
| Implementation feasibility | 4 | egui supports the chart, asymmetrical rails, focus, and custom painting directly. |
| Performance safety | 5 | Static geometry is cached; motion and effects never gate input or scan polling. |
| Consistency risk | 3 | The restrained palette and spacing scale are small enough to enforce throughout the app. |

`DFII = (4 + 5 + 4 + 5) - 3 = 15`.

## Differentiation anchor

Without the logo, DiskPie should still be recognizable as a **radial storage
instrument**: a large off-center sunburst lens, a compact live telemetry rail,
and a synchronized textual inspection column. The empty state uses quiet
calibration rings; real scan sectors replace them rather than sitting on top of
decorative graphics.

## Design system snapshot

- Typography: the already embedded Ubuntu proportional face carries labels and
  prose; the embedded Hack monospace face carries byte values, counts, paths,
  percentages, and status codes. This avoids adding a font download or a new
  license surface to the portable executable.
- Dominant color: Platter Midnight (`#062F57`). Scan/current accent: cyan
  (`#05BCEB`). Selection/destructive attention: Sector Ember (`#F79A1E`).
  Porcelain (`#FEFBF0`) is the warm light neutral. Dark and light themes derive
  surfaces and text contrast from this same hierarchy rather than introducing
  parallel palettes.
- Spacing rhythm: 4, 8, 12, 20, and 32 logical points. Dense telemetry uses the
  lower steps; confirmation and empty states use the upper steps.
- Motion: sparse and state-bearing only. Partial-result commits may use a short
  highlight; cancellation and progress use textual/state changes. Pointer
  movement never rebuilds chart geometry, and no continuous decorative
  animation requests frames while idle.
- Shape: thin separators and crisp focus outlines. Depth comes from sector
  hierarchy and panel contrast, not generic shadows or glass effects.

## Responsive and accessible behavior

- At wide widths the chart and inspection rail are side by side; at narrow
  widths the textual view follows the chart in the same reading order.
- Every chart action has a keyboard/list equivalent. The synchronized list is
  not a secondary diagnostic dump; it is the accessible way to select, zoom,
  inspect, and open the same real nodes.
- Status is never encoded by color alone. Scan phase, omissions, unknown size,
  hard-link precision, and destructive risk all receive text.
- Cancel remains reachable during scanning. Native dialogs and filesystem I/O
  stay outside the egui callback.
- Exact native paths remain separate from lossy display strings. Confirmation
  surfaces can show a lossless escaped representation when needed.

## Performance constraints

- The frame loop may only poll bounded channels and consume already-built
  immutable values.
- Snapshot reduction, path/volume resolution, layout materialization, Shell
  calls, settings writes, diagnostics flushes, and filesystem mutations never
  run in the egui callback.
- The textual view renders a bounded visible subset and uses stable node IDs.
- Hover reconstructs at most the hovered path; idle frames allocate no full
  path table and do not copy the hidden-branch set.

## Decision note 2026-09-04: system fallback fonts read at runtime

The embedded Ubuntu and Hack faces cover Latin, Greek, and Cyrillic only, so
file names in CJK, Thai, Devanagari, Arabic, Myanmar, Tibetan, or Javanese
rendered as boxes. Rather than embedding additional font files (a new license
surface and several megabytes in the portable executable), the composition
root reads, at startup and before the native window exists, the following
fonts from `%WINDIR%\Fonts` (falling back to `C:\Windows\Fonts`) when they are
present: `segoeui.ttf`, `msyh.ttc` (face 0), `malgun.ttf`, `YuGothR.ttc`
(face 0), `Nirmala.ttc` (face 0), `leelawad.ttf`, `ebrima.ttf`,
`mmrtext.ttf`, `himalaya.ttf`, and `javatext.ttf`. Missing or unreadable files
are skipped silently, the total bytes read are bounded to 64 MiB, and the
loaded faces are appended after the embedded fonts as fallbacks for both the
proportional and monospace families through one `Context::set_fonts` call in
`DiskPieShell::new`. The fonts are read from the user's own Windows
installation at runtime and are never redistributed with DiskPie.

The same change adds `raw-window-handle 0.6.2` (MIT OR Apache-2.0 OR Zlib,
already in the dependency graph through eframe and winit, no `unsafe` on the
call path DiskPie uses) as a direct dependency of the executable, because the
`HasWindowHandle` trait needed to read the owner HWND from `eframe::Frame` is
not re-exported by eframe.

## Acceptance evidence

- Automated reducer and renderer tests cover pointer and keyboard requests,
  metric changes, stale generations, synthetic sectors, and bounded mesh work.
- A headless UI test covers the empty, scanning, partial, complete, cancelled,
  and failed states at wide and narrow sizes.
- Windows smoke evidence covers per-monitor DPI, light/dark/system themes,
  keyboard focus, native folder selection, and responsive cancellation.
- Accessibility and UX reviewers must approve the final connected interface
  before release.
