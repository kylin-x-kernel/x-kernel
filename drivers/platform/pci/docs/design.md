# pci — Design

## Purpose

`pci` provides the kernel's PCI bus operations: ECAM/MMIO configuration
space discovery with a clear source precedence (runtime override →
firmware description → build-time constants), a `PciBus` wrapper that
binds a mapped configuration window to the `virtio-drivers` bus model,
BAR address allocation and device configuration, legacy INTx route lookup,
and MSI-X capability setup (`msix` module).

## Responsibilities

- Resolve the PCI configuration space base and bus range via
  `pci_config_space`, returning the chosen `PciConfigSource`
  (`RuntimeOverride`, `Firmware`, `Static`); `set_pci_config_space`
  installs the runtime override.
- Map the ECAM window through `memspace::iomap_device` (`iomap_mmio`) and
  own `PciBus`, which pairs a `PciRoot<MmioCam<'static>>` with a
  `PciConfigAccess` and records its base, bus end, and source;
  `parts_mut` splits it for bus walking.
- Provide `PciConfigAccess`, a re-implementation of MMIO CAM reads and
  16-/32-bit writes (virtio-drivers keeps its own access `pub(crate)`), used
  by MSI-X and other non-virtio callers. Halfword writes let callers update
  PCI Command without writing the adjacent write-one-to-clear Status register.
- Allocate BAR addresses: `PciRangeAllocator` (sequential, power-of-two,
  naturally aligned) plus `pci_bar_allocation_range` source resolution
  (firmware range → kbuild `PCI_RANGES[1]`), and `configure_device` to
  assign unassigned memory BARs and enable IO/MEM/BUS-master command bits.
- Resolve legacy interrupts: `legacy_interrupt_route` reads the interrupt
  pin from config space offset `0x3C` and asks the firmware description
  (`fw::pci_legacy_irq`) for the route, yielding `LegacyInterruptRoute`.
- MSI-X support in `msix`: capability discovery (`find_msix_capability`),
  layout validation against BAR bounds (`validate_msix_layout`),
  table mapping and entry programming on x86_64 (`MsixTable`,
  `configure_msix_entry`), enable/disable (`activate_msix`,
  `disable_msix`, `disable_msix_with_config`), and the `MsixCapability`
  / `MsixTableEntry` wire types.
- Re-export the `virtio-drivers` bus vocabulary (`PciRoot`, `Cam`,
  `BarInfo`, `DeviceFunction`, `Command`, `Status`, ...) and the
  firmware interrupt enums (`InterruptControllerKind`, `InterruptTrigger`
  from `khal::firmware::devices`) so downstream crates such as virtio
  drivers need no direct `khal` dependency.

## Non-Responsibilities

- No bus walking or device driver binding: enumerating `DeviceFunction`s
  and matching drivers belongs to the driver integration layer (`kdriver`).
- No interrupt controller programming: routes and MSI-X messages describe
  targets; actual controller setup belongs to `kirq` and the irqchip
  drivers.
- No ECAM region discovery logic: sources are override, firmware
  description, or kbuild constants; this crate never parses ACPI MCFG or
  device-tree `ranges` itself (that is `acpi` / `of` feeding `khal`).
- No x86 I/O-port (CAM) access: only MMIO configuration spaces are
  supported.
- No DMA or IOMMU management.

## Scope

```text
drivers/platform/pci/
├── src/
│   ├── lib.rs        # config-space resolution, PciBus, PciConfigAccess,
│   │                 # BAR allocation, configure_device, INTx routes
│   └── msix.rs       # MSI-X capability parse/validate/enable/entry setup
└── Cargo.toml
```

## Architecture

```text
kbuild_config ─┐
khal firmware ─┼-> pci_config_space() -> (base, bus_end, source)
runtime override ┘            |
                    memspace::iomap_device("pci-ecam")
                              |
        +---------------------+----------------------+
        v                                            v
PciRoot<MmioCam> (virtio-drivers)          PciConfigAccess (own MMIO CAM)
        |                                            |
   bus walk / bar_info / set_bar_*        MSI-X capability + entry writes
```

`PciConfigAccess` exists because `virtio-drivers` keeps configuration
accesses `pub(crate)`; both types point at the same mapped window and the
cam offset math is duplicated deliberately, documented on the type.

## Execution Context

- Bus initialization is an early-boot, boot-task activity: it needs
  `memspace` device mapping to be available and runs before device
  drivers probe. No userspace, allocator use, or sleeping is involved.
- The runtime override (`set_pci_config_space`) must be established before
  `PciBus::new` reads `pci_config_space`; the plain `Relaxed` atomics rely
  on that ordering rather than synchronization.
- MSI-X table programming (`configure_msix_entry`, `MsixTable`) is
  x86_64-only today (`#[cfg(target_arch = "x86_64")]`); capability
  discovery and enable/disable are cross-architecture.

## Concurrency Model

- Configuration-space override state: two `AtomicU64`/`AtomicU32` with
  `Relaxed` ordering; single-writer (early init), read at bus init.
- `PciConfigAccess` is `Copy` and its reads are volatile, but concurrent
  access to one config window is not otherwise coordinated; bus init and
  MSI-X setup run sequentially on the boot task.
- MSI-X table entries are volatile register blocks (`tock-registers`);
  `MsixTable` claims unique ownership of its table window for its
  lifetime (`# Safety` on `MsixTable::new`).

## Error Model

- `PciInitError` (`NoMemory`, `InvalidRange`, `MappingFailed`) covers bus
  construction, mapping (mapped from `memspace::IoMapError`), and BAR
  assignment.
- Absence is `Option`: no config space (`(0, 0, Static)` sentinel meaning
  "PCI skipped"), no firmware BAR range, no programmed INTx pin, no
  MSI-X capability.
- Panics: `configure_device` expects a BAR allocator to be present
  (`"No memory ranges available for PCI BARs!"`) — assigning BARs without
  an allocator range is a platform configuration error, not a runtime
  condition.

## External Boundary And Inputs

- Configuration space contents are hardware input: header fields (BAR
  sizes, interrupt pin, MSI-X capability) are read as volatile MMIO and
  validated before use — MSI-X table/PBA offsets are range-checked
  against their BAR sizes (`validate_msix_layout`), table entry counts
  capped at 2048, and BAR sizes checked before allocation.
- The firmware description (`khal::firmware::devices`) is trusted
  boot-provided data: ECAM base/size, bus range, BAR range, and INTx
  routes are taken as described; mismatches (CAM kind differs from
  build-selected `Cam`) fall back to static configuration with a warning.
- The MMIO ECAM window is mapped once through `memspace` and never
  hand-mapped here.

## Unsafe Code

- `PciBus::new` / `new_static`: `MmioCam::new` and
  `PciConfigAccess::new` over the `iomap`-mapped ECAM window;
  `SAFETY:` comments cite the full-window mapping.
- `PciConfigAccess::new`: documented `# Safety` contract (valid,
  4-byte-aligned, program-lifetime MMIO mapping sized for all later BDF
  accesses, no aliasing abstraction).
- `read_word` / `write_word`: volatile u32 accesses at cam-computed
  offsets, bounded by the CAM geometry. `write_halfword` uses an aligned
  volatile u16 access within the same selected configuration word.
- `msix::MsixTable::new`: documented `# Safety` contract (mapped table
  with `len` valid entries, unique ownership, lifetime coverage).
- `configure_msix_entry` and entry accessors: volatile writes plus a
  read-back flush of `msg_data`.

## Design Decisions

- Source precedence override → firmware → kbuild: the override exists for
  bring-up and QEMU experimentation without rebuilding; firmware wins over
  build constants because platform description beats stale defaults, and
  `PCI_ECAM_BASE == 0` cleanly means "this platform has no PCI".
- Re-implement `PciConfigAccess` instead of patching `virtio-drivers`:
  the upstream crate intentionally encapsulates config access; a small,
  documented duplicate keeps MSI-X self-contained and avoids forking the
  dependency.
- Firmware CAM-kind cross-check: a firmware/build mismatch silently using
  the wrong bus geometry would corrupt config accesses, so it falls back
  to static configuration loudly.
- Sequential power-of-two BAR allocator: natural alignment is a PCI
  requirement for BARs, and bus-wide assignment is a one-shot boot job,
  so a bump allocator is sufficient and predictable.
- Re-export `khal` interrupt enums: virtio and other PCI consumers get
  OS-neutral interrupt descriptions without each depending on `khal`.
- MSI-X preferred over INTx where available: edge-triggered per-vector
  messages avoid shared-line and routing complexity; INTx remains via
  firmware route lookup for devices without MSI-X.
