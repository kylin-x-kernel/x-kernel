# Driver Subsystem Boundary Rules

This document defines the hard architectural boundary for driver work.

## Model

The driver subsystem is a portable subsystem with two explicit interfaces:

- **Kernel capability contracts**: interfaces that drivers require from a host
  kernel, such as MMIO mapping, IRQ registration, DMA, time, synchronization,
  memory allocation, and work deferral.
- **Device class contracts**: interfaces that drivers provide to the host
  kernel, such as block, net, display, input, char, vsock, and virtio-9p device
  traits.

Concrete drivers must be written against these contracts. Porting the driver
subsystem to another kernel should require implementing the provider contracts
and consuming the class contracts, not editing concrete drivers to call the new
kernel directly.

```text
concrete drivers
        │
        ├── require kernel capabilities through device_res / driver contracts
        │
        └── provide device capabilities through kclass / driver_* traits

host kernel adapter
        │
        ├── implements capability contracts using host kernel APIs
        └── consumes class contracts exposed by the driver framework
```

## Hard Rules

- OS-neutral driver crates must not depend on host-kernel crates such as
  `khal`, `kirq`, `memspace`, `kdma`, `kruntime`, `ktask`, or `kwork`.
- Concrete driver code must not call host-kernel APIs directly. It must use
  driver subsystem contracts such as `device_res`, `driver_base`, `kclass`, and
  device-class traits.
- Host-specific adapter crates may depend on host-kernel crates only to
  implement driver subsystem contracts. Current examples are
  `device-res-xkernel`, irqchip drivers, timer drivers, and X-Kernel integration
  glue under `kdriver`.
- `device_res` remains OS-neutral. It defines capability traits and RAII/devres
  ownership helpers, but it must not depend on X-Kernel implementation crates.
- `kirq`, `memspace`, `kdma`, and other kernel subsystems must not depend on
  `device_res` or concrete driver crates.
- `kdriver` is the X-Kernel integration/orchestration layer. It may hold the
  X-Kernel provider and publish devices into `kclass`, but it must not expose
  host-kernel APIs as the public API expected by reusable concrete drivers.
- New driver needs must be represented as capability contracts first. Do not
  make a concrete driver call a host API as a shortcut.

## Crate Layers

Driver crates are grouped by role. See `drivers/README.md` for the local crate
inventory.

- **Contracts and model**: `driver_base`, `device-res`, `kdevice`, `kclass`,
  and device class crates such as `block`, `char_driver`, `display`, `input`,
  `net`, and `vsock`.
- **Portable concrete drivers**: reusable hardware/protocol drivers such as
  `virtio`. These crates consume contracts and implement device class traits.
- **X-Kernel adapters and integration**: `device-res-xkernel`, `kdriver`, and
  platform/host integration drivers such as `irq-driver`, `timer-driver`,
  `rtc-driver`, `x86-apic`, and early `console-driver` paths.

## Review Checklist

When reviewing driver changes, check:

- Does any OS-neutral driver crate add a dependency on a host-kernel crate?
- Does concrete driver code import `khal`, `kirq`, `memspace`, `kdma`,
  `kruntime`, `ktask`, or `kwork`?
- Is a new provider trait needed for kernel capabilities such as time, wait,
  workqueue, DMA cache maintenance, or threaded IRQ?
- Are device capabilities exposed through class traits rather than host code
  reaching into concrete driver internals?
- Is host-specific glue clearly located in an adapter/integration crate?
