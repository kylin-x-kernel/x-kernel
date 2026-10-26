# watchdog

Soft and hard lockup detection for system monitoring.

## Overview

watchdog combines timer-driven soft-lockup checks with optional NMI-based
hard-lockup detection. The soft-lockup heartbeat is a per-CPU watchdog task:
it is pinned to the owning CPU, runs with elevated scheduler priority, and
periodically updates the local CPU timestamp. Timer interrupt callbacks check
whether that timestamp is stale.

With `KFEAT_KWORK_STALL_CHECK`, the per-CPU watchdog task also requests warning-only
workqueue backlog inspection through `WorkqueueStallInspectorIf`. This never runs in
NMI or timer IRQ context, does not panic or recover queues, and does not require
workqueue callbacks to run in order to inspect their progress. The old fatal
system-workqueue check is no longer registered.

The hard-lockup detector uses a periodic NMI to check each CPU's watchdog
tasks, including whether timer interrupts are still arriving. On failure, all
CPUs rendezvous, dump task snapshots, and the cause CPU panics.

## Usage

Initialization is per-CPU and performed by the kernel runtime:

```rust
// Primary CPU, during kernel init.
watchdog::init_primary();
// Each secondary CPU, during its boot.
watchdog::init_secondary();
```

Soft lockup detection requires the `watchdog` feature.  Hard lockup detection
additionally requires `watchdog_hardlockup`, plus NMI support and a selected
NMI source in the platform configuration (on AArch64 QEMU: `KFEAT_NMI` +
`KFEAT_NMI_PMU`, backed by the PMU cycle counter).

## Architecture

Consumers never touch the NMI source.  The watchdog asks for a periodic NMI
through the source-neutral `khal::nmi::enable_periodic_nmi(period_ns, cb)`
interface; the platform backend (currently the PMU cycle counter) owns the
counter, the per-CPU GIC promotion (priority 0 / NMI attribute), and the
one-time global handler registration.

## Notes

- Currently supported on AArch64 only.
- Requires initialization on each core in multi-core systems.
- The NMI interrupt is promoted to the highest GIC priority (0).
- Hard lockup failure response: global snapshot dump via `ktask::snapshot`,
  then panic on the cause CPU.

## Development Status

- ✅ Soft lockup detection (timer + watchdog thread)
- ✅ Hard lockup detection (PMU-backed periodic NMI source)
- ✅ Multi-core support
- ✅ Hardware NMI delivery (GICv3.3, verified on QEMU 11 TCG with
  `-cpu max -accel tcg`, 2026-08-11)
