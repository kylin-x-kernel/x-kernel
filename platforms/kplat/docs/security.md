# Platform Contract Boundaries

Providers own hardware and firmware operations. Boot phases retain IRQ masking
and once-per-CPU sequencing obligations. CPU startup accepts validated logical
IDs; translating them to hardware IDs belongs to the platform. Power terminals
do not perform filesystem/process/device cleanup.

PMU IRQ registration precedes activation. NMI source and mechanism protocols
retain their separate contexts. The periodic backend owns routing preparation;
the mechanism API cannot accept an arbitrary IRQ number for promotion. AArch64
PMU preparation requires an initialized CPU mechanism and a disabled registered
PPI on the first call. KIRQ retains
partial-failure cleanup state; callers must not enable delivery after rejection.
The source owner still excludes counter restart during shutdown and cannot use
terminal quiescence as evidence that handlers/resources may be freed.
Reusable `shutdown_periodic_nmi` reports only the calling CPU's producer and
controller teardown. A false result retains resources; successful remote queue
submission is not a successful shutdown result. The owner must serialize restart
and observe completion from every participating CPU before shared handler removal.
`PmuInterruptIf::shutdown_overflow_irq` delegates that global responsibility to
the PMU owner. Consumers must authorize losing periodic service and exclude CPU
hotplug/system stop. Providers retain partial failure and block restoration until
all source/controller acknowledgements and software-reader retirement finish.
`restore_overflow_irq` republishes only the handler, not old consumer callbacks.
MMIO/DMA preparation errors must be handled by
their callers; moving or renaming contracts does not certify rollback on
partial platform sharing failure. The existing AArch64 partial-sharing error
policy is tracked separately in kiface's capability audit.

## x86 system sleep

On x86, `sys::SystemSleepIf` re-exports the [system sleep contract](../../../util/kiface/crates/power-api/docs/security.md).
