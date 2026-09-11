# DiskPie interface implementation summary

This is a compact working reference, subordinate to
[product direction](../ROADMAP.md) and
[UI product direction](../docs/research/ui-product-direction.md). Update it when
the implemented design changes; it does not create separate product policy.

## Direction

An approachable, polished Windows storage explorer with a shared inspector and
reviewable cleanup flow. Preserve Rust/egui and improve a connected slice. The
sunburst remains a useful view, but other views have equal access to selection,
navigation and the inspector. The earlier industrial radial-only composition
is not mandatory. Use existing components before introducing a new design layer.

## Starting resources

| Role | Existing anchor |
| --- | --- |
| Primary dark/brand | Midnight `#062F57` |
| Primary controls and focus | Cyan `#05BCEB` |
| Warm data emphasis | Ember `#F79A1E` |
| Light neutral | Porcelain `#FEFBF0` |
| Dark base | Slate `#20242B` |
| Risk/error | Red `#D95B67` |

These are starting resources, not a rebrand mandate. Derive coherent light,
dark and system themes from semantic roles; distinguish selection, data colors
and risk. State must also be expressed with labels or shape.

Use the existing proportional face for text, the monospace family for aligned
metrics, and the already implemented Windows fallback fonts for native names.
Keep paths inspectable and copyable. Use a consistent 4-point spacing rhythm,
readable control targets, visible focus and restrained borders/surface contrast.

## Components and behavior

- A clear location/navigation area, one primary exploration view, a view
  switcher, synchronized textual access and a responsive inspector.
- A staged cleanup list with explicit review and per-item outcomes, governed
  by the canonical recycle-only policy. Unsupported recycling leaves files
  untouched; there are no permanent-delete or empty-bin controls.
- Shared real-node identity across all views. Aggregate chart sectors are not
  filesystem action targets.
- Keyboard/list alternatives, sensible focus order and shortcuts that yield
  to text input and open dialogs. Verify changes against the implemented
  command mapping instead of inventing a conflicting shortcut scheme.
- Wide layouts may place an inspector beside the view; narrow layouts keep
  essential actions reachable and may relocate/collapse secondary panels.
- Cache chart geometry, virtualize lists, respect reduced motion and avoid
  continuous idle repaint. Never perform filesystem/Shell I/O in a frame.

Verify the actual screen at relevant window sizes, Windows scaling and themes.
Document planned components as planned until they are connected and checked;
this summary is not evidence that the expanded interface already exists.
