# ADR 0014: Parse the startup path with lexopt and preserve native strings

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie may receive an optional scan root at process startup. Windows paths
are native UTF-16 and can contain units that are not valid Unicode. A parser
that requires `String`, logs the raw command line, normalizes the path, or
silently accepts multiple roots violates the scanner's exact-path contract.

The initial grammar has zero subcommands and only one optional positional
path. Detailed candidate evidence is recorded in
[app infrastructure research](../research/app-infrastructure.md#startup-cli-and-native-path-preservation).

## Decision drivers

- Lossless `OsString` to `PathBuf` handling.
- A visible and deterministic zero-or-one-path grammar.
- Correct `--`, help/version, unknown-option, and excess-argument behavior.
- Small, maintained, auditable, and unsafe-free dependency surface.
- Structured UI errors that do not expose raw arguments in logs.
- No normalization policy hidden inside argument parsing.

## Options considered

### lexopt 0.3.2

lexopt reads `std::env::args_os`, yields positional values as `OsString`, has
zero dependencies, forbids unsafe code, and leaves the small grammar visible
in application code.

### pico-args 0.5.0

pico-args can preserve OS strings through its OS-string callbacks and is also
small, but its current release dates to 2022 and upstream activity stopped in
2023. It provides no material advantage over lexopt.

### clap 4.6.5

clap is active and correctly supports OS-native values. Its builder/derive,
generated help, completions, styling, and suggestion surface are broader than
DiskPie's one positional path.

### Handwritten `std::env::args_os` parsing

A handwritten parser avoids a dependency but must reproduce option
termination, help/version, unknown options, missing values, and consistent
diagnostics. The maintenance saving is negligible at this grammar size.

## Decision

DiskPie will parse startup arguments with exact lexopt 0.3.2.

1. The production entry point starts from `Parser::from_env`; tests use the
   iterable constructor. Filesystem values remain `OsString` and move directly
   into `PathBuf`.
2. The supported grammar is `diskpie.exe [--] [PATH]`. Zero paths starts the
   normal selection flow; one path becomes the initial scan request. Multiple
   positional values are usage errors.
3. Unknown options fail. `--` terminates option parsing so an option-like path
   is representable. Help and version are fixed ASCII protocol options.
4. Argument parsing performs no UTF-8 conversion, display-string round trip,
   environment expansion, globbing, trimming, canonicalization, existence
   check, separator rewrite, or extended-prefix removal.
5. Parsing occurs before UI construction and returns a plain application
   request or a structured usage error suitable for the GUI. Diagnostics can
   record a stable error code and argument count, never raw argument values.
6. pico-args is rejected for the current baseline. clap is deferred until
   subcommands, generated documentation, or completions justify it. If clap is
   later adopted, positional paths must remain OS-native and its features must
   start from `default-features = false`.

## Consequences

Positive outcomes:

- Every native Windows path representable by `OsString` remains representable
  at the scanner boundary.
- The accepted command-line surface is obvious and small.
- The dependency adds no transitive crates or unsafe code.
- Usage errors can be tested without reading process-global arguments.

Accepted tradeoffs:

- DiskPie does not gain clap's automatic completions, rich help layout, or
  derive API. None is required for a one-path GUI launcher.
- The parser intentionally does not prevalidate the filesystem. Scan policy
  and platform errors occur at their existing application/adapter boundaries.
- Help text remains application-authored and must be kept aligned with the
  small grammar.

## Validation

- Tests cover zero and one path, multiple values, `--`, unknown options,
  help/version, and sanitized error output.
- Windows tests build `OsString` with `OsStringExt::from_wide`, including an
  unpaired surrogate, and assert native-unit equality after parsing.
- Fixtures cover emoji, spaces, `%`, `&`, drive and UNC roots,
  extended-length prefixes, trailing separators, and option-like names.
- Dependency and source checks assert lexopt remains zero-dependency and
  unsafe-free at the locked version.
- Log/export tests assert the raw argument bytes/units and display form are
  absent from diagnostics.

## Revisit triggers

- Adopt clap only when real subcommands, completions, generated CLI docs, or a
  substantially larger option grammar are approved.
- Reevaluate lexopt if it stops accepting OS-native positional values, gains a
  materially larger dependency/unsafe surface, or becomes unmaintained.
- A multi-root CLI requires a new product grammar decision; it must not emerge
  from silently accepting extra positional values.
