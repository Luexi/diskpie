# Windows filesystem scanning research

- Research date: 2026-08-02
- Target: Windows 10/11 x86-64, portable executable, standard user
- Outcome: adopt a capability-driven conventional Win32 backend; defer any
  NTFS/MFT backend until it passes the gate in
  [ADR 0005](../adr/0005-ntfs-fast-backend-gate.md).

## Recommendation

Implement a portable scan coordinator over a small Windows adapter. The
correctness path enumerates with `FindFirstFileExW`/`FindNextFileW`, obtains
allocation and 128-bit identity through handle-based metadata when needed, and
never follows directory reparse points by default. Runtime-probe the faster
`FileIdExtdDirectoryInfo` information class per volume and fall back on any
unsupported or inconsistent response. A failure to obtain metadata is an
explicit omission, never an invented zero.

Use `windows` 0.62.2 for generated Microsoft bindings. Use the bounded
standard-library channel topology accepted in ADR 0001 for the initial
scheduler; retain `crossbeam-channel` 0.5.16 as a benchmark-triggered
alternative. Do not add a walk crate, Rayon, a file-ID convenience crate, or an
NTFS/USN parser to the initial scanner.

## Correctness contract

### Sizes

- **Logical size** is the end-of-file of the unnamed/default data stream for
  each visible file directory entry. A hard-linked file contributes at every
  path in logical mode.
- **Allocated size** is the filesystem-reported `AllocationSize` of that
  unnamed stream. Directories contribute zero. Allocation is deduplicated once
  per `(volume identity, 128-bit file identity)` across all roots in one scan.
- Alternate data streams, directory metadata, the MFT/journal, security
  descriptors, and other filesystem overhead are outside both totals.
- Sparse, compressed, and resident files are represented by the filesystem's
  allocation value; do not round logical length to cluster size.
- ReFS block cloning/deduplication means this number is a filesystem-reported
  charge, not a promise of exclusively reclaimable physical sectors. A hard
  link outside the selected roots likewise means the within-scope unique total
  is not guaranteed reclaimable.
- Aggregate with `u128` internally and clamp/format only at presentation or
  serialization boundaries.

Each node and scan exposes known logical bytes, known allocated bytes, unknown
entry counts, omission reasons, and a completeness state (`Complete`,
`Partial`, or `Cancelled`). Unknown is never folded into zero.

### Deterministic hard-link ownership

Maintain a scan-wide identity table. The first observed path is a provisional
allocated-size owner. The final owner is the lexicographically smallest raw
UTF-16 path key, using sorted selection-root key plus relative path; if a
smaller path arrives later, transfer the allocation delta from the old owner's
ancestor chain to the new one. Aliases retain logical size and file count but
receive zero allocated contribution. This makes the final chart independent of
worker order while still allowing partial updates.

When stable IDs are unavailable, count the reported allocation per directory
entry and mark `hard-link deduplication unavailable` on the affected subtree.
That conservative fallback must not be presented as a unique allocation total.

See [ADR 0002](../adr/0002-size-and-hard-link-semantics.md).

## Windows adapter design

### Paths and names

Keep paths as `PathBuf`/`OsString` and call wide APIs. Preserve returned UTF-16
code units, including names that cannot be converted to Unicode scalar values;
lossy strings are display-only and never identity keys. Make each selected root
absolute once without resolving links, then build descendants from returned
names. Use extended syntax (`\\?\` and `\\?\UNC\`) at the adapter boundary.
Embed `longPathAware` in the executable manifest, but do not depend on machine
registry policy for correctness. Do not canonicalize scan entries because that
can traverse reparse points.

Microsoft documents the opt-in and extended-path rules in
[Maximum Path Length Limitation](https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation).

### Per-volume capability record

Open/probe each root once and cache:

- a stable volume key, filesystem name, serial, and flags from
  [`GetVolumeInformationByHandleW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumeinformationbyhandlew);
- drive/remoteness from `GetDriveTypeW` and whether stable file IDs and the
  extended directory-information class work in practice;
- cluster context from
  [`GetDiskFreeSpaceW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getdiskfreespacew),
  and capacity/free space from
  [`GetDiskFreeSpaceExW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getdiskfreespaceexw);
- optionally, seek-penalty information from `IOCTL_STORAGE_QUERY_PROPERTY` as
  a scheduling hint only.

Capabilities, not filesystem-name assumptions, choose the code path. NTFS gets
the full path. ReFS should use 128-bit IDs but receives the same caveats about
block sharing. FAT/exFAT and removable media use whatever metadata the runtime
probe supports. SMB starts with concurrency one, treats IDs and allocation as
server-reported capabilities, and never attempts an MFT path. Unknown
filesystems use the baseline and surface missing semantics.

### Conventional enumeration tiers

1. **Portable-correct baseline.** Enumerate with
   [`FindFirstFileExW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfileexw)
   using `FindExInfoBasic` and, when accepted, `FIND_FIRST_EX_LARGE_FETCH`, then
   [`FindNextFileW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findnextfilew).
   `FindExInfoBasic` avoids short-name lookup. Treat only `ERROR_NO_MORE_FILES`
   as successful completion.
2. **Metadata.** For ordinary files requiring data not returned by enumeration,
   open a non-inheritable metadata handle with desired access zero, all of
   `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`, `OPEN_EXISTING`,
   and `FILE_FLAG_OPEN_REPARSE_POINT` (plus `FILE_FLAG_BACKUP_SEMANTICS` for a
   directory). Query
   [`FILE_STANDARD_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_standard_info)
   for EOF/allocation and
   [`FILE_ID_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info)
   for volume serial plus `FILE_ID_128`, via
   [`GetFileInformationByHandleEx`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getfileinformationbyhandleex).
   The zero-access open permits some metadata queries even where generic read
   would fail; any query that still fails becomes a typed omission.
3. **Batch fast path.** Prototype `GetFileInformationByHandleEx` with
   `FileIdExtdDirectoryInfo`, whose
   [`FILE_ID_EXTD_DIR_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_extd_dir_info)
   records include EOF, allocation, attributes, reparse tag, and a 128-bit ID.
   Microsoft's structure page and lower-level
   [`FILE_INFORMATION_CLASS`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ne-wdm-_file_information_class)
   availability table are not fully consistent about supported clients.
   Therefore runtime-probe it per volume, validate record boundaries and names,
   cache the result, and fall back on `ERROR_INVALID_PARAMETER`,
   `ERROR_NOT_SUPPORTED`, malformed data, or semantic mismatch.

Do not use
[`GetCompressedFileSizeW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getcompressedfilesizew)
as the general allocation primitive. Microsoft says ordinary non-compressed,
non-sparse files return their ordinary file size, and the function follows a
symbolic-link target. It may be a narrowly documented fallback only for a
non-reparse compressed/sparse file when standard allocation metadata is
unavailable.

All Win32 handles and buffers live in a small reviewed `unsafe` adapter with
RAII close behavior, checked structure offsets, checked integer conversions,
and no pointers escaping a call.

## Reparse points and cloud placeholders

Use `WIN32_FIND_DATAW.dwFileAttributes` and, for reparse entries,
`dwReserved0`/the batch record's tag before opening anything. By default:

- never enqueue a directory with `FILE_ATTRIBUTE_REPARSE_POINT`, including a
  junction, mount point, symlink, or unknown tag;
- retain the directory entry as a skipped boundary with its tag and reason;
- retain a file reparse entry but never follow its target for size or identity;
- derive Cloud Files state without opening the item using
  [`CfGetPlaceholderStateFromAttributeTag`](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfgetplaceholderstatefromattributetag);
- do not metadata-open an offline, recall-on-open, or partially-on-disk file;
  use enumeration metadata if complete, otherwise mark allocation unknown; and
- do not enumerate a directory carrying `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`,
  because enumerating children can fetch remote metadata. Report it as cloud
  content not enumerated.

Relevant contracts are
[`WIN32_FIND_DATAW`](https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-win32_find_dataw),
[reparse points](https://learn.microsoft.com/en-us/windows/win32/fileio/reparse-points),
[symbolic-link effects](https://learn.microsoft.com/en-us/windows/win32/fileio/symbolic-link-effects-on-file-systems-functions),
[file attributes](https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants),
and [`CF_PLACEHOLDER_STATE`](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_placeholder_state).
An optional future "follow links" mode requires explicit consent, a
directory-identity cycle set, a stay-on-volume option, and separate tests; it is
not part of the initial backend. See
[ADR 0003](../adr/0003-reparse-and-cloud-policy.md).

## Bounded scheduling and cancellation

The coordinator owns a directory queue and outstanding-work count. A fixed set
of dedicated blocking workers receives at most about twice the active worker
count in dispatched directories. Initial per-volume caps are conservative:
one worker for network/removable media, two for unknown/rotational media, and up
to four for a measured low-seek-penalty local volume, all bounded by a global
limit. Benchmarks may tune these values.

Workers send batches, not per-file events, through a bounded result channel
(start with 8-16 batches). Flush after 512-2,048 entries or 16-33 ms, whichever
comes first. Backpressure bounds memory. The UI consumes coherent generation-
tagged deltas/snapshots and performs no filesystem I/O.

Each scan has a monotonically increasing generation and atomic cancellation
token. Check before every directory operation and at least every 64 entries.
Cancellation stops dispatch and publication immediately, emits a `Cancelled`
partial result, and causes the UI to ignore late events from older generations.
[`CancelSynchronousIo`](https://learn.microsoft.com/en-us/windows/win32/fileio/canceling-pending-i-o-operations)
may be attempted against dedicated worker threads to shorten a blocked call,
but is best-effort and never a reason to terminate a thread. A slow or broken
network provider may delay worker cleanup even though UI cancellation is
immediate; expose that limitation in diagnostics and never accumulate
unbounded replacement workers.

See [ADR 0004](../adr/0004-bounded-scan-scheduling.md).

## Error and race behavior

Every directory batch carries typed omissions such as access denied, vanished
entry, sharing violation, unsupported metadata, disconnected volume, or cloud
boundary. Enumeration failure after some entries makes that directory partial.
Races between enumeration and metadata lookup are expected and do not abort
siblings. Root failure is a failed root inside the overall scan, not a process
failure. Handle closure is deterministic on success, error, cancellation, and
panic boundaries.

## MFT and USN assessment

Do not integrate either mechanism initially. Raw volume/MFT access commonly
conflicts with the standard-user/no-elevation requirement. USN journals can be
disabled, deleted, recreated, or trimmed, and a live scan is not a stable
snapshot. More importantly,
[`FSCTL_ENUM_USN_DATA`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data)
and
[`USN_RECORD_V2`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2)
provide identity, parent, name, reason/attributes, and timestamps, but not the
logical and allocated sizes DiskPie must report. Opening every returned file to
recover those values may erase the speed advantage.

If researched later, expose `FastBackend::try_open` behind the same protocol.
Access denied, unsupported filesystem, absent/unstable journal, remote source,
or removable media must select the conventional backend without prompting for
elevation. The full acceptance bar is in ADR 0005.

## Dependency evaluation

### Rubric

Every candidate below is evaluated at the exact baseline available on the
research date against all repository requirements:

1. version/API baseline and research date;
2. license compatibility with `MIT OR Apache-2.0`;
3. maintenance and recent release activity;
4. advisories, unsafe-code footprint, and security boundary;
5. Windows 10/11 x86-64 and portable-core compatibility;
6. compile-time, dependency-graph, and binary-size impact;
7. measurable value over a small local implementation; and
8. abandonment/lock-in risk plus a concrete fallback.

Advisory conclusions must be repeated against the resolved `Cargo.lock` with
the repository's audit tooling before merge and release; registry metadata is
not a substitute for that check.

### Microsoft bindings

#### `windows` 0.62.2 — adopt

- **License/version/maintenance:** MIT OR Apache-2.0; Microsoft-maintained,
  current generated bindings as of 2026-08-02.
- **Security/unsafe:** safe call shapes where generated, but Win32 remains an
  FFI boundary; confine raw buffers, unions, handles, and pointer arithmetic to
  the adapter. Recheck the resolved graph for advisories.
- **Compatibility:** supports the required desktop APIs on Windows 10/11 x64;
  keep it out of the portable core with target-specific dependencies.
- **Build impact/value:** feature selection generates more wrapper surface than
  hand declarations but prevents local ABI/constants drift. Enable only
  `Win32_Foundation`, `Win32_Storage_FileSystem`,
  `Win32_Storage_CloudFilters`, `Win32_System_IO`, and required threading
  features; measure clean build and release size.
- **Risk/fallback:** Microsoft API naming/features can churn between releases.
  Pin the minor line and isolate it behind local traits, allowing replacement
  with `windows-sys` or reviewed declarations.

#### `windows-sys` 0.61.2 — viable alternative, do not combine

- **License/version/maintenance:** MIT OR Apache-2.0; Microsoft-maintained and
  current on the research date.
- **Security/unsafe:** thin raw FFI makes nearly every call caller-unsafe and
  increases local review burden; audit the resolved graph.
- **Compatibility:** covers Windows 10/11 x64 and target-specific portable
  builds.
- **Build impact/value:** typically a thinner binding layer, but DiskPie would
  have to recreate RAII and conversions already provided by `windows`.
- **Risk/fallback:** low ecosystem lock-in because both crates map the same
  APIs. Use it only if a measured build-time/binary-size regression justifies a
  whole-adapter switch; do not carry both absent a measured, documented need.

### Scheduling and walking

#### `crossbeam-channel` 0.5.16 — benchmark-triggered alternative

- **License/version/maintenance:** MIT OR Apache-2.0; mature, actively
  maintained release current as of 2026-08-02.
- **Security/unsafe:** safe public channel API with reviewed internal unsafe;
  one small normal dependency. Audit the locked graph.
- **Compatibility:** platform-neutral and suitable for both portable core and
  Windows workers.
- **Build impact/value:** small compile/binary cost; bounded MPMC channels,
  selection, and disconnection semantics replace substantial synchronization
  code and directly enforce backpressure.
- **Risk/fallback:** low lock-in behind a tiny queue/event abstraction. ADR 0001
  found that capacity-one per-worker `sync_channel` queues plus a bounded MPSC
  result channel satisfy the initial topology without this dependency. Adopt
  Crossbeam only if contention, select, or cloned-receiver needs are measured.

#### `rayon` 1.12.0 — defer for scanning

- **License/version/maintenance:** MIT OR Apache-2.0; mature and active.
- **Security/unsafe:** safe work-stealing API with internal unsafe and a larger
  transitive graph; audit before any later use.
- **Compatibility:** cross-platform, but its global/general pool does not model
  per-volume concurrency or reliably interrupt blocking Win32 calls.
- **Build impact/value:** meaningful compile and binary cost. It is valuable for
  CPU-parallel layout/aggregation only after profiling, not for the blocking
  filesystem scheduler.
- **Risk/fallback:** easy to remove behind a compute helper; use fixed workers
  and channels for the scanner.

#### `walkdir` 2.5.0 — reject for the hot backend

- **License/version/maintenance:** Unlicense OR MIT; mature and maintained.
- **Security/unsafe:** largely safe portable Rust; locked dependencies still
  require advisory review.
- **Compatibility:** works on Windows, but its portable iterator does not expose
  allocation, reparse tag, 128-bit file ID, cloud state, or per-volume policy in
  one pass.
- **Build impact/value:** small cost and ergonomic traversal, but DiskPie would
  perform additional opens and lose precise scheduling/cancellation control.
- **Risk/fallback:** no need to accept abstraction lock-in; retain a small local
  traversal over the platform adapter. It can remain a test-only comparison.

#### `jwalk` 0.8.1 — reject

- **License/version/maintenance:** MIT; latest release is old relative to the
  research date (2022), increasing abandonment risk.
- **Security/unsafe:** safe-facing code but pulls Rayon/Crossbeam machinery;
  audit would cover the full graph.
- **Compatibility:** Windows traversal works, but metadata and scheduling have
  the same semantic gaps as `walkdir`, with parallelism not aligned to volumes.
- **Build impact/value:** materially larger graph for speed that is not proven
  on DiskPie's required metadata path.
- **Risk/fallback:** reject; benchmark the local adapter instead.

#### `ignore` 0.4.31 — reject for product scanning

- **License/version/maintenance:** Unlicense OR MIT; mature and active.
- **Security/unsafe:** safe public API but brings glob/regex/walk dependencies;
  audit the graph if ever used.
- **Compatibility:** Windows-capable, yet its purpose is ignore-rule-aware text
  search, not a complete disk accounting walk.
- **Build impact/value:** substantial irrelevant parsing/regex surface and no
  allocation/file-ID/cloud benefit.
- **Risk/fallback:** avoid semantic surprise from implicit ignore behavior; a
  local scanner includes every visible entry unless policy explicitly omits it.

### File identity and NTFS candidates

#### `file-id` 0.2.3 — defer

- **License/version/maintenance:** MIT; small maintained crate at the research
  baseline.
- **Security/unsafe:** wraps platform FFI behind a safe API; review its Windows
  handle/open behavior and locked graph before adoption.
- **Compatibility:** exposes 128-bit-capable Windows identities, but operates by
  path and therefore tends to add an open for every entry.
- **Build impact/value:** small graph and convenient portability, but little
  value when the Windows batch record already contains an ID.
- **Risk/fallback:** local `FileIdentity` plus the Microsoft binding is small;
  reconsider only for non-Windows adapters or demonstrated reduction in code.

#### `ntfs` 0.4.0 — study/prototype only

- **License/version/maintenance:** MIT OR Apache-2.0; small project with a stable
  but not rapidly released baseline.
- **Security/unsafe:** parser advertises no unsafe code, an advantage for
  untrusted on-disk bytes; parser fuzzing and locked advisory checks remain
  mandatory.
- **Compatibility:** format parser, not a standard-user volume acquisition
  solution. A caller must supply raw NTFS bytes; it does not support ReFS/SMB or
  solve live-snapshot, cloud-filter, permission, and Windows sharing behavior.
- **Build impact/value:** modest parser graph, but substantial DiskPie code is
  still needed for indexes, compression/sparse/allocation, reparse/security,
  races, and USN reconciliation.
- **Risk/fallback:** avoid coupling the domain model to parser types. A research
  adapter may use it without copied code; production fallback is conventional.

#### `mft` 0.7.0 — study/prototype only

- **License/version/maintenance:** MIT; current release on the research date.
- **Security/unsafe:** safe parser surface, but parsed disk bytes remain
  adversarial input and require fuzz/property tests; about 21 normal
  dependencies expand the audit boundary.
- **Compatibility:** parses an MFT snapshot rather than acquiring a consistent,
  standard-user live volume; NTFS-only and not suitable for ReFS/SMB.
- **Build impact/value:** comparatively heavy compile/binary graph. It can
  accelerate a controlled research prototype, but does not itself deliver
  complete logical/allocation/cloud semantics.
- **Risk/fallback:** high format/parser lock-in. Keep behind `FastBackend` and
  remove unless the full benchmark gate passes.

#### `usn-journal-rs` 0.4.1 — study/prototype only

- **License/version/maintenance:** MIT; recently active but young (small adopter
  base) as of 2026-08-02.
- **Security/unsafe:** Windows FFI and variable-length record parsing are the
  security boundary; require source review, malformed-buffer tests, and locked
  advisory checks.
- **Compatibility:** Windows/NTFS-specific, generally needs volume access not
  guaranteed to a standard user, and a journal can be absent or unstable. It
  does not provide complete allocation data.
- **Build impact/value:** moderate specialized graph; useful for a change cache
  or enumeration experiment, not a complete first scan.
- **Risk/fallback:** API/filesystem lock-in and immature maintenance profile.
  Keep out of production until ADR 0005's no-elevation and semantic gates pass;
  conventional enumeration is mandatory fallback.

Crate baselines: [`windows`](https://crates.io/crates/windows),
[`windows-sys`](https://crates.io/crates/windows-sys),
[`crossbeam-channel`](https://crates.io/crates/crossbeam-channel),
[`rayon`](https://crates.io/crates/rayon),
[`walkdir`](https://crates.io/crates/walkdir),
[`jwalk`](https://crates.io/crates/jwalk),
[`ignore`](https://crates.io/crates/ignore),
[`file-id`](https://crates.io/crates/file-id),
[`ntfs`](https://crates.io/crates/ntfs),
[`mft`](https://crates.io/crates/mft), and
[`usn-journal-rs`](https://crates.io/crates/usn-journal-rs).

## Verification matrix

- API paths: extended-directory path accepted/rejected/malformed; baseline
  fallback; all handles close on every exit.
- Names: emoji, non-Latin, case-distinct entries, raw unpaired-surrogate test
  fixture where the filesystem permits it, `MAX_PATH` boundary, deep `\\?\`
  and UNC paths.
- Sizes: 0/1/cluster-boundary files, greater than 4 GiB, resident, sparse,
  compressed, encrypted where supported, and allocation-query failure.
- Identity: two and many hard links in one directory, across directories and
  selected roots, owner arriving out of order, a link outside scope, missing or
  unstable IDs, and ReFS 128-bit IDs.
- Boundaries: symlink/junction loop, mount point, unknown reparse tag, file
  symlink, hydrated/dehydrated/partial cloud files, and recall-on-data directory.
- Failures/races: denied directory/file, disappearing entry, renamed directory,
  sharing violation, removed media, disconnected/slow SMB, and root failure.
- Filesystems: NTFS, ReFS, FAT32/exFAT removable media, SMB, and a mocked unknown
  capability set.
- Scheduler: queue/channel bounds under millions of entries, worker caps by
  volume, deterministic final tree under randomized completion order, repeated
  cancel/rescan generations, cancellation during enumeration/result
  backpressure, and no stale UI event application.
- Benchmarks: cold/warm throughput, time to first visible batch, total time,
  peak memory per million entries, cancellation publication/cleanup latency,
  handle count, worker/batch sweeps, and release binary size.
- Fast backend: parser fuzz corpus, journal recreation/wrap, access denied, no
  journal, mutation during scan, semantic differential against conventional,
  and every ADR 0005 threshold.
