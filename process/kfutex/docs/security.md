# kfutex security and reliability

## Scope, assets, and boundaries

All of `src/` is covered. Assets are waiter-list integrity (no
use-after-free of queue entries), the linearization guarantees of
wait/wake/requeue, and kernel availability (bounded work per operation).
Users control futex addresses, expected values, wake counts, bitsets,
requeue targets, and the wake-op encoding; `ksyscall` validates command
shape and clamps counts before calling this crate. Shared identity comes
only from `memspace`-resolved stable backing metadata.

## External boundaries

| Boundary | Direction | Content |
|---|---|---|
| User memory (via `kuaccess`) | read/CAS | futex words: read-only compare loads, nofault compare/CAS under bucket locks |
| Syscall layer | inbound | already range-checked addresses, decoded operations, clamped counts |
| Scheduler (`ktask`) | outbound | wakers invoked after lock release |

## Unsafe inventory

All unsafe code is in `waiter.rs` and carries inline `SAFETY:` notes.

| Location | Operation | Invariant |
|---|---|---|
| `waiter.rs` `unsafe impl Sync for FutexWaiter` | auto-trait override | `route` (`UnsafeCell`) is only read/written while holding `route_lock` or the waiter's current bucket lock(s); those exclusion sets cover all accessors |
| `waiter.rs` `route()` | `&*self.route.get()` | exclusive access via `route_lock` |
| `waiter.rs` `route_unlocked()` | `&*self.route.get()` | caller holds the covering bucket lock(s), excluding `requeue_to` writers |
| `waiter.rs` `requeue_to()` | `&mut *self.route.get()` | `route_lock` held plus caller-held bucket lock(s) exclude all readers |

User-memory atomics (`atomic_u32_eq`, `atomic_cmpxchg_u32_nofault`, ...)
are `kuaccess` calls, not unsafe in this crate.

## Memory-safety invariants

1. A private key must contain `mm_id`; a virtual address alone must never
   match across address spaces.
2. A shared key must contain the object-relative page index, never a
   VMA-relative offset.
3. Buckets live forever; waiters are only ever held through `Arc`; the
   queues never contain raw waiter pointers.
4. Only `Queued` waiters may be woken or requeued; `Woken` and `Cancelled`
   are terminal.
5. Route (key/bucket/generation) mutation requires both source and target
   bucket locks; readers either hold `route_lock` (snapshot) or the
   covering bucket lock(s).
6. User accesses inside bucket locks must use nofault APIs; page-fault
   handling must never run while holding a bucket spinlock.
7. Scheduler wakers are invoked only after the bucket lock is released.
8. Robust owner-death state lives in the user word only; the kernel keeps
   no mirrored owner-death cache that could go stale.

## Lock ordering

```text
lower bucket index -> higher bucket index -> waiter route / waker metadata
```

The cancellation path must not hold a route snapshot as if it were a lock:
it reads the snapshot, releases, takes the bucket lock, then re-validates
generation, which breaks the inverse ordering a requeue would otherwise
create.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Hash-collision scan cost | Low | many distinct keys mapping to one bucket | collisions only add scan time inside one lock; equality checks are unaffected; 256 buckets + seeded hash prevents targeted placement |
| T-02 | Memory blowup via huge wake/requeue counts | Low | `INT_MAX`-style counts | no allocation scales with count; operations only scan existing waiters |
| T-03 | Wake consumed by a cancelled waiter (lost wakeup) | Medium | timeout/signal races with wake | terminal-state CAS: only `Queued -> Cancelled` may return an error; if WAKE won, wait returns success (unit-tested) |
| T-04 | `EFAULT` despite valid mapping (spurious failure) | Low | transient eviction between precheck and enqueue | enqueue retries via waker re-arm; `EFAULT` only when the mapping is genuinely inaccessible |
| T-05 | ABBA deadlock on two-bucket operations | High (if violated) | requeue/wake-op on opposite key orders | global lower-index-first ordering in all double-lock paths |
| T-06 | Stale route cancellation removing the wrong queue entry | Medium | cancel racing with requeue | generation re-check inside the bucket lock before removal |
| T-07 | Scheduler invoked under non-preemptible lock | High (if violated) | waker called during drain | `drain_inactive` collects first, wakes after unlock |

## FMEA

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | Waiter stuck forever | waker lost | future never ready | task hang | 2 | waker stored per waiter under lock; drain re-invokes after terminal state; interruptible wait adds timeout/signal exits |
| F-02 | Requeue moves waiter into wrong bucket | route update bug | wakeups miss the waiter | user-level deadlock | 2 | route change only under both locks; unit tests pin key/bucket/generation |
| F-03 | Bucket lock contention | 256-way collisions | latency | degraded throughput | 4 | accepted; sharding is a tunable constant |
| F-04 | Lazy init race on first futex use | concurrent first calls | — | none | 4 | `klazy::lazy_static!` publishes once |

## Failure handling

Errors surface as `KError` (`EFAULT` for inaccessible words, `EAGAIN`
(`WouldBlock`) for compare mismatches, `EINTR`/`ETIMEDOUT` via the
interruptible wait). No panics are reachable from user input: the two
`expect`s in requeue/drain loop invariants are backed by the lock-held
length snapshot and are not user-triggerable. Timed waits interrupted by
signals return `EINTR`. The syscall layer maps an untimed interrupted wait
to `ERESTARTSYS` and a timed one to `ERESTART_RESTARTBLOCK`, then resumes
it through `restart_syscall`. This crate does not own that restart-block.

## Thread safety

Bucket queues are `SpinNoPreempt`-protected; waiter state transitions are
atomic CAS; route access follows the exclusion rules above. The public
handle is a zero-sized `Copy` value.

## Privacy analysis

No user data beyond 32-bit futex words is read; nothing is logged.

## Known limitations

- Non-PI futex only; PI commands return `ENOSYS` at the syscall layer.
- Realtime absolute waits are converted once at syscall entry to a
  monotonic-relative duration; wall-clock jumps during the wait do not
  re-adjust an already-queued deadline.
- Timed waits interrupted by a signal still return `EINTR` from this crate.
  `ERESTART_RESTARTBLOCK` and `restart_syscall` belong to `ksyscall`.

## Audit checklist

- Any new compound operation must take buckets in index order and use
  nofault user accesses inside the locks.
- New waiter state transitions must remain single-terminal (CAS) and be
  covered by a unit test.
- Cancellation must re-validate route generation under the bucket lock.
- Wakers must never be invoked while a bucket lock is held.
