# Platform Capability Contracts

kplat owns platform boot and hardware integration contracts, not generic task
or IRQ algorithms. PlatformBootIf coordinates firmware/memory/device phases;
CpuStartIf starts APs, independently of PlatformPowerIf's halt/power/suspend
operations. PmuInterruptIf wires the PMU interrupt; counter callbacks stay in
the PMU implementation. NmiMechanismIf initializes and reports the CPU/controller mechanism while
PeriodicNmiSourceIf arms a source, terminally quiesces it without releasing
storage, or explicitly shuts down its local producer/controller state for reuse.
Reusable shutdown retains storage on failure and does not free the global Line
handler or broadcast to other CPUs; its owner coordinates those operations.

`PmuInterruptIf::shutdown_overflow_irq` is that PMU-owned global operation for
exclusive periodic sources: it closes startup admission, retains the original
Line and per-CPU obligations, collects completed local stops and retires the
shared handler. Failure preserves retry state. `restore_overflow_irq` publishes
a new handler only after successful shutdown; local activation remains explicit.
Both require sleepable task context and boot-static CPU services, unlike the
boot-only registration and CPU-local IRQ operations. Unsupported backends return
an error rather than pretending that local-only shutdown is global completion.

Delivery preparation belongs to the selected periodic source. AArch64's PMU
backend owns its GIC PPI descriptor and delegates local admission to
`kirq::prepare_percpu_nmi`; native NMI sources need not identify or promote an
ordinary IRQ. `NmiMechanismIf` therefore exposes only early/late initialization
and mode/info. Source-owned preparation does not move generic registration,
dispatch or retirement out of KIRQ. Enabling NMI still requires exactly one
periodic trigger provider, which may be implemented without a PMU.

PlatformMmioIf prepares physical device access before mapping. PlatformDmaIf
prepares/releases platform sharing; neither allocates memory or owns the mapping.
Current CPU identity uses kiface-cpu-api. Generated device_interface facade functions
remain convenience calls; the wrapper uses kiface single-provider binding.

## Reset-aware x86 system sleep

On x86, `sys::SystemSleepIf` re-exports the [system sleep contract](../../../util/kiface/crates/power-api/docs/design.md).
