# ADR 0004: Bounded scan scheduling and cancellation

- Status: Accepted
- Date: 2026-08-02
- Owners: DiskPie maintainers

## Context

DiskPie must scan millions of entries, stream useful partial results, and cancel
without freezing egui. Filesystem work is blocking and has very different cost
on SSDs, rotational disks, removable media, and SMB. Per-entry tasks and
unbounded result events would turn a large directory into excessive allocation,
memory, scheduler, and UI overhead.

Supporting research is in
[Windows filesystem scanning](../research/windows-filesystem-scanning.md).

## Decision drivers

- Bounded memory and handle usage for arbitrarily large trees.
- Fast first visible result and deterministic final aggregation.
- Concurrency limited globally and per volume/device class.
- Observable cancellation independent of a slow filesystem provider.
- No filesystem or shell I/O on the UI thread.

## Options considered

### Spawn a task per file/directory

This is easy to express but gives no useful memory bound, overwhelms slow media,
and makes cancellation/result ordering expensive.

### General work-stealing pool

Rayon-style work stealing is effective for CPU work, but does not naturally
model per-volume permits or cancellation of blocking Win32 enumeration.

### Coordinator with fixed blocking workers and bounded channels

This makes queue, in-flight work, backpressure, generations, and device limits
explicit. It requires a small scheduler but directly encodes DiskPie's needs.

## Decision

Adopt the coordinator design.

1. One scan coordinator owns the pending-directory queues, outstanding-work
   count, per-volume permits, generation state, and aggregation order. Workers
   never recursively schedule work and never update UI state directly.
2. Use a fixed set of dedicated blocking workers. A work item is one directory,
   not one file. Workers return bounded batches containing entries, omissions,
   and discovered child-directory work; the coordinator alone enqueues children.
3. Keep no more than roughly twice the active worker count dispatched. Start
   conservatively with one permit for network/removable volumes, two for
   unknown/rotational volumes, and at most four for a measured low-seek-penalty
   local volume, subject to a global cap. Tune only from reproducible benchmarks.
4. Use the bounded standard-library topology in ADR 0001: one capacity-one work
   channel per worker and cloned senders into a bounded MPSC result channel.
   Start result capacity at 8-16 batches. Flush a worker batch at 512-2,048
   entries or after 16-33 ms, whichever comes first. These are benchmark knobs,
   not API promises.
5. Every work item and event carries a monotonically increasing generation.
   Replacing or cancelling a scan invalidates the generation, stops dispatch,
   and publishes a `Cancelled` partial snapshot immediately. The consumer drops
   late events from older generations.
6. The cancellation token is atomic and checked before every directory API
   operation and at least every 64 returned entries. Result sending is
   cancellation-aware so a full channel cannot trap a worker indefinitely.
7. `CancelSynchronousIo` may be attempted from the coordinator against a known
   dedicated worker thread. It is best-effort. Never use asynchronous thread
   termination, never abandon RAII handle cleanup, and never claim that every
   filesystem driver cancels promptly.
8. A blocked network provider may delay worker cleanup after the UI already
   shows `Cancelled`. Track that condition and cap retained/retiring workers;
   repeated rescans must not create unbounded replacement threads.
9. The UI drains a bounded number of coherent batches per frame, can coalesce
   presentation invalidations, and performs neither filesystem calls nor waits
   for worker completion.

## Consequences

- Queue/channel capacities give explicit memory and handle bounds.
- A very large directory can still stream progress and observe cancellation
  because checks and time-based flushes occur inside enumeration.
- Partial owners (including hard-link allocation ownership) may change as
  batches arrive; final aggregation remains deterministic.
- Backpressure can reduce peak throughput when the UI is slow, an accepted
  tradeoff for bounded memory. Coordinator instrumentation must expose queue
  depth, batch size/age, active permits, and blocked-send time.
- CPU-heavy layout work may later use Rayon behind a separate measured helper;
  that does not change the filesystem scheduler.

## Validation

- Stress tests scan synthetic million-entry/deep/wide trees while asserting
  queue, channel, thread, handle, and resident-memory bounds.
- A deterministic fake adapter randomizes worker completion and errors; final
  trees and totals must be identical across runs.
- Cancellation tests cover before dispatch, mid-directory, full result channel,
  metadata query, disconnected SMB, removable-drive loss, and rapid
  cancel/rescan generations. No stale event may mutate the current tree.
- Benchmarks report files/second, first visible batch, total time, peak memory
  per million entries, cancellation publication and worker-cleanup latency,
  handle peak, and worker/batch sweeps by storage class.
- Windows leak checks verify every enumeration/file/thread handle closes on
  completion, error, cancellation, and panic boundaries.
