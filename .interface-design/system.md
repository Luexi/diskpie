# DiskPie interface system

## Direction

DiskPie is a calm storage instrument for someone who needs to identify what is
filling a drive and act without uncertainty. The interface should feel spatial,
precise, responsive, and trustworthy rather than playful or dashboard-like.

Domain vocabulary: volume topology, radial sectors, rings, scan waves,
allocation, reclaimable space, hidden branches, omissions, and exact paths.

The signature element is the **radial lens**: the sunburst is the primary
workspace, and its center anchors the current path, selected-size summary,
navigation target, and live scan state. The same identity follows a branch into
the synchronized textual view.

Avoid these defaults:

- A permanent generic sidebar. Use a compact top command/path rail and a
  responsive inspection panel that can collapse below the radial lens.
- A grid of disconnected metric cards. Put volume context in a narrow telemetry
  strip and selection context in the radial-lens center/details.
- A rainbow visualization. Derive stable branch families from identity using
  the brand anchors, and reserve semantic colors for status and risk.

## Palette

Every color maps to a named role; components do not introduce arbitrary colors.

- `platter-midnight` `#062F57`: brand silhouette and dark chart families.
- `scan-current` `#05BCEB`: primary action, focus, and active progress.
- `sector-ember` `#F79A1E`: warm data families and important non-destructive
  emphasis; it is not a second control accent.
- `separator-porcelain` `#FEFBF0`: high-contrast dividers on dark chart areas.
- `chassis-slate` `#20242B`: dark-mode base surface.
- `danger-stop` `#D95B67`: destructive actions only, desaturated for dark mode.

Theme adapters derive foreground hierarchy, surfaces, borders, controls, and
status colors from these anchors while meeting contrast requirements in light
and dark themes.

## Depth and surfaces

Use borders plus whisper-quiet same-hue surface shifts. Do not use decorative
drop shadows or dramatic elevation jumps.

- `bed`: window background and navigation rail.
- `deck`: radial canvas and main content surface.
- `tray`: inspection/list surface.
- `popover`: menus, dialogs, and tooltips above their parent.
- `well`: controls and inputs inset slightly darker than their surface.

Borders progress from soft separation to normal structure, emphasis, and a
high-contrast keyboard focus ring. Side panels share the canvas hue and are
separated by one subtle border.

## Typography

- Use the bundled egui proportional family for UI copy until a reviewed
  permissive Unicode fallback is selected.
- Use the monospace family with tabular alignment for bytes, percentages,
  counts, rates, and timings.
- Maintain four text levels: primary, secondary, metadata, and disabled.
- Prefer weight and contrast over large size jumps. Exact paths may wrap or
  elide visually but remain fully available to copy and accessibility APIs.

## Spacing and shape

The base unit is 4 logical pixels. Common gaps are 4, 8, 12, 16, 24, and 32.
Controls use 8px internal horizontal padding and at least a 32px target height;
important toolbar targets are 36px or larger. Radius stays technical: 4px for
controls, 6px for trays, and 8px for modals. The radial lens itself is circular,
not placed inside a decorative rounded card.

## Interaction patterns

- Every control has default, hover, pressed, focus, disabled, loading, and error
  behavior where applicable.
- The chart and textual view share selection and focus by stable node ID.
- Backspace or Alt+Up moves to the parent; browser-style Back returns through
  history; Enter zooms/opens according to focus; Space toggles branch visibility.
- Reduced-motion preference disables navigation interpolation. Scan updates are
  coalesced and swap as coherent generations rather than constantly animating.
- Synthetic Other/Hidden sectors never expose filesystem mutation actions.
- Recycle/delete confirmations show the exact path. Permanent deletion has a
  visually stronger, separately worded confirmation.

## Responsive structure

Wide windows place the radial lens beside a 320-420px inspection rail. Narrow
windows put the inspection view below the lens and keep path/navigation commands
reachable. The lens size follows available space and DPI; toolbar actions move
into an overflow menu before labels become illegible.
