# Driver Crate Boundaries

The driver subsystem is split by role. Keep new code in the layer that owns the
contract it is implementing or consuming.

## Layers

```text
              driver framework core and contracts
               ┌────────────────────────────────────┐
               │ kdevice / kclass / driver-os-api   │
               │ driver_base / block / net / char / ... │
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

## Driver Framework Core

[base/](base/README.md) holds the crates that own device/class registries and
runtime state, plus `driver-os-api`, which supplies host capability contracts
without owning device registries:

| Crate | Role |
|------|------|
| `kdevice` | Device core: bus/device/driver model, resource descriptions, topology, lifecycle, binding/removal orchestration and device-managed cleanup helpers/storage. |
| `kclass` | Runtime publication layer for typed device classes, class availability events, and the activation bridge from `kdevice`. |
| `driver-os-api` | OS-neutral MMIO, IRQ, DMA, CPU memory and time contracts with owning guards. |

See [base/README.md](base/README.md) for current dependency facts, prohibited
dependencies, and the known `kclass` virtio-9p coupling.

## Driver Contracts

These crates primarily define capability contracts between drivers, kernel
subsystems and a host kernel; some class crates still carry subsystem runtime
implementations (see [base/README.md](base/README.md)):

| Crate | Role |
|------|------|
| `driver_base` | Common driver error/result and base device trait. |
| `block`, `char_driver`, `display`, `input`, `net`, `vsock`, `hwrng` | Device class contracts that concrete drivers implement and kernel subsystems consume. |

Framework core and contract crates should stay OS-neutral. They may depend on
other driver crates and small shared utility crates, but not on host-kernel
implementation crates such as `khal`, `kirq`, `memspace`, `kdma`, `kruntime`,
`ktask`, or `kwork`.

## X-Kernel Adapters

These crates or modules adapt the portable driver contracts to X-Kernel:

| Crate or module | Role |
|----------------|------|
| `driver-os-api-xkernel` | Implements `driver-os-api` provider traits using `memspace`, `kirq`, `kdma`, and `khal`. |
| `kdriver` | X-Kernel driver orchestration: bus discovery, provider holder, concrete-driver glue, and publication into `kclass`. |
| `kdevice-uevent` | Adapts ordered `kdevice` node lifecycle events to X-Kernel kobject netlink messages and shared sysfs formatting. |
| `irq-driver`, `timer-driver`, `rtc-driver`, `x86-apic`, `aarch64-pmuv3` | Host/platform driver integration pieces. |
| `console-driver` | Boot and console integration; ordinary reusable serial drivers should use driver contracts instead. |
| `pci`, `dice-driver` | Bus and boot-discovery integration. Keep host-specific conversions in adapter code, not in `kdevice` or portable drivers. |

Adapter crates may depend on host-kernel implementation crates, but should do so
only to implement or consume driver subsystem contracts.

## Firmware Descriptions

`firmware/` groups `fwnode`, `rs_fdtree`, `acpi`, and the transitional `of`
compatibility layer. These are not collectively host adapters: contracts,
description interpretation, and host access have distinct responsibilities.
See [Firmware Description Layers](firmware/README.md) for their dependency
direction and the placement of ACPI host services.

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

## Kernel Unit-Test Storage

Portable driver tests prefer inline values and retain existing vector buffers
without adding kernel allocator dependencies. The synthetic MMIO header lives
inline under its existing shared mutex; register borrows end before unlocking.
Network mock handles point into retained payload vectors; moving the vector
owners does not move those payloads. Singleton vectors and unshared reference
counted owners are not substitutes for uniquely owned heap storage.

CG-049 exemptions stay on individual functions or types that require existing
production Box contracts: block device operations, bio memory, waiters, character
class publication, and deferred binding cleanup. The MMIO and coherent-memory
test providers additionally preserve paired GlobalAlloc Box allocation and raw
reconstruction through their opaque address tokens. Portable fixture exceptions
also preserve Box where scoped storage cannot satisfy static firmware/MMIO
lifetimes, a large test backend must keep the UART enum compact, or an erased
FnOnce callback must retain unique ownership across wait calls. Serial fixture
allocation precedes port-locked I/O. RNG actions are taken once from their slot,
called outside its lock, and release their captures on completion. These narrow
exceptions do not authorize portable production drivers to depend on kernel
allocation types or change production ownership contracts.
