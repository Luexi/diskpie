# ADR 0003: Reparse-point and cloud-placeholder policy

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

Windows reparse points can redirect traversal through symlinks, junctions,
mount points, cloud providers, and unknown filters. Following them can create
cycles, leave the selected tree or volume, double-count content, and hydrate
remote data. Even enumerating a recall-on-data directory may fetch metadata.

Supporting research is in
[Windows filesystem scanning](../research/windows-filesystem-scanning.md).

## Decision drivers

- No reparse loops or unexpected tree/volume escape.
- No unintended cloud hydration or surprise network transfer.
- Useful, explicit partial results instead of invisible omissions.
- Safe handling of unknown/new reparse tags.
- A future opt-in mode can be added without weakening the default.

## Options considered

### Follow every directory entry

This maximizes apparent coverage but is unsafe and nondeterministic for cycles,
mount points, providers, and disconnected targets.

### Special-case known symlinks and junctions

This still fails open for new/unknown tags and does not by itself prevent cloud
recall or cross-volume traversal.

### Treat reparse/cloud entries as explicit boundaries

This is conservative, predictable, standard-user friendly, and matches the
legacy product's no-linked-directory behavior.

## Decision

Adopt boundary behavior by default:

1. Classify entries from enumeration attributes and reparse tag before any
   open. Never enqueue a directory with `FILE_ATTRIBUTE_REPARSE_POINT`, whether
   the tag is a symlink, junction, mount point, cloud tag, or unknown.
2. Keep the entry in the tree as `SkippedBoundary` with raw tag, kind if known,
   and omission reason. Mount points therefore do not cross volumes; a user may
   select the mounted volume/path as a separate root.
3. Keep file reparse entries, but never follow the target for size or identity.
   Use allocation/identity included in safe directory-enumeration metadata for
   the reparse object. If that is absent, mark allocation/identity unknown
   rather than opening a target.
4. Classify Cloud Files using attributes/tag and
   `CfGetPlaceholderStateFromAttributeTag`, which does not require opening the
   item. Do not metadata-open an offline, recall-on-open, or partially-on-disk
   file. Do not enumerate a directory with
   `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`; report cloud content not enumerated.
5. `FILE_FLAG_OPEN_REPARSE_POINT` is mandatory for any metadata open that policy
   allows, along with non-inheritable handles and full sharing. It is defense in
   depth, not permission to ignore the classification policy.
6. Unknown tags fail closed as boundaries. No code may infer safety from an
   unrecognized tag number.

Relevant contracts:
[`WIN32_FIND_DATAW`](https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-win32_find_dataw),
[reparse points](https://learn.microsoft.com/en-us/windows/win32/fileio/reparse-points),
[symbolic-link effects](https://learn.microsoft.com/en-us/windows/win32/fileio/symbolic-link-effects-on-file-systems-functions),
[`CfGetPlaceholderStateFromAttributeTag`](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfgetplaceholderstatefromattributetag),
[`CF_PLACEHOLDER_STATE`](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_placeholder_state),
and [file attributes](https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants).

## Consequences

- Default results deliberately exclude contents reached only through directory
  reparse points and risky cloud directories; completeness and tooltips expose
  that boundary.
- A selected root that is itself a reparse point is not silently dereferenced.
  Root validation reports the boundary and asks the caller to choose the real
  target explicitly.
- Some locally present bytes associated with a placeholder may remain unknown
  when the provider does not put allocation into enumeration metadata. Avoiding
  hydration takes priority over a guessed total.
- A future explicit "follow links" feature requires a separate Accepted ADR,
  user-visible consent, directory identity cycle detection, stay-on-volume
  control, depth/work bounds, cloud-recall policy, and dedicated tests.

## Validation

- Fixtures cover a symlink loop, junction loop, mount point, file symlink,
  dangling target, and mocked unknown tag; no target is opened or enqueued.
- Cloud tests cover hydrated, dehydrated, offline, partially-on-disk,
  recall-on-open, and recall-on-data directory states. Provider/ETW diagnostics
  or a controlled test provider confirms that the default scan causes no
  hydration.
- Adapter tests assert `FILE_FLAG_OPEN_REPARSE_POINT` and that denied or missing
  metadata yields an omission, not target traversal or zero.
- Long-path and raw-UTF-16 names are tested on both ordinary and boundary nodes.
