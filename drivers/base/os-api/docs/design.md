# Driver OS capability contracts

This no_std crate replaces device-res, not the device model. Hosts implement
MMIO, IRQ, raw DMA, CPU allocation and monotonic time operations. kdevice owns
resource inventories and LIFO cleanup; kdriver selects providers and maps errors.
One driver-os-api-xkernel adapter implements production capabilities.

Modules group each capability's vocabulary, operations and guards. CPU storage
is separate from DMA; IoPortRange describes ports, not MMIO mappings.
irq-types supplies shared trigger/controller/domain values to kirq and os-api.
Domain values do not register or retain a host mapping.

## Device DMA views
DmaWindow is one fully resolved CPU-to-device window; DmaInfo pairs it with a
device address mask and the effective coherency decision. Neither type applies
ancestor translation, reads firmware or supplies defaults. Identity is an
explicit construction, an absent window is not unlimited, and noncontiguous
masks, disjoint range maps and IOMMU address spaces are rejected rather than
approximated. `translate_range` validates the whole interval, including the
last byte, against both the window and the mask.

DeviceDma is a held, device-scoped capability over one immutable DmaInfo. Every
allocation and mapping is constrained by that view; it never selects a device
from ambient state. Coherent allocations are zero-filled over their full
extent as part of the contract, so queue users never depend on the page
allocator returning clean memory. Coherent buffers are returned with the
original release token, and streaming mappings are non-cloneable guards
carrying the native release hook, so a translated bus address cannot lose its
reverse mapping. Hosts that cannot implement the window or coherency policy
return Unsupported.

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

## MMIO range claims
MmioClaimOp is the host-side registry for exclusive physical MMIO ranges. A
successful `claim_mmio` returns an opaque MmioClaimToken for
`[base, base + size)`; any overlapping live token is refused with Busy, and a
failed call transfers no ownership. MmioClaim owns the exact token and releases
that acquisition on drop, so release never normalizes a region. Implementations
perform the overlap check and insertion as one operation and may allocate while
claiming; callers use sleepable probe/removal task context rather than IRQ or
preemption-disabled context. The contract is limited to its participants:
mappings made through other entry points, and owners that never claim, are not
arbitrated, so a claim is not system-wide MMIO exclusivity.

## IRQ
IrqRouteDesc is an unresolved legacy route. IrqResource distinguishes plain
resolved virq from a mapped virq carrying its origin, without a zero-number
sentinel. `map_fw_irq` is the firmware path: it carries the exact controller
firmware node and the complete binding cells, and the provider maps them through
the host IRQ core. A provider must not re-decode cells, guess a controller
family, or fall back to the raw firmware number; its default is Unsupported.
`map_irq` remains the family-and-hwirq route path for PCI legacy INTx and other
controller-local routes; `IrqRouteDesc.hwirq` is never a GSI. `map_gsi` resolves
a firmware-described GSI through the platform's published controller topology
into the owning domain and local pin; an unowned GSI is InvalidResource, never
an identity mapping. Providers without a GSI namespace keep the Unsupported
default.
Registration and line control resolve both resource shapes by logical number:
the IRQ core descriptor created by the mapping is authoritative, so retained
route metadata is never rebuilt into a second descriptor.
Explicit domains survive conversion. IrqEvent conveys handled/wake-thread;
queue-specific events stay in drivers, not an unused source bitmap.
Threaded defaults remain Unsupported. `try_free_msix` consumes the resource on
success; rejection returns `(ResError, MsiResource)` with the original resource
unchanged and no release side effects. The owner can retry explicitly after
correcting the rejection cause. The default is Unsupported with ownership returned.
`free_msix` is a terminal compatibility wrapper: rejection panics instead of
silently losing the release identity. Explicit teardown must use the fallible API.

MSI release requires source stop, ordering of outstanding messages, synchronous
action detach/drain, a logically disabled line and serialized owner operations.
It runs in sleepable task context without spinlocks, IRQ/preemption exclusion or
RCU read scopes, with host retirement services running. Release establishes none
of these device-side prerequisites itself. This contract adds no retry scheduler,
recoverable action-guard teardown or complete dynamic-remove protocol.
IRQ mock tests cover default ownership return, rejection without release followed
by explicit retry, and successful terminal-wrapper delegation; they do not panic
the kernel to test terminal rejection.

## Extraction scope
No driver-model, class/RNG routing, transport driver, clock control, per-device
DeviceDma policy or NativeIrq entry is introduced. Mainline device lifecycle and
block claim/open/sector-I/O behavior are preserved. UT cover provider pairing,
failure paths, typed coherent views, CPU memory, MMIO and IRQ guards; kdevice UT
exercise the real managed-cleanup implementation.

## Monotonic clock context

TimeOp::monotonic_time is non-allocating and non-sleeping in task, IRQ and
softirq context, including under a short IRQ-safe block-accounting lock. Hosts
must not implement this operation with a sleepable lock or task-only resource.
