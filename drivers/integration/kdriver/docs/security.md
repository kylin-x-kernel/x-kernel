# kdriver — Security and Reliability Analysis

## Scope

This analysis covers the entire crate: `src/lib.rs`, `src/manager.rs`,
`src/enumeration.rs`, `src/resource.rs`, `src/firmware_dma.rs`,
`src/block_completion*.rs`,
`src/block_irq.rs`, `src/bus/`, and `src/driver_registry/` (including
the VirtIO, block, char, and net driver glue), plus `ut-tests/`, which
is test-only. Reusable concrete drivers behind the registry are audited
in their own crates; this document covers the host integration glue.


## Trust Model

```text
Firmware (DT / ACPI) / PCI configuration space
   |
   | Untrusted: physical addresses, IRQ numbers,
   |            compatible strings, PCI vendor:device IDs
   v
kdriver
   +-- Safe boundary
   |   +-- BusManager enumeration dispatch
   |   +-- DriverRegistrar registration
   |   +-- EnumerationContext buffering
   |   +-- Ownership summary API
   +-- Unsafe boundary
       +-- VirtIoHalImpl
       |   +-- dma_alloc/dealloc
       |   +-- mmio_phys_to_virt
       |   +-- share/unshare
       +-- AhciDriver / SdMmcDriver
       |   Probe (MMIO -> vaddr)
       +-- IxgbeHalImpl
       |   DMA and MMIO address translation
       +-- virtio::identify_mmio_device / open_mmio_transport
           Raw MMIO register reads
   |
   | Validated mappings, IRQ handler closures, DMA buffers
   v
driver-os-api-xkernel / kirq / khal / kdma / memspace / driver crates
```

- `kdriver` trusts `driver-os-api-xkernel` to implement the `driver_os_api` provider contract
  correctly. Driver-facing devres APIs are exposed through
  `kdriver::resource::DeviceResourceExt`.
  `kdriver::resource` explicitly passes `XKernelResourceProvider` and converts
  `ResError` into `DriverError`.
- `driver-os-api-xkernel` trusts `kirq::try_register_shared()` to validate interrupt
  descriptors, manage action fanout, and provide teardown synchronization.
  `driver_os_api` reserves a threaded-IRQ provider contract; the X-Kernel provider based
  on `main` currently still returns `Unsupported`.
- Platform enumeration passes the firmware declaration's exact controller node
  and complete binding cells to `IrqOp::map_fw_irq` before publishing an IRQ
  resource. The provider and IRQ core own cell interpretation, Domain selection
  and descriptor preparation. Only the returned logical identity and
  authoritative metadata are exposed; mapping errors never fall back to a raw
  hardware number, and an unreadable declaration is an enumeration error, not
  an absent IRQ. Platform mapping failure prevents device publication.
  PCI INTx mapping failures are logged by the PCI INTx resolver and leave no
  INTx resource, without excluding later MSI/MSI-X setup. The resolution
  result is stored on the device as the driver-neutral `IntxRouteStatus`; an
  INTx-only probe that reports `NoInterrupt` fails with an error derived from
  that status instead of polling, while a transport-creation failure remains a
  transport error. On x86 the route comes from the published `_PRT` table;
  `pci` reads Interrupt Pin (`0x3D`) and never Interrupt Line. Mapping
  preparation neither installs a handler nor activates hardware. This adapter
  does not dispose a shared route after a failed driver probe.
- Firmware node handles are untrusted only in the sense that the tree they point
  at is firmware-authored. `kdriver` never fabricates or re-searches a handle:
  `register_firmware_device` stores the handle already associated by
  `khal::firmware::devices::for_each_compatible` on the descriptor, and probe
  reads the same handle from the `DeviceObject`. The handle is a borrowed
  `(backend, local id)` pair, not an owned object; the backend must keep its
  storage alive and return `Err(FwError::InvalidNode)` from fallible queries
  for any handle it does not recognise. Infallible accessors such as `name` and
  `parent` return `Option` and therefore report `None` for both an absent
  relationship and an invalid handle. Property access follows the fwnode
  contract: absent or out-of-range values are `Ok(None)`, malformed values are
  `InvalidValue`,
  and unsupported operations are `Unsupported` — a missing value is never
  replaced with a default. Because a `'static` handle cannot by itself prove
  boot-static storage, publication is restricted to boot-static firmware
  backends.
- Firmware DMA declarations are untrusted input. `firmware_dma` requires an
  explicit device mask, walks the DMA-parent path and ancestor windows with
  checked arithmetic, validates the whole interval, and rejects multi-window,
  IOMMU, interconnect-redirected, root-boundary and untranslatable topology
  instead of guessing. A missing `dma-ranges` or coherency declaration is
  never converted to an unlimited window, identity mapping or coherent
  default, except by the QEMU `virt` platform policy: a `virtio,mmio` node
  under a recognized Arm or RISC-V machine root may use the firmware-described
  RAM interval as an identity window, and RISC-V may use its architecture's
  `ARCH_DMA_DEFAULT_COHERENT` value. That exception is scoped to the machine
  root, available nodes and `simple-bus` containers; it is not a general
  fallback. The resulting `DmaInfo` is a constraint passed to `kdma`; it does
  not itself authorize a mapping, and `None` means no resolved view. Platform
  Hwrng probe fails without a view instead of falling back to the static HAL.
- `kdriver` and `driver-os-api-xkernel` trust `memspace::iomap_device` to reject mappings
  of invalid physical address ranges.
- `driver-os-api-xkernel` trusts `kdma::allocate_dma_memory` and
  `kdma::deallocate_dma_memory` to handle matching, valid `(cpu_addr, bus_addr)` pairs.
- `kdriver` trusts `virtio::identify_mmio_device` and
  `virtio::open_mmio_transport` to perform the required volatile-read safety
  checks before accessing MMIO registers.
- `kdriver` trusts each driver crate (`block::ahci`, `block::sdmmc`, and `net::ixgbe`)
  to define the safety preconditions of its `new(vaddr)` entry point correctly.
- External callers in the `kruntime` boot path trust that `init_drivers` runs after
  the platform's `early_driver_init`.

## External Boundaries and Attack Surface

The block wait provider is a safe-code host adapter, not a DMA owner. It
validates sleepable task context before source construction and checks task
identity when waiting. An already-completed request needs no task registration.
Otherwise a scoped stack node is registered under the same IRQ-safe lock
as the completion state. Registration cannot fail or allocate. Spurious wakes
recheck the permanent terminal state. There is no admission wait.
Signals retain no request/device pointer and may outlive the waiter.
Notifications run outside device/registry locks; the constrained scheduler
wake runs under the completion source lock, which also serializes node cleanup.
Arc/Box preparation retains the infallible OOM policy, including BioWait's lazy
post-submission preparation. Pending requests and DMA retirement remain owned
by the device queue independently of the caller-side waiter.

`kdriver` is a core kernel crate that processes hardware description data.
Its attack surface primarily consists of untrusted physical addresses, IRQ numbers,
and device identities supplied by firmware, together with runtime configuration
values in PCI BAR registers.

The module directly or indirectly interacts with these boundaries:

- **Firmware input:** DT compatible strings, ACPI HID/CID, MMIO physical addresses,
  IRQ line numbers, firmware source types (`DeviceTree` / `ACPI`), and firmware
  node handles whose properties and parent links are exposed to drivers
  unchanged through the fwnode contract.
- **PCI configuration space:** Vendor:device IDs, class/subclass, BAR addresses and
  sizes, header types, bridge secondary/subordinate bus numbers, and legacy INTx routing.
- **VirtIO MMIO registers:** Discovery registers such as `MagicValue`, `Version`,
  `DeviceID`, and `VendorID`.
- **Driver probe paths:** MMIO regions mapped through `iomap_first_mmio` or
  `devm_iomap`, and DMA buffers allocated through `devm_alloc_coherent`.
- **Interrupt registration:** IRQ line numbers supplied by firmware or PCI INTx routing
  are converted into `kirq::IrqSpec` by `driver-os-api-xkernel` and connected to `kirq`
  through shared hardirq actions.
- **Compile-time static configuration:** Fixed platform addresses such as
  `kbuild_config::AHCI_PADDR` and `kbuild_config::SDMMC_PADDR`.

This module does not directly dereference userspace pointers; device discovery runs
in kernel process context.
It does parse physical addresses from firmware tables and PCI configuration space,
so physical address validation and MMIO mapping safety are central concerns.

Threat analysis should cover:

- Whether invalid firmware addresses can make MMIO mappings overwrite critical kernel data.
- Whether zero addresses left after PCI BAR allocation can bypass checks and map page zero.
- Whether devres IRQ handlers and `kirq` shared-action tokens have matching lifetimes.
- Whether malformed, absent or ambiguous firmware DMA ranges can widen a
  device's reachable memory or mark it incorrectly coherent.
- Whether mismatched DMA allocation/free layouts can corrupt memory.
- Whether paired `share`/`unshare` operations in the VirtIO HAL withstand malicious device DMA.
- Whether devres release ordering during device removal can cause use-after-free.

## Unsafe Code Inventory

### Block IRQ Provider Delegation

block_irq adds no unsafe code and no IRQ membership registry. Each registered
closure owns only a Weak device and the reclaimer Arc. Activation must retain
the strong device until the returned DevIrq has been dropped and the reclaimer
has stopped. The X-Kernel provider releases only the device's action token and
delegates escaped-callback synchronization to kirq; block does not duplicate
that bookkeeping or mask a shared line. Current kirq synchronization waits for
line-wide in-flight dispatches, not only this device's callback.

Provider resource/capacity errors propagate without publication or fallback.
Platform-local ID exhaustion likewise returns `DriverError::NoMemory` through
the existing enumeration error path, preserving the counter instead of wrapping
or panicking on firmware-controlled device counts.
The closure is dropped on failed registration, releasing its reclaimer clone.
The adapter mock verifies forwarding and token ownership, not asynchronous
release; existing kirq tests cover in-flight dispatch synchronization.
Production activation owns the actual admitted-I/O and transport shutdown path.
BlockActivation remains task-owned and is never captured by an IRQ callback.
Its unique registry close action is taken outside the registry mutex and invoked
by driver.remove before bus teardown, or by failed-probe cleanup. Device-core
serializes removal with begin_removing; devres fallback repeats only after the
first close has returned. IRQ registration and class publication failures run
the same partial-owner destructor without publishing a polling-only disk.

Close prepares task-bound waiters before changing state, marks Closing and
withdraws its owned block lookup, then drains all admitted calls including FIFO
and preparation. IRQ/reclaimer stay live through that drain. Only then does close
suppress device interrupts, release/synchronize the native action, stop the
reclaimer and destroy the transport. Close cannot return a recoverable error
that permits device-core to free resources with DMA still live. Invalid context
is fatal; no forced reset or timeout is introduced. Retained Gendisk references
contain inert device state, not a transport whose final Drop could occur in IRQ.

### Block Device Completion State

`block_completion_dispatch` uses UnsafeCell<ProcessingState> to couple each stable BlockIoReclaimer's state to
the single PENDING_DEVICES lock, rather than adding independently ordered slot locks.
Every state access, list push and list pop holds that lock. Only the owning,
pinned CPU consumes its finite detached batch. Queued also means detached, so
remote requests cannot link the same device completion twice. LinkedList<PendingAdapter>
(an Arc adapter) retains the allocation; the module never manufactures raw ownership pointers.
Double-linking a reclaimer panics; the phase state machine makes that unreachable.

The unsafe Sync implementation relies on this lock discipline, stable Arc
ownership, and Send+Sync devices. The fields remain private to block_completion_dispatch.
Device upgrade/callback/drop happen outside PENDING_DEVICES, and drop precedes publishing
Idle or notifying stop. Stop signals are independently Arc-owned and invoked
outside the lock. Arc Drop does not stop producers; activation must explicitly
call stop_and_wait with a fresh prepared waiter before freeing device resources.
The task-only stop_lock serializes concurrent closers through the wait so the
single stop_notification cannot be overwritten. IRQ/softirq never acquire it.
The only nesting is stop_lock -> PENDING_DEVICES; callbacks never hold either.
The provider trusts finite non-sleeping callbacks and online CPU progress.
No user-controlled pointers or device DMA state enter this executor.

State mutation is centralized in private `update_state`, except the atomic
batch-pop/Running transition under the same lock. The closure cannot return a
borrow into the protected state. The host provider validates bio callers before
submission without allocation. A pending bio prepares its task-bound waiter
when waited on; the block layer serializes notification enrollment with result
publication. Device-close drain/stop still prepare their waiters before exposing
notification handles. Signals never retain a task, device or data pointer.

### 1. driver-os-api-xkernel — DMA Allocation

Location: `drivers/adapters/xkernel/provider/src/dma.rs`

```rust
let info = unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
```

Invariants:

- `layout` is constructed through `Layout::from_size_align(spec.len, spec.align)`,
  which validates the allocation layout.
- The returned `DmaAllocation` is owned exclusively by devres and released through
  `free_coherent` when the `DeviceObject` is removed.
- `cpu_addr` and `bus_addr` describe the same physical memory.

Safety rationale:

- `alloc_coherent` validates the size/alignment through `Layout` before calling
  `allocate_dma_memory`.
- `DmaAllocation` has no public destruction path; release is routed through
  `free_coherent` in the devres callback.
- The safety contract of `kdma::allocate_dma_memory` requires a valid layout and
  exclusive caller ownership of the returned buffer.

Callers:

- Driver probe paths through `devm_alloc_coherent`.

### 2. driver-os-api-xkernel — DMA Release

Location: `drivers/adapters/xkernel/provider/src/dma.rs`

```rust
unsafe { kdma::deallocate_dma_memory(info, layout) };
```

Invariants:

- `info` and `layout` match the original `alloc_coherent` call.
- Each `DmaAllocation` is released once, through devres LIFO cleanup and exclusive ownership.
- Device DMA has stopped before release, as guaranteed by the driver's remove callback.

Safety rationale:

- `free_coherent` reconstructs the layout using the same `Layout::from_size_align`
  arguments as `alloc_coherent`.
- `DmaAllocation` does not implement `Clone`; devres holds the sole ownership.

Callers:

- The `free_coherent` method of the `driver-os-api-xkernel` provider, triggered by
  `driver_os_api::DmaAllocation` destruction or devres release.

### 3. VirtIoHalImpl — Unsafe Trait Implementation

Location: `src/driver_registry/virtio/glue.rs:279`

```rust
unsafe impl VirtIoHal for VirtIoHalImpl { ... }
```

Overall invariants:

- `dma_alloc` and `dma_dealloc` use matching `(paddr, vaddr, pages)` tuples.
- `mmio_phys_to_virt` is called only for physical addresses aligned to `PAGE_SIZE_4K`.
- `share` and `unshare` are paired with matching directions.
- `dma_alloc` zeroes newly allocated memory through `write_bytes(0)` so the device
  cannot read residual kernel data.

Individual operations:

#### 3a. `dma_alloc`

Location: `src/driver_registry/virtio/glue.rs:274,281`

- Constructs `DmaSpec::new(pages * PAGE_SIZE_4K, PAGE_SIZE_4K)` before
  calling `resource_provider().alloc_coherent`.
- Zeroes the allocation through `write_bytes(0)`:
  `unsafe { core::ptr::write_bytes(cpu_ptr, 0, size) }`.
- Returns `(0, NonNull::dangling())` on failure; the VirtIO transport detects and
  reports the error.

#### 3b. `dma_dealloc`

Location: `src/driver_registry/virtio/glue.rs:308,309`

- Uses the same `DmaSpec` as allocation: `pages * PAGE_SIZE_4K` bytes aligned to `PAGE_SIZE_4K`.
- Reconstructs `DmaAllocation` from `paddr` and `vaddr` before calling `resource_provider().free_coherent`.

#### 3c. `mmio_phys_to_virt`

Location: `src/driver_registry/virtio/glue.rs:318`

```rust
unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
    let region = MmioRegion { base: paddr as usize, size };
    match resource_provider().map_mmio(region, "virtio-hal") { ... }
}
```

- The mapping goes through `resource_provider().map_mmio`.
- Mapping failure logs an error and calls `panic!`. This path runs only after the VirtIO transport has confirmed
  that the device exists, so failure indicates a platform configuration error.

#### 3d. `share`

Location: `src/driver_registry/virtio/glue.rs:356,361`

```rust
unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection, ...) -> PhysAddr {
    match resource_provider().map_streaming(buffer, hal_direction(direction)) {
        Ok(m) => m.bus_addr as PhysAddr,
        Err(err) => { /* log */ 0 }
    }
}
```

- `buffer` is a valid buffer supplied to the VirtIO transport by `dma_alloc` or an upper layer.
- `direction` is translated into `DmaDirection` by `hal_direction`.
- On failure, `share` logs an error and returns paddr 0 instead of panicking.

#### 3e. `unshare`

Location: `src/driver_registry/virtio/glue.rs:371,387`

```rust
unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection, ...) {
    if paddr == 0 { return; }
    resource_provider().unmap_streaming(DmaMapping { .. });
}
```

- `paddr` and `buffer` come from the same `share` operation.
- A `paddr` of 0 returns early; otherwise a `DmaMapping` is reconstructed and passed to `unmap_streaming`.
- The `virtio` transport guarantees paired calls.

Callers:

- The `virtio` transport layer: `VirtIoNetDev`, `VirtIoBlkDev`, `VirtIoGpuDev`,
  `VirtIoInputDev`, `VirtIoVsockDev`, `VirtIoRngDev`, and `VirtIo9pDev`.

### 4. VirtIO MMIO Discovery During Platform Enumeration

Location: `src/bus/platform_backend.rs` (`register_firmware_device`)

Discovery performs no MMIO access for `virtio,mmio` nodes. It records the
firmware description — `DeviceLocation::Mmio { base, size }` when the node
declares a first MMIO range, otherwise `FirmwareNode { id }` — together with
the platform identity `firmware_id = "virtio,mmio"` and `transport: None`.

Invariants:

- The MMIO declaration is validated first: an unreadable or unrepresentable
  range rejects the node before any id allocation, IRQ mapping, or
  registration, and the error is returned to the caller.
- Empty slots, unknown device ids and missing MMIO ranges are not filtered
  at enumeration.
- The firmware node already resolved by discovery is stored unchanged.

Safety rationale:

- There is no unsafe register access in this phase, so no MMIO mapping
  validity or register semantics are assumed here.

### 5. VirtIO MMIO Identification During Driver Activation

Location: `src/driver_registry/virtio/mod.rs` (`activate_virtio_mmio`)

```rust
let claim = crate::resource::claim_mmio_region(mmio, "virtio-mmio-probe")?;
let regs = crate::iomap_mmio(mmio.base, mmio.size, "virtio-mmio-probe")?;
let access = MappedMmio {
    regs,
    mapping: MmioMapping::new(VirtAddr::from_mut_ptr(regs.as_ptr()), claim),
};
activate_mmio_with(device, device_kind, vtype, mmio.size, access)
// identify (read-only) -> classify
//   no match: `access` drops, releasing the claim and mapping reference
//   match: `access.open()` moves both into `OwnedMmioTransport`
```

Invariants:

- `regs` comes from `iomap_mmio`, which validates the mapping.
- **Exclusive access is range-scoped, not descriptor-scoped.**
  `read_mmio_header` creates a `UniqueMmioPointer`, whose contract forbids
  other aliases that access the region while it exists. The descriptor
  binding lock cannot establish that: `DeviceManager::rescan` re-enumerates
  (`manager.rs`), the platform backend's default rescan calls `enumerate`
  again (`bus/backend.rs`), and `device_desc_add` gives every registration a
  fresh descriptor and binding lock (`lifecycle/mod.rs`). A live transport
  and a new descriptor for the same firmware range therefore have unrelated
  locks and could otherwise access the same registers.
  The shared `driver_os_api::MmioClaimOp` contract closes that path:
  `activate_virtio_mmio` claims `[base, base + size)` through
  `resource::claim_mmio_region` before the first register read, and the host
  provider's table rejects any overlapping live claim with `ResourceBusy`
  without touching the registers. The claim is released only after the
  transport is dropped (`MmioMapping` unmaps, then its `_claim` field drops),
  so no second probe or transport can alias the range during identification
  or the device lifetime. Duplicate descriptors from rescan are still an
  existing hotplug-removal debt; they now fail safely instead of aliasing
  registers.
  This exclusivity is scoped to claim participants. `memspace::iomap_device`,
  `kdriver::iomap_mmio`, `devm_iomap` and drivers that do not call the claim
  contract are not arbitrated, so the claim is not proof of system-wide MMIO
  ownership.
- `iomap_mmio` takes one mapping reference per successful call, including
  cached hits. The reference is owned by `MmioMapping`. On success that
  guard moves into `OwnedMmioTransport`, whose `transport` field is
  declared first, so the transport's final access and `Drop` reset complete
  before the guard unmaps. On identification/classification/open failure the
  guard drops and releases its reference. No path decides unmapping from
  `dispatch_virtio_try_new`'s return value.
- That ordering bounds transport access; it does not by itself order the
  device owner's DMA releases. Removing a published device drops the class
  runtime first, so a VSOCK or sound owner unsets each queue, resets the
  device, then detaches/unshares/frees RX buffers and releases queue rings
  before the transport, mapping and claim are dropped. The reset return value
  proves the device stopped accessing shared memory (status readback); it does
  not by itself prove host-side IRQ callbacks have quiesced. VSOCK is polled
  and registers no IRQ callback. The reset has no timeout and busy-polls, so
  removal can wait with interrupts disabled. The MMIO integration tests assert
  that the production HAL and provider counters return to their pre-probe
  baseline after removal.
- `identify_mmio_device` checks alignment, minimum region size (`0x100`),
  magic and protocol version, and reads only `magic`, `version` and
  `device_id`. It constructs no `MmioTransport`, so it cannot write
  `DeviceStatus`; identification never resets the device.
- The raw device id is compared with the driver's declared type before any
  transport exists. An empty slot (id 0), a different known type, or an
  unknown id returns `Unsupported` with no side effect.
- `open_mmio_transport` runs only after an exact id match, with the same
  region, size and claim preconditions identification already checked.

Safety rationale:

- Success from `iomap_mmio` means the physical address lies in a valid MMIO
  window and the mapping has been established.
- Both unsafe calls require `regs` to point to a valid, accessible MMIO
  region of at least `mmio.size` bytes; the size is the one recorded during
  enumeration and used to create the mapping.
- The mapping remains valid while the transport exists because the transport
  owns the guard that holds the `iomap_mmio` reference and the range claim.

Callers:

- `activate_virtio_mmio`, through `activate_virtio_device` and the VirtIO driver's `probe_device`.

### 6. AHCI Driver Probe

Location: `src/driver_registry/block/ahci.rs`

```rust
// Ordering only: not data-cache clean/invalidate.
karch::dma_read_barrier();

// Constructor first checks the HAL's explicit coherent-DMA capability.
let ahci = match unsafe {
    block::ahci::AhciDriver::<AhciHalImpl>::new(vaddr, resource::resource_provider())
} { ... };
```

Invariants:

- `vaddr` comes from `iomap_first_mmio(device, "ahci")`, with its lifetime managed by devres.
- The pointer returned by `iomap_first_mmio` remains valid while `device` is alive.
- `karch::dma_read_barrier()` only orders accesses; it does not establish cache
  coherency for data or command memory. The SDK builds PRDT entries directly
  from physical addresses and has no data-buffer cache-sync hook.
- `AhciHalImpl` implements the unsafe `AhciHal` admission contract with
  `DMA_COHERENT = cfg!(target_arch = "x86_64")`. All other hosts are rejected
  before SDK initialization. Non-coherent AHCI requires a separate SDK/HAL
  DMA/cache protocol and must not be enabled by merely setting the flag.

Safety rationale:

- The safety precondition of `AhciDriver::new` requires `vaddr` to point to a valid,
  exclusive AHCI HBA MMIO window.
- `iomap_first_mmio` establishes mapping validity through `devm_iomap` and
  `memspace::iomap_device`.

Callers:

- `AhciDriver::probe_device` (feature `ahci`).

### 7. SDMMC Driver Probe

Location: `src/driver_registry/block/sdmmc.rs:42`

```rust
let dev = unsafe { block::sdmmc::SdMmcDriver::new(vaddr) };
```

Invariants:

- `vaddr` comes from `iomap_first_mmio(device, "sdmmc")`.
- The safety precondition of `SdMmcDriver::new` requires `vaddr` to point to a valid
  SD/MMC controller register region.

Safety rationale:

- Follows the AHCI pattern: `iomap_first_mmio` establishes mapping validity and
  devres manages its lifetime.

Callers:

- `SdmmcDriver::probe_device` (feature `sdmmc`).

### 8. IxgbeHal — Unsafe Trait Implementation

Location: `src/driver_registry/net/ixgbe_hal.rs:33`

```rust
unsafe impl IxgbeHal for IxgbeHalImpl { ... }
```

Individual operations:

- `dma_alloc` (line 34): `DmaSpec::new(size, 8)` followed by `resource_provider().alloc_coherent`.
- `dma_dealloc` (line 49): Reconstructs `DmaSpec`/`DmaAllocation`, then calls `resource_provider().free_coherent`.
- `mmio_p2v` (line 63): Directly converts a physical address to a virtual address through
  `khal::mem::p2v`, assuming that the caller supplies a valid physical address.
- `mmio_v2p` (line 74): Performs the reverse conversion through `khal::mem::v2p`.

Invariants:

- DMA allocation/deallocation use matching `(paddr, vaddr, size)` tuples.
- `mmio_p2v` and `mmio_v2p` run only after the probe has confirmed physical address validity.

Safety rationale:

- The `ixgbe` feature is currently a placeholder and does not enable the downstream
  dependency, so the HAL implementation is not called.

Callers:

- The `ixgbe` driver crate (feature `ixgbe`, currently a placeholder).

### 9. PL011 Serial Driver Probe

Location: [`src/driver_registry/char/serial.rs:99`](../src/driver_registry/char/serial.rs#L99),
where the `SerialKind::Pl011` branch of `resolve_port` calls `SerialPort::new_mmio_pl011`.

Invariants:

- `vaddr` comes from `device.devm_iomap(mmio, "serial")`; `paddr` and `mmio.size`
  describe the same MMIO resource.
- The firmware resource must describe a real PL011 register window, aligned for
  32-bit accesses and covering all registers used during construction and I/O.
- The mapping must remain valid while the port is in use.
  Device removal must stop port accesses before releasing the devres mapping.
- New ports are initialized exclusively before `publish` exposes them.
  The stdout path reuses the existing instance through `take_early_port` without reinitialization.

Safety rationale:

- `devm_iomap` uses the `driver-os-api-xkernel` MMIO provider and `memspace::iomap_device`
  to establish a valid mapping. Mapping errors propagate through `?` before the
  constructor is called.
- The device's devres owns the mapping and manages its lifetime, releasing it on
  probe failure or device removal. `SerialPort` does not own the mapping itself.
- `Pl011SerialDriver::probe_device` completes construction in `resolve_port` before
  exposing the port through `publish`, preserving exclusive initialization.

Callers:

- `Pl011SerialDriver::probe_device` (feature `serial-pl011`).

## Memory Safety Invariants

1. **MMIO virtual address lifetime:** The `NonNull<u8>` returned by `devm_iomap` is valid
   only while the `DeviceObject` is alive. Probe failure or device removal releases
   the mapping through `iounmap`.
2. **Exclusive DMA buffer ownership:** The `DmaAllocation` returned by
   `devm_alloc_coherent` is owned exclusively by devres and has no public clone/copy interface.
3. **Paired DMA allocation/free:** `alloc_coherent` and `free_coherent` reconstruct
   `Layout` from the same `DmaSpec`, preserving size and alignment.
4. **IRQ handler registration order:** A devres handler is wrapped as a `kirq` action,
   then atomically inserted into the descriptor's action list by `kirq`.
5. **IRQ handler release:** Shared hardirq actions are removed by token; regular
   threaded actions are released through their regular action.
   `kirq` masks the line and waits for in-flight hardirq snapshots and the IRQ thread to exit.
6. **No heap allocation during IRQ dispatch:** `kirq` performs action fanout through
   a fixed-size stack snapshot.
7. **VirtIO DMA zeroing:** `dma_alloc` clears new allocations through `write_bytes(0)`
   to prevent devices from reading residual kernel data.
8. **Paired VirtIO share/unshare:** `share` and `unshare` are paired with matching
   directions by the `virtio` transport.
9. **Rejection of zero PCI BAR addresses:** BARs that remain zero after allocation
   are skipped during enumeration and are not registered as valid resources.
10. **Firmware physical address validation:** MMIO addresses supplied by firmware
    pass through `memspace::iomap_device` validation before mapping.
11. **Device identity allowlist:** VirtIO PCI device IDs are translated through the
    `pci_device_id_to_virtio_type` allowlist. Unknown IDs are registered as generic
    PCI devices without binding a VirtIO driver.
12. **Firmware-node handle validity:** A `FwNode` is a `Copy` borrow of a backend
    and a local node id. `kdriver` stores and copies the handle but never
    dereferences backend storage directly. Fallible queries (`property`,
    `mmio_resource`, `irq_spec`, `is_available`, and similar) return a typed
    `FwError` for an unrecognised handle; infallible structural accessors
    (`name`, `parent`, `first_child`, `next_sibling`) return `Option` and
    cannot distinguish an invalid handle from an absent relationship.
    Handles reach probe only from the producer that resolved them during
    firmware discovery, and only boot-static backends are published, so the
    borrow remains valid for the kernel lifetime. A `None` handle means the
    device has no firmware description and is never defaulted or re-looked-up.

## Thread Safety

| Type | Send Conditions | Sync Conditions |
|------|-----------------|-----------------|
| `DeviceManager` | Fields satisfy `Send` | `SpinNoPreempt<BusManager>` provides interior mutability. |
| `BusManager` | `Vec<(BusId, Box<dyn BusBackend>)>` satisfies `Send` | Shared access is protected by `SpinNoPreempt`. |
| `EnumerationContext` | `Vec<DeviceDesc>` satisfies `Send` | Does not implement `Sync`; used by one thread. |
| `DriverRegistrar` | Zero-sized type | Accesses shared state only through the global `kdevice` lock. |
| `driver-os-api-xkernel::XKernelResourceProvider` | Zero-sized type | The provider has no internally mutable state; `kirq` protects IRQ action state. |
| `PCI_BAR_ALLOCATOR` | `SpinNoPreempt<Option<PciRangeAllocator>>` satisfies `Send` and `Sync` | `SpinNoPreempt` provides interior mutability. |
| `PlatformBackend` | The `LocalIdAlloc` field is `Copy` and satisfies `Send` | Does not implement `Sync`; access is serialized by the `BusManager` lock. |
| `PciBackend` | `Cam` satisfies `Send` | Does not implement `Sync`. |
| `VirtIoHalImpl` | Zero-sized type | Calls the `resource_provider()` DMA and MMIO methods, which are responsible for their own thread safety. |

## Threat Analysis

| ID | Threat | Impact Level | Trigger | Mitigation |
|----|--------|--------------|---------|------------|
| T-01 | Invalid firmware physical addresses cause MMIO mappings to overwrite critical kernel data | High | DT/ACPI describes malicious addresses that `memspace::iomap_device` does not reject | `iomap_device` checks platform MMIO window membership and returns `InvalidRange` for invalid ranges. |
| T-02 | A PCI BAR remains zero after allocation, causing a page-zero mapping | High | The BAR allocator is exhausted or its range is uninitialized, and configuration fails to reject zero addresses | `configure_pci_device_if_needed` returns `NoMemory` on allocation failure; enumeration pass 3 skips BARs with `address == 0`. |
| T-03 | IRQ registration races with interrupt arrival, invoking an unready handler | High | The interrupt reaches the virtual IRQ before its action is installed | `kirq` installs the action in its IRQ descriptor before enabling the line according to policy; the threaded default-primary entry enforces generic `ONESHOT`. |
| T-04 | DMA double-free corrupts memory | High | `free_coherent` is called more than once or the layout does not match | `DmaAllocation` does not implement `Clone`; devres has exclusive ownership, and allocation/free reconstruct `Layout` from the same `DmaSpec`. |
| T-05 | A VirtIO device accesses unauthorized kernel memory through malicious DMA descriptors | High | A malicious or faulty VirtIO device constructs an invalid descriptor chain | Individual VirtIO devices currently lack IOMMU isolation; `dma_alloc` zeroing prevents information disclosure, and `kdma` manages `share`/`unshare`. |
| T-06 | Firmware spoofs a device compatible string and binds the wrong driver | Medium | A false DT compatible string matches a registered `FirmwareMatchSpec` | An unresponsive device fails probe and enters the unclaimed list. |
| T-07 | A PCI device spoofs its vendor:device ID and matches the wrong VirtIO type | Medium | A malicious PCI device advertises the Red Hat vendor ID and a known VirtIO device ID | `probe_pci_device` validates the transport response again during activation and returns `Unsupported` on mismatch. |
| T-08 | Too many handlers on a shared IRQ cause unbounded traversal | Medium | More than 4 handlers register on one IRQ | `request_irq` returns `ResError::Busy`; dispatch uses a fixed-size stack snapshot. |
| T-09 | A PCI BAR allocator race assigns the same MMIO address to two devices | Medium | Concurrent BAR allocation is not serialized correctly | `PCI_BAR_ALLOCATOR` is protected by `SpinNoPreempt`, and allocation occurs under the lock. |
| T-10 | The serial driver remaps the stdout UART's MMIO and creates duplicate ownership | Medium | The serial driver calls `devm_iomap` again for the stdout node | Serial probe uses `take_early_port` and `SerialIdent` to adopt the early stdout instance without a second mapping. |
| T-11 | Incorrect devres release ordering frees resources while a device still uses them | Medium | The driver's remove callback returns without stopping device DMA | Devres LIFO cleanup preserves release ordering; the driver's remove callback is responsible for stopping the device. |
| T-12 | VirtIO MMIO probe reads unmapped or invalid registers | Medium | Firmware advertises `virtio,mmio` but no VirtIO device exists at the physical address | `identify_mmio_device` checks alignment, minimum size, `MagicValue` and version, reads only read-only header fields, and performs no write; a missing protocol fails the candidate. |
| T-13 | An unchecked firmware IRQ number is registered on the wrong vector | Medium | `kirq::register` does not validate the IRQ number sufficiently | Depends on the platform IRQ backend and `kirq` descriptor handling; IRQ numbers passed by `kdriver` come from firmware or PCI INTx routing. |
| T-14 | Static platform device addresses such as AHCI_PADDR are misconfigured at compile time | Low | `kbuild_config` constants specify invalid physical addresses | `iomap_first_mmio` validates through `devm_iomap` and `iomap_device`; mapping failure prevents driver activation. |
| T-15 | Incorrect conversion between devres and kernel IRQ types causes bad IRQ configuration, lost handled results, or unwoken IRQ threads | Medium | The `driver-os-api-xkernel` adapter omits trigger/controller/handled/wake-thread fields | `driver-os-api-xkernel` centralizes `driver_os_api` to `kirq` conversion; device-specific queue routing stays in the driver, and the IRQ core does not depend on devres. |
| T-16 | VirtIO input is published before its interrupt action is usable | Medium | Input construction succeeds but IRQ registration fails or races publication | `virtio/input.rs` requires an IRQ, registers its devres action first, and calls `publish_input` only after registration succeeds. |
| T-17 | A stale or forged firmware-node handle lets probe read another device's properties | Medium | A handle is constructed for storage that has been freed, or one backend is confused with another | `FwNode` equality includes the backend pointer and local id; fallible backend queries return `FwError::InvalidNode` for unknown handles (infallible `name`/`parent` accessors return `None`); only boot-static backends are published; `kdriver` copies the handle resolved during discovery and never re-searches by name or compatible. |
| T-18 | Two enumeration/driver-registration paths bind the same descriptor, producing duplicate devices or a deadlock | Medium | A backend rescans while a probe or another registration scan is in flight | `kdevice` serializes binders on the descriptor's stable binding lock; `kdriver` treats `Busy` (same-task recursion only) and core errors as "leave Pending, log, retry later", and `Skipped` as already bound. |

Impact levels:

- High: Undefined behavior, memory corruption, or privilege escalation.
- Medium: Panic, service unavailability, or inconsistent state.
- Low: Performance degradation, lost logs, or reduced functionality.

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Cause | Local Effect | System Effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | PCI bus enumeration fails | ECAM/MmioCam mapping failure or inaccessible configuration space | All PCI devices unavailable | PCI-dependent functions such as virtio-blk/net/gpu are unavailable | 2 | Log an error and return when `PciBus::new` fails, without blocking platform bus enumeration. |
| F-02 | Firmware provides no device description | Missing DT/ACPI tables or `has_device_description()` returns `false` | No devices registered through firmware enumeration | Only static devices remain, such as ramdisk and compile-time AHCI/sdmmc | 3 | Static device discovery is independent of firmware; log at info level. |
| F-03 | One device fails probe | Every matching driver's `probe_device` returns an error | That descriptor stays `Pending`; no runtime object leaks | Other devices on the same bus activate normally; a later probe or driver registration may retry | 4 | `kdevice` rolls each candidate back and requeues; `kdriver` logs the `Unclaimed` outcome and places the descriptor in the unclaimed list, then continues with subsequent devices. |
| F-04 | PCI BAR allocator uninitialized | `pci_bar_allocation_range()` returns `None` and a device has unassigned memory BARs | The PCI device is skipped | One PCI device unavailable | 3 | `configure_pci_device_if_needed` returns `NoMemory`, and the device is skipped. |
| F-05 | VirtIO MMIO probe finds no matching device | Empty slot (id 0), a different or unknown device id, or a `MagicValue` mismatch | The candidate fails with `Unsupported`/`Io` and the descriptor stays `Pending` | Other platform devices and candidates are unaffected | 4 | The raw id is classified before any transport is constructed; identification is read-only, so a failed candidate cannot reset the device. Log the raw id and continue with the next candidate. |
| F-06 | IRQ handler registration fails | `kirq` rejects a shared action or its action limit is reached | The device cannot receive interrupts | Block and input devices are not published; no polling fallback | 3 | `request_irq` returns `Busy`; partial activation is dropped before probe returns an error. |
| F-07 | PCI host bridge adoption fails | Platform bus unregistered or `adopt_active_device` fails | PCI devices have no host bridge parent | PCI endpoints are still enumerated, but the device tree is incomplete | 3 | Log a warning and continue enumeration with a parentless layout. |
| F-08 | Static device MMIO mapping fails | Invalid `kbuild_config` address or absent hardware | The static device unavailable | Other devices on the same bus unaffected | 3 | `iomap_first_mmio` returns an error, and probe fails. |
| F-09 | Bus-type matcher is not ready when a driver registers | `register_bus_type` runs after driver registration | No device matches the driver | Devices enter the unclaimed list | 2 | `default_bus_manager` registers bus-type matchers before bus backends; drivers are then registered in `DeviceManager::new`. |
| F-10 | Rescan produces duplicate device descriptors | The backend uses the default `rescan` hook and reruns full `enumerate` | `kdevice` may reject duplicates or create redundant descriptors | Incomplete hot-plug support | 3 | Default `rescan` reruns `enumerate`; backends may override it with incremental scanning. |
| F-11 | VirtIO transport type differs from the driver's declaration | Reported `DeviceKind` (PCI) or raw MMIO device id differs from the matched driver's `device_type` | Driver probe returns `Unsupported` before constructing a transport | The device enters the unclaimed list | 4 | `activate_virtio_pci` revalidates the transport type; `activate_virtio_mmio` compares the read-only raw id and only then opens the transport. |
| F-12 | Quiesce fails to stop device interrupts | Incorrect bus backend `quiesce` implementation or delayed hardware response | Interrupts continue arriving during shutdown | IRQ handlers may access released resources | 2 | Devres resources are released during remove, not quiesce; quiesce only masks interrupt sources. |
| F-13 | A device reaches probe without a firmware node | Static registration, PCI enumeration or adoption intentionally passes `None`, or discovery had no node | Probe cannot read firmware properties | Driver must rely on static configuration or fail with its own error; no incorrect association is invented | 4 | `None` is the documented normal case; probe queries firmware only when a handle is present and never falls back to a name/compatible lookup. |

Severity levels:

- 1: Fatal; system crash or data loss.
- 2: Serious; functionality unavailable and restart required for recovery.
- 3: Moderate; degraded functionality with automatic recovery possible.
- 4: Minor; limited impact that users can tolerate.

## Failure Management

- Device probe failures return `DriverError` (`InvalidInput`, `Io`, `NoMemory`,
  `ResourceBusy`, `Unsupported`, or `BadState`) without panicking.
- PCI bus initialization failure logs through `error!` and returns `Ok(())`, allowing
  platform bus enumeration to continue.
- Individual firmware device registration errors are collected through
  `first_error.get_or_insert`; enumeration returns the first error after completing.
- MMIO mapping failures are converted through `memspace::IoMapError`, `ResError`,
  and `DriverError`, with traceability at each layer.
- IRQ registration failures return `ResError::Busy`, `ResError::NoMemory`,
  `ResError::Unsupported`, or `ResError::InvalidResource`; the upper probe layer
  returns the corresponding error.
- Unmatched devices enter the `unclaimed` list. `info!` logs their identity,
  location, and origin to help diagnose missing drivers.
- There is no blacklist: a descriptor whose matching candidates all failed
  stays eligible for a later probe or registration scan, and the probe
  statistics are cumulative diagnostics rather than a cooldown.
- Driver registration delegates to the `kdevice` scan, which waits on a
  descriptor currently being bound by another task and rechecks after the
  wait. Registration attempted from inside the same descriptor's probe is
  logged and skipped, not retried automatically.
- Re-entry across descriptors is not detected at runtime:
  `is_owned_by_current()` only diagnoses the descriptor whose lock the current
  task holds. Two tasks can each hold one descriptor lock and register a
  driver, then wait on each other's descriptor. No global hold tracking exists;
  auditing production probe and remove hooks for `register_driver_object` is
  the current gate.
- Except for the `expect` in `VirtIoHalImpl::mmio_phys_to_virt`, which runs only after
  the VirtIO transport has confirmed device presence, unsafe failure paths return
  `Result` or log errors.
- Panic paths primarily come from `expect` after `LazyInit::call_once`, where static
  initialization failure indicates a platform configuration error; `LocalIdAlloc::alloc` exhaustion returns `DriverError::NoMemory` instead of panicking.

## Privacy Analysis

`kdriver` processes firmware device identity information: compatible strings,
ACPI HID/CID, PCI vendor:device IDs, and class/subclass values.
It also handles hardware resource descriptions such as physical addresses and IRQ numbers.
Debug/info logs include device names, BDF addresses, physical address ranges, and IRQ
line numbers, without user process data.

The module does not persist data.
Device topology is stored in the shared `kdevice` core and its lifetime is managed
by the global device registry.

Trace logs expose compatible strings encountered during firmware enumeration and
register values read during VirtIO MMIO discovery.
Production deployments should control exposure through log levels.

## Known Limitations

- PCI supports only segment 0. Multiple segments require extending the `PciBackend`
  domain parameter.
- The PCI BAR allocator uses simple sequential allocation through `PciRangeAllocator`,
  without BAR relocation or defragmentation.
- A shared IRQ supports at most 4 handlers; exceeding the limit prevents device activation.
- The `ixgbe` feature is a placeholder. Its HAL implementation is not called and has
  not been validated at runtime.
- Without a firmware description, `fxmac` only logs a warning and skips the device;
  its MMIO base cannot be obtained from compile-time configuration.
- Firmware enumeration does not process ACPI _DSD or complex device properties;
  it reads only compatible/HID/CID, MMIO, and IRQ resources.
- PCI hot-plug is supported only through full re-enumeration with `rescan`.
  There is no native hot-plug event handling or PCIe AER / hot-plug controller driver.
- Explicit device-owner teardown (queue unset and reset before buffer/ring
  release) exists only for VSOCK and sound. Other VirtIO device kinds rely on
  their pre-existing transport/queue `Drop` paths and have not been audited
  for the same ordering guarantee.
- Firmware-node association assumes boot-static firmware storage. Backends are
  borrowed, not owned or reference-counted; there is no teardown path for a
  dynamically created backend, and `FwNode`'s `'static` bound cannot detect one
  that is deliberately leaked. Dynamic firmware-node providers are out of scope.

## Audit Checklist

When modifying this module, verify:

- Every `unsafe` block has a `SAFETY:` comment.
- New MMIO mapping paths use `devm_iomap` or `iomap_mmio`, which validate physical
  address ranges internally.
- New DMA allocation paths use `devm_alloc_coherent` or `kdma::allocate_dma_memory`,
  with matching layouts at `free` time.
- New IRQ registration passes through the `driver-os-api-xkernel` devres provider into
  the `kirq` action list, instead of maintaining separate line-local fanout in `kdriver`.
- VirtIO input keeps PCI INTx masked until its devres handler is installed,
  masks it again before handler release, and leaves an empty input ISR
  unclaimed. Its input-layer notifier is called only for a queue interrupt;
  configuration interrupts do not wake input readers.
- New IRQ release uses `driver-os-api-xkernel` to call the matching `kirq` release API
  by token; `kirq` performs masking, synchronization, and cleanup after the final action leaves.
- When a new bus backend implements `BusBackend`, an individual device error in
  `enumerate` does not block enumeration of other devices.
- New PCI device-ID to VirtIO-type mappings are added in `pci_device_id_to_virtio_type`.
- New firmware compatible matching rules are declared in `firmware_specs.rs` and
  register the corresponding platform driver.
- New `DeviceDriver::probe_device` implementations return `DriverError` on failure
  rather than panicking.
- New firmware-node producers pass the node through `DeviceDescSpec` /
  `register_firmware_device` instead of omitting it or looking the node up again;
  new producers of a device without firmware description pass `firmware_node: None`
  explicitly. Probe code reads `DeviceObject::firmware_node()` and must not
  construct a second property parser or search the firmware tree by compatible.
- New `VirtIoHal` methods preserve `share`/`unshare` and `dma_alloc`/`dma_dealloc` pairing.

## Hardware entropy completion provider

`wait_until` requires a sleepable current task. `CompletionWait::prepare`
rejects invalid context before the driver's predicate can consume bytes.
Task interruption does not cancel this internal read. The device notification
registration is removed by RAII when the wait ends.

The provider registers wake sources before checking readiness. A notification
between that check and `park` remains a wake hint, preventing a lost completion;
spurious wakes only trigger another check. The driver must call IRQ-safe
`notify` after releasing its request lock, and its predicate must not sleep.
The provider retains neither the predicate nor DMA storage. Invalid context and
preparation failures do not establish a device fault. Once the predicate is invoked,
scoped registration and wakeup are allocation-free and cannot return registration errors.
