# kpoll — Security and Reliability Analysis

## Unit-Test Waker Ownership

Counter and reentrant fixtures use Arc-backed Wake implementations, so every
cloned waker retains its state without leaked Box storage. The reentrant state
owns a PollSet clone and is released after registration and waker teardown.
The last-strong-owner scenario retains its custom clone hook: its raw wakers
borrow stack-pinned state declared before every waker and registration. Their
drop hook is a no-op and never reconstructs an allocation owner.

## Weak Source Lifetime

Weak capture cannot prolong source ownership or revive a destroyed source.
Registration guards remain weak even after a successful temporary upgrade.
The existing token checks govern cancellation and late wakes; source expiry is
not a registration error or a readiness event. Temporary-owner release occurs
after source locking ends, but callers must also exclude outer callback locks.
No new unsafe production code or allocator-failure policy is introduced.

## Trust Model

```
User-space syscall / kernel subsystem
        │
        │ Pollable / PollContext / PollSet API
        v
┌──────────────────────────────────────────┐
│ kpoll                                    │
│  - PollRegistrations lifetime is held by the caller │
│  - The PollSet slot table is protected by SpinNoIrq │
│  - registration holds only Weak<source>  │
└──────────────────────────────────────────┘
        │
        │ Waker::wake (outside the lock)
        v
   Scheduler / task resume
```

- The caller is responsible for: holding the registration owner, rechecking after register, and mapping
  `PollRegisterError` to a user-visible error.
- `kpoll` is responsible for: token correctness, cancel/wake linearization, calling the Waker without holding a lock in wake,
  not executing `Waker::clone`/cancel-path drop inside `SpinNoIrq`, and returning on registration growth failure.

## External Boundaries / Attack Surface

Upon review, `kpoll`:

- **does not directly access user memory**;
- **does not directly operate MMIO / PIO / DMA**;
- **does not parse firmware / DT / ACPI**;
- **has quantities indirectly controlled by the user**: number of concurrent waiters, number of poll fds, number of epoll interests.

Therefore the main attack/abuse surfaces are:

- Memory pressure caused by a large number of concurrent registrations;
- Forgetting to hold `PollRegistrations`, causing hangs or stale waiters;
- Calling `wake` at the wrong lock level, causing deadlock;
- Treating wake as a reliable state notification and omitting the recheck.

## unsafe Code Inventory

The current `kpoll` implementation itself contains no `unsafe` code blocks. The custom
`RawWaker` in the unit tests uses `unsafe`; every site has a `SAFETY:` comment, and it is used only in unittests.

## Memory Safety Invariants

1. The `(slot, id)` of each active slot is unique; deregistration must match both.
2. `next_id` increases monotonically and is never wrapped around or reused; exhaustion returns `IdExhausted`.
3. `PollRegistration` holds only `Weak<PollSetInner>` and cannot prolong the source lifetime.
4. After wake takes the slot, the corresponding token is permanently invalidated; a late Drop is a no-op.
5. If a composite registration fails midway, the items already registered successfully are still held by the owner, and clear/drop rolls them all back completely.
6. `PollEvent` generations signal state changes; callers recheck their own
   predicates and never interpret notifications as consumable completion tokens.

## Thread Safety

`PollSet::register` returns a weak RAII guard. Keep it across the logical wait
and recheck readiness after registration. Dropping it cancels a queued waiter
but cannot retract a detached late wake. Synchronous completion state and wait
node lifetimes are outside this crate's boundary.

- `PollSet: Send + Sync`: the internal `SpinNoIrq` protects the slot table.
- `PollRegistration` / `PollRegistrations`: moved by ownership along a single logical wait path;
  sharing the same owner across threads is not required.
- `Waker::wake` may re-enter `PollSet::wake`; because the drain happens outside the lock, it will not deadlock on the same lock.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | A large number of waiters exhaust memory | Medium | Amplification by concurrent user poll/epoll | `try_reserve` returns `NoMemory`; syscall maps it to `ENOMEM` |
| T-02 | Waiters remain after the wait ends | Medium | Missing owner / forgetting to clear | Ordinary poll goes through `PollContext`, and the single-source bridge holds a single guard; RAII Drop deregisters |
| T-03 | ABA wrongly removes a new waiter | High | Old guard dropped after slot reuse | The token contains a monotonic `id` |
| T-04 | IRQ wake re-entrancy deadlock while holding a lock | High | `wake` called inside an outer lock and the callback takes the lock in reverse | wake runs after the outer lock is released; register clone / cancel drop are both outside the lock |
| T-05 | OOM panic instead of an error return | Medium | Bare `push`/`with_capacity` in the registration path | The registration path uniformly uses `try_reserve`; the known limitation is only the `Arc::new` in `PollSet::new` |
| T-06 | A late wake is treated as the final state | Low | Race between cancel and wake | Documented convention that wake may be late; callers must recheck |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Registration failure | Heap growth failure / ID exhaustion | This wait fails immediately | User receives `ENOMEM` | 3 | Error propagated to syscall/async |
| F-02 | Lost wakeup | check/register race | Task keeps sleeping | Blocking hang | 2 | Mandatory recheck after register; block_on woke flag |
| F-03 | Wrongly removing a concurrent wait | Deduplicated deregistration by Waker | Another wait never wakes | hang | 1 (eliminated) | Independent token per logical wait |
| F-04 | Allocation failure on the wake path | New buffer on the hot path | IRQ allocation/panic | System unstable | 1 (eliminated) | Under the lock, only move out the slot table and reset metadata; traversal and `wake()` both run outside the lock |
| F-05 | Source destroyed first | fd/device released before waiters | Waiters never woken | hang | 2 | source Drop wakes all waiters |

## Fault Management

- Registration failure: returns `PollRegisterError`, which `ktask`/`ksyscall` map to
  `KError::NoMemory` or `InvalidInput` (`InvalidState`).
- Normal cancellation: `PollRegistrations` Drop/clear.
- Late wake: allowed, not treated as an error.
- panic is not used in normal control flow; assertions in unittests are the exception.

## Privacy Analysis

The module does not process user data content; it only stores the task `Waker` and registration metadata.

## Known Limitations

1. `PollSet::new` still depends on the infallible `Arc::new`; the repository has no general `Arc::try_new` yet.
2. `Waker::clone` may still allocate in extreme custom waker implementations; the kernel default `KWaker`
   uses Arc reference counting.
3. The cost of a single `wake` broadcast is still O(n); the lifecycle guard prevents unbounded accumulation. The cost under the lock only includes
   detaching the slot table and resetting metadata; the actual traversal/wakeup runs outside the lock.
4. Multi-waiter event sources such as IRQ register directly through the caller-provided `PollContext`,
   and the registration lifetime is managed by `PollRegistrations` that spans `Pending`.
## Audit Checklist

- [ ] All `Pollable::register` / `register_poll` go through `PollContext`.
- [x] Every `Poll::Pending` path has a live `PollRegistrations` (including signal /
  wait / interruptible / knet-rx; on registration failure, must not sleep without a waiter).
- [x] A readiness recheck always follows register (including the `poll_interrupt` Ready race).
- [ ] Registration growth uses `try_reserve`, and the wake path does not allocate under the lock.
- [x] `wake` does not call `Waker` while holding a lock (including the IRQ descriptor state lock).
- [ ] The outer lock order does not conflict with the wake callback in reverse.
- [ ] An epoll interest clears its registration on DEL/ONESHOT/Drop, and MOD updates the configuration in place and reinstalls the registration.
- [ ] Tests cover >64 waiters, cancel deregistration, token ABA, and wake re-entrancy.

## Advisory waiter snapshot

A waiter snapshot does not reserve registrations or synchronize a later stop
decision. Users must close that registration/stop race explicitly; evdev
publishes its inactive flag before taking the snapshot. Cancellation and wake
still own registration removal.
