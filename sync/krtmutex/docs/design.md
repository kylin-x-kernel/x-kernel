# krtmutex design

## Purpose and scope

`krtmutex` owns a sleeping mutex with priority inheritance. A waiter lends its
priority to the owner, and through the owner to whatever that owner waits for.
The scheduler implements `PiHost`: it embeds one `PiTaskNode` in each task,
reports a donation level, and applies the boost. `ktask::TaskPiHost` supplies
the kernel provider, exposed through `ksync::RtMutex<T>`.

The lock algorithm does not depend on `ktask` or `khal`. Blocking uses
`kiface_task_api::TaskExecutionIf::block_on`.

Ordinary futex `WAIT`/`WAKE` does not enter this crate. `RawRtMutex` exposes
the same graph without Rust data or a guard: it adopts a validated live owner,
enrolls the current task through `PiWait`, and releases an explicit owner.
Successful waits retain ownership; `cancel` consumes a wait and returns false
when handoff won, retaining that grant. Dropping an unfinished wait instead
withdraws or passes on a grant. Ordinary raw operations require task context with no external wait-source or
scheduler locks. The deferred variants permit bucket -> graph nesting:
enrollment and external ownership commit occur under the graph lock, then
`PiEffects` is dropped after bucket unlock to publish boosts and wakes. Commit
callbacks must not block, allocate, reenter graph/scheduler locks or unwind.
Explicit-task PI requeue is not provided.

## Architecture

```text
sync/krtmutex/
├── src/lib.rs      # exports
├── src/host.rs     # PiHost: donation level and boost
├── src/graph.rs    # waiter trees, donor trees, chain walk, deadlock
└── src/mutex.rs    # current-task RtMutex and guard
```

```text
RtMutex::lock / try_lock / Guard::drop
        │
        ▼
graph (PI_GRAPH)
  each lock: waiters ordered by donation level; the top waiter is linked
             into the owner's donor tree
  each task: blocked_on and a donor tree
        │  after the graph lock is released
        ▼
PiHost::set_boost → the host run queue
```

`RtMutex` acts only on the current task. Level `0` donates nothing. The host
maps a Deadline task to `99`, an RT task to its effective priority, and every
other class to `0`. The graph does not name a scheduling class.

## Execution context

`lock` requires an installed current task in sleepable context. It blocks, so
it cannot run in interrupt context, with IRQs disabled, or while a spinlock
is held. `try_lock` does not sleep, but still requires a current task. Early
boot and interrupt context are unsupported.

`PiHost::donor_priority` runs under the graph lock and must not take a
run-queue lock. `PiHost::set_boost` runs after that lock is released, so the
host may take run-queue locks, but it must not voluntarily block.
`PiEffects` drop calls it with preemption disabled from deboost through wake
delivery. The host calls `adjust_waiter_priority` after `sched_setattr` has
dropped its run-queue lock.

The guard is not `Send`. The owning task may migrate between CPUs, but another
task cannot take the guard. Locking a mutex this task already owns returns
`EDEADLK`. There is no poisoning. The wait is not interruptible and has no
timeout.

## State and algorithm

A lock has one owner. Waiters sit in a red-black tree ordered by donation
level, highest first, with a sequence number to break ties. The highest
waiter is also linked into the owner's donor tree. The owner's boost is the
maximum donor level, or `0` when it has no donors.

1. The fast path of `try_lock` and `lock` sets the current task as owner when
   the lock is free, under the graph lock.
2. When the lock is busy, the slow path allocates a waiter, rejects a cycle,
   inserts the waiter, and refreshes the owner's donor.
3. The graph lock is released. Propagation starts at the owner: read the top
   donation, call `set_boost`, re-sort that task's own wait if its level
   changed, and continue to the next owner when needed.
4. The current task sleeps through `block_on`. Handoff moves the lock to the
   front waiter and takes its waker under the graph lock. `wake` runs only
   after that lock is released and both boosts have been propagated.
   Preemption stays disabled until wake delivery, so deboost cannot dispatch
   a medium-priority peer before the new owner becomes runnable.
5. After `sched_setattr` changes a waiting task's policy,
   `adjust_waiter_priority` re-sorts its key and propagates from the owner.

Propagation stops when a step does not change effective dispatch, when the
task is not blocked on another lock, or at the depth bound of 1024. A chain
that returns to the waiter, or that exceeds that bound, fails `lock` with
`EDEADLK` and does not insert the waiter. Dropping the guard hands the lock
to the highest waiter and recomputes both boosts.

## Concurrency

One global `PI_GRAPH` (`SpinNoPreempt`) guards every waiter tree and donor tree.
Access is task context only, so the lock does not mask IRQs.
The critical section only edits those trees and calls `donor_priority`.
`set_boost` and `Waker::wake` run after the graph lock is released, so they
do not deadlock with a task blocked in `schedule` that already holds its
run-queue lock. After each boost submission, propagation rechecks the highest
donation under the graph lock and retries stale values before proceeding.

Lock state is allocated on first use. Each waiter is an `Arc` so it can be
linked into two trees. The single lock keeps the chain walk from taking and
dropping per-lock locks. Every inheritance lock shares it.

## Decisions

- The graph and the scheduler are separate. Policy stays in `PiHost`. The
  graph only compares levels in `0..=99`.
- `RtMutex<T>` owns Rust data and an RAII guard. `RawRtMutex` reuses its
  graph and wait implementation for explicit ownership and cancellation.
- One global graph lock, rather than a wait lock per mutex plus a pi lock
  per task.
- `const fn new` is possible because the shared state is a lazy `Arc`. A
  static mutex does not allocate until first use.

## Drop and lifecycle

Dropping the guard releases the lock and wakes the next owner. If the wait
inside `lock` is dropped before the lock is granted, for example during
unwind, the waiter is removed and its donation is withdrawn. If unwind
happens after handoff, the lock is passed on, so it is not left owned by a
waiter that no longer exists.

Dropping an `RtMutex` hands the lock down any leftover owner chain, so a
boost does not survive the lock. Callers must keep the mutex alive while
other tasks are blocked in `lock`. Dropping it at that point is a caller bug:
those tasks resume against a freed mutex.
