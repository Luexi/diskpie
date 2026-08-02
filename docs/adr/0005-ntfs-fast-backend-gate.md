# ADR 0005: Gate an optional NTFS fast backend

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

Raw MFT or USN enumeration can list NTFS names and relationships faster than a
directory walk, but DiskPie also needs logical size, allocation, hard-link
identity, reparse/cloud policy, partial-error reporting, cancellation, and
standard-user operation. Raw volume access may be denied. A USN journal can be
absent, trimmed, deleted, or recreated and does not contain the required size
fields. Opening every enumerated path afterward may remove the speed advantage.

Supporting API/crate analysis is in
[Windows filesystem scanning](../research/windows-filesystem-scanning.md).

## Decision drivers

- The conventional backend is the correctness oracle and universal fallback.
- No automatic elevation or reduced semantic honesty for speed.
- Complexity and parser attack surface require a large measured user benefit.
- ReFS, FAT/exFAT, removable media, SMB, and unsupported NTFS configurations
  must continue to work.

## Options considered

### Ship raw MFT parsing now

This promises high enumeration throughput but introduces privileged/raw I/O,
format parsing, live-mutation consistency, and substantial semantic work before
the correct portable path is established.

### Use `FSCTL_ENUM_USN_DATA` as the scanner

This uses a supported Windows control path, but USN records supply file/parent
identity, name, attributes, and change data—not EOF or allocation. It is neither
a complete scan snapshot nor a universal capability.

### Keep conventional scanning and gate a replaceable experiment

This protects correctness and delivery while allowing a fast backend only if
end-to-end evidence, not name-enumeration microbenchmarks, justifies it.

## Decision

Ship no MFT/USN backend initially. The conventional backend is always compiled
and remains the oracle. A future backend may exist only behind the same scan
protocol and a runtime `FastBackend::try_open` capability check. It must fall
back silently (with diagnostics, not an elevation prompt) for access denied,
non-NTFS, unsupported API/version, remote/removable source, absent or unstable
journal, malformed data, or a failed consistency check.

A candidate is accepted only when one reviewed implementation passes every gate:

1. **Privileges:** runs as a normal user on supported Windows 10 and 11
   installations and never requests elevation. Tests include ACL-restricted and
   volume-open-denied cases with clean conventional fallback.
2. **Semantic parity:** on a quiescent fixture, produces byte-for-byte equivalent
   path hierarchy, logical size, allocated size, hard-link ownership,
   reparse/cloud boundaries, and completeness to the conventional oracle. Every
   unknown or omission is explicit; no field is guessed from logical length.
3. **End-to-end speed:** on at least two reproducible local NTFS datasets with
   one million or more entries, reaches at least `2.0x` the conventional
   backend's median total-scan throughput/time after all required size and
   identity acquisition. Name-only enumeration does not count. Time to first
   visible batch may not regress by more than 10%.
4. **Resource bound:** peak resident memory is no more than 120% of the
   conventional backend on the same corpus; queues, handles, and parser buffers
   remain bounded. Portable release-size/build-time deltas are reported and
   accepted explicitly.
5. **Cancellation:** follows ADR 0004 generations and observable cancellation
   behavior, checks inside long parse/enumeration loops, and does not leave a
   raw volume handle or unbounded cleanup task behind.
6. **Live consistency:** detects journal ID/range changes and material directory
   mutation during the scan. It either reconciles deterministically or discards
   the attempt and restarts/falls back; it never publishes a falsely complete
   hybrid snapshot.
7. **Security and maintenance:** all raw-record lengths, offsets, parent
   references, cycles, and integer arithmetic are checked. The parser has a fuzz
   corpus, property tests, zero known locked advisories, compatible license, a
   documented unsafe boundary, and an acceptable dependency/build footprint.
8. **Fallback coverage:** differential tests cover ReFS, FAT/exFAT, SMB,
   removable drives, no journal, recreated/wrapped journal, corrupt/truncated
   records, unsupported OS/API, and access denied. All select conventional
   behavior without losing the user's scan request.

If per-file metadata opens needed to recover EOF/allocation erase the 2x
advantage, reject the backend rather than weaken the size contract.

Official evidence:
[`FSCTL_ENUM_USN_DATA`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data),
[`USN_RECORD_V2`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2),
and [change journal records](https://learn.microsoft.com/en-us/windows/win32/fileio/change-journal-records).

## Consequences

- Initial implementation effort stays on correctness, coverage, streaming, and
  cancellation that all filesystems need.
- `ntfs` 0.4.0, `mft` 0.7.0, and `usn-journal-rs` 0.4.1 may be studied or used in
  isolated prototypes only; no parser type enters the domain model.
- The 2x threshold intentionally prices in raw-I/O/parser complexity and future
  maintenance. Failing any gate keeps the experiment out of production.
- Even an accepted backend remains optional and observable in diagnostics; the
  user-facing semantics do not depend on which backend ran.

## Validation

Publish a benchmark/differential report containing OS build, hardware/storage,
filesystem features, corpus generator and checksum, cold/warm protocol, sample
count, medians/distribution, first-batch and total times, memory/handle peaks,
cancellation timing, mutation/journal outcomes, binary/build deltas, and the
resolved dependency license/advisory report. Preserve failing cases as tests.
