# driver-os-api-xkernel — Security And Reliability

## Trust and ownership

This host-only adapter implements driver-os-api using memspace, kdma, kalloc,
KIRQ and the host clock. It does not own device lifecycle state, install a
global provider or stop hardware. The caller must stop device DMA and interrupt
generation before resource cleanup and obey the original allocation/token
pairing contracts.

## IRQ teardown

Registration may invoke the callback before returning. KIRQ owns dispatch,
mapping validation and synchronization. Explicit route domains are preserved;
plain virtual IRQs are not reinterpreted as hardware numbers.

Action release requires sleepable context without locks needed by the handler.
Both KIRQ errors and missing actions fail closed rather than returning permission
to reclaim callback resources. This deliberately strengthens mainline's missing
action warning behavior. No shared peer is removed as a fallback.
Tests cover the result classifier without inducing a kernel panic.

MSI reservation and IRQ action lifetimes remain distinct. Mainline MSI free
rejection is still logged, not converted into a retained retry owner. Neither
action drain nor allocation-free MSI deletion proves device-side message
quiescence. Network close, MSI hardware retirement and new threaded IRQ support
are not implemented by this extraction.

## Memory boundaries

MMIO mappings hold the acquisition returned by memspace; unsafe unmap requires
that exact live token and no remaining accesses. This is a mapping reference,
not proof of exclusive physical-range ownership.

CPU storage uses the fallible allocator and original pointer/Layout release;
it performs no DMA sharing or page-attribute conversion. Existing memory tests
exercise sizes, alignment and initialization.

DMA retains the mainline DmaOp implementation and its caller obligations:
coherent metadata must be paired exactly, streaming buffers must remain valid,
and device access must end before release. This migration does not introduce
per-device DMA policy, new bounce handling or hardware validation. The existing
zero-bus streaming-release shortcut is unchanged, not certified as a general
DMA-address rule. Time forwarding retains the host monotonic-clock assumption.
