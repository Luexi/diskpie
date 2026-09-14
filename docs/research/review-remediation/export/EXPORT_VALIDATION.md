# Export correctness and measured work — 2026-09-07

Archive note: original focused-test logs, source snapshots, libraries, binaries
and fixtures referenced below remain under
`target/actions-fixes-20260907-aux01/`. This documentation copy preserves only
the report, measurement CSV/output and helper/script.

The implementation fixes the three reviewed composition errors: partial Explorer removal retains mutation progress and triggers fresh inspection, recoverable partial installation enables Install/Repair, and diagnostic values not measured by the runtime are explicitly unknown.

## Correctness checks

- `cargo test -p diskpie-app --all-features --locked`: 181 unit tests and 1 doctest passed (`app-tests.txt`).
- `cargo test -p diskpie --all-features --locked support_`: 10 passed (`support-tests.txt`).
- Four focused native/reader tests passed (`collection-tests.txt`, `tail-tests.txt`, `collection-identity-test.txt`). They cover recent-source selection and aliases, the partial-prefix tail contract, exactly 1 MiB read after one seek despite later appended bytes, short-read refusal, and replacement after directory enumeration.
- The root subsequently completed the workspace format, clippy, test, rustdoc and release gates. The first local clippy failure was a redundant slice in a new test, corrected before the root gate; subsequent local failures belonged to concurrent native filesystem changes and were corrected by their owner.

The suffix algorithm is tested against a deliberately simple all-lines/forward reference at several byte budgets, with short lines, source boundaries, unreadable sources, invalid UTF-8, redaction, truncation and incomplete final records. The exact prefix length is compared with actual rendering across numeric-width boundaries and all notice combinations. Golden output and preview/artifact allocation identity remain tested.

Schema 2 replaces misleading session-like counters with `[last_scan]`: `outcome`, `generation`, `items_observed`, `bytes_observed`, `scan_omissions`, and `scan_duration_ms`. Optional values serialize as `unknown`; zero is reserved for a supplied zero measurement. The current composition can supply generation, outcome and entry count. It cannot supply bytes, elapsed time or total omissions: scanner progress excludes root-resolution omissions and a branch's merged display has a different scope.

## Seven-run comparison

The same `export-native-perf.rs` helper was compiled twice. It calls the actual Windows `collect_diagnostic_logs` on eight self-created native fixture files, verifies the collected bytes equal the files and that every source is at most 1 MiB, and then times only `build_export`. File collection, validation, file reads and post-build assertions are outside the timer.

Each measurement used a new process. Before/after order alternated across seven runs. Compilation completed before timing. The root and other agents suspended builds, tests and benchmarks during this exclusive measurement window. Raw data are preserved in `export-seven-runs.csv` and `export-seven-runs-output.txt`.

| Input | Sources | Bytes/source | Total bytes | Complete lines | Before median | After median | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Synthetic structured events | 8 | 991,250 | 7,930,000 | 65,000 | 278.898 ms | 202.734 ms | −27.31% |
| Newline-only stress input | 8 | 131,072 | 1,048,576 | 1,048,576 | 1,186.860 ms | 30.639 ms | −97.42% |

The newline-only fixture is pathological; normal typed logging does not produce it. Both versions included all eight sources and exactly the same recent-log bytes, without omissions. The artifact is 18 bytes smaller in the new version because schema 2 corrects the metadata fields. This is a comparison of the complete old/new pure export function, not an isolated causal estimate for one changed function.

These timings are neither FPS nor whole-application latency. No process-memory measurement was taken and no causal memory saving is claimed. Code now retains bounded suffix bytes plus only a header-sized metadata margin, instead of one retained record per input line. The separate reader test proves the byte-read bound; the comparison above does not measure filesystem latency or the seek optimization.

## Source and binary provenance

Both helpers use Rust 1.97.1 (`8bab26f4f`, 2026-07-14), `--edition 2024 -C opt-level=3 -C target-feature=+crt-static`, and the locked workspace release dependencies. The before app/platform libraries were the pre-fix libraries already verified and reused by the root's initial release gate; the after libraries came from the successful workspace release gate completed before this measurement window. The original working tree includes deliveries 1–3 and the Scanner renovado design, not an earlier HEAD-only substitute. Later changes to other subsystems require their own validation; these frozen helpers identify exactly the code timed here.

The before export source was recovered with its exact SHA-256 recorded before any implementation changes. `export-before-source.rs` and `export-after-source.rs` preserve the compared export implementations; the platform after source is also retained. The helper remains unchanged between its two compilations.

| Artifact | SHA-256 |
| --- | --- |
| Before export source | `8A620F8C0783817EAAC6785B9B5C454F6F9264A562E98E290ABC969090C293D9` |
| After export source | `898F70892EF19C61D684E93378830EBC121ECB08F0EC668605CF7A5AA888FF33` |
| After diagnostic file source | `C1A21571BE9214323F1241AD69E404E87184256D7F55B48868E7093595BC0671` |
| Shared native benchmark source | `8DABEF883E3BE4B28C3A9DBCFF8B5F8D926F5A1CA8F0C60E491EA5CE5489BA5E` |
| Before app rlib | `0C1E30723A882B501209D15A324929CDCA053902F2A176750C7717F494F6E2D6` |
| Before platform rlib | `A2B9A265EA8496B2977ACB94D6A4339D1B7C9A202517408719179B0A64FE2E3F` |
| After app rlib | `025A3BFA9AEB16F56E66845CEE0095CAE8BD332A829396C12065FD32926A55DB` |
| After platform rlib | `345905CC8800F56DD00FFDEFDBE47DAB68DEA35A4F50E9BA2F833D1B19ED9583` |
| Before native helper exe | `71FACB09866EB905699A7C89BA1052B235A5AA0D058D34F3E89256721A9A750D` |
| After native helper exe | `897154874645067BB4CC0C3E0917C6B122DA9ABDC68AE14C2F13DC90A5080407` |
| Each event fixture file | `AB9A84A5898A1CEC2E4EA168ADCB2D67B793D7E8A3D498226EC55E03225711B4` |
| Each blank fixture file | `23E126F09A5B7B481ED7732E865E0EF57D5C62053599530FBD7D9B413F863185` |

Before compilation used `libdiskpie_app-fecde2c896390bdd.rlib` and `libdiskpie_platform-6664b09206be1fe9.rlib`; after compilation used `libdiskpie_app-fecde2c896390bdd.rlib` and `libdiskpie_platform-9d5ca7bffdca65e9.rlib`. The app filename is reused by Cargo, so its hash, rather than the filename or modification date alone, identifies each version.

The earlier non-native helper executables in this evidence directory were preparation artifacts and were not used for this final comparison. No Explorer registry entries, user preferences, real Recycle Bin contents or proprietary Scanner artifacts were touched by this work.
