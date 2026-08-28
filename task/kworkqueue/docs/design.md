# kworkqueue Design

## Role

`kworkqueue` is the pure workqueue semantic core. It owns logical queue state,
per-CPU queue bindings, work-item lifecycle, active/inactive accounting, flush
colors, pending cancellation, disable/enable state, and executor operations.

The crate does not create workers, wait, sleep, arm timers, raise softirqs, or
run callbacks. Runtime users connect it to an executor by applying returned
`ExecutorOp` values.

## Main Types

- `Work` is a reusable work-state object. It contains no callback.
- `WorkQueue<CPUS, PENDING_CAP>` is a global logical queue with one binding per
  CPU index.
- `WorkQueueBinding` is the queue-to-executor binding selected by a runtime.
- `BindingId` identifies the binding that produced executor entries.
- `EntryOwner` is the executor grouping key used by worker-pool inactive
  promotion. Current integrations derive it from the binding, but runtimes
  must treat it as a grouping value, not as a queue lookup key.
- `ExecutorEntry` is the opaque entry submitted to an executor. It carries both
  the producing `BindingId` and the executor `EntryOwner`.
- `ExecutorOp` describes the executor-side action needed after a queue
  transition.
- `ClaimedWork` is the queue-owned proof that a claimed work instance may run.
- `FlushSnapshot` and `WorkFlushSnapshot` represent wait predicates for queue
  and single-work flush.

## Basic Usage

Runtime integrations normally wrap this lower-level protocol instead of
exposing it directly to drivers:

```rust
use kcpu_id_map::LogicalCpuId;
use kworkqueue::{ClaimResult, ExecutorOp, Work, WorkQueue};

static QUEUE: WorkQueue<4, 128> = WorkQueue::new("example", 16);
static WORK: Work = Work::new();

let binding = QUEUE.binding(LogicalCpuId::new(0)).unwrap();
let outcome = binding.queue_work(&WORK).unwrap();

let entry = match outcome {
    kworkqueue::QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
    kworkqueue::QueueWorkOutcome::Inactive(ExecutorOp::EnqueueInactive(entry)) => entry,
    kworkqueue::QueueWorkOutcome::QueuedWhileRunning => return,
};

match binding.claim(entry, &WORK, 0, 1) {
    ClaimResult::Run(claimed) => {
        // The integration runs the external callback outside kworkqueue.
        let finish = binding.finish(&WORK, claimed);
        let _ = finish;
    }
    ClaimResult::Stale => {}
}
```

## Queue Binding Model

A `WorkQueue` is global. Its `binding(cpu)` method selects one per-CPU binding.
`max_active` is enforced per binding, not globally across all CPUs. If one work
item is queued on CPU 0 and another item is queued on CPU 1, they occupy
different active counters and may run independently.

Each binding owns:

- active count;
- current flush color;
- in-flight counts per color;
- pending record table for runnable and inactive entries.

## State Machine

```text
Idle
  -> DelayedPending
  -> Pending(active or inactive)

DelayedPending
  -> Pending(active or inactive)
  -> Idle

Pending
  -> Running
  -> Idle

Running
  -> Idle
```

`WorkInstanceId` is allocated whenever idle work becomes pending. Pending
records store the instance id, binding owner, entry key, flush color, and active
flag. Claim, cancel, and finish paths revalidate those fields before mutating
state.

## Active and Inactive Work

If a binding has fewer than `max_active` active entries, queueing returns an
`ExecutorOp::EnqueueRunnable`. Otherwise it returns
`ExecutorOp::EnqueueInactive`. When running work finishes or active pending
work is canceled, the binding can promote inactive entries and returns
`ExecutorOp::PromoteInactive` to the runtime.

The workqueue core decides when inactive work may become runnable. The executor
owns only runnable/deferred storage and dispatch.

## Flush and Cancel

Queue flush captures a color snapshot for every binding. Single-work flush
captures the currently observed pending or running instance. A flush is
complete only when the corresponding in-flight count or work state no longer
matches the snapshot.

Pending cancel removes the pending record and returns the executor entry that
must be removed from the backend. `cancel_work()` marks a running instance so a
synchronous waiter can observe completion after the callback exits.
`cancel_work_nonblocking()` uses the same pending and delayed removal rules but
does not modify running work; it only reports that the callback is running.

## Executor Contract

Runtime users must apply `ExecutorOp` after a successful queue transition:

- `EnqueueRunnable` makes an entry eligible for immediate execution.
- `EnqueueInactive` stores an entry until the queue later promotes it.
- `Remove` removes a pending entry from the executor.
- `PromoteInactive` asks the executor to move inactive entries for one owner to
  runnable storage.

The executor must return stale entries to the queue claim path instead of
running them. Stale handling keeps accounting correct and prevents old entries
from executing after cancel, disable, or delayed-work generation changes.
