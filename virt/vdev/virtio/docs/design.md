# vdev-virtio — Design Document

## Positioning

`vdev-virtio` is the VirtIO MMIO device-model infrastructure crate of X-Kernel: it provides the standard MMIO
transport register state machine, the split virtqueue (descriptor table / available ring / used ring), and
a generic abstraction of the device configuration space. Concrete devices (such as `vdev-virtio-blk`) build on top of it and implement only
the device-specific configuration space and data path.

This crate belongs to the reusable virtual-device layer and is backed by the interface provided by `vdev-core` (`GuestDma`) and
the interrupt-masking lock provided by `kspin`. It does not depend on any host VMM crate (`kvmm`), architecture-specific
HAL, or kernel memory management, so it can be reused by different VMMs and can also be driven standalone in host-side tests.

## Background

The original implementation lived in `src/vdev/virtio/` of `kvmm`. The code itself references only its own modules and
`alloc`/`core`/`kspin`/`log`, and is neither architecture-specific nor KVMM-specific; the only thing truly
coupled to KVMM is the translation capability "guest physical address → host-dereferenceable address". After extracting this device model into an
independent crate, `kvmm` retains only the `GuestDma` implementation and architecture-specific devices, giving a clearer boundary.

## Scope

```
virt/vdev/virtio/
├── src/
│   ├── lib.rs            # crate entry and public type re-exports
│   ├── addr.rs           # GuestPhysAddr / PhysAddr / AccessWidth
│   ├── config.rs         # device configuration space view
│   ├── device_type.rs    # VirtioDeviceId
│   ├── dma.rs            # vdev_core::GuestDma → GuestMemory adapter
│   ├── error.rs          # VirtioError / VirtioResult
│   ├── memory.rs         # GuestMemory / GuestMemoryAccessor
│   ├── constants/        # MMIO registers, queue, and common constants
│   ├── mmio/             # transport register decoding + VirtioMmioState state machine
│   └── queue/            # descriptor chain, available/used ring, VirtioQueue
├── ut-tests/            # kernel harness: 2 descriptor cases, 1 each for available/used
└── Cargo.toml
```

### Kernel Unit Tests

The original 4 host `#[test]` cases were migrated to `ut-tests/{descriptor,available,used}.rs`;
the original queue submodules mount a private `tests` module via `unittest::path_mod_test!`,
and the cases are registered with `#[unittest::def_test]`. The original assertions, fixtures, and private access are preserved;
the test entry point changes from the Cargo host harness to the kernel harness under `cfg(unittest)`.
`unittest` is used as a regular workspace dependency; production APIs and device behavior are unchanged.

These cases use local memory or pure layout computation, do not create a VM, and do not access real devices.
The descriptor fixture allocates a Vec with the kernel allocator, so execution requires a properly initialized kernel test environment;
tests do not share devices or buffers. The configuration needs this crate to be linked into the test kernel,
which can be done by enabling `KFEAT_VMM=y` along the existing VMM dependency path, and then running with the filter
`make unittest UNITTEST_CRATE=vdev_virtio`.
There is no need to perform VM initialization or a guest world switch inside the tests, and they are no longer run by bare Cargo host tests.

## Architecture

```
┌──────────────────────────────────────────────┐
│  vdev-virtio-blk / vdev-virtio-net (device implementations)  │
│  device config space + request data path                    │
└───────────────┬──────────────────────────────┘
                │ VirtioMmioState<T> / VirtioQueue<T>
┌───────────────┴──────────────────────────────┐
│              vdev-virtio (this crate)           │
│  mmio::state   standard MMIO register state machine          │
│  mmio::transport  register decoding and validation             │
│  queue        descriptor table / avail / used ring     │
│  memory       GuestMemory / GuestMemoryAccessor│
│  dma          DmaGuestMemory (GuestDma adapter)   │
└───────────────┬──────────────────────────────┘
                │ vdev_core::GuestDma
┌───────────────┴──────────────────────────────┐
│  VM guest memory translation (the submitter implements GuestDma)        │
└──────────────────────────────────────────────┘
```

| Component | Responsibility |
|------|------|
| `mmio::state::VirtioMmioState<T>` | Maintains the standard registers such as status, feature selector, queue selector, interrupt status, and config generation, and holds the queue vector |
| `mmio::transport` | Validates access width, determines whether a register falls within the device configuration region, and decodes register offsets |
| `queue::VirtioQueue<T>` | Queue configuration (desc/avail/used addresses and size), ready validation, descriptor chain traversal, and used-completion write-back |
| `queue::{AvailableRing, UsedRing, DescriptorTable}` | Layout computation and read/write for the three kinds of ring structures |
| `memory::GuestMemory` | The minimal guest memory read/write capability required by the queue/data path |
| `memory::GuestMemoryAccessor` | An accessor that can "translate an address and return a contiguous upper bound", adapted by `AddressSpaceMemory` |
| `dma::DmaGuestMemory` | Adapts `vdev_core::GuestDma` into `GuestMemory`; it is the default implementation on the production path |
| `constants` | MMIO register offsets, queue constants, and feature/status/descriptor bits |

## Calling Constraints / Execution Context

- **Non-interrupt context**: MMIO reads/writes and queue processing are invoked by the VMM on the guest-exit path and run in task
  context, so interrupt masking is not required; the internal lock uses `kspin::SpinNoIrq`, and the critical section covers only registers and
  queue metadata, containing no waiting other than bulk copies of guest memory.
- **No sleeping**: this crate performs no blocking waits; if a backend may block, the device implementation must turn
  `QUEUE_NOTIFY` into deferred processing (see `QueuePending` in `vdev-virtio-blk`).
- **Guest memory capability is injected by the caller**: the production path passes a `GuestDma`-backed
  `DmaGuestMemory`; scenarios that only compute layouts and do not process queues may pass
  `NoGuestMemoryAccessor` and use the step-by-step operations in `_with_memory` form.
- **No periodic/scheduling dependencies**: it does not access CPU-local state and does not depend on the scheduler or platform initialization, so it can run in
  early boot or a host-side test environment (as long as `alloc` is available).
- **Reentrant**: concurrent access to the same `VirtioQueue`/`VirtioMmioState` is serialized by the internal lock;
  different device instances are independent of each other.

## State Machine

### Device status register

```
0 ──ACKNOWLEDGE──> ACK ──DRIVER──> ACK|DRIVER ──FEATURES_OK──> ... ──DRIVER_OK──> Ready
│                                                                           │
└────────────────────────── status write 0 (reset)─────────────────────────────┘
```

- When the driver writes `FEATURES_OK`, it is checked that `driver_features` is a subset of `device_features`;
  if not, `FEATURES_OK` is cleared.
- Writing status `0` triggers `MmioWriteAction::Reset`.
- `is_driver_ok()` lets the device implementation determine whether queues can be processed.

### Queue configuration

```
Unset ──QUEUE_SEL/QUEUE_NUM/QUEUE_DESC|AVAIL|USED──> Programmed ──QUEUE_READY=1──> Ready
   ^                                                                                  │
   └────────────────────────── reset / QUEUE_READY=0 ─────────────────────────────────┘
```

When `QUEUE_READY` is written, the layout range of each ring is computed from the desc/avail/used addresses and size, and any
out-of-bounds range or address overlap causes the ready bit to be refused.

### Interrupt status

- `set_interrupt(bits)`: `pending |= bits` and `raised_after_read |= bits`.
- After the driver reads `INTERRUPT_STATUS`, `INTERRUPT_ACK` clears the corresponding bits of `pending`.
- If any bit is set after the read, `mmio_write` returns `MmioWriteAction::InterruptPending`,
  and the device implementation injects the interrupt again accordingly, avoiding a missed notification.

## Algorithm Flow

### MMIO read

1. `transport` validates the access width.
2. If a standard register is hit → return `MmioReadOutcome::Standard`.
3. If the device configuration region is hit → return `MmioReadOutcome::DeviceConfig { offset, width }`,
   and the device implementation interprets its own configuration space.

### MMIO write

1. `transport` validates the width and parses the register.
2. Writes to feature/queue registers update `VirtioMmioState`; `QUEUE_READY` performs ring layout validation.
3. `QUEUE_NOTIFY` → `MmioWriteAction::QueueNotified(index)`.
4. status write 0 → `MmioWriteAction::Reset`.

### Queue data path

1. The device implementation pops the head descriptor index from the available ring.
2. It traverses the descriptor chain (`VIRTQ_DESC_F_NEXT` / `VIRTQ_DESC_F_INDIRECT`) and constructs a
   `DescriptorChain`.
3. It reads and writes the descriptor buffers according to device semantics.
4. Via `complete_with_memory`, it writes the head and the written length into the used ring and advances the used idx.

## Concurrency Model

- Each mutable register and the queue vector in `VirtioMmioState` is protected by its own `SpinNoIrq` lock;
  the lock granularity is small, and the locks are held only during register updates/queue metadata access.
- The accessor inside `VirtioQueue` is shared as `Arc<T>`, with `T: Clone`; the queues themselves are exposed to the device data path via
  `queues_lock()`, ensuring that only one data path traverses them at any given time.
- Interrupt injection/backend callbacks are not in this crate and are decided by the device implementation; this crate does not touch hardware directly.

## Design Decisions

- **Keep both layers, `GuestMemoryAccessor` and `GuestMemory`**: the former is for host-side tests/standalone device models that need "translation +
  contiguous upper bound", and the latter is the minimal capability that queue operations actually need.
- **Converge on `vdev-core` through `DmaGuestMemory`**: the production path no longer has each device define its own guest memory
  adapter, but uniformly goes through `GuestDma`, so this crate depends only on the `vdev-core` interface.
- **Do not put this crate into `vdev-core`**: `vdev-core` holds only cross-VMM trait contracts;
  VirtIO is an "implementation family", and being a sibling crate of `vdev-core` fits the boundary division better.
- **Move out of `kvmm`**: the device model is VMM-independent, and after the move `kvmm` retains only the `GuestDma`
  implementation, the world-switch hook, and architecture-specific devices.

## Drop / Resource Release

This crate has no custom `Drop`. Queues and accessors are destroyed along with the device struct that owns them; an `Arc<T>` accessor is reclaimed after the
last reference is released. Guest memory is managed by the owner of `GuestDma` (the VMM); this crate
only performs scoped reads and writes and does not hold ownership of host memory.
