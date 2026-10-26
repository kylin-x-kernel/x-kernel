# Platform Capability Contracts

kplat owns platform boot and hardware integration contracts, not generic task
or IRQ algorithms. PlatformBootIf coordinates firmware/memory/device phases;
CpuStartIf starts APs, independently of PlatformPowerIf's halt/power/suspend
operations. PmuInterruptIf wires the PMU interrupt; counter callbacks stay in
the PMU implementation. NmiMechanismIf selects/configures delivery while
PeriodicNmiSourceIf arms/quiesces a source.

PlatformMmioIf prepares physical device access before mapping. PlatformDmaIf
prepares/releases platform sharing; neither allocates memory or owns the mapping.
Current CPU identity uses kiface-cpu-api. Generated device_interface facade functions
remain convenience calls; the wrapper uses kiface single-provider binding.
