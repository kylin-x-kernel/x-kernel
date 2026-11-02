# Virtual Device Core Ownership Safety

This document covers the MMIO owner boundary. It does not audit device-specific
register semantics, guest DMA translation, interrupt controllers or console
channel synchronization.

Callers allocate a KBox before registering a device. Registration transfers
exclusive ownership; no borrowed reference to a temporary constructor value is
retained. The VM wrapper locks the bus for publication and dispatch. Device
methods and destructors retain their own context requirements; NOWAIT on an
owner does not make those operations nonblocking or interrupt-safe.

The migration adds no unsafe code. A failed owner preparation leaves no owner
to publish; successful transfer drops the concrete device once with the bus.
The bus Vec and nested device resources remain infallibly allocated, so this is
not an end-to-end OOM guarantee. KVMM tests exercise dispatch and destruction
and reject invalid allocation flags; they do not inject global allocator OOM.
