# kmutex design

## Purpose and scope

`kmutex` owns the blocking mutex formerly implemented in `ksync`: owner state,
adaptive contention policy, the owned FIFO wait queue, the `lock_api` wrapper,
and lock statistics. It is `no_std` and depends on the scheduler only through
`kiface-task-api` and the `ktask-types::OwnerKey` value type; it has no `ktask`
or `khal` dependency. `ksync` re-exports `Mutex`, `RawMutex`, `MutexGuard`, and
`SpinConfig`; it retains its separate rwlock spin helper, using the same
configuration type.

## Architecture

- `src/mutex.rs`: the single mutex implementation and compatibility wrapper.
- `src/wait.rs`: stack-pinned waiter nodes, the intrusive FIFO wait list, and
  the latch future that parks a blocked task.
- `src/spin.rs`: `SpinConfig` and private mutex adaptive spinning.
- `src/lib.rs`: mutex exports; task contracts are imported from `kiface-task-api`.
- `kiface-task-api::TaskExecutionIf`: task identity, yielding and synchronous waiting.
- `kiface-task-api::TaskLockDiagnosticsIf`: optional task diagnostic bookkeeping.
- `ktask-types::OwnerKey`: the shared owner-key boundary type carried by the lock word.

The host task provider supplies identity, scheduling, future execution and
diagnostics. It owns timestamps and task bookkeeping. It does not own mutex
state, waiter admission, wakeup policy, or lock-class accounting.

## Execution context

Acquisition requires a current task with a stable unique owner key. The
provider returns `ktask_types::OwnerKey`, which keeps the unlocked sentinel
excluded and the low flag bits clear; `RawMutex` extracts its numeric value
with `.as_raw()`. Blocking acquisition requires sleepable context, with no
IRQ/preemption or spinlock exclusion preventing scheduling. Early boot before
task installation and interrupt contexts are unsupported. `try_lock` never
yields or waits; startup callers must still have a valid task and satisfy host
hook constraints.

Guards use `GuardNoSend`: another task cannot take responsibility for unlock.
CPU migration is allowed because identity belongs to the task, not its CPU.
Recursive `lock` panics; recursive `try_lock` returns false. There is no poisoning.
The provider must not reenter this mutex or unwind from diagnostic hooks.

## State and algorithm

`owner_id` is the lock word: the owner key in the high bits, with
`WAITERS_FLAG` (bit 0, Linux `MUTEX_FLAG_WAITERS`) set while the wait list is
non-empty, or `0` when unlocked. The key is already flag-reserved — its low
`OwnerKey::FLAG_BITS` bits are zero — and the `OwnerWord` wrapper masks the
flag bits before every ownership test. A successful Acquire CAS from a zero
owner-key part grants exclusion and observes the previous owner's protected
writes; the CAS preserves any set flag bits, as Linux `__mutex_trylock_common`
does.

After bounded exponential spinning and task yielding, a waiter parks under
`wait_state`, a task-context spinlock (preemption disabled, no IRQ masking)
that serializes every wait-list mutation and flag transition:

1. Re-check the owner with a mask-aware try-acquire while holding
   `wait_state`; success returns without queueing.
2. Push the stack-pinned node at the tail, then publish `WAITERS_FLAG` with a
   flag-only `fetch_or` from the resulting list state.
3. Re-read the word. A zero owner-key part means an unlock raced this
   admission: unlink the node, clear the flag if the list drained, and retry
   instead of sleeping. Otherwise release `wait_state` and block on the
   node's latch.

The waiter's flag publish and its owner re-read touch the same word, so the
lost-wakeup window reduces to single-location coherence: an unlock that clears
the owner part either precedes the `fetch_or` and is seen by the re-read, or
follows it and returns a word whose preserved `WAITERS_FLAG` routes it to the
slow path.

The hibernation is a small `WaitFuture` driven by `TaskExecutionIf::block_on`.
The future and the waking task agree through one atomic `state` on the node
(`Empty -> Registered -> Claimed -> Done`), with no lock: a poll stores the
waker and publishes `Registered`, or resolves once it observes `Done`; the
waking task's `latch` first claims the waker (`Registered -> Claimed`), takes
it, and only then publishes `Done`. A wake that beats the first poll claims
straight from `Empty` to `Done`, so the next poll resolves immediately, and a
wake racing registration either claims the waker or is observed by the failed
poll. The pinned node may be read by the unlocking task while the waiter can be
re-polled at any time (its `block_on` park may return spuriously or be woken by
an external `wake_task`/interrupt), so a poll frees the frame only on `Done`,
which is published after the node's last access. The waking task drops
`wait_state` before `latch` and calls `wake_by_ref` after it, so the wake runs
outside the wait lock; `latch` is the unlocking task's last access to the node
and the future never escapes the synchronous call.

Wake selection is FIFO (the unlocking task pops the list head), but
acquisition is not: spinners and `try_lock` callers may barge ahead of sleeping
waiters, and there is no handoff, so starvation remains unbounded. A woken
task does not inherit ownership; it reloads the word and retries the CAS.

Unlock clears the owner-key part with `Release`, keeping the flag bits, updates
diagnostics, and inspects the `WAITERS_FLAG` of the value it returned. Only
while that flag was set does it take `wait_state`, pop one waiter, restore or
clear the flag from the remaining list state, latch the waiter, and wake it
after releasing the locks. Preserving the flag across the unlock keeps a
barging acquire-and-release from dropping the wakeup: the barging task inherits
the flag, so its release enters the slow path too. The flag mirrors list
non-emptiness and is maintained under `wait_state` after every list mutation.
The unlock-side owner-key lookup is a `debug_assert_eq!` only: `GuardNoSend`
confines the guard to the acquiring task, so a mismatched release is reachable
only through raw-API misuse, and release builds pay no cross-crate owner-key
call.

## Features and diagnostics

`stats` preserves successful-acquisition counting and counts contention once
per blocking acquisition that reaches a wait, even if it waits repeatedly.
Acquisitions are counted on both the fast CAS and the wait-lock re-check path.
`RawMutex::new` uses the no-op class; `Mutex::new` binds the caller's init-site
class under `stats` and is otherwise const. Explicit stats constructors remain.

`Mutex::with_config(value, config)` selects a per-lock spin policy and retains
the same caller-site class registration as `Mutex::new` under `stats`. It is
const only without `stats`; runtime callers do not need an untracked raw-lock
constructor. Equal spin and yield thresholds use bounded pauses before the
ordinary queue-and-park path. This neither changes the default policy nor
adds an acquisition fairness or owner-progress guarantee.

`watchdog` gates invocation, not declaration, of all diagnostic hooks.
`lock_acquired(address)` clears waiting diagnostics and records ownership for
both acquisition paths. `lock_waiting(address)` lets the host record its clock
before each wait; `lock_released(address)` removes ownership before notification.
Numeric task keys replace task-name diagnostics.

## Lifecycle and validation

`lock_api` guards release on drop, including unwinding of caller code. Leaking a
guard leaks exclusion; the crate does not recover abandoned owners.

Waiter nodes are stack-owned by the blocking call. A node is linked into the
wait list only while its frame is blocked, and the unlocking task unlinks it
before setting its latch. Task exit must therefore unwind through the blocking
call or retain the waiter storage; a non-unwinding exit while queued would
leave a dangling link. This is the retention obligation that any
stack-registered waiter requires from the host.

The guard alias includes a type-only compile-fail doctest proving it is not
Send. Scheduler contention, wakeup and migration tests remain with the real
`ktask`/`ksync` integration; a second mock scheduler is not introduced here.

## Private Backoff Helpers

The short Spin helper remains private here and in ksync's RwLock implementation.
Their current step algorithm is similar, but Mutex owns one acquisition budget
while RwLock owns owner-sensitive budget selection/reset and reschedule checks.
Sharing the public SpinConfig preserves the existing caller API; exporting an
execution-state helper from kmutex solely to remove this small duplication
would make it responsible for another lock's internal strategy. This is an
intentional local duplication, not a second mutex implementation.
