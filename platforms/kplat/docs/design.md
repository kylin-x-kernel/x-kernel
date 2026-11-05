# Platform Capability Contracts

kplat owns platform boot and hardware integration contracts, not generic task
or IRQ algorithms. PlatformBootIf coordinates firmware/memory/device phases;
CpuStartIf starts APs, independently of PlatformPowerIf's halt/power/suspend
operations. PmuInterruptIf wires the PMU interrupt; counter callbacks stay in
the PMU implementation. NmiMechanismIf selects/configures delivery while
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

NmiMechanismIf::configure_nmi is a compatibility preparation entry for local
sources: it validates the mode and delegates to KIRQ's per-CPU admission and
irqchip setup. Initial preparation precedes enable; repetition is idempotent
but rejects Closing. Shared sources are unsupported. This contract owns neither
handler retirement nor producer state, and terminal source quiescence does not
mean that local controller teardown or remote handler completion has occurred.

PlatformMmioIf prepares physical device access before mapping. PlatformDmaIf
prepares/releases platform sharing; neither allocates memory or owns the mapping.
Current CPU identity uses kiface-cpu-api. Generated device_interface facade functions
remain convenience calls; the wrapper uses kiface single-provider binding.
