# DiskPie UI product direction

> Updated 2026-09-11. Implements the current
> [product direction](../../ROADMAP.md) and supersedes the mandatory
> “forensic radial instrument” styling in the earlier design brief.
> This is a design target; it does not claim all screens exist today.

## Product experience

Build an approachable, modern Windows application that helps someone understand
what occupies a drive, inspect real files and confidently choose what to
recycle. Visual clarity and finish are part of the work, alongside correctness
and a reasonable footprint. Windows 10/11 x64 is the only product platform;
the application remains free and open source.

Keep Rust and egui/eframe. Deliver one polished, connected slice containing
navigation, a real visualization, synchronized selection, an inspector and the
cleanup flow before proposing a different frontend. An owner's approval is
required for a technology migration. No exact mockup or wholesale rebrand is
implied by this direction.

Use the existing palette, fonts, chart and responsive shell as starting
resources. The old industrial identity, off-center radial lens and particular
rail arrangement are not immutable product requirements. A sidebar, tabs or
other navigation may be appropriate when demonstrated by the actual screens.
Create original design assets and wording; DiskBuddy's public features are a
scope reference, not permission to copy its code, graphics or interface.

## Implemented composition (2026-09-07)

[ADR 0021](../adr/0021-scanner-renovado.md) records the current map-first
composition: one compact toolbar (location, navigation, rescan, cancel, metric,
Show list, Tools), an informative map center, a footer with hover/focus detail
and progress, and an optional 260–400 point list panel with search, name/size
ordering and collapsible details; below 800 points the control switches between
map and list. Surfaces are neutral light/dark, Segoe UI is preferred when
installed, body text is 14 points and small/numeric text 13 points. Sunburst
rings adapt their depth to the tree and color families follow native paths.
Native captures and checks are in the
[redesign validation record](scanner-renovado-validation.md). This composition
is the starting point for the view switcher, inspector and cleanup basket below,
not a finished design for them.

## Information and interaction

- Keep the current location, scan state and primary navigation easy to find.
  Avoid presenting every technical counter with equal visual emphasis.
- Show one primary exploration view at a time, with an obvious view switcher,
  shared selection and a consistent inspector. The eight-view target is listed
  in [architecture](../architecture.md#one-shared-scan-multiple-views-target).
- Switching visualization preserves the relevant location and selection and
  uses existing scan data. Different charts may need their own layout work.
- Offer textual/keyboard access to the same real items shown by a chart. Make
  selected, hovered, focused and staged-for-cleanup states distinguishable.
- Present a reviewable cleanup list with exact paths, estimated space and
  per-item results. Keep browsing and staging predictable; confirmation is an
  explicit step. The authoritative recycle-only behavior is in
  [product direction](../../ROADMAP.md).
- Explain disabled/unavailable actions in ordinary language. Oversized items,
  network locations or unavailable bins never produce a permanent-delete
  fallback. Do not offer empty-bin controls.
- Label age views by last modification; do not call old files “unused” or safe
  to remove. Explain omissions, unknown allocation and scan incompleteness.
- Show duplicate, quick-cleanup and leftover findings as reviewable evidence,
  with the reason they were suggested. Do not promise an exact reclaimable
  total when filesystem identity or allocation makes that uncertain.
- Keep primary language straightforward in English and Spanish. Surface a
  technical detail only when it helps the user choose an action or understand
  a limitation.

## Visual resources

Use semantic roles consistently across light, dark and system themes. The
existing anchors are Midnight `#062F57`, cyan `#05BCEB`, Ember `#F79A1E`,
Porcelain `#FEFBF0`, slate `#20242B` and risk red `#D95B67`. They may evolve
with the connected design; components should not introduce arbitrary colors.
Selection and file categories must remain distinguishable from action risk.

The embedded proportional/monospace faces already provide UI text and numerical
alignment. The application also loads available Windows fonts at startup as
bounded Unicode fallbacks; that work is implemented, not a new font project.
It uses the installed fonts without redistributing them. Preserve native names
when rendering multilingual paths, and make the full path inspectable/copyable.

Use consistent spacing, readable hierarchy and restrained surfaces. Prefer
space, typography and clear grouping over decorative cards or dense telemetry.
Motion must explain state and remain optional; the application must stay quiet
when idle. [.interface-design/system.md](../../.interface-design/system.md)
contains a short implementation summary, subordinate to this direction.

## Responsive behavior and truthful states

Wide windows can pair the primary view and inspector; narrow windows may move
or collapse the inspector while keeping navigation and Cancel reachable.
Respect Windows display scaling, legible labels, keyboard focus and theme
contrast. Never rely on color alone for warnings, selection or progress.

Design and verify empty, selecting, scanning, partial, cancelling, completed,
cancelled-with-results and failed states using the existing scan state machine.
Closing a picker preserves prior results. Cleanup additionally needs empty
selection, review, running, cancellation, partial completion and unsupported
recycling states; these remain planned until connected and tested.

Keep filesystem and Shell I/O, large layouts and content loading outside the
frame callback. Reuse cached geometry and virtualize lists; decorative effects
must not reduce cancellation responsiveness or cause continuous idle repaint.

## Evidence for a finished slice

A delivered slice is visible in the application and demonstrable using real
scan results. Record screenshots of its important states and a short Windows
check of the complete journey, including keyboard navigation, scaling/theme
behavior and a cancellation/failure case appropriate to the change. Recycle
checks operate only on disposable fixtures created for the test.

Use automated checks for changed state transitions, identity and layout rules;
do not require a standing committee or multiple independent reviews for every
small adjustment. Record performance evidence when the change could affect
large scans or frame work. A prototype, disabled control or adapter alone is
not a completed user-facing feature.
