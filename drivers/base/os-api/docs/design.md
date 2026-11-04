# Driver OS capability contracts

This no_std crate replaces device-res, not the device model. Hosts implement
MMIO, IRQ, raw DMA, CPU allocation and monotonic time operations. kdevice owns
resource inventories and LIFO cleanup; kdriver selects providers and maps errors.
One driver-os-api-xkernel adapter implements production capabilities.

Modules group each capability's vocabulary, operations and guards. CPU storage
is separate from DMA; IoPortRange describes ports, not MMIO mappings.
irq-types supplies shared trigger/controller/domain values to kirq and os-api.
Domain values do not register or retain a host mapping.

## Ownership and context
IoMap retains its mapping/provider, CoherentMem its exact allocation/provider,
CpuBuffer its original Layout/provider, and DevIrq its resource/action token.
Guards release once on drop using their original static providers.
No unused external-hook or shared-provider acquisition paths are introduced.
Typed coherent views wrap CoherentMem rather than a second allocation owner.
Raw DMA callers retain the original tuple and cache synchronization obligations.

Failed acquisitions register no cleanup. Device-managed raw pointer views expire
at framework cleanup, not at the last device reference's drop. Drivers stop
hardware and consumer accesses first; provider release obeys its context
contract. IRQ detach/drain must complete before dependent storage is reclaimed.
Neither guards nor raw addresses authorize access after shutdown.

## IRQ
IrqRouteDesc is an unresolved route. IrqResource distinguishes plain resolved
virq from a mapped virq carrying its origin, without a zero-number sentinel.
Explicit domains survive conversion. IrqEvent conveys handled/wake-thread;
queue-specific events stay in drivers, not an unused source bitmap.
Threaded defaults remain Unsupported. Consuming MSI free retains its mainline
limitation: no retryable owner is returned on failure.

## Extraction scope
No driver-model, class/RNG routing, transport driver, clock control, per-device
DeviceDma policy or NativeIrq entry is introduced. Mainline device lifecycle and
block claim/open/sector-I/O behavior are preserved. UT cover provider pairing,
failure paths, typed coherent views, CPU memory, MMIO and IRQ guards; kdevice UT
exercise the real managed-cleanup implementation.
