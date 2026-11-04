# Driver Crate Boundaries

The driver subsystem is split by role. Keep new code in the layer that owns the
contract it is implementing or consuming.

## Layers

```text
                  driver contracts and device model
               ┌────────────────────────────────────┐
               │ driver_base / driver-os-api / kdevice │
               │ kclass / block / net / char / ...  │
               └───────────────┬────────────────────┘
                               │
        ┌──────────────────────┴──────────────────────┐
        │                                             │
        ▼                                             ▼
portable concrete drivers                 X-Kernel adapters/integration
consume capability contracts              implement/consume contracts
implement class contracts                 using host-kernel APIs
```

Portable concrete drivers and X-Kernel adapters meet at the published driver
contracts. They must not call into each other through private implementation
details, and portable concrete drivers must not call X-Kernel host APIs
directly.

## Driver Contracts

These crates define the portable driver framework and the contracts between
drivers and a host kernel:

| Crate | Role |
|------|------|
| `driver_base` | Common driver error/result and base device trait. |
| `driver-os-api` | OS-neutral MMIO, IRQ, DMA, CPU memory and time contracts with owning guards. |
| `kdevice` | Shared bus/device/driver model, resource descriptions, topology, lifecycle and device-managed cleanup helpers/storage. |
| `kclass` | Runtime publication layer for typed device classes. |
| `block`, `char_driver`, `display`, `input`, `net`, `vsock`, `hwrng` | Device class contracts that concrete drivers implement and kernel subsystems consume. |

These crates should stay OS-neutral. They may depend on other driver contract
crates and small shared utility crates, but not on host-kernel implementation
crates such as `khal`, `kirq`, `memspace`, `kdma`, `kruntime`, `ktask`, or
`kwork`.

## X-Kernel Adapters

These crates or modules adapt the portable driver contracts to X-Kernel:

| Crate or module | Role |
|----------------|------|
| `driver-os-api-xkernel` | Implements `driver-os-api` provider traits using `memspace`, `kirq`, `kdma`, and `khal`. |
| `kdriver` | X-Kernel driver orchestration: bus discovery, provider holder, concrete-driver glue, and publication into `kclass`. |
| `kdevice-uevent` | Adapts ordered `kdevice` node lifecycle events to X-Kernel kobject netlink messages and shared sysfs formatting. |
| `irq-driver`, `timer-driver`, `rtc-driver`, `x86-apic`, `aarch64-pmuv3` | Host/platform driver integration pieces. |
| `console-driver` | Boot and console integration; ordinary reusable serial drivers should use driver contracts instead. |
| `pci`, `of`, `acpi`, `dice-driver`, `rs_fdtree` | Firmware, bus, and boot-discovery support. Keep host-specific conversions in adapter code, not in `kdevice` or portable drivers. |

Adapter crates may depend on host-kernel implementation crates, but should do so
only to implement or consume driver subsystem contracts.

## Portable Concrete Drivers

Concrete reusable drivers must depend on the driver contracts rather than host
kernel APIs. Current examples include:

| Crate | Expected boundary |
|------|-------------------|
| `virtio` | Uses `virtio-drivers` plus `driver-os-api` / class traits. It must not depend on `khal`, `kirq`, `memspace`, `kdma`, `kruntime`, `ktask`, or `kwork`. |

When a concrete driver needs a new kernel facility, add a capability contract
first, implement it in an adapter crate, then pass the provider through
`kdriver` glue.

## Naming Guidance

- Use `*-xkernel` for X-Kernel implementations of portable contracts, as with
  `driver-os-api-xkernel`.
- Keep portable contracts free of `xkernel`, `khal`, `kirq`, `memspace`, or
  other host implementation terms in their public API.
- Avoid adding broad public helper APIs in adapter crates. Adapter crates should
  mainly expose provider types or integration entry points.

ResourceDesc/Set and explicit-provider managed cleanup helpers belong to kdevice.
The old device-res contract and adapter are removed, not retained as aliases.
