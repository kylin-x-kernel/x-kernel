# io/watchdog — Security and Reliability Analysis

## Trust Model

This crate is a trusted kernel component.
Its inputs are hardware events (timer interrupts and NMI/PMU overflows) and internal
kernel state (task scheduling and mutexes).
It accepts no user input.

## External Boundaries and Attack Surface

- Timer interrupts and NMI events from PMU overflows: Event frequency and correctness
  depend on the hardware or emulator.
- Kernel internals: `ktask` snapshots, task state, and mutex state examined by `check_mutex_deadlock`.
- Triggered diagnostic output: `kprint_atomic!` uses emergency serial transmission
  without ordinary locks and may discard output when its polling budget is exhausted.
  Argument evaluation and formatting must also avoid locks.
  It does not guarantee complete messages or serialized output across CPUs.

## Unsafe Code Inventory

### `init.rs`

- The timer callback in `init_softlockup_detection` reads the previous report
  timestamp through `LAST_SOFTLOCKUP_REPORT.current_ref_raw()` and updates it through
  `current_ref_mut_raw()`.
  Invariant: The callback runs with IRQs disabled and cannot migrate, so per-CPU raw
  pointer access cannot race with migration. The same non-migrating callback
  exclusively performs updates.
  Safe entry point: The timer callback registered by `init_softlockup_detection`.

### `lockup_detection.rs`

- `touch_softlockup`, `timer_tick`, `check_softlockup`, and
  `register_hardlockup_detection_task` access `LOCKUP_DETECTION.current_ref_{mut_}raw()`.
  Invariant: Watchdog tasks are pinned to a CPU with preemption disabled;
  timer callbacks run with IRQs disabled. Neither path can migrate.
  Safe entry points: The callbacks and pinned tasks registered by `init_softlockup_detection`.

### `watchdog_task.rs`

- `register_watchdog_task` and `check_watchdog_tasks` access
  `WATCHDOG_TASK_QUEUE.current_ref_mut_raw()`.
  Invariant: Registration occurs only during per-CPU initialization, when migration
  is impossible. Checks run in NMI context and cannot migrate.
  Safe entry points: `init_nmi_watchdog` and `register_watchdog_task`.

## Memory Safety Invariants

- Per-CPU state may be accessed only by its owning CPU.
- The `&'static` per-CPU pointers referenced by NMI callbacks live as long as the kernel.
- Atomic ordering: `Release` stores and `Acquire` loads make soft-lockup timestamp
  initialization visible; rendezvous state transitions use `AcqRel`.

## Thread Safety

- NMIs and timer interrupts may concurrently access the same CPU's `LockupDetection`;
  all fields are atomic.
- Global rendezvous atomics are visible across CPUs.
  `try_trigger` uses `compare_exchange` to select a unique initiating CPU.
- `ARRIVED_BITMAP` bits are written by CPU ID.
  IDs at or above `usize::BITS` are ignored, under the assumption that the platform
  has at most 64 CPUs.

## Threat Analysis

| ID | Threat | Impact Level | Trigger | Mitigation |
|----|--------|--------------|---------|------------|
| T-01 | NMI callback acquires an ordinary IRQ lock | High (same-CPU self-deadlock) | A pseudo-NMI preempts an ordinary IRQ path holding the lock | NMI paths use only atomics and spinning; enforced through code review. |
| T-02 | NMI does not arrive at the expected interval | Medium (false hard-lockup report) | Delayed or lost NMI under TCG emulation | The hard-lockup counter must first be initialized (`current != 0`); detection is disabled at boot if NMI is unavailable. |
| T-03 | A CPU can never enter NMI context | High (initiating CPU spins forever) | The CPU's interrupt/NMI handling has stopped | The strong rendezvous intentionally has no timeout because the system is already unusable; documented in the design. |
| T-04 | Snapshot reentry | Medium (snapshot corruption/deadlock) | NMI interrupts an existing snapshot operation | Skip the dump if `nmi_begin()` fails; `kprint_atomic!` bypasses normal serial locks for best-effort output. |
| T-05 | False soft-lockup reports flood the console | Low (log storm) | The watchdog task is starved while the timer still runs | Limit reports to one per threshold interval. |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Cause | Local Effect | System Effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | NMI mechanism unavailable | GICv2 or no FEAT_NMI | Hard-lockup detection disabled | Loss of hard-lockup detection; soft-lockup detection cannot cover a hard hang | 3 | Log at boot, check `mode()`, and return. |
| F-02 | Periodic NMI arming fails | PMU unavailable or arming returns `false` | Hard-lockup detection disabled on this CPU | One CPU loses hard-lockup detection | 3 | Log an error when `enable_periodic_nmi` fails. |
| F-03 | Watchdog task starved | Scheduling problem | False soft-lockup report | Log storm and snapshot dumps | 3 | Update the timestamp every 4 seconds, use a 20-second threshold, and rate-limit reports. |
| F-04 | Timer interrupts stop | Interrupt masking or hardware failure | `hrtimer_interrupts` stops advancing | NMI detects a hard lockup, then rendezvous and panic follow | 2 | This is the intended behavior of hard-lockup detection. |
| F-05 | Initiating CPU panics during rendezvous | A task check failed | System stops | Shutdown with diagnostic dump output retained | 1 | Intentional: stop the system while preserving diagnostics. |

## Failure Management

- Soft lockup: Log the failure and dump scheduling statistics and CPU tasks without
  stopping the system.
- Hard lockup: Enter the global rendezvous, collect every CPU's snapshot, and panic
  on the initiating CPU to stop the system.
- Boot-time failures (NMI unavailable or arming failed): Log and disable the affected
  detection without blocking boot.

## Privacy Analysis

No user data is processed.
Dump output may contain internal state such as kernel task names and is not intended
for users.

## Known Limitations

- The strong rendezvous has no timeout: If any CPU cannot enter NMI context, the
  initiating CPU spins forever.
- Hard-lockup detection depends on NMI timing accuracy.
  The platform PMU backend currently converts thresholds using a fixed 2.5 GHz clock;
  see `platforms/kplat-aarch64/src/peripherals/pmu.rs`.
  A code TODO calls for reading the DT OPP frequency instead.
- `usize::BITS` limits the bitmap: `mark_arrived` ignores CPU IDs of 64 or above;
  `all_arrived_mask` handles the upper limit of the bitmap width.
- Concurrent use of `reset()` while other CPUs are still spinning in NMI context
  requires care, as stated in its comment.

## Audit Checklist

- [ ] Do NMI callback paths contain ordinary IRQ spinlocks or blocking calls?
- [ ] Are all per-CPU accesses protected against migration by CPU pinning, disabled IRQs, or NMI context?
- [ ] Do all rendezvous state transitions use the correct atomic ordering?
- [ ] Does `nmi_begin()` protect every snapshot path against reentry?
- [ ] Are NMI-unavailable and arming-failure paths explicitly logged and degraded?

## Cache Alignment Boundary

`LockupDetection` embeds its private atomic state in
`karch::cache::CacheAligned`. The wrapper owns the state and changes layout only;
it performs no unsafe access or synchronization. Existing atomic orderings,
per-CPU access preconditions and NMI lifetime assumptions remain unchanged.
The alignment policy is not a DMA or cache-maintenance contract.
