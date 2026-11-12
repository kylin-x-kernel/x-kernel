# kpoll — Design Document

## Weak Source Capture

`WeakPollSet` retains only the identity of an existing source. Capturing or
cloning it neither allocates a new source nor installs a waiter. Registration
temporarily upgrades it and uses the existing one-shot registration owner;
an expired source succeeds before reserving storage or cloning a waker.
Expiry never selects a replacement source. The temporary strong reference may
be the last owner, so registration can trigger source-destruction wakeups.
Callers must release outer locks that waker callbacks could reenter first.

## Positioning

`kpoll` is the core primitive in x-kernel for I/O readiness queries and one-shot waiter registration.
Subsystems such as `poll`/`select`/`epoll`, blocking read/write, signal waits, and TIPC
all use it to convert "condition not ready" into an asynchronous wait that can be woken.

Synchronous completion tokens and blocking waits belong to the independent
`kcompletion` primitive and its task provider. `kpoll` exposes readiness sources
only, with no completion compatibility API. Process exit uses `PollSet` plus
`Process::is_exited()` as its persistent readiness predicate.

## Background

The old implementation stored `Waker`s in a fixed 64-slot array and silently overwrote on overflow, so
waiters on heavily shared fds (for example hackbench) lost wakeups and blocked forever. The subsequent unbounded `Vec<Waker>` fixed
the lost wakeups, but it lacked:

- registrations that are unregistered along with the wait lifetime;
- fallible registration growth;
- a zero-allocation guarantee on the wake path.

This refactor introduces a per-registration token plus an RAII owner, aligning with the Linux `poll_wqueues`
semantic boundary of "growable, fallible, reclaimed when the call ends", while retaining the Rust async/`Waker`
model.

## Scope

```
task/kpoll/
├── src/
│   ├── lib.rs           # Pollable trait and public exports
│   ├── events.rs        # IoEvents
│   ├── source.rs        # PollSet / slot table / wake
│   ├── event.rs         # PollEvent generation-based broadcast source
│   └── registration.rs  # PollRegistration / PollRegistrations / PollContext
├── ut-tests/
│   └── kpoll_unittest.rs # PollSet / PollEvent regression tests
└── docs/
    ├── design.md
    └── security.md
```

Main consumers:

- `task/ktask`: `poll_io`, `interruptible`, task interrupt, GC wait;
- `fs/kvfs`: `FileOperations::register_poll`;
- `core/ksyscall`: poll/select, wait, signal;
- `process/kfd_objects`: eventfd/epoll, etc.; `fs/kvfs`: pipe/FIFO;
- `net/knet`, `io/ktty`, `arch/kirq`, `tee/tipc*`.

## Architecture

```
Pollable::register(context, events)
        │
        v
   PollContext ──register──> PollSet
        │                       │
        │                       ├─ slot table (SmallVec inline + fallible spill)
        │                       └─ SpinNoIrq state
        v
 PollRegistrations (owner across Poll::Pending)
        │
        └─ Drop / clear() ──unregister──> corresponding slot
```

| Component | Responsibility |
|------|------|
| `IoEvents` | Linux poll event bits |
| `PollSet` | IRQ-safe broadcast source; holds the slot table of active waiters |
| `PollRegistration` | RAII guard for a single registration (Weak back-reference to the source) |
| `PollRegistrations` | All guards owned by one logical wait |
| `PollContext` | Short-lived registration capability for the current poll round |
| `Pollable` | Readiness snapshot + registration through `PollContext` |
| `PollEvent` | generation-based broadcast wake source |

## Calling Constraints / Execution Context

- **`PollSet::wake` may be called from IRQ context**: it uses `SpinNoIrq` internally; under the lock it moves out the old slot table and
  resets active/free_head, and calls `Waker::wake` outside the lock. A whole wake acquires the spin lock only once and
  allocates no new slot table; the old heap capacity is released along with the moved-out table and is not recycled into the source.
- **`register` / `unregister` may be called from task context**; they may also run concurrently with an IRQ wake.
  `Waker::clone` completes before taking the lock; a `Waker` removed by cancel is dropped after unlocking, to prevent a custom
  waker callback from reentering inside `SpinNoIrq`.
- **Do not call `wake` while holding an outer lock that may be inverted with scheduler/Waker callbacks**.
- **Registration can fail**: a growth failure returns `PollRegisterError::NoMemory`; ID exhaustion returns
  `IdExhausted`; a target that is not ready returns `InvalidState`. Upper layers map these to `KError::NoMemory` /
  `ENOMEM` or `InvalidInput` / `EINVAL`.
- **The caller must hold the registration guard across the wait**: ordinary poll uses `PollRegistrations`,
  and a single-source bridge may use `PollRegistration`; it is unregistered automatically on timeout, Ready, or future drop.
- **A wake is a hint, not a state transition**: late wakes are allowed; correctness relies on rechecking after register.
- **The internal `Arc::new` in `PollSet::new` may still panic in the global allocator on OOM**;
  this limitation does not extend to every register.

## State Machine

```
register()
   │
   v
 Queued ──guard drop/cancel──> Cancelled (slot freed)
   │
   ├── wake()/source Drop ──> Detached ──outside lock──> Waker::wake
   │
   └── late guard drop ──> No-op (token already invalid)
```

Unregistration and wake are linearized by the same `SpinNoIrq` lock:

- cancel wins first: no wake;
- wake wins first: one late wake is allowed.

## Algorithm Flow

### A Blocking Wait (`poll_io`)

1. Perform the non-blocking operation / readiness check.
2. On `WouldBlock`, clean up the previous round's registrations with `PollRegistrations::context(cx)`.
3. `pollable.register(&mut context, events)?`; on failure return `NoMemory` immediately.
4. Perform the check again, closing the check/register race.
5. If still blocked, return `Pending`; the owner keeps the registrations.
6. On wakeup, timeout, interrupt, or drop, RAII unregisters all sources.

### `PollSet::wake`

1. Check `active` under the lock; return if it is 0.
2. Use `mem::replace` to move the whole slot table out of `State` and reset `active/free_head`.
3. Release the lock.
4. Outside the lock, iterate the old slot table and call `waker.wake()` on each occupied slot.

### `PollSet::register` / cancel

1. `Waker::clone` outside the lock.
2. Install the owned waker under the lock; `next_id` is incremented only after the slot is installed successfully.
3. cancel removes the waker under the lock and drops it after unlocking.

### `PollEvent`

`PollEvent` tracks a generation for broadcast state-change notifications.
Waiters snapshot the generation, register through `PollContext`, then recheck
`has_changed_since` before returning `Pending`. `notify_defer_wake` publishes
the new generation and returns a `PollSet` clone for waking after outer locks
are released; `notify` publishes and wakes immediately. Notifications prompt
predicate rechecks and never supply consumable tokens or synchronous waits.

## Concurrency Model

- Shared state: `SpinNoIrq<State>`.
- Registrations are not deduplicated: multiple logical waits of the same task own independent tokens, so one guard cannot remove another by mistake.
- Complexity: register/unregister O(1) (except growth); wake O(n); no more `will_wake` scan under the lock.
- epoll holds `PollRegistrations` (a `Mutex`) long-term on `EpollInterest`, and uses a generation
  to filter old `InterestWaker`s that have already been drained and are running outside the lock; file `register_poll` runs outside the lock.
  When replacing/clearing the owner, `mem::replace` / `mem::take` moves the old value out before dropping it, to avoid nesting the source
  `SpinNoIrq` unregistration inside the `Mutex`.

## Design Decisions

| Decision | Rationale | Rejected Alternative |
|------|------|----------|
| RAII registration + token | Express the lifetime through ownership, with automatic reclamation | Shared `Vec<Waker>` / unregistering by Waker |
| `SmallVec` inline + `try_reserve` | Zero heap on the common path, fallible on overflow | Fixed-limit array; bare `Vec::push` |
| Wake path moves out the slot table, then wakes outside the lock | Single lock acquisition on the IRQ hot path, no allocation and no waker calls under the lock | Building a temporary waker buffer under the lock; taking the lock a second time to recycle capacity |
| Ordinary `Pollable` uses `PollContext` | Forbid stuffing a bare Waker permanently into an fd without an owner | A registration API without an RAII owner |
| registration holds a `Weak` | Prevents reference cycles, allows the source to be destroyed first | registration holds a strong reference |

## Drop / Resource Release

- `PollRegistration::Drop`: if the token still matches an occupied slot, unlink and reclaim it.
- `PollRegistrations::Drop` / `clear`: drop the guards one by one.
- `PollSetInner::Drop`: remove all waiters and wake them outside the lock, so nothing blocks forever after destruction.
- epoll interest Drop / DEL / ONESHOT: unregister by clearing its `PollRegistrations`; MOD
  updates the configuration of the same interest in place and reinstalls the registration.

## Advisory waiter snapshot

`PollSet::has_waiters` reads the existing active count under its IRQ-safe lock.
It neither allocates nor wakes, and returns an O(1) snapshot. Producers that
stop retrying on `false` must let a racing/new registration restart the retry.
