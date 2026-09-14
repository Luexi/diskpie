# ADR 0020: Complete product workflows through bounded background services

- Status: Accepted
- Date: 2026-09-06
- Owners: DiskPie maintainers
- Refines: ADR 0008, ADR 0009, and ADR 0016 item 14 for interactive exports

## Context

Scanner, action, integration, and diagnostic policies existed behind native
adapters, but several were not reachable from the application. Completing them
must preserve immutable frame coherence, exact native paths, bounded ownership,
and explicit user confirmation. Ranking a large directory every frame also made
the textual companion view depend on the renderer's time budget.

An existing export target can be pinned with a read-data handle that denies
delete sharing. Windows requires that pin to be closed before replacing the
name. Revalidation followed by close-and-replace is not compare-and-replace:
another process can install a different object in that final interval. Interactive
export does not need overwrite support to satisfy the diagnostic contract.

## Decision

1. Branch rescan begins only from a settled committed frame. An intent retains
   its exact snapshot, generation, revision, node, and native path. The scanner
   enumerates that branch while the old frame remains visible. After the
   coordinator has actually joined, the supervisor replaces the branch and
   re-aggregates the complete tree off-thread. Layout, navigation, presentation,
   and snapshot commit together. Cancellation or failure preserves the old
   frame; stale intents and late generations cannot replace a newer tree.
   Refinement 2026-09-07: the token survives join and preparation, and cancellation
   acceptance shares a final lock-protected decision with terminal publication.
2. Preserve each file's observed allocation before hard-link deduplication.
   Rebuilding a branch re-evaluates ownership across the complete tree, including
   surviving aliases outside the branch. Pending directories are incomplete;
   access failures and failed roots become visible omissions when other work
   can continue.
3. A dedicated `ItemListService` ranks and filters every immediate child of the
   view root off-thread. It has one running request, one coalesced pending
   request, and one completion. Generation, revision, root, basis, and query
   identify a result. The UI virtualizes rows and never treats a chart sector
   cap as the number of list items. Layout and list work check cancellation
   within expensive traversal and sorting steps.
4. A single support worker composes existing target validation, registry policy,
   log collection, and export adapters without egui or unsafe code. At most four
   jobs are accepted but not yet drained, including queued and completed jobs.
   Its reaper owns the thread handle from startup. Drop only requests stop;
   bounded finish reports an actual join or retained background ownership. A
   timed-out worker prevents a replacement worker until its real join completes.
5. Action confirmation remains in the pure `ConfirmationFlow`. The support
   worker validates a snapshot target before review and again after confirmation;
   the UI checks the capability binding, and the STA retains its final native
   validation immediately before mutation. Recycle requires a proven fixed
   local NTFS provider. Missing file identity is disclosed as lower assurance,
   not fabricated or used to prohibit every ordinary directory. Cancel has
   initial focus; permanent delete and empty bin require `DELETE` and `EMPTY`.
6. Every consumed destructive capability retains its `PendingObligation` through
   rejection, cancellation, failure, and shutdown. Settle it exactly once and
   request the parent branch, full scan fallback, or bin query as required.
   Results remain stale until reconciliation. Unexpected permanent deletion
   blocks further destructive attempts in that session. Empty-bin estimates
   are advisory, and its native call has no reliable mid-operation cancellation.
7. Explorer preferences inspect the actual HKCU registry state asynchronously.
   Install, repair, and remove reuse the existing ownership/staging/rollback
   policy. Refinement 2026-09-07: operation and post-inspection results travel
   independently, including after partial removal failure. Unknown refreshed
   state invalidates prior UI/preference claims while preserving the operation
   outcome when inspection fails. Recoverable partial installations expose the
   domain's capabilities.
8. Diagnostic export builds one immutable redacted artifact before destination
   selection. Path inclusion resets to false for each export; secret redaction
   remains mandatory. The preview pages together contain exactly its bytes.
   The existing STA shows `IFileSaveDialog` with a UTC-dated
   `DiskPie-diagnostics-YYYY-MM-DD-HHMMSS.txt` suggestion and no overwrite prompt.
9. Prepare the selected destination in the support worker without creating any
   file. Retain the parent and inspected identities; resolve actual transport
   through the parent handle, including mapped shares, before destination
   confirmation. Existing destinations require another filename. The UI retains
   only a token, display path, and transport; the worker retains the capability
   and exact artifact. Tokens are single-use, and abandoned/stale preparations
   are discarded off-thread.
10. The public prepared export commit is create-only. It refuses a pre-existing
    destination before creating a temporary sibling, revalidates after writing
    and flushing, and publishes with native parent-relative no-replace rename.
    A name created after the last inspection cannot be overwritten. Reject
    directories, reparses, multiply linked targets, owned path/identity aliases,
    and exports directly inside the managed log directory. Configured existing sources
    are pinned with `FILE_READ_DATA` while denying delete sharing. Cleanup can
    delete only the temporary object whose handle the operation owns.
11. The older eager `write_diagnostic_export` API retains replacement semantics
    for compatibility and native adapter tests; the interactive workflow never
    calls it. Its close-before-replace interval is explicitly documented and is
    not claimed to provide compare-and-replace. Item 10 supersedes ADR 0016's
    overwrite wording for the interactive flow.
12. Log collection enumerates the retained managed directory with fixed native
    buffers and explicit iteration/entry bounds. It selects at most eight newest
    exact dated log names, validates the initial 16 MiB size ceiling, then seeks
    and reads only a bounded tail of at most 1 MiB each through the retained
    handle (refinement 2026-09-07). Partial prefix records and
    unavailable/unsafe sources are omitted and counted. The final artifact
    remains subject to ADR 0016's 8 MiB and 16 KiB line limits. Export never
    modifies the source logs or starts a network client.
13. The composition root includes list and support join receipts in shutdown
    proof. A timeout or uncertain destructive terminal result cannot mint clean
    shutdown or successful mutation evidence. Release preparation, tags,
    publication, and workflow changes are outside this delivery.
    Missing receipts after partial startup cannot assert that no runtime was
    composed; they preserve the marker without a clean-shutdown proof.

Diagnostic schema refinement (2026-09-07): schema 2 reports `[last_scan]`, with
explicitly unknown unmeasured fields rather than zero-valued session totals.
Exact preview/artifact identity, redaction and create-only publication remain
unchanged. The compositor retains bounded suffix bytes and only header-margin
metadata; it does not keep an object per input line. See the
[remediation record](../research/review-remediation-validation.md) for regressions
and measurements; the original delivery evidence remains historical.

## Consequences

The renderer submits and polls bounded work while retaining a coherent frame.
Interactive exports avoid an unprovable overwrite race at the cost of requiring
a fresh name. Branch replacement still uses O(total nodes) rebuilding and can
retain the old and replacement trees concurrently; it is not claimed to be an
incremental-memory merge. Slow filesystem calls can delay background completion,
but bounded shutdown retains their owner rather than blocking the UI or spawning
unlimited replacements.

The conventional filesystem adapter remains the production provider. The
batch-enumeration experiment belongs to the isolated benchmark harness until
measurements and provider parity justify a separate decision.

## Validation

- Platform tests cover deferred creation, existing destinations, late aliases,
  retained source identity, exact bounded log tails, and a deterministic alias
  created immediately before no-replace native publication.
- Runtime/core tests cover branch commit, failure/cancellation retention, stale
  generations, and cross-branch hard-link accounting.
- Support and UI tests cover bounded admission/retirement, single-use export
  tokens, exact preview bytes/pages, action obligations, and late completions.
- Current combined gate results are recorded in [ROADMAP](../../ROADMAP.md).
  Native visual and Windows 10/11 disposable-VM validation are tracked separately;
  automated fixture coverage is not evidence that a user's Recycle Bin was emptied.
