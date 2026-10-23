# kcompletion design

## Purpose and scope

`kcompletion` supplies synchronous counting completion and predicate broadcast
waiting without scheduler or poll dependencies. Kernel IRQ descriptors can own
the non-generic `Completion`; RNG adapters can own `WaitQueue` while multiple
readers independently evaluate driver state. Hosts provide `SyncWake` and a
matching parker. Source is in `src/completion.rs`, guest tests in `ut-tests`,
and the standalone host probe in `tests/host`.

## Architecture

Both public sources use the same private `CompletionState`, `LinkedList<WaitAdapter>`,
notification helpers and `with_waiter` scope. `WaitQueue` leaves the shared
state's token field unused at zero. This keeps one registration/lifetime
protocol. The queue does not own the nodes or notification capability.

The list uses workspace-pinned `intrusive-collections` with an
`UnsafeRef<WaitNode>` adapter and embedded `LinkedListLink`. Entries neither
allocate nor own node storage. All link reads and writes hold the source lock,
so non-atomic links suffice. Cursor removal checks membership first; only the
private registration can enroll its node, and only into its own source.

```
Completion / WaitQueue -- SpinNoIrq -- LinkedList<WaitAdapter>
                                          |
                               pinned synchronous call stack
                                          |
                                borrowed host SyncWake
```

Each node stores intrusive links, an erased shared pointer, its matching
monomorphized notification function, and `PhantomPinned`. The private
registration guard retains node and wake borrows and unlinks under the source
lock on return or unwind. The caller cannot obtain or forget that guard. Both
raw waits are unsafe host boundaries: borrowing cannot prevent nonunwinding
task exit from reclaiming an abandoned stack without running cleanup.

## Context and lock ordering

Construction does not allocate or require a task. Queries and notification
require platform support for `SpinNoIrq` and may execute in IRQ context.
Blocking waits require the host to validate sleepable context before entry.
The crate cannot validate task, IRQ masking, RCU, or scheduler state itself.
The host must release unrelated locks forbidding sleep before waiting. It must
keep the execution stack, source and borrowed wake alive until private cleanup
finishes, including abnormal nonunwinding task exit. It may instead prevent all
paths bypassing cleanup. Ordinary Rust unwinding is supported.

Notification holds the source lock through callback completion. A callback
must not allocate, block, unwind, retain borrowed storage, or reenter the source.
No callback lock may have an owner that waits for the source lock. Parking and
predicate callbacks execute after dropping the source lock. Predicate callbacks
may reenter the source, including `notify_all`, but must not sleep.

The parker must latch early notification and tolerate spurious returns. A host
using the ktask protocol retains a strong task reference, sets the notification
latch before scheduler wake, and publishes Blocked before releasing the latch
inside the scheduler handshake. A safe wrapper must retain the task/stack owner
across the entire raw wait, including predicate execution, not only inside park.
If task exit skips Drop, that owner must leak instead of permitting reclamation.
Wake or source storage outside that stack needs its own lifetime guarantee.
SyncWake alone cannot constrain arbitrary closures from exiting the task.

## Completion state and flow

`done == 0` is empty, intermediate counts are consumable tokens, and
`usize::MAX` is sticky completion. `complete` increments with saturation and
notifies at most the oldest waiter. Saturation intentionally matches Linux:
incrementing `usize::MAX - 1` enters sticky state. This differs from the old
kpoll completion, which caps ordinary tokens at `usize::MAX - 1`.
`complete_all` publishes sticky state and drains the current queue.

`wait_with` checks/consumes a token under the source lock or inserts its stack
node, then parks outside the lock. A locked `is_linked` check skips duplicate insertion, keeping
FIFO position after spurious returns. A notified node is removed, so a waiter
whose token was consumed by another caller reenrolls. Notification counts are
not token-consumption counts; FIFO notification does not reserve tokens.

## Predicate flow

Each `wait_until` iteration unlinks any old enrollment and reenrolls under the
same lock, then evaluates the predicate outside it. If false, it parks. A wake
between enrollment and parking is retained by the host latch. A wake before
enrollment is covered by the subsequent predicate check. Each reader retains
its own predicate semantics; broadcast creates no completion token.

## Deferred publication and reset

`complete_all_defer_wake` publishes sticky state without invoking callbacks and
returns an Arc clone retaining the source. An outer lock can therefore protect
related state publication, followed by `DeferredWake::wake` after unlocking.
Dispatch drains the current list; it never restores tokens or completion state.
It may run after `reinit`, giving only a spurious hint to the new generation.

Reset must not abandon waiters depending on an earlier completion. Predicate
users recheck authoritative state and arrange a subsequent completion if needed.
There is no generation fencing and no requirement that deferred dispatch happen
before reset. Dropping an undispatched handle does not wake parked waiters.

## Resources and limitations

Construction, enrollment, unlink, notification and repeated waiting allocate
and deallocate nothing. Deferred dispatch clones an existing Arc without
allocation; its final release may deallocate the completion. IRQ use of that
release depends on the allocator's context contract. Broadcast holds IRQs off
for work linear in the number of enrolled waiters. No timers, cancellation,
signal interruption, fairness guarantee for token acquisition, or polling are
implemented. Unwinding from host closures unregisters before stack reclamation;
nonlocal exit bypassing Rust cleanup requires the unsafe caller to retain all
referenced storage. Such retention may leak the stack and registration forever;
it preserves memory safety, not resource reclamation or liveness.

## Validation

### Source-lock notification tradeoff

Notification includes the host scheduler wake, not just setting a latch. In
ktask it can acquire runqueue and tracing locks and send a remote reschedule
IPI. Thus IRQ-off duration includes source-lock contention plus all enrolled
waiters' scheduler/trace/IPI costs; there is no fixed microsecond bound and no
latency measurement is claimed. This is a larger source-lock scope than the
previous poll slot snapshot, even though it removes allocation from registration.

Linux `kernel/sched/completion.c` likewise calls `swake_up_locked` and
`swake_up_all_locked` under `raw_spin_lock_irqsave`; `kernel/sched/swait.c`
calls `try_to_wake_up` before releasing that source lock. This is a structural
comparison, not proof of equal latency on our scheduler.

The lock also excludes retirement of borrowed stack waiters. Moving detached
nodes or borrowed wake pointers outside it would permit use-after-return.
Unlock-before-dispatch requires a separately owned, allocation-free scheduler
wake-batch mechanism first; `DeferredWake` only releases an outer producer
lock, not this lifetime constraint. Such a prerequisite and paired latency
measurements must precede any future optimization; real-time bounds are not
provided by the current implementation.

See [validation.md](validation.md) for the contract matrix, negative controls,
reproduction commands and evidence limits.

See `tests/host/README.md` for the standalone command and allocation measurement
boundary. Kernel tests exercise tokens, saturation, FIFO/token stealing,
spurious returns, predicate reentry, deferred lifetime and deferred reset.
Scheduler integration and real IRQ/preemption behavior require kernel tests.

The intrusive-collections migration passed build and Clippy, 755 related
AArch64 kernel tests, 13 host tests and 13 Miri tests. Existing allocation probes
still report zero allocations/frees in their measured wait/notify intervals.
Local logs: `/tmp/completion-intrusive-{build,clippy,ut,host,miri}.log`.
