# krtmutex security and reliability

This analysis covers the entire crate: `src/lib.rs`, `src/host.rs`,
`src/graph.rs`, and `src/mutex.rs`.

## Trust boundaries

Kernel callers place protected data in `RtMutex<T>`. The `PiHost` provider
supplies task identity, one `PiTaskNode` per task, the donation level, and
the write of a boost into the scheduler. The level must be that task's own
effective priority, including any boost it already inherited. `set_boost`
must not take a run-queue lock while the graph lock is held.

The graph does not read user memory. `RtMutex` selects the current task;
`RawRtMutex` additionally accepts a validated live owner from a trusted host.
Its wait is task-bound and borrows the raw lock; success or a grant winning
cancellation requires explicit unlock. Raw ownership protects no Rust data.

## External boundaries

This crate does not access user memory, MMIO/PIO, DMA, firmware, filesystems,
network data, or device input. Lock contention can indirectly reflect
external workload. The crate cannot establish scheduler progress on its own.

The futex user-word protocol is outside this crate. A later layer that turns
a TID stored in a user word into an owner owns that trust boundary. The
current-task API here does not establish it.

## Unsafe inventory and invariants

Every `UnsafeCell` in the graph is read and written only while `PI_GRAPH` is
held. `GraphToken` is created only inside that lock's closure, so the
critical section is visible in the function signatures.

| Site | Invariant |
| --- | --- |
| `Send`/`Sync` for `Waiter`, `LockState`, and `PiTaskNode` in `graph.rs` | Cells and red-black links are accessed only under `PI_GRAPH`. The key changes only after both links are unlinked. The task reference is itself `Send + Sync`. |
| `Waiter::key`, `set_priority`, `is_granted`, `grant`, `register_waker` | Called with the graph lock held. `set_priority` runs only after the waiter was unlinked from both trees. |
| `LockAdapter::get_key`, `DonorAdapter::get_key` | The trees are queried only under the graph lock, and the key does not change while the link is live. |
| `LockState::inner`, `PiTaskNode::blocked`, `set_blocked`, `donors` | The graph lock is not re-entered, so the references live only inside the closure. |
| `Send`/`Sync` for `RtMutex` | The lock gives `&mut T` to one owner at a time, so `T: Send` is sufficient. |
| `Sync` for `RtMutexGuard` | Sharing a guard only shares `&T`, so `T: Sync` is required. The guard is not `Send`. |
| `Deref` / `DerefMut` | The guard proves the current task owns the lock. `&mut self` on `DerefMut` excludes other borrows through the same guard. |

## Memory safety

- A waiter's `Arc` keeps the lock state alive while the waiter is linked.
- Handoff and waker registration share the graph lock, so a wakeup is not lost.
- Preemption stays disabled through deboost and wake delivery, preventing a
  medium-priority peer from delaying the new owner before it becomes runnable.
- `wake` and `set_boost` are not called under the graph lock, so they do not invert the order with a run-queue lock.
- Every submitted boost is rechecked against the highest donor; stale writes
  retry before propagation returns or follows the owner chain.
- The guard cannot move to another task, so the task that locked is the task that unlocks.

## Thread safety

`RtMutex<H, T>` is `Send + Sync` when `T: Send`. Several tasks may call
`lock` and `try_lock` concurrently. Exclusive access to the value exists only
through the guard. The global graph lock serializes structural updates of
every inheritance lock.

## Threats

| Threat | Condition | Impact | Control | Residual |
| --- | --- | --- | --- | --- |
| Priority cycle | Two tasks wait for locks the other holds | Both sleep forever | The enqueue walk follows `blocked_on`. Returning to the waiter, or passing 1024, returns `EDEADLK` and does not insert | A chain longer than the bound is also reported as deadlock, even without a cycle |
| Run-queue lock under the graph lock | `set_boost` or a wake is called inside the graph-lock closure | Deadlock with a task blocked in `schedule` | `propagate` and `wake` run after the graph lock is released | A host that takes a run-queue lock inside `donor_priority` still breaks the order |
| Owner disappears during unwind | Handoff completed, then the wait is dropped | Later waiters never obtain the lock | `Drop` of the wait passes the lock on when it was already granted | None in this path |
| Mutex dropped while waiters are blocked in `lock` | The caller frees the mutex while other tasks still hold `&RtMutex` inside `lock` | Those tasks resume against freed memory | `Drop` only clears leftover boosts. The mutex must outlive every `lock` call | The type system does not reject a caller that violates this |

## Failure modes

| Mode | Cause | Local effect | System effect | Handling |
| --- | --- | --- | --- | --- |
| Self-lock | The owner calls `lock` or `try_lock` again | `EDEADLK` | The caller must drop the existing guard | No enqueue and no sleep |
| Cycle or over-deep chain | Opposite lock order, or a chain past 1024 | `EDEADLK` | The caller must not assume it was queued | The failure path inserts nothing |
| Propagation hits the depth bound | The chain exceeds 1024 and the deadlock walk did not return first | Boosts above that point may stay stale | Those tasks keep their previous priority | The same bound as deadlock detection |
| Allocation failure | The waiter `Arc` cannot be allocated | The allocator applies its kernel policy | The enqueue does not complete | Allocation happens under the graph lock. A failure must not leave a half-linked node |

## Failure handling

Errors are `KResult`. A deadlock is `KErrorKind::Deadlock`. A task that
already waits on another inheritance lock gets `BadState`. A release by a
task that is not the owner gets `OperationNotPermitted`. Lock-protocol
failure does not panic. A failed owner check while dropping a guard is a
debug assertion only, because the guard cannot leave the task that holds it.

## Privacy

The crate does not process user private data. Task identity exists only as
the reference the host gives the graph. It is not written to a log.

## Known limitations

- Every inheritance lock shares one global graph lock.
- Waiters are allocated while that lock is held.
- Enqueueing on behalf of another task is unsupported. `RtMutex::lock` is
  uninterruptible; raw waits expose cancellation for a host timeout/signal wrapper.
- Deadline bandwidth is not inherited. A Deadline waiter donates RT `99`.

## Audit checklist

- New `set_boost`, `wake`, or run-queue-lock paths still run outside the graph lock.
- `donor_priority` includes a boost the task already inherited, so the chain does not stop at the first hop.
- A waiter's key is changed only after it is unlinked from both trees.
- A failed enqueue leaves no `blocked_on` and no tree node.
- The guard is still not `Send`.

Deferred raw operations return task-bound `PiEffects`. Callers release their
wait-source locks before dropping effects, polling or cancelling a wait.
`unlock_deferred` invokes a trusted nofault commit under the graph lock before
ownership transfer; a commit error leaves graph state unchanged. Its callback
must not block, allocate, acquire graph/scheduler locks or unwind.
