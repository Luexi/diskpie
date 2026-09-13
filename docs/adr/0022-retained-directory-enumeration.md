# ADR 0022: anchor Windows enumeration and child metadata to one directory

- Status: Accepted
- Date: 2026-09-07
- Refines: ADR 0003; preserves the conventional per-file metadata provider

## Context

The independent review reproduced a directory being renamed and replaced with
a junction between visitor callbacks. The pathname-based enumerator retained
the original names, while later metadata opens read the junction destination.
Checking the pathname again before each open would retain a check/open race.

## Decision

Retain one directory handle for enumeration and open child metadata relative to
that handle. Native name resolution refuses intervening reparse processing;
opened directory and file attributes are checked against the existing
reparse/cloud policy before enumeration or metric queries. Opens retain the
no-recall option. Names remain native UTF-16 values and never become commands.

`GetFileInformationByHandleEx` reads directory records through that handle using
an aligned 64 KiB buffer. `FileIdExtdDirectoryInfo` supplies tags; providers that
reject it may use `FileFullDirectoryInfo` on the same handle. Missing tags are
unknown, never permission to follow a reparse point. Both record parsers validate
lengths and offsets. Metadata still comes from native per-file queries,
including allocation and full file identity. The isolated benchmark prototype's
metadata shortcuts are not adopted.

## Dependency and ownership consequences

Enable `Wdk_Foundation` on the already pinned `windows 0.62.2` dependency for
`OBJECT_ATTRIBUTES` and `NtCreateFile`. No package/version or third-party
license is added. The maintained MIT/Apache-2.0 Windows bindings already ship
with DiskPie. Additional project unsafe stays target-gated in `diskpie-platform`,
with documented buffer/lifetime invariants and immediate `File` RAII ownership.
There is no new runtime, service or worker family.

Rechecking names without retaining their common directory identity was rejected
because it does not close the race. The chosen approach adds attribute checks
and changes enumeration APIs. Child opens now request `FILE_READ_ATTRIBUTES`
instead of zero access; restrictive ACLs can therefore leave allocation or
identity unknown where the old open might have succeeded. Unsupported providers
produce visible omissions
instead of falling back to pathname traversal. Remote/cloud provider acceptance
remains necessary. Driver calls remain cooperatively cancelled between calls,
not forcibly interruptible. Rejected boundaries and failed queries are reported
as omissions or unknown metrics. Enumeration is not an atomic filesystem
snapshot: replacing an ordinary child within the retained directory can yield
the replacement's current metadata, since its identity is not compared with
the earlier directory record.

Lock-in remains confined to the Windows adapter and the portable `ScanFs`
contract is unchanged. Binary/build impact and local validation are recorded
separately; NTFS fixtures do not prove clean Windows 10/11, ReFS/UNC or cloud
acceptance.

## Validation

See [review remediation validation](../research/review-remediation-validation.md).
Regressions exercise substituted directories, native names, hard links, empty
directories, cancellation, malformed records and the fallback class, using only
directories created by the tests. No user file action or bin-empty call runs.

Primary API contracts: [OBJECT_ATTRIBUTES](https://learn.microsoft.com/en-us/windows/win32/api/ntdef/ns-ntdef-_object_attributes),
[NtCreateFile](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntcreatefile),
[GetFileInformationByHandleEx](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfileinformationbyhandleex).
