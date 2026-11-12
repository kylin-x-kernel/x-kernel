# kspin - Security and Reliability Analysis

Explicit `NoPreempt::release_without_check` suppresses the consumed guard's
Drop, preventing a second decrement. The provider removes exactly one credit
without scheduling or RCU cleanup. The caller must supply a subsequent explicit
checkpoint; this API does not clear pending requests or override outer guards.

## Overview

`kspin` is a low-level synchronization primitive.
It contains `UnsafeCell` dereferences, raw-pointer guards, `Send`/`Sync` unsafe impls,
and explicit unsafe APIs such as `force_unlock`.
Errors in lock acquisition, lock release, local IRQ/preemption protection, or memory ordering
can lead to data races, deadlocks, IRQ state leaks, or cross-CPU visibility errors.

## Trust Model

```text
kernel subsystem caller
   │
   │ safe API:
   │   SpinLock::{new,lock,try_lock,is_locked,get_mut,into_inner}
   │   SpinLockGuard::{Deref,DerefMut,Drop}
   │
   │ unsafe API:
   │   SpinLock::force_unlock
   v
┌─────────────────────────────────────────────┐
│ kspin                                      │
│                                             │
│ unsafe boundary                            │
│  ├─ UnsafeCell<T> -> *mut T -> &/&mut T    │
│  ├─ unsafe impl Send/Sync                  │
│  ├─ AtomicBool Acquire/Release protocol    │
│  └─ force_unlock caller contract           │
└──────────────────┬──────────────────────────┘
                   │ BaseGuard::acquire/release
                   v
karch IRQ state / ktask preemption interface
```

- Callers of the safe API trust `kspin` to provide unique mutable access for the lifetime of a guard.
- `kspin` trusts the `BaseGuard` implementation to pair acquire/release correctly and to have correct nesting semantics.
- kspin trusts `kiface_task_api::TaskPreemptionIf` to pair preemption credits correctly:
  acquisition and deferred-check release do not schedule; ordinary release may
  check and schedule when the context permits. Neither modifies the IRQ mask.
- The caller of `force_unlock` must itself prove that the current execution path actually holds the lock.

## Unsafe Code Inventory

### 1. The `Sync` implementation for `SpinLock<G, T>`

Location: `src/lock.rs`

```rust
unsafe impl<G: BaseGuard, T: ?Sized + Send> Sync for SpinLock<G, T> {}
```

Invariants:

- When a `SpinLock` is accessed through a shared reference,
  the inner `T` can only be referenced through a `SpinLockGuard` that holds the lock.
- When `smp` is enabled,
  the `AtomicBool` flag serializes guard creation across CPUs.
- When `smp` is not enabled,
  the guard chosen by the caller must prevent reentrancy on the same CPU.

Why it is safe:

- `lock` and `try_lock` create the data pointer from the `UnsafeCell` only after acquire succeeds.
- `SpinLockGuard::drop` releases the flag and restores the guard state.
- `T: Send` matches the sharing condition of `std::sync::Mutex`.

### 2. The `Send` implementation for `SpinLock<G, T>`

Location: `src/lock.rs`

```rust
unsafe impl<G: BaseGuard, T: ?Sized + Send> Send for SpinLock<G, T> {}
```

Invariants:

- Moving the lock does not leave a valid access entry point at the old location.
- The protected data can have its ownership transferred across threads.

Why it is safe:

- Rust ownership moves guarantee that safe code can no longer use the old `SpinLock` location.
- After the move, access is still protected by the same guard and atomic protocol.

### 3. Creating the pointer from `UnsafeCell` after `lock` succeeds

Location: `src/lock.rs`

```rust
ptr: unsafe { &mut *self.storage.get() },
```

Invariants:

- The current guard has already executed `G::acquire()`.
- When `smp` is enabled, the current CPU has successfully changed the flag from false to true.
- No other mutable reference is created before the guard is dropped.

Why it is safe:

- After the lock is successfully held,
  `SpinLockGuard` is the only entry point for accessing `T`.
- The pointer is stored only in the guard,
  and the guard's lifetime constrains the references.

### 4. Creating the pointer from `UnsafeCell` after `try_lock` succeeds

Location: `src/lock.rs`

The invariants are the same as for `lock`,
except that `try_lock` uses a strong CAS.
The failure path calls `G::release(guard_state)`
and does not create a data pointer.

### 5. `get_mut`

Location: `src/lock.rs`

```rust
unsafe { &mut *self.storage.get() }
```

Invariants:

- The caller holds `&mut SpinLock<G, T>`.
- Rust's exclusive borrow guarantees that no other guard or shared reference exists.

Why it is safe:

- No runtime lock is needed;
  the compile-time exclusive borrow already proves there is no concurrent access.

### 6. `SpinLockGuard::deref`

Location: `src/lock.rs`

```rust
unsafe { &*self.ptr }
```

Invariants:

- `ptr` comes from the `UnsafeCell` after the lock was successfully held.
- The returned reference cannot outlive the guard.

Why it is safe:

- `Deref` borrows `&self`,
  and the shared reference exists only while the guard still holds the lock.

### 7. `SpinLockGuard::deref_mut`

Location: `src/lock.rs`

```rust
unsafe { &mut *self.ptr }
```

Invariants:

- The current guard is exclusively borrowed through `&mut self`.
- The guard still holds the lock.

Why it is safe:

- `&mut self` prevents the same guard from creating multiple mutable references at once.
- Other guards are excluded by the lock protocol.

### 8. `SpinLock::force_unlock`

Location: `src/lock.rs`

```rust
pub unsafe fn force_unlock(&self)
```

Invariants:

- The current execution path already holds the lock.
- References created from the leaked guard are not used after the call.
- The caller is responsible for handling the guard state,
  because `force_unlock` only clears the SMP flag and does not restore IRQ/preemption state.

Why it is safe:

- The function itself performs only a Release store.
- Its safety depends entirely on the caller's contract,
  so it must remain an `unsafe fn`.

Review of external callers:

- `core/ktracing::TraceRawLock` calls this API when implementing `lock_api::RawMutex`.
  Its `lock` / `try_lock` call `mem::forget` on the guard after success,
  and `unlock` is guaranteed by the `lock_api` contract to be called only while the lock is held.
  A `SAFETY:` comment has been added at the call site.

### 9. `kspin_unittest.rs` fake guard counter

Location: `ut-tests/kspin_unittest.rs`

The `static mut IRQ_CNT` in the tests is used to verify that fake guard acquire/release calls are paired.
These unsafe accesses exist only in `#[cfg(unittest)]` test code,
and the test preconditions are documented before each unsafe block.

## Memory Safety Invariants

1. **Unique access through the guard**:
   Any `&T` / `&mut T` must be derived from a `SpinLockGuard`.
2. **Flag bound to pointer creation**:
   When `smp` is enabled,
   `ptr` may be created only after a successful CAS.
3. **Release write on unlock**:
   Dropping the guard uses a `Release` store,
   and the next lock holder observes the critical-section writes through an `Acquire` CAS.
4. **Failure path restores guard state**:
   A failed `try_lock` must call `G::release`.
5. **`force_unlock` does not restore the local guard**:
   Callers must not use it as an ordinary unlock.
6. **`SpinRaw` requires an external non-reentrancy guarantee**:
   `NoOp` does not disable preemption or IRQs.

## Thread Safety

| Type | Send condition | Sync condition |
|------|-----------|-----------|
| `SpinLock<G, T>` | unsafe impl when `T: Send` | unsafe impl when `T: Send`, relying on the guard + atomic protocol |
| `SpinLockGuard<'_, G, T>` | Not implemented explicitly; determined by its fields | The guard holds a raw pointer and should not be shared across threads for access |
| `SpinRwLock<G, T>` | unsafe impl when `T: Send` | unsafe impl when `T: Send + Sync`, since read guards can expose `&T` concurrently |
| `SpinRwLockReadGuard<'_, G, T>` | Not implemented explicitly; determined by its fields | Shared access through the lock's reader protocol |
| `SpinRwLockWriteGuard<'_, G, T>` | Not implemented explicitly; determined by its fields | The write guard holds exclusive access and should not share mutable access across threads |
| `NoOp` | zero-sized | zero-sized |
| `IrqSave` | Saves IRQ flags | Should acquire/release only on the current CPU |
| `NoPreempt` | zero-sized | Depends on `TaskPreemptionIf` |
| `NoPreemptIrqSave` | Saves IRQ flags | Depends on `TaskPreemptionIf` + `karch` IRQ restore |

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Creating `&mut T` without holding the lock causes a data race | High | `UnsafeCell` is dereferenced before the CAS | The code creates the pointer only after the guard is successfully acquired; `get_mut` relies on `&mut self` |
| T-02 | `SpinRaw` is used in a preemptible or IRQ-reentrant context | High | The caller mistakenly picks `SpinRaw` to protect IRQ-shared data | The type alias documentation states the restriction explicitly; `SpinNoIrq` is recommended by default |
| T-03 | A failed `try_lock` leaks the IRQ/preemption disabled state | High | The failure path forgets `G::release` | The current failure path releases immediately; unit tests cover fake guard restoration |
| T-04 | Too weak a memory ordering in unlock makes writes invisible | High | The Release/Acquire protocol is downgraded | unlock uses Release; a successful lock uses Acquire |
| T-05 | After a guard is `mem::forget`-ed, the lock stays locked forever | Medium | The caller leaks the guard | RAII releases automatically on the normal path; `force_unlock` is only an unsafe escape hatch |
| T-06 | `force_unlock` is called by a non-holder | High | FFI, the `lock_api` adapter, or tests misuse the unsafe API | `force_unlock` is an unsafe fn; the `TraceRawLock` and unittest call sites have been reviewed |
| T-07 | `BaseGuard` acquire/release ordering is wrong | High | A newly added guard does not correctly save/restore IRQ or preemption state | The guard trait is a centralized abstraction; `NoPreemptIrqSave` always disables preemption first, then IRQ, and releases in reverse order |
| T-08 | Debug output recursively tries to lock while the lock is held | Low | Formatting an object that is already locked | `Debug` uses `try_lock` and prints `<locked>` on failure |
| T-09 | A long critical section makes the CPU spin and be occupied | Medium | Blocking, I/O, or complex loops are executed while holding the lock | The documentation limits spin locks to short critical sections; callers audit hot spots |
| T-10 | Relying on `NoPreempt` without enabling the `preempt` feature | Medium | Misconfiguration | Explained in the feature table; the platform defconfig must match the scheduling model |
| T-11 | `SpinRwLock` read-to-write upgrade self-deadlocks | Medium | Trying to acquire a write guard while holding a read guard | The documentation states explicitly that upgrade is not supported; the caller must release the read guard before entering a write transaction |
| T-12 | `SpinRwLock` reader count overflow | Medium | Extreme recursion, or continuing to acquire read locks after leaking read guards | Panic when the reader count reaches its limit, to avoid overwriting the writer bit |
| T-13 | `SpinRwLock` writer starvation | Medium | New readers keep entering while a writer is waiting | The current implementation is a simple reader-preferred spin rwlock, used only for short critical sections; a queued rwlock will be designed separately if fairness is needed |

Impact level definitions:

- High: causes UB, memory corruption, or privilege escalation.
- Medium: causes panic, service unavailability, or data inconsistency.
- Low: causes performance degradation, log loss, or feature degradation.

## Failure Mode and Effects Analysis

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Deadlock | The same context acquires the same non-reentrant lock twice | The current CPU spins | The whole system may hang | 1 | Avoid recursive locking; the `lock` documentation notes that it may deadlock |
| F-02 | IRQ state not restored | A guard release call is missed or a guard is leaked | IRQs are disabled on this CPU | Timer and device interrupts stall | 1 | RAII drop; `try_lock` releases on failure; tests cover fake guards |
| F-03 | Preemption state not restored | Incorrect `TaskPreemptionIf` implementation | The current task cannot be preempted | Scheduling latency or hang | 2 | The scheduler implements the interface centrally under the `preempt` feature |
| F-04 | Spinning too long | The critical section is too large or the lock holder is stuck | CPU usage rises | System latency rises | 2 | Use only for short critical sections; avoid I/O while holding the lock |
| F-05 | Continuing to use a leaked guard after `force_unlock` | The unsafe caller violates the contract | Two guards access the data at the same time | Data race or memory corruption | 1 | Keep the API unsafe; audit all call sites |
| F-06 | Mistakenly assuming cross-CPU mutual exclusion in a single-core configuration | The `smp` feature is not enabled but the system runs on multiple cores | No atomic flag | Data race | 1 | The defconfig must match the CPU topology |
| F-07 | Debug output hides data inside the lock | `try_lock` fails | Only `<locked>` is visible | Debug information is degraded | 4 | Avoid Debug blocking or recursive deadlock |

Severity definitions:

- 1: Fatal; system crash, data loss.
- 2: Serious; feature unavailable, a reboot is needed to recover.
- 3: Moderate; feature degraded, can recover automatically.
- 4: Minor; limited impact, tolerable to users.

## Fault Management

- `lock` does not return an error;
  it keeps spinning under contention.
- `try_lock` uses `Option` to indicate success or failure,
  and the guard state has already been restored on failure.
- `force_unlock` performs no runtime checks;
  misuse is a violation of the unsafe contract.
- This crate does not log,
  to avoid introducing extra dependencies in critical sections and low-level synchronization paths.

## Privacy Analysis

`kspin` does not directly process user data.
The `T` it protects may contain arbitrary kernel or user-related data,
but the crate does not read, copy, or print `T`,
except that the `Debug` implementation formats the inner data when the caller explicitly formats it and the lock is acquired successfully.

## Known Limitations

1. **Unfair lock**:
   The current implementation has no queue or priority inheritance.
2. **No deadlock detection**:
   Reentering the same lock will spin or deadlock.
3. **`NoPreempt` is a no-op without the preemption feature**:
   The configuration must match the scheduling model.
4. **`force_unlock` releases only the atomic flag**:
   It does not restore the IRQ/preemption guard state.
5. **Spin locks are unsuitable for long critical sections**:
   Holding the lock for a long time amplifies interrupt latency and CPU usage.

## Other Notes (Template Sections)

| Section | Description |
|------|------|
| Baseline | Governed by this repository's `docs/templates/module-docs-guide.md` and `AGENTS.md` |
| Redundancy design | None |
| Overload control | None; callers must keep critical sections short |
| Human error | No direct user interaction |
| Fault prediction and prevention | None |
| Non-disruptive upgrade | None |

## Audit Checklist

When modifying `kspin`, verify that:

- [ ] Every newly added unsafe block, unsafe impl, or unsafe API has a preceding `SAFETY:` comment.
- [ ] The `try_lock` failure path restores the guard state.
- [ ] The Acquire/Release memory ordering of lock/unlock is not weakened.
- [ ] The acquire/release ordering of newly added guards is paired and auditable.
- [ ] New call sites of `SpinRaw` already have an external guarantee of no IRQ/preemption reentrancy.
- [ ] No operation that may sleep, perform blocking I/O, or run a long loop is executed while holding a spin lock.
- [ ] The `smp` / `preempt` features in the defconfig are consistent with the actual platform model.
