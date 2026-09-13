# Scanner renovado: validation record

Date: 2026-09-06. Scope: ADR 0021 and the approved map-first composition A,
with optional list B. This record supplements, rather than replaces, the
deliveries 1–3 evidence. No release, distribution validation or destructive
operation on user data is part of this redesign.

This is the historical record for the sources and binaries identified below.
Subsequent fixes and measurements are documented in
[review remediation validation](review-remediation-validation.md); they do not
retroactively change this dataset or certify new native interactions.

## Reference and reproducibility

The pre-redesign working sources, including deliveries 1–3, were copied to
`target/design-before` before implementation. The older initial implementation
under `target/bench-baseline` is not this design's reference. Build artifacts
are isolated by source tree. The performance build manifest records source and
executable SHA-256 hashes before measuring and verifies them afterwards.

`tools/diskpie-bench/visual_matrix.inc.rs` is an opt-in Windows test instrument.
It runs the real eframe shell, its scan/runtime/presentation services and Glow
renderer with a deterministic in-memory filesystem. Native eframe screenshot
events provide the captured pixels. A bounded writer saves them off the UI
thread; the harness checks worker shutdown after closing each window.

For the reference build, the same instrument was added inside `shell::tests`,
with only the two new list-visibility/width assignments removed. A test-only
pending-reconciliation setter and the already locked winit dev dependency were
backported so both versions receive equivalent fixture states. Production
behavior in the reference sources was not changed. The reference retains its
permanent inspector; it cannot have the new optional-list composition.

## Native visual coverage

The deterministic tree has five main folders, two nested folders per family,
1,024 root files and positive data through three radial levels. Both builds
receive the same native paths, identities and measurements. The fixture also
provides active scanning, a settled omission, cancellation with results,
pending reconciliation, and a cancelled branch rescan retaining prior data.
No fixture action deletes or recycles anything.

Requested client sizes are **800×600, 1280×850 and 1920×1080 logical points**,
with English/Spanish, light/dark and 1×/2× pixels per point. The latter produces
physical screenshots up to 3840×2160; it exercises application scaling, not a
physical mixed-monitor DPI transition. The CSV manifests record requested and
actual pixel/point sizes, scale and observed state. Focus, pointer navigation,
sorting and context-menu target validity also have automated egui/policy tests;
static screenshots alone do not validate those interactions.

An additional 700×600-point case checks the narrow-window map/list switch.
The native runner loads the same installed system fonts as production before
creating the event loop. `scanner-renovado/fonts.txt` records the loaded faces;
Segoe UI was available. A missing disclosure glyph found during review was
replaced with a vector triangle. A partial size column that hid its numerical
value was replaced with the same compact `≥ X` notation as the center, and
partial percentage headings and active-work progress were made explicit.

The final native matrix contains **60 current captures and 30 reference
captures**, with no mismatched pixel/point dimensions. The complete-state
matrix contains 48 current cases, plus two 700-point cases. The other ten
captures cover the five additional states in A/B at 1280×850, Spanish/light/1×;
those states are not a full cross-product of every theme, language and size.
Each reference case uses the old permanent inspector. The full local pixel
evidence is under `target/design-evidence/native-before-final-v2` and
`target/design-evidence/native-after-final-v4`; the checked manifests and
representative images are retained with this document:

- [Before, 1280×850](scanner-renovado/before-1280-light-es.png)
- [Side-by-side comparison](scanner-renovado/comparison.png)
- [Current map, 1280×850](scanner-renovado/after-1280-light-es-a.png)
- [Current list, dark theme](scanner-renovado/after-1280-dark-es-b.png)
- [Active scan with list](scanner-renovado/scanning-1280-light-es-b.png)
- [Settled partial result](scanner-renovado/partial-1280-light-es-b.png)
- [Narrow list, 700×600](scanner-renovado/after-700-light-es-b.png)
- [Reference manifest](scanner-renovado/before-manifest.csv) and
  [current manifest](scanner-renovado/after-manifest.csv)

Contact sheets and full-size examples were inspected separately from dimension
checks. The final toolbar labels fit at 700 points; only the breadcrumb contracts.
Partial values retain their numerical lower bound, percentage headers carry
their partial marker, progress shows the entry count, and disclosure indicators
render without missing glyphs. Light-theme working/partial/failure text uses
the theme's darker text roles (contrast against white: 5.70/5.45/6.97).
This is not a certification of every interaction or accessibility scenario.

## Independent review and regressions

The independent reviews covered geometry/cache identity, service concurrency
and UI command targets. They closed stale context/deferred-command targets,
row keyboard focus, metric-transition labels and a real supervisor-readiness
race. A branch launch could move from the inbox to the reserved slot between
separate observations, falsely appearing settled. Readiness now observes the
status, inbox and pending output under one brief mutex guard. A deterministic
test covers that handoff and undrained terminal results. No I/O is added under
the guard and no public API changes. This last synchronization correction does
not change the renderer; the native matrix above precedes it, while the final
automated gates and measured binaries include it.

Coverage includes adaptive depths 0/1/2/8+, grouping and hidden-branch scale
stability, hit-test boundaries, cooperative cancellation, stable color families,
overlay cache reuse, double-click suppression, folder/file activation, row focus,
stale generations/revisions, rapid metric changes with different unknown sizes,
sorting/search beyond 600 rows, and compatible list preferences. Existing
confirmation, action and reconciliation tests remain in the workspace suite.

Final automated gates passed on Windows/MSVC with locked dependencies:

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed without warnings |
| `cargo test --workspace --all-features --locked` | 554 unit/integration tests and 1 rustdoc test passed |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked` | Passed |
| `cargo build --release -p diskpie --locked` | Passed |

The eight ignored entry points comprise six subprocess helpers (their parent
tests run in the suite), the opt-in frame benchmark and the native screenshot
test. The latter two run separately. The final executable identity is recorded
in [checks.json](scanner-renovado/checks.json); complete local command logs are
under `target/design-evidence/final-*.txt`. These checks do not publish a release.

The HTML design proposal uses fictional data and is not evidence of the native
application. Preliminary captures and failed fixture runs remain under
`target/design-evidence/` for diagnosis and are not the final matrix.

## Performance

See the [performance report](../../tools/diskpie-bench/DESIGN_REPORT.md), generated from seven interleaved runs
per variant by `scripts/summarize-design.py`. The before and after variants use
the same CPU-frame benchmark body. Layout uses flat, balanced and deep trees
at 100,000 and 1,000,000 requested entries. Hashes, hardware, configuration,
individual frames, process samples and layout results accompany the report.

The frame measurements are headless CPU measurements at 1280×800 points with
embedded fonts. They exclude native Segoe UI, GPU rasterization, compositor and
vsync; they are not FPS or a general scanner-speed claim. The adaptive-depth
traversal and eight-byte color-family index are deliberate additional work.

The completed dataset contains 33,684 measured frames, 84 layouts and 126
processes. It shows no uniform frame-CPU improvement. Balanced-tree layout at
one million entries increases from 0.662 to 32.075 ms (+31.413 ms), off the UI
thread; the new depth prepass happens before early grouping. The report retains
this regression, all other timings, sampled process memory/handles and the
limits of interpreting those samples. An independent review verified the CSV
counts/statistics and source/executable hashes against the build manifest.

## Limits

Native fixtures do not exercise the Windows folder picker, Explorer integration,
file actions or diagnostic destination dialogs. Existing contract tests cover
their wiring and safety policies. Narrator, Windows 10/11 VM coverage and
physical mixed-monitor DPI transitions remain separate manual validation.
The real Recycle Bin is not emptied; that evidence still requires a dedicated
disposable environment. The proprietary reference application is neither
modified nor included in these artifacts.
