# DiskPie roadmap and continuation handoff

> Updated 2026-09-07 for deliveries 1–3, Scanner renovado and independent-review remediation:
> reliable results, connected product workflows, a map with an optional list,
> and measured performance work. This is development status, not a
> release announcement. Release preparation and publication are outside this
> delivery; no tag, workflow, or publishing action is authorized by this handoff.

## Product contract

DiskPie is a clean-room, Windows-first disk usage analyzer written in stable
Rust with egui/eframe. The eventual first distribution is a portable Windows
10/11 x86-64 executable. The acceptance source remains
[the legacy analysis](docs/legacy-scanner-analysis.md), interpreted through the
[requirements matrix](docs/requirements-traceability.md) and
[architecture decisions](docs/adr/README.md).

The proprietary Scanner artifacts in the parent directory must never be moved,
modified, copied into this repository, committed, or distributed. Preserve
concurrent work and inspect `git status --short` before editing; this document
makes no claim that the current working tree is committed or clean.

## Implemented development scope

| Area | Current behavior | Evidence and remaining limits |
| --- | --- | --- |
| Reliable scan results | Pending directories remain incomplete; isolated root resolution errors become omissions while valid roots continue. | Session, coordinator, and resolver tests; no claim that partial scans observed inaccessible contents. |
| Branch rescan | Settled snapshot intent, isolated enumeration, join-before-merge, whole-tree re-aggregation, one coherent commit. Cancel/failure preserves the prior frame. | Runtime and branch merge tests, including stale generations and hard links crossing the branch boundary. Merge is O(total nodes) and may retain multiple trees. |
| Sunburst and navigation | The map is the default workspace. Effective positive-weight depth fills the available rings; native-path color families stay stable through zoom, sorting and rescans. Click enters a folder or selects a file, center goes up, Enter activates and Space selects. | Core layout/property, renderer, presentation and navigation regressions. The compact family index adds 8 bytes per node; native visual/accessibility evidence is tracked separately. |
| Textual companion | Show list opens the complete searchable, virtualized immediate-child list. Name/Size and ascending/descending order run off-thread; default order is Size descending. Below 800 points the control switches between map and list. | Shared comparator and binary-search lookup, 640-row sort/filter tests, a 600-row real-focus keyboard test, stale-result and stale-navigation regressions. |
| Item actions | Open/reveal, confirmed recycle and strong permanent delete, result classification, refresh obligations and stale-results handling. | Pure confirmation/receipt tests and native adapter fixtures. Recycle is enabled only for proven fixed local NTFS; wider provider validation remains pending. |
| Recycle Bin / Installed Apps | Advisory query, strong empty confirmation, exact scope and uncancellable-call explanation; fixed Installed Apps URI. | Fake/marshalling coverage. Real empty-bin execution is reserved for a disposable VM and is not run against the user's bin. |
| Explorer integration | Tools preferences asynchronously inspect actual HKCU state and offer owned install/repair/remove. | Registry policy and isolated native registry tests. Windows 10/11 Explorer menu VM checks remain pending. |
| Diagnostics export | Exact redacted preview, per-export path consent, native Save As, actual transport preflight, retained single-use destination token, create-only publication. | Native race/alias/bounded-source tests and support/UI tests. Existing destinations require another name; no upload client exists. |
| Settings and recovery | Retained-handle `app.ron`, bounded daily logs, previous-panic notice and receipt-based clean-shutdown proof. Additive list preferences default to hidden and 300 points, clamped to 260–400; System theme, Allocated metric and existing preferences are preserved. | Old-document defaults, validation and explicit-save tests; list and support joins participate in shutdown evidence. |
| Localization | 188 typed messages in both English and Spanish cover connected workflows, the optional list and partial/unverified Explorer states. | Completeness/argument tests; command-line help remains English. |
| Performance | Reproducible isolated harness, conventional-provider baseline, bounded/cancellable layout and list work, batch-enumeration experiment outside production. Redesign comparisons use the pre-redesign working tree with deliveries 1–3 already present. | See [benchmark harness](tools/diskpie-bench/README.md) for reproduction and [redesign validation](docs/research/scanner-renovado-validation.md) for evidence status. Compiled source/binary manifests guard the seven-run comparison; smoke data is not acceptance evidence. |

## Verification evidence

The [review remediation record](docs/research/review-remediation-validation.md)
is the current source for the 2026-09-07 fixes, regression coverage and new
measurements. It covers directory-handle identity, cancellation/publication,
failed-root retries, Explorer recovery, truthful last-scan diagnostics, startup
receipts, indexed depth/roots and bounded export work. Historical checks below
and the redesign reports remain evidence only for their recorded sources.

The following checks were recorded for deliveries 1–3 before the redesign.
They do not certify subsequent UI changes. The current redesign's exact checks,
native captures, limitations and performance evidence are recorded in
[Scanner renovado validation](docs/research/scanner-renovado-validation.md).

- Focused platform run: 160 unit tests passed, followed by the added deterministic
  final-interval export alias test passing (161 platform unit tests now present).
- Integrated `cargo clippy --workspace --all-targets --all-features --locked --
  -D warnings`, `cargo test --workspace --all-features --locked`, and workspace
  rustdoc with `RUSTDOCFLAGS=-D warnings` passed after the review fixes. The binary
  suite includes 129 tests and 6 subprocess helpers ignored by design.
- Four gated native tests passed with `DISKPIE_DESTRUCTIVE_FIXTURES=1`: recycle
  a self-created file and directory, permanently delete a self-created file,
  and delete a self-created directory link while preserving its external
  sentinel. No real Recycle Bin empty operation ran.
- Final optimized-build, format/diff and native visual results for the redesign
  belong to the linked validation record. Earlier 2026-09-04 checks are also
  historical and are not proof for the current changes. Headless tests do not
  substitute for a native visual or accessibility review.
- Still pending: clean Windows 10/11 VM launches, Explorer-menu verification,
  Narrator, mixed-DPI/multi-monitor checks, manual preference restart, OneDrive
  Files On-Demand, and the disposable Recycle Bin/provider matrix.
- Do not execute real Empty Recycle Bin during routine validation. It can delete
  unrelated user data and belongs only in the documented disposable VM matrix.
- No release preparation, tag, public release, or workflow change is part of
  this delivery. Existing packaging infrastructure remains available for a
  separately authorized release phase.

## Invariants for future changes

- All filesystem, COM, registry, expensive ranking/layout, and resource disposal
  work stays off egui callbacks. Submit and drain bounded messages only.
- Snapshot, layout, navigation, and presentation must belong to one committed
  generation/revision. Exact native paths are identity; display strings are not.
- One coordinator schedules fixed scan workers. Cancellation does not claim that
  pending directories are complete, and denied access does not abort siblings.
- A retained native directory supplies both enumeration and child metadata;
  full rescans retain the original native selection, including unresolved roots.
- Positive depth by metric is computed during aggregation, independently of
  hiding/grouping. Availability reads indexed roots, not the complete arena.
- Runtime retirement keeps its 64 ordinary slots plus one reserved teardown slot
  occupied until disposal returns. Hidden-state deltas retain the 64-depth bound.
- Settings and marker adapters use retained local handles and classify native
  writes from actual identity evidence. Clean shutdown requires joined receipts
  for every owned service, including list and support workers.
- Recycle/delete capabilities are exact-target, single-use, and purpose-bound.
  Every attempt retains a reconciliation obligation; uncertain outcomes never
  become successful deletion. A recycle safety violation stops later mutations.
- Export remains at most 8 MiB, secret-redacted, and path-free by default. Native
  preflight creates no file, and interactive publication never replaces an
  existing name. Source logs/settings/crash files are protected, including aliases.
- UI-owned service Drop must not wait for worker completion. Bounded finish
  retains timed-out ownership in an existing bounded reaper; registration
  failure fails closed. Internal synchronous joins belong on background
  teardown paths.
- All destructive tests act only on temporary fixtures that the test created.
  Never construct interpreter commands from untrusted paths.

[ADR 0020](docs/adr/0020-complete-product-composition.md) records branch merging,
background list/support composition, and the interactive create-only export
refinement of ADR 0016. [ADR 0021](docs/adr/0021-scanner-renovado.md) records the
map-first redesign, adaptive radial depth, stable color families and additive
list preferences.
[ADR 0022](docs/adr/0022-retained-directory-enumeration.md) records retained
Windows directory enumeration; the ADR 0021 addendum records depth indexing.

## Remaining work, in order

1. Complete the manual Windows 10/11, accessibility, DPI, cloud-provider, and
   disposable-VM action checks. Record failures against the specific requirement;
   passing portable tests is not evidence for an untested Windows provider.
2. Broaden the recorded seven-run CPU comparisons with full-frame, worker
   handover, memory, first-result and provider measurements to
   decide the next optimization. Keep the experimental filesystem backend out of
   production until semantics and repeatable measurements support adoption.
3. In a separately authorized phase, verify portable packaging and clean-machine
   release behavior, run the release security/license gates, prepare version and
   release notes, and publish reviewed artifacts with SHA-256 evidence.

There is no percentage-complete estimate: implementation, automated evidence,
manual acceptance, performance evidence, and release publication are tracked
separately in the requirements matrix.
