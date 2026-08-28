# kworkerpool Design

## Role

`kworkerpool` owns worker-pool scheduling. It manages worker slots,
runnable/deferred entry queues, concurrency accounting, CPU-intensive marking,
dynamic worker creation, idle retirement, manager actions, ktask-backed worker
threads, and ktask-backed manager threads.

The crate does not know what a workqueue is. Entries are opaque
source/owner/key/payload values. Product layers decide what those values mean
and validate them during claim.

## Main Types

- `WorkerPool` is the pure locked state machine.
- `KtaskWorkerPool` wraps `WorkerPool` with ktask thread references and wake
  sources.
- `PoolEntry` is an opaque executable entry. Its source is carried back to the
  runtime for lookup, while its owner is used only as the pool grouping key for
  deferred promotion.
- `WorkerRuntime` is the callback interface used by a worker task.
- `RunnableClaimer` validates a popped entry outside the pool lock.
- `WorkerTask` is the ktask-backed worker loop.
- `ManagerTask` is the ktask-backed manager loop.
- `WorkerPoolPolicy` configures per-instance lifecycle behavior.

## Basic Usage

Most kernel users should use a product layer such as `kwork`; direct users must
provide a runtime that executes returned actions and validates claimed entries.

```rust
use kcpu_id_map::LogicalCpuId;
use ktime_types::TimeSpan;
use kworkerpool::{
    EntryKey, EntryOwner, EntryPayload, EntrySource, PoolEntry, PoolId, PoolKind,
    WorkerId, WorkerPool, WorkerPoolPolicy, WorkerPoolPolicyConfig,
};

let policy = WorkerPoolPolicy::new(WorkerPoolPolicyConfig {
    min_workers: 1,
    initial_workers: 1,
    max_workers: 4,
    idle_retire_after: Some(TimeSpan::from_secs(30)),
    create_retry_delay: TimeSpan::from_millis(10),
    cpu_intensive_threshold: TimeSpan::from_millis(10),
    manager_managed: true,
    dynamic_create: true,
    idle_retire: true,
});

let pool_id = PoolId::new(PoolKind::new(0), LogicalCpuId::new(0));
let mut pool: WorkerPool<(), 4, 128> = WorkerPool::new(pool_id, policy);

let entry = PoolEntry::new(
    EntrySource::new(1),
    EntryOwner::new(1),
    EntryKey::new(1),
    EntryPayload::new(1),
);
let actions = pool.enqueue_runnable(entry, ktask::monotonic_time()).unwrap();
let _ = actions;

let _ = pool.worker_ready_to_park(WorkerId::new(0), ktask::monotonic_time());
```

## Worker States

```text
Empty
  -> Creating
  -> Idle
  -> Preparing
  -> Claiming
  -> Running
  -> Sleeping
  -> RetireRequested
  -> Exiting
  -> Empty
```

`Creating` reserves a slot while the manager creates a runtime thread. `Idle`,
`Preparing`, `Claiming`, `Running`, `Sleeping`, `RetireRequested`, and `Exiting`
are installed states. `Running` with normal accounting contributes to bounded
concurrency; sleeping and CPU-intensive executions do not.

Each execution has a `WorkerExecutionToken`. Tick, block, resume, finish, and
discard operations must carry the current token so stale operations cannot
mutate a reused worker slot.

## Dispatch Contract

The worker loop calls `worker_ready_to_park()` before blocking. If runnable work
exists, the worker prepares a runnable candidate, drops the pool lock, and asks
the runtime to claim the opaque entry. A successful claim enters `Running`; a
stale claim is discarded and the worker can retry another entry.

The pool owns FIFO runnable selection and deferred storage. The runtime owns
external object validation and callback execution.

## Manager Contract

Fast paths return `ImmediateAction` values for wakeups and bottom-half raises.
Slow lifecycle decisions are represented as `ManagementAction` values and are
processed by a manager task.

The manager is responsible for creating workers after the pool reserves a
`Creating` slot and for requesting idle retirement. Worker exit happens in the
worker's own context. A pending retire request can be canceled before the worker
enters `Exiting`; once `Exiting` is reached, the slot is released only by
`worker_exit_complete()`.

The manager handles a small bounded number of actions per pass so one busy pool
does not monopolize the manager loop.

## CPU-Intensive Accounting

A running worker starts with normal concurrency accounting. If scheduler ticks
show that the same execution exceeds the policy threshold, the worker is marked
CPU-intensive. That execution keeps running, but it no longer consumes bounded
concurrency. If runnable entries remain, the pool can wake or create another
worker.

The ktask integration stores only an opaque pool/worker/token identity in
`ktask::TaskExecutionContext`. Product-layer work identity never enters
`ktask`.

## Locking Rules

`WorkerPool` is a state container and must be protected by the runtime's pool
lock. Returned actions must be executed after the lock is dropped. Runtime
callbacks, worker creation, wakeups, and blocking waits must not run while the
pool lock is held.
