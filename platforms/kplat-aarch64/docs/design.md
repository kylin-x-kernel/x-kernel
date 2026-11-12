# kplat-aarch64 Design

## PMU NMI Preparation

The runtime NMI mode is initialized before PMU registration and remains stable.
With `nmi-pmu`, an available mechanism selects `register_nmi_startup`; mode
`None` selects an ordinary action, as do PMU-only builds. Ordinary PPIs
(GIC INTIDs 16..32, exclusive upper bound) use `try_register_percpu_startup`
and remain disabled until runtime CPU services call `PmuInterruptIf::enable_irq`
on the BSP and each AP. Ordinary SPIs retain `try_register_startup` with
`disabled()` for `nmi-pmu` builds and `auto_enable()` for PMU-only builds, and
add `restrict_user_affinity()` in both cases. This is a user-only routing
restriction owned by the regular action; it does not block the trusted kernel
setter, change automatic selection, or establish an SPI-to-CPU initial route.
PPI and real-NMI lifecycle paths are unchanged. An ordinary action needs no NMI
preparation or local action publication.
Activation skips `configure_nmi` in mode `None`. Periodic NMI shutdown and global
NMI retirement remain unsupported in that mode; they do not retire the ordinary
fallback action.

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

`arm_periodic_source` owns configuration as one transaction. It rejects NMI,
Closing and any allocated counter before hardware changes or exclusive storage
access. It disables/tears down the vacant source, then prepares the disabled PPI
before installing the counter and callback. Preparation verifies hardware disable
completion and absence of active claims even when no previous preparation record
exists. This is the exclusion proof for the mutable manager borrow, not the
ordinary IRQ guard. A vacant manager is never mutably borrowed by shutdown.

After publication, the counter becomes ready and starts while the PPI remains
masked; only then is delivery enabled. This avoids exposing a stale overflow to
a handler whose software counter is not ready. Setup failure uses checked local
shutdown, retaining storage on teardown failure. Repeated active arm cannot
replace the callback or threshold. The unused platform-level split initialization,
callback-registration and threshold-mutation functions have been removed; the
lower-level PMUv3 driver remains responsible for counter register operations.
Live event reconfiguration is not part of this exclusive periodic-source contract.

Linux's `perf_pmu_disable` / `perf_pmu_enable` bracket event removal, and the Arm
PMU provider stops counters before clearing event slots. This implementation
keeps that owner responsibility outside KIRQ and additionally proves local
delivery exclusion before constructing Rust exclusive storage references.

`shutdown_periodic_nmi` is a separate reusable local operation. It rejects
other allocated PMU counters before changing hardware, stops the cycle counter
and clears its overflow, disables the local PPI, then asks KIRQ to perform
checked local teardown. Failed teardown keeps the counter/callback allocated
for retry. Only successful teardown clears
the local source storage. The shared Line handler remains installed; callers
must collect completion on every participant before global removal. Restart
must prepare the local source and enable its PPI again. No shutdown callback
is installed in KIRQ and no implicit broadcast is performed.
KIRQ's PerCpu gate is non-nesting, like Linux `irq_percpu_disable`: repeated
shutdown simply masks again, and one local enable restores delivery. No separate
PMU disable-credit marker is needed.

An idle CPU may have a startup-prepared/enabled PMU PPI without an allocated
cycle counter. Shutdown still disables and tears down that local line; absence
of a counter skips only the producer-stop step, not controller cleanup. Other
allocated event counters continue to reject exclusive periodic shutdown.
Local IRQ/preemption exclusion serializes attempts; shutdown never initializes
NMI-visible counter storage merely to remember controller state.

## Global PMU Source Retirement

`src/peripherals/pmu/lifecycle.rs` implements the source-owner operations
`PmuInterruptIf::shutdown_overflow_irq` and `restore_overflow_irq`. A sleepable
mutex serializes global shutdown, retry and republication; target callbacks
never acquire this mutex. A source admission flag rejects local counter setup,
counter start and PPI activation while closing. Each local setup holds its
CPU/IRQ guard throughout the transaction: an already admitted start finishes
before that CPU can execute its synchronous shutdown callback.

Shutdown retains KIRQ's original-Line `NmiShutdown` claim. Its pending set
includes every boot-static CPU permitted to start the source, even if it has
no enabled/prepared slot, and every participant reported by the claim. SMP uses
the configured present CPU prefix under the all-boot-CPUs-ready prerequisite;
UP uses only CPU 0, irrespective of additional firmware-described CPUs.
`kipi::run_on_cpu_sync` validates target service readiness rather than treating
presence as readiness. An unavailable target returns an error, never a skipped
acknowledgement. Each remaining CPU stops the PMU and tears down its GIC state.
Completed CPUs are removed from the pending set; a failed
CPU and the original claim remain owned for retry. Completed CPUs are not
contacted again on retry. After all acknowledgements, `finish` verifies
local action publications are revoked and destroys their shared owner in
task context, without an action grace period. Only then is the
state Stopped.
Restoration uses task-context NMI registration before reopening admission;
it neither enables PPIs nor starts counters. Periodic arming prepares locally,
publishes the payload, starts the counter and then enables its PPI, so it also
works after restoration.

The broader stop set closes the pre-preparation startup window: a local arm
may already have passed admission and removed its vacant preparation record.
Its full IRQ-off region finishes before this CPU can run the stop callback.
Admission remains closed until every such callback completes, preventing an
old start from crossing shutdown/republication. This source-owner guarantee
does not depend on a KIRQ action-reader grace period.

The protocol is for exclusive periodic PMU ownership on boot-static CPUs with
IPI services ready. The consumer must authorize service loss; other allocated
event counters reject local shutdown. It is not a general perf event scheduler,
CPU-hotplug lease or system-stop protocol. Startup registration remains a
boot-only operation, distinct from task-context restoration.

Linux v7.2 `drivers/perf/arm_pmu.c` similarly places per-CPU disable/teardown
and final free in the PMU ownership layer, not in generic `free_percpu_nmi`.
Its PMU initialization installs a CPU-hotplug instance for local lifecycle work.
This kernel instead supports a fixed boot CPU set and completed CPU calls; it
does not claim Linux's CPU-hotplug serialization or implement a second hotplug
framework in KIRQ.
Our retained close claim and CPU-local action publications encode lifetime explicitly;
they are not substitutes for the controller/source shutdown obligations.

## Scope

This crate implements the generic AArch64 device-tree platform and common
peripherals. `src/lib.rs` gates the crate on `target_arch = "aarch64"` and
`k_plat_name = "kplat-aarch64"`; peripheral feature gates remain in
`src/peripherals/mod.rs`.

## PMU Interrupt Ownership

The PPI registration path requires KIRQ's resolved `PerCpu` flow; failure does
not retry through generic registration. SPI registration retains compatibility,
not Linux's full SMP PMU SPI ownership model: `of::pmu_irq_or` selects one global
IRQ, without per-CPU SPI assignments or forced affinity. Linux
`drivers/perf/arm_pmu_platform.c` parses those assignments and
`drivers/perf/arm_pmu.c::armpmu_request_irq` binds non-percpu-devid IRQs to their
owning CPU. This migration does not add that SPI routing contract.

The platform prepares the PMU's GIC domain mapping with `kirq::try_map` before
installing either its ordinary IRQ action or NMI action. Mapping preparation
does not configure or enable the source. The mapping belongs to platform
lifetime, independently of the handler; failed installation leaves it reserved
for retry rather than attempting early-boot RCU reclamation. Mapping errors
are logged with the descriptor and returned as registration failure. Runtime
mode selects ordinary or NMI ownership at registration; there is no live
promotion of an ordinary fallback action to NMI ownership.
This startup owner exposes no mapping-disposal operation; explicit preparation
does not change KIRQ's generic action-teardown behavior.

## Guest Test Layout

The ordinary PMU activation test is mounted directly under PMU with the `ipi`
feature, independently of the `nmi-pmu` lifecycle module. It runs in PMU-only
builds and in mode-None NMI-source builds on the GICv2 QEMU override; it skips
when an NMI-source build selects a real NMI mechanism.
It checks ordinary ownership, rejects NMI retirement,
and reads each CPU's real GICv2 PPI enable bit before disable, after disable and
after the production enable path. This verifies post-boot activation and restore,
not the pre-activation disabled state or counter-overflow delivery. Use
`NET=n VSOCK=n QEMU_ARGS='-machine virt,gic-version=2'` with PMU and IPI
enabled, with or without `nmi-pmu`. The hardware-state assertion is specific
to this GICv2 validation.

Guest tests live in crate-root `ut-tests/`, mounted by
`unittest::path_mod_test!` at their original parent modules:

| Parent module | Test file | Cases |
| --- | --- | --- |
| `peripherals::smccc::tests` | `ut-tests/peripherals_smccc.rs` | 4 |
| `peripherals::trng::tests` | `ut-tests/peripherals_trng.rs` | 6 |
| `peripherals::pmu::tests_shutdown` | `ut-tests/peripherals_pmu_shutdown.rs` | Local shutdown ordering, retry, reuse and rejection with another event-counter owner |
| `peripherals::pmu::tests_ordinary` | `ut-tests/peripherals_pmu_ordinary.rs` | Ordinary PPI activation/restoration on each boot CPU and ordinary SPI user-routing policy declaration |
| `peripherals::pmu::lifecycle::tests` | `ut-tests/pmu_lifecycle.rs` | Real cross-CPU PMU/GIC shutdown, injected incomplete pass, retry and republication |

An event-counter fixture verifies that periodic arm and shutdown reject another
owner without changing its counter or callback or entering controller teardown.
Its cleanup retains storage when controller teardown fails.

The PMU setup fixture injects rejection after real controller preparation,
before delivery enable, and during teardown. Its CPU-local fault mask is restored
by a pinned RAII scope; combined faults target failed-setup rollback rather than
the initial vacant shutdown. Hooks exist only in unittest builds and cannot
turn a failed controller operation into success. Assertions cover retained
counter/callback ownership, stopped producer state, rejected reuse and successful
cleanup/rearm after retry. The global fixture also stages a prepared CPU without
counter storage before Closing and checks that its stop obligation is retained.
These ordered interleavings do not claim exhaustive concurrent schedule coverage.
Another injected transition closes only the source admission gate after payload
publication, before counter activation. It verifies local start rejection and
rollback; it is not a substitute for the separate real Line Closing test. The
scope restores the gate and asserts every requested fault was consumed, so an
unrelated earlier rejection cannot silently satisfy the failure case.

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
