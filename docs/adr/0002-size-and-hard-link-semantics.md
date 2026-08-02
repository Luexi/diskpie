# ADR 0002: Size and hard-link semantics

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must switch between logical and allocated size while remaining honest
for compressed, sparse, resident, cloned, and hard-linked files. A path-based
walk naturally sees every hard link, while the underlying allocation belongs to
one file identity. Missing metadata and links outside the scan prevent a simple
number from always meaning reclaimable bytes.

Supporting research is in
[Windows filesystem scanning](../research/windows-filesystem-scanning.md).

## Decision drivers

- Deterministic results under parallel, multi-root scanning.
- Correct Windows logical/allocation values without cluster-size guesses.
- No accidental hard-link inflation, especially in system trees.
- Explicit uncertainty rather than misleading zeroes.
- A compact, platform-independent domain contract.

## Options considered

### Count every path in both modes

This is simple and makes each directory independently additive, but can grossly
inflate allocated space for hard links and misrepresent disk use.

### Deduplicate both modes

This approximates unique files, but makes logical directory contents surprising:
an ordinary path may show no size solely because another alias was visited first.

### Count paths logically and identities for allocation

This matches the meaning of the two selectable modes. It needs identity-aware
aggregation, deterministic ownership, and an explicit fallback when IDs are not
available.

## Decision

Adopt the third option.

1. A file path's **logical size** is the end-of-file of its unnamed/default data
   stream. Every visible hard-link path contributes logical bytes and file count.
2. A file's **allocated size** is the filesystem-reported `AllocationSize` of
   that unnamed stream. It contributes once per `(VolumeKey, FileId128)` across
   all selected roots in a scan generation. Never infer it by rounding logical
   length to a cluster boundary.
3. Directories contribute zero. Alternate data streams, filesystem metadata,
   journals, security metadata, and directory allocation are excluded.
4. The identity's allocated contribution belongs to the lexicographically
   smallest deterministic raw-UTF-16 path key (sorted root key, then relative
   path). First observation is provisional; if a smaller owner arrives later,
   aggregation transfers the delta between ancestor chains. Other aliases show
   zero allocated contribution but retain their logical size and alias status.
5. `FILE_STANDARD_INFO`/equivalent supplies EOF and allocation; `FILE_ID_INFO`
   or an equivalent batch record supplies a 128-bit identity. Identity state is
   generation-local because file IDs can be reused after deletion.
6. When allocation or identity cannot be obtained, record known bytes separately
   from an unknown-entry count and reason. If allocation is known but identity is
   not, count it per path and label hard-link deduplication unavailable. Never
   silently claim a unique total.
7. Totals use `u128` internally. The result model carries `Complete`, `Partial`,
   or `Cancelled` state at scan/subtree granularity.

`GetCompressedFileSizeW` is not the general allocated-size source: for ordinary
non-compressed/non-sparse files Microsoft documents ordinary file size, and it
follows symbolic-link targets. It is permitted only as a constrained fallback
for a non-reparse compressed/sparse file.

Relevant contracts:
[`FILE_STANDARD_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_standard_info),
[`FILE_ID_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info),
[`GetCompressedFileSizeW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getcompressedfilesizew),
and [hard links and junctions](https://learn.microsoft.com/en-us/windows/win32/fileio/hard-links-and-junctions).

## Consequences

- Final allocated totals are stable despite nondeterministic worker completion.
- A partial chart may move an identity's provisional allocation to an earlier
  owner as more paths arrive; generation-tagged deltas must support that update.
- A unique within-scope total is not a deletion forecast. ReFS block sharing,
  deduplication, snapshots, and hard links outside selected roots can retain
  storage. When total link count exceeds observed aliases, the UI may make that
  limitation more specific, but the count is race-prone and diagnostic only.
- Missing identity reduces precision without aborting the scan.
- File counts remain path/entry counts; a separate unique-file count may be
  added but must not replace it.

## Validation

- Windows fixtures for ordinary, empty, resident, greater-than-4-GiB, sparse,
  compressed, and cluster-boundary files compare DiskPie values with handle
  metadata.
- Hard-link fixtures cover same-directory, cross-directory, cross-root,
  randomized arrival order, links outside scope, and identity-query failure.
- Property tests randomize batches and assert identical final owners/totals,
  conservation during owner transfer, `u128` overflow safety, and
  `known + unknown` propagation.
- ReFS/block-clone tests verify the value and limitation label without claiming
  exclusive physical sectors.
