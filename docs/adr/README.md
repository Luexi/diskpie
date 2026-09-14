# Architecture decision records

ADRs preserve the rationale and evidence for lasting choices. Read relevant
records when changing that area, not all records for every task. The
[current roadmap](../../ROADMAP.md), [architecture](../architecture.md) and
[AGENTS.md](../../AGENTS.md) take precedence over historical scope/workflow.

## Owner decision update, 2026-09-11

- Windows is permanently the only product platform. Neutral modules and Linux
  unit checks create no other platform commitments.
- Keep Rust/egui; simplify incrementally. Broad rewrites/migration need an owner
  decision, and existing infrastructure does not need expansion by default.
- File cleanup is recycle-only. Permanent deletion, fallback prompts offering
  it, and emptying the Recycle Bin are excluded.
- The permanent-delete/empty-bin portions of
  [ADR 0008](0008-destructive-confirmation-contract.md) and corresponding
  proposals elsewhere are superseded as product requirements. Historical
  implementation evidence remains; code removal is not claimed. Native path
  identity, confirmations and refusal guarantees remain relevant to recycling.
- Old release-blocking full-Scanner plans and mandatory investigation/review
  rituals are superseded by scoped usable deliveries and proportional checks.

## Recent records

- [0020: complete product composition](0020-complete-product-composition.md)
  records branch merging, background list/support composition and the
  create-only export refinement of ADR 0016. Its connected permanent-delete
  and empty-bin surfaces are superseded by the owner decision above.
- [0021: Scanner renovado](0021-scanner-renovado.md), including the depth-index
  addendum, records the map-first composition, adaptive radial depth, stable
  color families and additive list preferences.
- [0022: retained Windows directory enumeration](0022-retained-directory-enumeration.md)
  records the shared directory handle for enumeration and child metadata.

For a new lasting decision, use 0000-template.md, explain the concrete tradeoff
and link evidence. Ordinary edits/refactoring do not need a new ADR. Mark
replaced decisions as superseded with a link instead of erasing their history.
