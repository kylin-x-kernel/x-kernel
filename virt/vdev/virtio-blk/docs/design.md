# vdev-virtio-blk — Design Document

## Positioning

`vdev-virtio-blk` is the VirtIO MMIO block device model crate of X-Kernel. It assembles the
transport/queue infrastructure provided by `vdev-virtio` into a usable block device: the device configuration space
(capacity, geometry, I/O limits), the request data path (OUT/IN/FLUSH), and a pluggable
`BlockBackend`. `bus::VirtioBlkDevice` then adapts the model to
`vdev_core::MmioDevice` and registers it on the VMM's MMIO bus.

This crate belongs to the reusable virtual device layer: it depends only on `vdev-core`, `vdev-virtio` and synchronization primitives,
and does not depend on `kvmm`, the architecture HAL, or kernel memory management. The VM only needs to provide the `GuestDma` translation capability and
the `IrqSender` injection capability.

## Background

The original implementation lived in `src/vdev/virtio_blk/` and `virtio_blk_bus.rs` of `kvmm`. The device model
itself depends only on `vdev-core` interfaces, and `bus.rs` uses only
`vdev_core::{GuestDma, IrqSender, MmioDevice}`. After the whole thing was moved out, `kvmm` no longer holds
any VirtIO device code; only `kvmm-api` constructs and registers this device when assembling a VM.

## Scope

```
virt/vdev/virtio-blk/
├── src/
│   ├── lib.rs          # crate entry and re-exports
│   ├── backend.rs      # BlockBackend trait
│   ├── bus.rs          # VirtioBlkDevice + RamDiskBackend (MmioDevice binding)
│   ├── selftest.rs     # host-side driver sequence self-test
│   ├── block/config.rs # VirtioBlockConfig configuration space
│   ├── constants/      # block device constants (re-exports the common vdev-virtio constants)
│   └── mmio/           # VirtioMmioBlockDevice + BlockDeviceEvent + request header
├── ut-tests/device.rs  # 3 mock device regression cases in the kernel harness
└── Cargo.toml
```

| Component | Responsibility |
|------|------|
| `mmio::VirtioMmioBlockDevice<B, T>` | Combines `VirtioMmioState<T>` with the block configuration, handles configuration space reads and the request queue |
| `backend::BlockBackend` | Storage backend contract: `read`/`write`/`flush` and whether processing is deferred |
| `bus::RamDiskBackend` | Synchronous in-memory RAM disk backend (16 MiB by default) |
| `bus::VirtioBlkDevice` | Implements `MmioDevice` with `GuestDma`/`IrqSender`, forwards MMIO and used-ring interrupts |
| `mmio::BlockDeviceEvent` | Action the VMM needs to perform after an MMIO write |
| `selftest::vmm_selftest_virtio_blk` | Complete driver sequence self-test without starting a VM |

### Kernel Unit Tests

The original 3 host `#[test]` cases were moved in full into `ut-tests/device.rs`. Through
`unittest::path_mod_test!` the private `mmio::device::tests` module is preserved,
and `#[unittest::def_test]` registers the over-limit request, capacity out-of-bounds, and reset-clears-deferred-request-head cases.
The original assertions and the private accesses to `process_request` and `pending_head` are unchanged;
`unittest` uses a regular workspace dependency and adds no public business interface.

The tests use local Vecs, mock memory and a synchronous mock backend, so no VM, real block device or real IRQ needs to be created.
They run in the kernel harness under `cfg(unittest)`, which requires an initialized allocator and an
IRQ/preempt guard runtime environment; each case constructs its own device and buffers, and releases the resources on normal return.
The configuration requires this crate to be linked into the kernel; `KFEAT_VMM=y` can be enabled along the existing VMM dependency path, and then
`make unittest UNITTEST_CRATE=vdev_virtio_blk` is used to run with filtering.
These cases are no longer executed by the bare Cargo host harness; the callable self-test in `src/selftest.rs` remains unchanged.

## Architecture

```
┌───────────────────────────────────────────┐
│            VMM MMIO bus(vdev-core)          │
│        MmioDevice::read/write_for_vcpu     │
└───────────────┬───────────────────────────┘
                │
┌───────────────┴───────────────────────────┐
│           bus::VirtioBlkDevice             │
│  address translation → mmio_read/mmio_write_with_memory│
│  event translation → IrqSender / deferred latch      │
└───────────────┬───────────────────────────┘
                │ DmaGuestMemory(vdev-virtio)
┌───────────────┴───────────────────────────┐
│      mmio::VirtioMmioBlockDevice<B, T>     │
│  VirtioMmioState (standard registers) + block config space    │
│  request parsing: header / data / status descriptor chain   │
└───────────────┬───────────────────────────┘
                │ BlockBackend
┌───────────────┴───────────────────────────┐
│   RamDiskBackend / other backend implementations             │
└───────────────────────────────────────────┘
```

## Calling Constraints / Execution Context

- **MMIO reads and writes come from the vCPU exit path**: task context, not interrupt context; `write_for_vcpu`
  processes the queue of a synchronous backend inline.
- **Synchronous backends are processed inline**: `RamDiskBackend`'s `requires_deferred_processing()`
  is `false`, and `QUEUE_NOTIFY` completes the reads and writes within the MMIO write path and immediately injects the used-ring interrupt.
  If a backend is changed to one that may block, `VirtioMmioBlockDevice` returns `QueuePending`, and the VMM needs to provide a
  polling path; currently `VirtioBlkDevice` defensively records this with the `deferred_pending` bit.
- **Guest memory is injected through `GuestDma`**: a `GuestPhysAddr` is first translated into an offset within the device, and then handed to
  `DmaGuestMemory` for reads and writes; it does not rely on the assumption of a direct host mapping.
- Interrupt injection is done by `IrqSender`: the IRQ number is fixed in `bus.rs` (slot 1: base `0x0a00_1000`, IRQ 49), and the target vCPU is the `vcpu_id` of the vCPU that performed the MMIO write (plain `write` uses vCPU 0).
- **No sleeping, no dependence on the scheduler**: it can be constructed and registered during boot.

## State Machine

### Request Processing

```
Idle ──QUEUE_NOTIFY──> Process ──synchronous backend──> Complete ──used ring + IRQ──> Idle
                          │
                          │ backend WouldBlock
                          └──> QueuePending (handed to the polling/deferred path)
```

### Transport Reset

```
Active ──status write 0──> Reset (discard pending_head) ──> Idle
```

`pending_head` records a descriptor that was taken out but not completed when the backend defers processing; reset must clear it, to avoid
processing a stale request after reset.

### Backend Deferral Policy

- `requires_deferred_processing() == false`: processed inline, and the MMIO write returns
  `BlockDeviceEvent::InterruptPending` or `None`.
- `== true`: the MMIO write returns `QueuePending(index)`, and the runtime calls
  `process_pending_queue`.

## Algorithm Flow

### Configuration Space Read

1. `VirtioMmioState::mmio_read` handles the standard registers.
2. Configuration region reads are handed to `read_config_space`: capacity, `size_max`, `seg_max`, geometry,
   block size, etc.

### Request Processing (`handle_queue_notify`)

1. Validate `DRIVER_OK` and queue validity.
2. Loop to fetch the head (`pending_head` first, otherwise pop from the available ring).
3. `process_request`:
   - Validate that the descriptor chain length is ≥ 2; the first descriptor is the device-readable request header; the last is the writable status.
   - Parse `request_type` and `sector`.
   - `IN` → the backend reads into the buffer and writes it back to each data descriptor; `OUT` → read into the buffer from the data descriptors
     and write to the backend; `FLUSH` → `backend.flush()`.
   - Write back the status (OK/IOERR/UNSUPP); `WouldBlock` returns `None` and suspends.
4. `complete_with_memory` writes the used ring; if necessary `trigger_interrupt` sets the interrupt bit.

### Address and Capacity Validation

- `validate_data_request` checks the descriptor count/single-segment length, that the total length is aligned to 512 bytes, and
  that the request range falls within the capacity; on failure it returns `InvalidBufferSize`/`InvalidSector`, and completes
  the request with IOERR instead of panicking.

## Concurrency Model

- The internal state of `VirtioMmioBlockDevice` is protected by the `SpinNoIrq` lock of `VirtioMmioState`;
  `pending_head` is additionally protected by `SpinNoIrq<Option<u16>>`.
- Shared through the `Arc<T>` accessor; `RamDiskBackend` uses `kspin::SpinNoIrq<Vec<u8>>` to serialize
  RAM disk accesses. Backend I/O runs inside the queue `SpinNoIrq` lock, so the backend lock must also be a non-sleeping
  spin lock: `ksync::Mutex` blocks the current task under contention, and at that point local interrupts are already masked,
  which could deadlock.
- `deferred_pending` of `VirtioBlkDevice` is an `AtomicBool`, used only as a defensive record.
- Interrupt injection calls `IrqSender::inject` outside the lock.

## Design Decisions

- **Both the device model and the bus binding live in this crate**: `bus::VirtioBlkDevice` depends only on
  `vdev-core` traits, and placing it in `kvmm` would needlessly tie generic code to the host crate.
- **Default base/IRQ are fixed in `bus.rs`**: it follows slot 1 of `virtio_mmio` on the QEMU `virt` platform
  (`0x0a00_1000`, IRQ 49), with slot 0 left for `vdev-virtio-net`. The device can be reused by multiple VMs,
  and the VM side isolates them through their own `GuestDma`/`IrqSender`.
- **The synchronous RAM disk is the default backend**: it lets the data path work as early as boot and needs no deferred-processing mechanism.
- **The self-test lives inside the crate**: `selftest` drives the complete sequence with a fake `GuestDma` + a recording `IrqSender`,
  and it can be called directly by the kernel unittest (`vdev_virtio_blk::selftest::vmm_selftest_virtio_blk`).

## Drop / Resource Release

There is no custom `Drop`. The `Arc<VirtioMmioBlockDevice>`, the RAM disk of `RamDiskBackend` and the queues
are released when the structs are destroyed. After the device is removed from the MMIO bus (the VMM releases the `Box<dyn MmioDevice>`), it no longer
receives accesses; in-flight requests rely on the synchronous backend having completed them inline, and reset discards `pending_head`.
