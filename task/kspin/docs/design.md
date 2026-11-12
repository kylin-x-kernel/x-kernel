# kspin - Design Document

## Positioning

`kspin` is the kernel spinlock crate of x-kernel.
It provides `SpinLock<G, T>` with an RAII guard,
and uses the guard type to select at compile time whether entering the critical section
disables local IRQs
and whether it disables kernel preemption.

The target audience is developers who maintain the scheduling, IRQ, driver, and low-level synchronization paths.

## Background

The kernel runs in a `no_std` environment,
and many paths cannot sleep or cannot rely on a blocking mutex.
A spinlock suits short critical sections,
but in the kernel a plain atomic lock is not enough:
preemption or an interrupt handler on the same CPU may re-enter and access the same data,
causing a deadlock or a data race.

`kspin` separates the "lock state" from the "CPU-local protection before entering the critical section":

- `SpinLock` is responsible for cross-CPU mutual exclusion.
- The guard type is responsible for the local IRQ / preemption state.

## Scope

`NoPreempt::release_without_check` consumes one guard and restores only its
preemption credit through `TaskPreemptionIf::enable_preempt_without_check`.
It leaves IRQ state and outer credits unchanged. The completed normal IRQ
adapter uses this before its explicit scheduler checkpoint, avoiding an earlier
ordinary guard-drop check. Normal Drop behavior is unchanged; with preemption
support disabled, or in host builds, release remains a no-op.

When stats is enabled, `static_lock!` emits klockstat's registration macro; when stats is disabled, it emits the
static item unchanged, consistent with the entry point in ksync. Callers therefore do not need to duplicate lock definitions for the stats configuration.

Source files involved:

```text
task/kspin/
├── src/
│   ├── lib.rs                  # crate docs, public re-exports and type aliases
│   ├── lock.rs                 # SpinLock and SpinLockGuard
│   ├── guard/
│   │   ├── mod.rs              # BaseGuard
│   │   ├── types.rs            # NoOp/IrqSave/NoPreempt/NoPreemptIrqSave
│   │   ├── arch/mod.rs         # karch IRQ save/restore adaptation
│   │   └── percpu.rs           # kcpu_slot CpuPin/CpuLocalGuard impls
│   └── rwlock.rs               # SpinRwLock and read/write guards
├── ut-tests/
│   └── kspin_unittest.rs       # unittest tests
├── README.md                   # crate-level rustdoc content
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## Architecture

```text
caller
  │
  │ lock()
  v
┌─────────────────────────────────────────────┐
│ SpinLock<G, T>                              │
│  marker: PhantomData<G>                     │
│  flag: AtomicBool       (feature = smp)     │
│  storage: UnsafeCell<T>                     │
└──────────────────┬──────────────────────────┘
                   │ G::acquire()
                   v
┌─────────────────────────────────────────────┐
│ BaseGuard implementation                    │
│  NoOp / NoPreempt / IrqSave                 │
│  NoPreemptIrqSave                           │
└──────────────────┬──────────────────────────┘
                   │ acquired
                   v
┌─────────────────────────────────────────────┐
│ SpinLockGuard<'_, G, T>                     │
│  guard_state: G::State                      │
│  ptr: *mut T                                │
│  flag_ref: &AtomicBool  (feature = smp)     │
└─────────────────────────────────────────────┘
```

| Component | Responsibility |
|------|------|
| `SpinLock<G, T>` | Holds the protected data and the optional SMP atomic state |
| `SpinLockGuard` | Provides `Deref` / `DerefMut` while the lock is held; on drop, releases the lock and restores the guard state |
| `SpinRwLock<G, T>` | Holds the protected data and the reader/writer atomic state |
| `SpinRwLockReadGuard` | Provides `Deref` while the read lock is held; on drop, releases the reader slot and restores the guard state |
| `SpinRwLockWriteGuard` | Provides `Deref` / `DerefMut` while the write lock is held; on drop, releases the writer slot and restores the guard state |
| `BaseGuard` | Abstracts the acquire/release protocol for entering/leaving the local critical section |
| `TaskPreemptionIf` | Enable/disable preempt interface provided by the scheduler under the `preempt` feature |
| `NoOp` | Performs no local protection; suited to contexts where the caller has already disabled IRQs/preemption |
| `IrqSave` | Saves and disables local IRQs; restores them on drop |
| `NoPreempt` | Disables kernel preemption; restores it on drop |
| `NoPreemptIrqSave` | Disables preemption first, then saves and disables IRQs; on release, restores IRQs first and then preemption |

## Module Boundary Self-Review

The responsibility of `kspin` stays at "low-level spinlock and local critical-section guard":

- Preemption control uses `kiface_task_api::TaskPreemptionIf`, provided by `ktask`.
  kspin consumes this contract; it does not define it or implement scheduling.
- It does not operate on architecture registers directly;
  IRQ save/restore is uniformly wrapped by `karch`.
- It does not wrap sleeping waits, condition variables, or semaphores;
  these are handled by higher-level synchronization crates such as `ksync`.
- It only provides a spin-based reader-writer lock that never sleeps,
  suited to short critical sections in the style of Linux `*_lock_irq()`.
- It does not depend on `ktask`, `kservices`, `ktracing`, or driver subsystems;
  the dependency direction is that these upper-layer crates use `kspin`.

Public API self-review results:

- Public exports include `SpinLock`, `SpinLockGuard`, `SpinRwLock`,
  `SpinRwLockReadGuard`, `SpinRwLockWriteGuard`, `BaseGuard`, `IrqSave`, `NoOp`,
  `NoPreempt`, `NoPreemptIrqSave` and the lock aliases. `TaskPreemptionIf`
  belongs to kiface-task-api and is not reexported by kspin.
- `guard::arch` is a private module;
  the IRQ save/restore helpers are not exposed across crates.
- The old duplicate implementation `src/base.rs`, which was not wired into the module tree, has been removed,
  avoiding a second unsafe spinlock API and a duplicated audit surface.
- `force_unlock` remains an `unsafe fn`,
  serving only adaptation scenarios such as `lock_api::RawMutex` where an RAII guard cannot be held.

## State Machines

### SMP lock state

```text
Unlocked(flag = false)
   │ compare_exchange(false -> true, Acquire)
   v
Locked(flag = true)
   │ SpinLockGuard::drop()
   │ store(false, Release)
   v
Unlocked
```

| From | To | Trigger |
|----|----|----------|
| Unlocked | Locked | `lock` or `try_lock` CAS succeeds |
| Locked | Locked | `lock` spins and waits after a failed CAS |
| Locked | Unlocked | `SpinLockGuard::drop` performs a Release store |
| Locked | Unlocked | `unsafe force_unlock` forcibly releases the lock when the caller proves it holds the lock |

### Spin rwlock state

```text
Unlocked(state = 0)
   ├─ read CAS(state -> state + 1, Acquire) ──> Readers(state = n)
   └─ write CAS(0 -> WRITE_LOCKED, Acquire) ─> Writer

Readers(state = n)
   ├─ read CAS(n -> n + 1, Acquire) ─────────> Readers(state = n + 1)
   └─ read guard drop fetch_sub(1, Release) ─> Readers/Unlocked

Writer
   └─ write guard drop store(0, Release) ────> Unlocked
```

| From | To | Trigger |
|----|----|----------|
| Unlocked | Readers | `read` or `try_read` CAS succeeds when there is no writer |
| Readers | Readers | A new reader's CAS succeeds when there is no writer, or other readers remain after a reader drops |
| Unlocked | Writer | `write` or `try_write` CAS succeeds when state is 0 |
| Writer | Unlocked | `SpinRwLockWriteGuard::drop` performs a Release store |

### Non-SMP lock state

```text
SingleCpu
   │ lock / try_lock
   v
GuardedByLocalGuard
   │ drop
   v
SingleCpu
```

When the `smp` feature is not enabled,
the atomic flag is compiled out.
Mutual exclusion relies on the local non-reentrancy guarantee provided by the guard type,
for example disabling preemption or IRQs.

### `NoPreemptIrqSave` state order

```text
Normal
  │ disable_preempt
  v
PreemptDisabled
  │ save_disable_irq
  v
PreemptDisabledIrqDisabled
  │ restore_irq
  v
PreemptDisabled
  │ enable_preempt
  v
Normal
```

This order avoids being scheduled out when IRQs have been restored but the preemption state is not yet consistent.

## Algorithm Flow

### `lock`

1. Call `G::acquire()` to enter the local critical section and save the guard state.
2. When `smp` is enabled, try to take the lock with `compare_exchange_weak(false, true, Acquire, Relaxed)`.
3. When the CAS fails, execute `spin_loop()` for as long as `is_locked()` is true.
4. On success, take the pointer out of the `UnsafeCell<T>`
   and construct the `SpinLockGuard`.
5. The caller accesses the data through the guard's `Deref` / `DerefMut`.

### `try_lock`

1. First execute `G::acquire()`.
2. When `smp` is enabled, use a strong CAS to try to change the flag from false to true.
3. On success, return the guard.
4. On failure, immediately execute `G::release(guard_state)`,
   restore the local IRQ / preemption state, and return `None`.

### `SpinLockGuard::drop`

1. When `smp` is enabled, write the flag back to false with a `Release` store.
2. Call `G::release(guard_state)` to restore the local state.

The release order guarantees that another CPU, after its Acquire CAS succeeds, can see the writes made inside the critical section.

### `get_mut`

`get_mut(&mut self)` relies on Rust's exclusive borrow
and does not need to set the atomic flag.
Because the caller holds `&mut SpinLock`,
no guard or other reference can exist at the same time.

### `force_unlock`

`force_unlock` only clears the SMP flag;
it does not restore the guard state,
nor does it drop a leaked guard.
It is used only at boundaries that RAII cannot express, such as FFI or tests,
and the caller must prove that the current execution path really holds the lock.

## Concurrency Model

Locking strategy:

- When the `smp` feature is enabled, cross-CPU mutual exclusion is provided by an `AtomicBool`.
- `SpinRwLock` uses the high bit of an `AtomicUsize` to represent the writer and the low bits to represent the reader count.
- When the `smp` feature is disabled, there is no cross-CPU contention,
  and mutual exclusion relies on the guard preventing same-CPU preemption or IRQ reentrancy.
- `SpinRaw` provides no local protection,
  and may only be used in contexts where the caller has already guaranteed non-reentrancy.
- `SpinNoIrq` is the most conservative default choice,
  and covers the case where task context and IRQ context access the same data.
- `SpinRwNoIrq` disables IRQs/preemption on both the reader and writer sides,
  and is suited to short, non-sleeping, read-mostly relation snapshots and transaction boundaries.
  It does not support read-to-write upgrade, and the caller must not wait for a writer while holding a read guard.

Memory ordering:

- A successful acquire uses `Ordering::Acquire`.
- Unlock uses `Ordering::Release`.
- Spin-observing `is_locked` uses `Ordering::Relaxed`,
  serving only as a wait hint and carrying no synchronization.

## Cargo Features

| Feature | Purpose |
|---------|------|
| `smp` | Enables the `AtomicBool` lock flag to implement cross-CPU mutual exclusion |
| `preempt` | Invokes the scheduler's preemption toggle through `TaskPreemptionIf` |

When `preempt` is not enabled,
the preemption-toggle part of `NoPreempt` and `NoPreemptIrqSave` is compiled to a no-op.

## Design Decisions

### Why the guard is a type parameter

The guard type parameter encodes the critical-section policy into the type:
the local protection mode of a given `SpinLock` instance cannot be changed by mistake at runtime.
From the type alias alone, a call site shows which contexts the lock is suited to.

### Why the atomic flag is removed on a single core

Without the `smp` configuration there is no contention from other CPUs.
Keeping the atomic flag would only add instruction and storage overhead.
The mutual exclusion problem reduces to whether this CPU can be preempted or reentered by an interrupt,
which is the guard's responsibility.

With `stats` enabled, UP builds record acquisitions only. The contention
recording helper is compiled only with `smp`, matching its spin-wait caller;
`stats` without `smp` remains a supported configuration.

### Why `try_lock` acquires the guard first even when it fails

If local preemption or IRQs are not disabled first,
the path may be reentered by a local interrupt/preemption path between checking the lock flag and returning.
Acquiring the guard first lets both the success and failure paths operate under a consistent local critical-section model.
On failure it is released immediately, to avoid leaking IRQ/preemption state.

### Why `force_unlock` exists

In a few FFI or test scenarios the RAII guard cannot be dropped normally.
`force_unlock` keeps an escape hatch,
but exposes it as an `unsafe fn`,
and the safety documentation requires the caller to prove that the current path holds the lock.

## Drop / Resource Release

`SpinLock` has no custom `Drop`.
When the lock is destroyed,
the protected `T` is dropped according to ordinary Rust ownership rules.

`SpinLockGuard::drop` is the core release path:
it releases the SMP flag and restores the guard state.
If the guard is `mem::forget`-ed,
the lock stays in the locked state,
unless the caller uses `unsafe force_unlock`.
