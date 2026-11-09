# kiface-cpu-api Design

CurrentCpuIf is provided by kplat; CpuLocalIrqIf by karch; ExceptionContextIf by kcpu; CpuRescheduleNotifyIf by kipi when reschedule delivery is selected. CPU identity is a sample, never a pin. Exception snapshots hold ordinary IRQ exclusion across the synchronous body. These contracts add no state, scheduler or per-CPU storage.

`CpuInterruptReadinessIf::ready_cpus` is provided by runtime's CPU startup owner.
Interrupt-routing consumers can sample a conservative mask without allocation,
locking or local exclusion. It records completed trap/controller preparation,
local source activation and IRQ-exit service setup, not firmware presence or a
runqueue count. Publication may precede local IRQ enable. Fixed IPI transport
keeps its independent readiness contract. The irqchip still owns target
addressing and reachability checks; a ready CPU need not be addressable
by every controller. Membership only grows during boot. This is not a CPU
hotplug or complete online-state API, and it grants no lifetime lease.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
