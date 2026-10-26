# kmutex design

## Purpose and scope

`kmutex` owns the blocking mutex formerly implemented in `ksync`: owner state,
adaptive contention policy, event-listener wakeups, the `lock_api` wrapper,
and lock statistics. It is `no_std` and has no `ktask` or `khal` dependency.
`ksync` re-exports `Mutex`, `RawMutex`, `MutexGuard`, and `SpinConfig`; it retains
its separate rwlock spin helper, using the same configuration type.

## Architecture

- `src/mutex.rs`: the single mutex implementation and compatibility wrapper.
- `src/spin.rs`: `SpinConfig` and private mutex adaptive spinning.
- `src/lib.rs`: mutex exports; task contracts are imported from `kiface-task-api`.
- `kiface-task-api::TaskExecutionIf`: task identity, yielding and synchronous waiting.
- `kiface-task-api::TaskLockDiagnosticsIf`: optional task diagnostic bookkeeping.

The host task provider supplies identity, scheduling, future execution and
diagnostics. It owns timestamps and task bookkeeping. It does not own mutex
state, listener admission, wakeup policy, or lock-class accounting.

## Execution context

Acquisition requires a current task with a stable unique owner key. The
provider returns `core::num::NonZeroU64`, mechanically excluding the unlocked
sentinel; `RawMutex` extracts its numeric value with `.get()`.
Blocking acquisition requires sleepable context, with no IRQ/preemption or
spinlock exclusion preventing scheduling. Early boot before task installation
and interrupt contexts are unsupported. `try_lock` never yields or waits;
startup callers must still have a valid task and satisfy host hook constraints.

Guards use `GuardNoSend`: another task cannot take responsibility for unlock.
CPU migration is allowed because identity belongs to the task, not its CPU.
Recursive `lock` panics; recursive `try_lock` returns false. There is no poisoning.
The provider must not reenter this mutex or unwind from diagnostic hooks.

## State and algorithm

Zero in `owner_id` means unlocked; a nonzero key identifies the owner. A
successful Acquire CAS grants exclusion and observes the previous owner's
protected writes. The blocking path uses weak CAS retries; `try_lock` uses one
strong CAS so spurious failure cannot report contention.

After bounded exponential spinning and task yielding, the waiter registers a
stack listener, rechecks owner state with Acquire, and waits only if still held.
Registration before recheck closes the unlock-before-wait race. The listener
future is safely pinned and synchronously lent to `TaskExecutionIf::block_on`, which
must drive it to completion without retaining its borrow. Waking does not grant
ownership: the task reloads owner state and retries the CAS.

Unlock swaps the owner to zero with Release, checks the releasing task key,
updates diagnostics, and calls `event.notify(1)`. Ownership handoff and wakeup
ordering are unchanged by extraction. There is no FIFO or starvation guarantee.

## Features and diagnostics

`stats` preserves successful-acquisition counting and counts contention once
per blocking acquisition that reaches a wait, even if it waits repeatedly.
`RawMutex::new` uses the no-op class; `Mutex::new` binds the caller's init-site
class under `stats` and is otherwise const. Explicit stats constructors remain.

`watchdog` gates invocation, not declaration, of all diagnostic hooks.
`lock_acquired(address)` clears waiting diagnostics and records ownership for
both acquisition paths. `lock_waiting(address)` lets the host record its clock
before each wait; `lock_released(address)` removes ownership before notification.
Compared with the old try-lock diagnostic path, the unified acquired hook also
clears waiting diagnostics. Numeric task keys replace task-name diagnostics.

## Lifecycle and validation

`lock_api` guards release on drop, including unwinding of caller code. Leaking a
guard leaks exclusion; the crate does not recover abandoned owners. Listener
storage remains stack-owned and is dropped after each attempt.

The guard alias includes a type-only compile-fail doctest proving it is not
Send. Scheduler contention, wakeup and migration tests remain with the real
`ktask`/`ksync` integration; a second mock scheduler is not introduced here.
Workspace dependency wiring and the concrete provider are integration-owned.

## Private Backoff Helpers

The short Spin helper remains private here and in ksync's RwLock implementation.
Their current step algorithm is similar, but Mutex owns one acquisition budget
while RwLock owns owner-sensitive budget selection/reset and reschedule checks.
Sharing the public SpinConfig preserves the existing caller API; exporting an
execution-state helper from kmutex solely to remove this small duplication
would make it responsible for another lock's internal strategy. This is an
intentional local duplication, not a second mutex implementation.
