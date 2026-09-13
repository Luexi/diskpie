# Research records

Research notes justify significant reuse and platform choices before the code
depends on them. Prefer official documentation, crates.io metadata, upstream
repositories, release notes, and original research papers over secondary posts.

Every candidate review should record:

1. exact version or API baseline and research date;
2. license compatibility with `MIT OR Apache-2.0`;
3. recent maintenance and release activity;
4. advisories, unsafe-code footprint, and security boundary;
5. Windows 10/11 and portable-core compatibility;
6. compile-time and binary-size implications;
7. concrete benefit over a small local implementation; and
8. abandonment, lock-in, and fallback risks.

Finish each note with an actionable recommendation: adopt, prototype behind an
adapter, defer pending benchmarks, study without copying, or reject.

Product-facing UI decisions are recorded in
[`ui-product-direction.md`](ui-product-direction.md).
The accepted Scanner renovado implementation has a separate
[`validation record`](scanner-renovado-validation.md) for native captures,
automated checks and the limits of its CPU benchmarks.

The subsequent [review remediation record](review-remediation-validation.md)
tracks correctness fixes, retained native enumeration, schema-2 diagnostics,
depth/root indexing and new performance measurements. It preserves the original
reference sources, raw results and acceptance limits.
