# io/watchdog — Soft and Hard Lockup Watchdog Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.
Enable `KFEAT_WATCHDOG=y` in the kernel configuration to include these tests.

## Purpose

The optional `workqueue-stall-check` feature declares `WorkqueueStallCheck` through
`kiface`. A linked workqueue provider implements task-context, warning-only backlog
inspection. The per-CPU watchdog task invokes it after updating its soft-lockup
timestamp and before sleeping for four seconds. It does not depend on a concrete
workqueue implementation. Queue backlog only produces warnings; the legacy fatal
system-workqueue check is no longer registered. Soft/hard lockup checks are unchanged.

`io/watchdog` provides X-Kernel's soft-lockup and hard-lockup detection:

- Soft lockup: A watchdog task on each CPU periodically updates a timestamp;
  a timer callback checks whether that timestamp has expired.
- Hard lockup: NMI support (the `nmi` feature) periodically checks whether the local
  CPU's timer interrupt count is still advancing.
  A failed check triggers a global rendezvous, collects snapshots, and panics.

Dependencies: `khal` (NMI, per-CPU state, and time), `ktask` (tasks, timer callbacks,
and snapshots), and `ktime_types`.

## Background

The system may become unresponsive when interrupts remain masked or tasks are not
scheduled for an extended period.
Soft-lockup detection depends on scheduling and timers, which may themselves be affected.
Hard-lockup detection uses an NMI source independent of ordinary IRQs and can still
run when ordinary interrupts have stopped entirely.
X-Kernel uses the PMU cycle counter as its NMI source through `kplat::NmiPeriodic`;
this crate is the NMI consumer.

## Scope

- `src/init.rs`: `init_primary`, `init_secondary`, `init_common`, `init_nmi_watchdog`, and `init_softlockup_detection`.
- `src/lockup_detection.rs`: Per-CPU `LockupDetection` state, thresholds, and the `WatchdogTask` implementation.
- `src/watchdog_task.rs`: The `WatchdogTask` trait, per-CPU task queues, and mutex deadlock checks.
- `src/rendezvous.rs`: The global rendezvous state machine triggered by a failed check.
- `src/lib.rs`: Exports.

## Architecture

```text
           Timer interrupt                         NMI (periodic PMU source)
                  |                                           |
        +---------+-------------------+                       |
        v                             v                       v
 touch/check_softlockup           timer_tick          check_watchdog_tasks
 (watchdog task + timer)      (hard-lockup count)         (NMI context)
        |                             |                       |
        +-----------------------------+-----------------------+
                                      |
                                Failed check
                                      |
                  +-------------------+-----------------------+
                  v                                           v
       Soft lockup: log + dump                      Global rendezvous
                                                    Initiating CPU collects
                                                    all CPU snapshots -> panic
```

## Calling Constraints and Execution Context

| Entry Point | Execution Context | Constraints |
|-------------|-------------------|-------------|
| `init_primary` / `init_secondary` | Primary / secondary CPU boot | Once per CPU. |
| `init_nmi_watchdog` | Boot, with the NMI feature | Checks `khal::nmi::mode()`; logs and disables hard-lockup detection when it returns `None`. |
| NMI callback registered through `enable_periodic_nmi` | NMI context | Only NMI-safe operations: atomics, snapshots, and `kprint_atomic!`, including lock-free argument formatting. Ordinary IRQ spinlocks are forbidden. |
| Timer callback (`timer_tick` / soft-lockup check) | Timer interrupt, with IRQs disabled | Per-CPU atomic accesses. |
| Watchdog task | One task pinned to each CPU | Updates the soft-lockup timestamp, optionally inspects workqueue backlog, then sleeps for 4 seconds. |

## State Machines

### Per-CPU Hard-Lockup Detection

`hrtimer_interrupts`, incremented by timer interrupts, is compared with
`hrtimer_interrupts_saved`, the value observed by the previous NMI check.
If the count has been initialized but has not advanced, the hard-lockup condition
holds: `check_hardlockup` returns `true`.
`WatchdogTask::check` negates this result, so a healthy task returns `true`.

### Global Rendezvous

```text
Idle      -- try_trigger (first failing CPU transitions atomically) --> Triggered
Triggered -- mark_arrived in each CPU's NMI -----------------------> wait for all_arrived_mask
Triggered -- initiating CPU collects/prints snapshots, mark_dump_done --> DumpDone
DumpDone  -- initiating CPU panics (system stops) -----------------> terminal state
```

`reset()` provides a path back to Idle.
Its comment warns that other CPUs may still be spinning in NMI context, so callers
must use it carefully.

## Algorithms

### Hard-Lockup Check on Each NMI

1. `check_watchdog_tasks()` scans the local CPU's queue (`HardLockupDetection` and
   `MutexDeadlock`) and returns the task name when a check fails.
2. On failure, if `ktask::snapshot::nmi_begin()` succeeds, call `rv::try_trigger()`.
   The CPU that wins the atomic compare-and-swap from Idle to Triggered becomes
   the initiating CPU.
3. Each CPU calls `mark_arrived` and `nmi_collect_local()` in its own NMI context.
4. After `wait_all_arrived_strong()`, the initiating CPU prints the failure,
   calls `nmi_dump_all` and `mark_dump_done`, and panics.
   Other CPUs spin while waiting for `is_dump_done`.

### Soft-Lockup Detection

- The watchdog task pinned to each CPU calls `touch_softlockup` every 4 seconds.
- Each CPU retains a `ktask::Timer<SoftlockupTimer>` in `SOFTLOCKUP_TIMER`,
  allocated once during local initialization. The first sample is 4 seconds
  later; the callback explicitly rearms on its owner CPU for completion time
  plus 4 seconds. It does not chase missed samples or allocate timer storage.
- On every sample, the timer callback calls `timer_tick()` and checks
  `now - soft_timestamp > 20s`.
  When the threshold is exceeded, it logs the failure and calls `dump_sched_stats`
  and `dump_cpu_tasks`, with reports rate-limited by the threshold interval.
- The separate `register_timer_irq_note` hook still counts every timer IRQ for
  hard-lockup detection. Neither this migration nor delayed sampling changes
  NMI registration or the hard-lockup policy.
- Reinitializing a CPU's sampling owner panics. An out-of-range initial deadline
  also panics; an out-of-range callback rearm logs and stops sampling instead.

## Concurrency

- `LockupDetection` holds its private `LockupState` in
  `karch::cache::CacheAligned`, replacing its local 64-byte alignment literal.
  The shared policy isolates independently updated state; `AtomicU64` and
  `AtomicBool` fields, not the wrapper, provide synchronization between NMIs,
  timer interrupts and the watchdog task. The public type and methods remain
  unchanged, and no allocation or lock is added.
- `LockupDetection`'s task, IRQ, and NMI entry points take only shared
  references; all state transitions go through atomic fields, so no overlapping
  mutable reference is ever formed for the atomics.
- The per-CPU watchdog task list is a one-time publication object: per-CPU
  initialization exclusively writes the `Vec`, then publishes it with a
  `Release` store once all registrations are complete. Periodic NMIs may only
  traverse it after an `Acquire` observes publication, and only through a
  shared slice.
- The rendezvous uses global atomics (`PHASE`, `CAUSE_CPU`, and `ARRIVED_BITMAP`)
  and spinning, without taking locks in NMI context.
- NMI paths never acquire ordinary IRQ spinlocks, avoiding self-deadlock when a
  pseudo-NMI preempts a lock holder on the same CPU.

## Design Decisions

- **Thresholds:** 20 seconds for soft lockups and 10 seconds for hard lockups.
  The watchdog task updates its timestamp every 4 seconds, providing five chances
  within the 20-second threshold to avoid false reports.
- **Strong rendezvous without a timeout:** The initiating CPU waits for every CPU
  before dumping snapshots, avoiding missing CPU snapshots.
  If any CPU cannot enter NMI context, the wait is permanent; this is accepted
  because the system is already unusable.
- **Explicit degradation when NMI is unavailable:** Boot logs state that hard-lockup
  detection is disabled, rather than failing silently.
- **Snapshot reentry protection:** If `nmi_begin()` fails because another snapshot
  is in progress, skip the current dump.
- **Publish before arming NMI:** The task list is permanently read-only once
  published; periodic NMIs are armed only after publication, and a failed arming
  does not reopen the registration window.
- **Soft-lockup report rate limit:** At most one report per threshold interval.

## Drop and Resource Release

Per-CPU state lives as long as the kernel and is not released at runtime.
This includes the retained timer owners. Their handlers never access the
owner slots and cannot drop or synchronously cancel their own delivery.
`reset()` is used only when the rendezvous needs to be restarted.
