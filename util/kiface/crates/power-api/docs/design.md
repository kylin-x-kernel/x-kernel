# kiface-power-api Design

`SystemSleepIf` defines the platform operations used by the system PM
coordinator. The x86 platform provides them, and `khal::power` re-exports
the interface. The coordinator serializes the transaction, freezes tasks,
quiesces devices and orders clock recovery. The platform prepares wake
resources, saves remote CPU contexts, enters firmware sleep on the boot CPU
and restarts those contexts. It releases wake resources after callback join.
The separate `PlatformPowerIf::suspend_to_ram` entry remains available for
callers of the single-step platform operation.
