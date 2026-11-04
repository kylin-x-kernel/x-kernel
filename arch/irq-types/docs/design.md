# Shared IRQ values

irq-types owns only IrqTrigger, IrqController and IrqDomainId. kirq and
driver-os-api consume this dependency-free no_std vocabulary. kirq retains
its original exports, root constants, domain registry, mapping and lifecycle.
There is no reverse dependency from kirq to driver-os-api.

Controller identifies a family, not an instance. DomainId is an opaque u32,
not registration or authorization. Unknown trigger flags are preserved.
These values share one lib.rs; no allocation, lock or context restriction exists.
