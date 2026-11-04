# driver-os-api-xkernel — Design

This crate migrates the mainline device-res host provider to driver-os-api.
XKernelResourceProvider is the single stateless host implementation; there is
no NativeIrq entry, device-model dependency, provider registry or new bus policy.
The framework owns provider custody and resource cleanup ordering.

## Capabilities

- MmioOp forwards mapping acquisitions and releases to memspace.
- DmaOp preserves the mainline kdma coherent and streaming adapter.
- CpuMemoryOp uses the fallible inherent kalloc allocator, not DMA storage.
- IrqOp uses KIRQ shared hardirq registration and x86_64 MSI-X allocation.
- TimeOp forwards to the host monotonic clock.

These implementations live in src/{mmio,dma,memory,irq,time}.rs.
Kernel tests live in ut-tests and are mounted by their source modules.

## IRQ identity and results

PlainVirq remains a handle-only lookup. DomainMapped retains the virtual IRQ,
hardware IRQ, trigger and controller. An explicit domain always wins; only a
missing domain permits the existing GIC, PLIC or IO-APIC root fallback.
map_irq returns the domain used in the descriptor, not just the original hint.
No root is invented for Unknown, MSI or LoongArchExtioi.

Requests return a shared action token. Regular-action release remains supported
for compatibility, but is not a fallback for failed shared-action release.
NoMemory and VirqExhausted map to NoMemory; InvalidHwirq and the remaining
descriptor errors retain mainline InvalidResource classification.

Ordinary handlers report handled/unhandled. WAKE_THREAD remains handled without
a threaded wake because this path has no paired thread; threaded entry points
retain Unsupported defaults. Device-specific source routing is outside this
contract; the former source bitmap is no longer forwarded to KIRQ.

## Deliberate release change

Mainline warned and returned on Ok(None) from action removal. This adapter
instead treats absence as failed release and panics, as it already does for
KIRQ errors: an infallible return must not falsely authorize reclamation.
The shared result classifier is tested for success, absence and errors.
A future fallible close must preserve its resource owner on failure.

MSI-X behavior is otherwise unchanged: allocation uses Any affinity and a
plain virtual IRQ; free_msix still warns on backend rejection and returns ().
There is no new retry owner, network close protocol or hardware-drain claim.
Non-x86_64 MSI-X allocation remains Unsupported.

## Extraction review checklist

Relative to the mainline device-res-xkernel provider, reviewers should distinguish
the following observable changes from the package/path migration:

| Area | Previous behavior | Extracted behavior and evidence |
|---|---|---|
| Explicit IRQ domain | Controller-root selection ignored the supplied domain | Explicit domain wins for both route mapping and mapped-resource conversion; route-domain and mapped-resource tests in ut-tests/irq.rs |
| Returned mapped identity | map_irq retained controller/hwirq, but not the selected domain | DomainMapped retains the domain actually supplied to KIRQ, including a default root when selected |
| Missing release action | Ok(None) logged a warning and returned | Both regular/shared release reject absence; the infallible outer entry panics; missing_release_action_cannot_authorize_cleanup tests the classifier, not a live panic |
| Handler event metadata | Claimed events forwarded the source bitmap | Only handled/unhandled reaches KIRQ; source bits are zero, and WAKE_THREAD still does not create a threaded wake; ordinary_events_do_not_create_threaded_wakes_or_source_bits covers conversion |
| MMIO API safety | Safe trait and safe token release | Implements the extracted unsafe MmioOp contract and unsafe unmap; the underlying iomap/iounmap behavior is unchanged |

NoMemory/InvalidHwirq classification, request rejection as Busy, shared token
identity, MSI allocation/free behavior, unsupported threaded entry points,
CPU allocation, DMA and time forwarding are preserved. Tests cover selected
error classifications and unsupported threaded calls, not all hardware paths.
Shared IRQ vocabulary now uses irq-types through driver-os-api and KIRQ;
there is no new interrupt controller implementation or second provider.

Source-bit retirement requires consumer-side event routing to remain inside the
driver; provider conversion tests alone do not establish that migration.
Likewise, preserving MSI free warnings does not solve rejected-release ownership
or network close. Those are not silently included in this extraction.

The provider extraction has not run build, clippy or kernel tests locally.
Integration validation belongs to the coordinating PR run; these added tests
are coverage intentions until that run executes them.
