# kiface-cpu-api Design

CurrentCpuIf is provided by kplat; CpuLocalIrqIf by karch; ExceptionContextIf by kcpu; CpuRescheduleNotifyIf by kipi when reschedule delivery is selected. CPU identity is a sample, never a pin. Exception snapshots hold ordinary IRQ exclusion across the synchronous body. These contracts add no state, scheduler or per-CPU storage.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
