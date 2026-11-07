# driver-os-api-xkernel — Security And Reliability

## Trust and ownership

This host-only adapter implements driver-os-api using memspace, kdma, kalloc,
KIRQ and the host clock. It does not own device lifecycle state, install a
global provider or stop hardware. The caller must stop device DMA and interrupt
generation before resource cleanup and obey the original allocation/token
pairing contracts.

## IRQ teardown

IRQ resources must refer to descriptors prepared by mapping, MSI-X allocation
or fixed-source setup. KIRQ rejects a missing descriptor before publishing
handler ownership; the adapter does not reconstruct one from a raw IRQ number.

Registration may invoke the callback before returning. KIRQ owns dispatch,
mapping validation and synchronization. Explicit route domains are preserved;
plain virtual IRQs are not reinterpreted as hardware numbers.

`map_fw_irq` passes the untrusted firmware binding cells to the controller's
registered translator by exact controller-node identity. The adapter does not
guess a controller family from compatible strings and does not fall back to the
raw cell as a logical IRQ. Malformed, unsupported, out-of-range, not-ready and
conflicting declarations remain distinct failures; the returned resource is
KIRQ's stored descriptor, so later registration cannot silently replace its
trigger or ownership metadata.

Action release requires sleepable context without locks needed by the handler.
Both KIRQ errors and missing actions fail closed rather than returning permission
to reclaim callback resources. This deliberately strengthens mainline's missing
action warning behavior. No shared peer is removed as a fallback.
Tests cover the result classifier without inducing a kernel panic.

MSI reservation and IRQ action lifetimes remain distinct. `try_free_msix` returns
the unchanged resource on rejection without release side effects. The x86 core
bool cannot distinguish context, identity or live-use rejection: all map to Busy,
not an exact diagnosis or a promise of eventual success. Non-x86 release returns
Unsupported with ownership intact. The inherited terminal `free_msix` wrapper
panics on rejection instead of discarding the resource and continuing cleanup.

The owner must stop message production, order outstanding messages, detach/drain
actions, logically disable the line and serialize owner operations. Release needs
a watched, sleepable task and running RCU service, with no spinlocks, IRQ/preemption
exclusion or RCU read scope. Neither action drain nor allocation-free MSI deletion
proves device-side message quiescence or DMA stop. Returned ownership enables an
explicit retry only while an owner retains it; no automatic retry, recoverable
Drop, complete dynamic removal or new threaded IRQ support is implemented here.

## Memory boundaries

MMIO mappings hold the acquisition returned by memspace; unsafe unmap requires
that exact live token and no remaining accesses. This is a mapping reference,
not proof of exclusive physical-range ownership.

CPU storage uses the fallible allocator and original pointer/Layout release;
it performs no DMA sharing or page-attribute conversion. Existing memory tests
exercise sizes, alignment and initialization.

DMA retains the mainline DmaOp implementation and its caller obligations:
coherent metadata must be paired exactly, streaming buffers must remain valid,
and device access must end before release. DeviceDma adds one bounded,
device-scoped view: noncoherent devices are rejected rather than defaulted,
every coherent allocation is checked over its page-rounded span against both
the window and mask, and streaming guards retain the native kdma release token
so the translated address cannot orphan its bounce mapping. It does not add
IOMMU support, multiple windows, cache maintenance for noncoherent devices or
hardware validation. The existing zero-bus streaming-release shortcut on the
legacy DmaOp path is unchanged and is not certified as a general DMA-address
rule. Time forwarding retains the host monotonic-clock assumption.
