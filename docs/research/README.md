# Research records

These notes preserve evidence behind existing choices. Their proposals,
mandatory investigation sequences and platform ambitions are historical unless
adopted by the current [roadmap](../../ROADMAP.md) and
[architecture](../architecture.md). Read a note only for the question at hand.
The active [UI direction](ui-product-direction.md) is maintained separately.

Research an unresolved decision when it can materially change the result.
Use primary documentation and inspect the actual repository versions. For a
significant dependency, briefly record its concrete benefit, license
compatibility, maintenance and material Windows/build/runtime costs. Use an
existing note or change description; do not require an exhaustive report for
every edit.

Windows 10/11 x86-64 is the only product target. Cleanup is recycle-only, without
permanent-delete fallback or an empty-bin feature. Older research cannot
authorize those features or other platforms. Parallel agents and independent
review are tools for bounded work and meaningful risk, not prerequisites for
each task. Follow [AGENTS.md](../../AGENTS.md).

## Validation records

- [Scanner renovado validation](scanner-renovado-validation.md): native
  captures, automated checks and the CPU benchmark limits of the map-first
  redesign (ADR 0021).
- [Review remediation validation](review-remediation-validation.md):
  correctness fixes, retained native enumeration, schema-2 diagnostics,
  depth/root indexing and performance measurements with their raw results.
- [Benchmark harness](../../tools/diskpie-bench/README.md): reproducible
  seven-run comparisons; smoke data is not acceptance evidence.
