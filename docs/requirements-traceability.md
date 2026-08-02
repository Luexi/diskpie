# Requirements traceability

This living matrix translates the acceptance criteria in
`legacy-scanner-analysis.md` into verifiable product work. A feature is only
complete when its evidence column names an automated check, a documented manual
check, or both.

| ID | Requirement | Planned evidence | Status |
| --- | --- | --- | --- |
| F-01 | Select and scan a folder or drive | Scanner integration tests; Windows smoke test | Planned |
| F-02 | Present a summary of multiple drives | Domain aggregation tests; UI smoke test | Planned |
| F-03 | Render a proportional sunburst | Layout unit and snapshot tests | Planned |
| F-04 | Show path, selected size, both sizes, and file count on hover | Hit-test tests; UI smoke test | Planned |
| F-05 | Zoom by clicking a segment | Navigation state tests | Planned |
| F-06 | Navigate back and to the parent folder | Navigation history tests | Planned |
| F-07 | Hide and restore chart branches | Visibility/pruning tests | Planned |
| F-08 | Rescan an entire selection or one branch | Scan-generation integration tests | Planned |
| F-09 | Stream partial results and useful progress | Event-protocol tests; first-result benchmark | Planned |
| F-10 | Cancel promptly without freezing the UI | Cancellation integration test and benchmark | Planned |
| F-11 | Open files and folders with the platform shell | Windows adapter test; manual smoke test | Planned |
| F-12 | Recycle a confirmed item | Temporary-fixture Windows test; manual confirmation test | Planned |
| F-13 | Permanently delete only after strong confirmation | Action-policy unit tests; temporary-fixture test | Planned |
| F-14 | Empty the recycle bin after confirmation | Windows adapter test; manual smoke test | Planned |
| F-15 | Open modern Installed Apps settings | Windows adapter unit/manual test | Planned |
| F-16 | Accept an initial path through the CLI | CLI parsing and launch integration tests | Planned |
| F-17 | Write logs and export local diagnostics | Redaction/export tests | Planned |
| F-18 | Add/remove optional Explorer integration | Idempotence and rollback tests; Windows smoke test | Planned |
| F-19 | Persist preferences | Settings round-trip and corrupt-file recovery tests | Planned |
| F-20 | Provide original English and Spanish strings | Locale completeness test; UI smoke test | Planned |
| FS-01 | Switch between logical and allocated size | Aggregation tests; UI mode test | Planned |
| FS-02 | Show both sizes in details | Presentation-model tests | Planned |
| FS-03 | Define and enforce hard-link counting semantics | ADR; file-ID fixture tests | Planned |
| FS-04 | Avoid link/reparse loops and tree escape by default | Junction/symlink fixture tests | Planned |
| FS-05 | Handle sparse and compressed allocation | Windows sparse/compression tests | Planned |
| FS-06 | Avoid unintended cloud-placeholder hydration | Attribute/policy tests; documented manual check | Planned |
| FS-07 | Continue after inaccessible entries | Permission fixture tests | Planned |
| FS-08 | Define mount-point and volume-crossing policy | ADR; adapter tests | Planned |
| UX-01 | Resize freely and remain per-monitor DPI aware | Manifest/build inspection; manual Windows test | Planned |
| UX-02 | Follow system theme and allow light/dark override | Settings and visual smoke tests | Planned |
| UX-03 | Offer keyboard navigation and a textual companion view | Accessibility review and manual test | Planned |
| Q-01 | Keep format, clippy, and tests clean | Local quality script and CI | Planned |
| Q-02 | Cover critical logic with unit/integration/property tests | Test inventory in release evidence | Planned |
| Q-03 | Benchmark scan throughput, first result, total time, memory, cancellation, layout/render, and binary size | Reproducible benchmark report | Planned |
| Q-04 | Audit dependency security and licenses | CI audit jobs and release audit report | Planned |
| R-01 | Produce a portable Windows x86-64 executable | Release workflow and local artifact validation | Planned |
| R-02 | Publish a public repository with green CI | GitHub repository and Actions links | Planned |
| R-03 | Publish a stable release with SHA-256 and verified limitations | GitHub release and checksum asset | Planned |

Status values are `Planned`, `Implemented`, `Verified`, or `Blocked`. A blocked
row must link to evidence and an explicit product decision.
