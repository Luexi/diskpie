# DiskPie UI product direction

- Decision date: 2026-09-06
- Frontend: egui/eframe 0.35
- Direction: **Scanner renovado**, with an optional synchronized list
- Supersedes the forensic instrument composition; see ADR 0021.

## Purpose and hierarchy

The map answers where the space went; its center identifies the current folder,
active size and measurement. A compact toolbar provides location, navigation,
rescan, cancellation, metric, list visibility and Tools. The footer carries hover
or keyboard-focus information and progress. Unknown sizes, omissions and pending
reconciliation remain explicit; missing data never appears as an exact zero.

Show list opens a resizable 260-400 point panel, initially 300 points, with a
searchable complete list and collapsible details. Below 800 points the same
control switches between map and list without rewriting the saved preference.
The table has 30-point rows and aligned size/percentage columns. Sorting and
filtering run off-thread.

Compact size columns and the map center use `≥ X` for a known lower bound;
tooltips/details retain the localized full explanation. Partial percentages use
`%*` and explain that the denominator is the known size in the current view.
Active work shows a live entry count in the footer. Disclosure arrows are
drawn geometrically so navigation does not depend on symbol-font coverage.

## Visual system

- Neutral white/light-gray surfaces and a matching charcoal dark theme.
- System theme and Allocated metric remain the defaults; existing saved theme,
  locale and metric choices survive this change.
- Segoe UI is the primary proportional face when available on the user's
  Windows installation. Existing embedded and multilingual fallback fonts remain.
  No system font is redistributed or downloaded.
- Body/button text: 14 points; small/numeric text: 13 points. Toolbar: 40 points;
  footer: 28; map margin: 16. Clear focus and readable secondary text.
- Color families begin at direct children of scanned roots. Descendants inherit
  the hue with bounded depth/identity variations, based on native paths rather
  than current sorting or relative zoom depth.

## Geometry and interaction

The center radius is 0.28. Eight levels remain the default depth ceiling. The
snapshot aggregation calculates positive-weight depth per metric before hiding,
minimum-angle aggregation and sector budgeting. The worker reads this value,
applies the configured ceiling and distributes the rings over that radius.
Hover, selection and sector budget alone cannot change scale. A tiny deep branch
can reserve depth even if grouped: this is the explicit stability tradeoff.

Click enters a folder or selects a file without opening it. Center goes up;
Enter activates, Space selects, arrows move focus and Shift+F10 opens the context
menu. Double-click handling must not enter two successive folders after the first
click changes the layout. The center always describes the view root. Hover,
selection, menu subject and confirmed action target remain distinct.

## Performance and evidence

No filesystem, Shell, settings, sorting or layout work moves into egui callbacks.
Geometry and immutable presentation commit coherently. Focus, hover and selection
are separate overlays. List ranking and selected-item lookup share a comparator.

The accepted HTML mockup uses fictional data and is not product validation.
Measurements and native/headless coverage are recorded separately in
`tools/diskpie-bench/DESIGN_REPORT.md` and
`docs/research/scanner-renovado-validation.md`. Scripts live under
`tools/diskpie-bench/scripts/`. Headless timing is not an end-to-end FPS claim.
