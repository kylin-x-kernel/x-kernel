# kwork Security and Reliability

## Trust Model

`kwork` trusts callbacks as kernel code, but it does not trust callers to use
the correct execution context, teardown order, wait dependency, or resource
lifetime. Sleepable APIs validate context before blocking, and wait sources are
treated only as wake hints; state predicates are always rechecked after wakeup.

Task-context callbacks may sleep. Bottom-half callbacks run in softirq context
and must not sleep or call workqueue wait APIs.

## External Boundaries

`kwork` sits between several kernel subsystems:

- callers submit callbacks and perform flush, cancel, disable, and destroy
  operations;
- `ktask` provides worker tasks, manager tasks, waits, timers, and scheduler
  execution accounting;
- `kirq` provides bottom-half softirq execution and interrupt-context checks;
- delayed work depends on timer cancellation and generation checks;
- callback-captured resources remain owned by the caller.

The crate does not directly access user memory, MMIO, DMA buffers, firmware
tables, filesystem data, network packets, IPC payloads, FFI, inline assembly,
or architecture-specific raw interfaces.

## Unsafe Code

`kwork` contains no `unsafe` blocks, unsafe trait implementations, or raw
pointer dereferences. Address-derived keys are used only as opaque identities;
they are not converted back into references.

## Core Invariants

- Pending entries, running callbacks, and delayed timers hold references that
  keep scheduled work alive.
- Dynamic queue owners are retained by queued, running, and delayed instances.
- `WorkInstanceId` protects pending, running, and delayed state from stale
  timer or completion events.
- `WorkerExecutionToken` protects worker-slot reuse during tick and finish
  accounting.
- Queue binding keys must match the selected per-CPU queue binding.
- Enqueue failure must leave no partial workqueue or worker-pool state.
- Active, in-flight, runnable, deferred, and worker concurrency counters must
  stay consistent with their backing records.
- Worker wake, callback execution, timer registration, and sleepable wait
  happen after internal spin locks are released.
- Completion wake sources are notifications only; waiters must recheck queue
  and work state.
- IRQ-like producers must pre-create work objects and use only immediate
  enqueue paths.

## Threats

| ID | Threat | Impact | Mitigation |
| --- | --- | --- | --- |
| T-01 | Sleepable API called from hardirq, softirq, or BH-disabled context | Deadlock or scheduler misuse | Check `kirq::context` and return `InvalidContext` |
| T-02 | Callback waits on itself or on work that cannot progress in the same bounded pool | Deadlock | Running work records pool identity; wait paths reject impossible dependencies |
| T-03 | Enqueue targets an invalid CPU or an uninitialized pool | Work never runs | Return `InvalidCpu` or `WorkerUnavailable` before committing state |
| T-04 | Shared pending storage is full | Lost or half-queued work | Return `QueueFull` and preserve the previous work state |
| T-05 | Dynamic queue is destroyed while producers still run | Callback may outlive external teardown | Destroy gate rejects later enqueue and flushes accepted work; callers must stop producers first |
| T-06 | Delayed timer fires after cancel or modification | Canceled work is requeued | Generation and instance checks discard stale timer events |
| T-07 | Queue active accounting uses the wrong binding | Other queues are throttled or leaked | Entries carry binding owner and key; promotion and finish filter by binding |
| T-08 | Stale pool entry is claimed | Wrong callback runs or accounting leaks | Claim revalidates work state and repairs stale accounting |
| T-09 | CPU-intensive callback monopolizes a bounded pool | Other work stops progressing | Scheduler tick accounting marks the worker CPU-intensive and wakes management |
| T-10 | Worker creation fails repeatedly | Manager busy-loop | Retry deadline throttles create attempts |
| T-11 | Bottom-half callback sleeps | Softirq context corruption | Sleep and wait APIs fail from interrupt-like context |
| T-12 | Budgeted poller loses notify while running | Device or network backlog stalls | `RunningPending` records missed wake and schedules another round |

## Failure Handling

Public operations return typed results: enqueue paths use `QueueWorkResult` and
`QueueDelayedWorkResult`; non-blocking cancellation uses `CancelWorkResult`;
wait and teardown paths use `WorkqueueError`.

Internal stale states do not panic. They complete associated waiters when
possible, repair accounting, and emit warnings so stress tests and watchdogs
can expose the underlying race.

## Known Limits

- `WORKQUEUE_PENDING_CAP` is a fixed per-pool entry capacity.
- `WORKQUEUE_WORKERS_PER_POOL` is a fixed worker-slot limit.
- `WQ_MEM_RECLAIM` and rescuer workers are not implemented.
- CPU hotplug drain and rebinding are not implemented.
- Ordered, unbound, and NUMA-aware policies are not implemented.
- Dynamic bottom-half queues are not exposed.
- Full Linux flusher queue overflow chaining is not implemented.
