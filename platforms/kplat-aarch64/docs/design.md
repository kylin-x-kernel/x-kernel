# kplat-aarch64 Design

## PMU NMI Preparation

With an available NMI mechanism and `nmi-pmu`, PMU IRQ activation first prepares
the local registered Line through `NmiMechanismIf::configure_nmi` and KIRQ's
`prepare_percpu_nmi`. Failure leaves the line disabled and is diagnosed.
The platform selects the mechanism; irq-driver programs attributes, while KIRQ
owns admission and per-CPU preparation records. Later periodic-source setup
rechecks preparation before starting the counter. Repeated preparation is a
no-op, not live priority rewriting. PMU counter operations remain outside KIRQ
locks. Terminal quiescence still stops only the producer and retains resources;
it is not a reusable-source teardown or proof that remote handlers have drained.

Periodic-source setup pins the CPU and masks ordinary IRQs across the complete
counter/callback/PPI/start sequence, including failure rollback. Per-access guards
alone would allow setup steps to select different CPUs. This local transaction
does not exclude NMI or supply remote shutdown serialization.

`shutdown_periodic_nmi` is a separate reusable local operation. It rejects
other allocated PMU counters before changing hardware, stops the cycle counter
and clears its overflow, disables the local PPI once, then asks KIRQ to perform
checked local teardown. Failed teardown keeps the counter/callback allocated
and remembers the disable credit for retry. Only successful teardown clears
the local source storage. The shared Line handler remains installed; callers
must collect completion on every participant before global removal. Restart
must prepare the local source and enable its PPI again. No shutdown callback
is installed in KIRQ and no implicit broadcast is performed.
Successful shutdown retains its disable-credit marker too: repeated shutdown
must not increase nesting depth. Successful local rearm consumes the credit
and clears the marker; failed rearm leaves it available for retry.

An idle CPU may have a startup-prepared/enabled PMU PPI without an allocated
cycle counter. Shutdown still disables and tears down that local line; absence
of a counter skips only the producer-stop step, not controller cleanup. Other
allocated event counters continue to reject exclusive periodic shutdown.
The disable-credit record is a statically initialized per-CPU atomic slot,
separate from the lazy counter manager. Local IRQ/preemption exclusion
serializes attempts; shutdown never initializes NMI-visible counter storage.

## Global PMU Source Retirement

`src/peripherals/pmu/lifecycle.rs` implements the source-owner operations
`PmuInterruptIf::shutdown_overflow_irq` and `restore_overflow_irq`. A sleepable
mutex serializes global shutdown, retry and republication; target callbacks
never acquire this mutex. A source admission flag rejects local counter setup,
counter start and PPI activation while closing. Each local setup holds its
CPU/IRQ guard throughout the transaction: an already admitted start finishes
before that CPU can execute its synchronous shutdown callback.

Shutdown retains KIRQ's original-Line `NmiShutdown` claim and snapshots its
enabled/prepared participants, not the firmware present-CPU list. It invokes
`kipi::run_on_cpu_sync` for each remaining participant to stop the PMU and tear
down its GIC state. Completed CPUs are removed from the pending set; a failed
CPU and the original claim remain owned for retry. No completed CPU acquires
another nested disable credit. After all acknowledgements, `finish` withdraws
the action and waits for its RCU readers. Only then is the state Stopped.
Restoration uses task-context NMI registration before reopening admission;
it neither enables PPIs nor starts counters. Periodic arming performs local
prepare/enable/start, so it also works after restoration.

The protocol is for exclusive periodic PMU ownership on boot-static CPUs with
IPI services ready. The consumer must authorize service loss; other allocated
event counters reject local shutdown. It is not a general perf event scheduler,
CPU-hotplug lease or system-stop protocol. Startup registration remains a
boot-only operation, distinct from task-context restoration.

Linux v6.12 `drivers/perf/arm_pmu.c` similarly places per-CPU disable/teardown
and final free in the PMU ownership layer, not in generic `free_percpu_nmi`.
Our retained close claim and RCU slot encode software lifetime explicitly;
they are not substitutes for the controller/source shutdown obligations.

## Scope

This crate implements the generic AArch64 device-tree platform and common
peripherals. `src/lib.rs` gates the crate on `target_arch = "aarch64"` and
`k_plat_name = "kplat-aarch64"`; peripheral feature gates remain in
`src/peripherals/mod.rs`.

## PMU Interrupt Ownership

The platform prepares the PMU's GIC domain mapping with `kirq::try_map` before
installing either its ordinary IRQ action or NMI action. Mapping preparation
does not configure or enable the source. The mapping belongs to platform
lifetime, independently of the handler; failed installation leaves it reserved
for retry rather than attempting early-boot RCU reclamation. Mapping errors
are logged with the descriptor and returned as registration failure. Existing
normal-IRQ activation and later NMI promotion remain separate operations.
This startup owner exposes no mapping-disposal operation; explicit preparation
does not change KIRQ's generic action-teardown behavior.

## Guest Test Layout

Guest tests live in crate-root `ut-tests/`, mounted by
`unittest::path_mod_test!` at their original parent modules:

| Parent module | Test file | Cases |
| --- | --- | --- |
| `peripherals::smccc::tests` | `ut-tests/peripherals_smccc.rs` | 4 |
| `peripherals::trng::tests` | `ut-tests/peripherals_trng.rs` | 6 |
| `peripherals::pmu::tests_shutdown` | `ut-tests/peripherals_pmu_shutdown.rs` | Local shutdown ordering, retry and reuse |
| `peripherals::pmu::lifecycle::tests` | `ut-tests/pmu_lifecycle.rs` | Real cross-CPU PMU/GIC shutdown, injected incomplete pass, retry and republication |

The private `tests` modules retain access to parent helpers through `super`.
They are compiled only with `cfg(unittest)`, within the crate's architecture
and platform gates. The SMCCC conduit expectation uses `cfg!(feature = "vmm")`:
both feature settings register all four cases, with different expected HVC
handling. PMU device lifecycle cases are serial and skip when an existing
watchdog/perf consumer owns counters. No case is user-mode.

SMCCC cases exercise conduit decoding and selection without issuing firmware
calls. TRNG cases inject SMCCC results into `read_trng_random_with` to check
errors before data and partial progress after data. Moving these tests does
not change production code or firmware initialization.
