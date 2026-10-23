# kdriver — Design

## Purpose

`kdriver` is x-kernel's device-driver orchestration and host-integration
crate. It owns bus-backend management, driver registration and matching,
the full discover → bind → activate pipeline, and devres-lifecycle-based
resource management (mmio / irq / dma / time). All activated runtime
devices are ultimately published into the matching typed `kclass`
registries.

`kdriver` is allowed to touch X-Kernel host APIs, but only as a provider
implementation holder, bus discovery backend, or concrete driver glue.
Reusable concrete drivers must still depend only on driver subsystem
contracts.

The intended audience is developers adding new bus backends, new device
drivers, or changing device discovery/matching policy.

## Background

The core of the Linux device model separates bus, device, and driver: the
bus discovers devices, drivers declare the device characteristics they
support, and the kernel performs matching and binding in between.
`kdriver` lands that architecture as crate-level abstractions:

- bus backends are abstracted behind the `BusBackend` trait, with PCI and
  platform instances;
- device descriptors (`DeviceDesc`) are produced by bus discovery and do
  not directly create runtime objects;
- drivers declare matchers and probe callbacks through the `DeviceDriver`
  trait;
- `kdevice` provides the shared device core, owning the persistent
  topology of `BusInstance`, `DeviceObject`, and `DriverObject`;
- `kclass` provides the typed publication entry per category
  (net / block / display / input / vsock / char / 9p).

## Scope

```text
drivers/integration/kdriver/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs                   # crate entry, init_drivers, ownership summary API
    ├── manager.rs               # DeviceManager and the unified discovery pipeline
    ├── enumeration.rs           # EnumerationContext, descriptor buffer bus backends write
    ├── resource.rs              # provider holder and DriverResult wrappers
    ├── block_completion.rs      # block wait/signals host provider
    ├── block_completion_dispatch.rs  # block completion reclaimer (softirq batches)
    ├── block_irq.rs             # shared block IRQ request adapter
    ├── bus/
    │   ├── mod.rs               # bus module root
    │   ├── backend.rs           # BusBackend trait
    │   ├── manager.rs           # BusManager multi-backend management
    │   ├── pci_backend.rs       # PCI bus discovery
    │   ├── pci_support.rs       # PCI BAR allocation and device configuration
    │   ├── platform_backend.rs  # platform bus (firmware + static devices)
    │   └── local_id.rs          # bus-backend local id allocator
    ├── driver_registry/
    │   ├── mod.rs               # DriverRegistrar, ownership summary types
    │   ├── firmware_specs.rs    # firmware match specs for platform drivers
    │   ├── virtio/
    │   │   ├── mod.rs           # VirtIO driver descriptors and activation
    │   │   ├── ids.rs           # VirtIO device type codes and PCI id mapping
    │   │   └── glue.rs          # VirtIoHal implementation, kdma/iomap binding
    │   ├── block/               # block driver registration
    │   │   ├── mod.rs           #   ahci, bcm2835_sdhci, ramdisk, sdmmc registry
    │   │   ├── ahci.rs          #   AHCI platform driver glue
    │   │   ├── bcm2835_sdhci.rs #   BCM2835 SDHCI glue
    │   │   ├── ramdisk.rs       #   ramdisk glue
    │   │   └── sdmmc.rs         # sdmmc glue
    │   ├── char/
    │   │   ├── mod.rs           # char device driver registration (console)
    │   │   └── serial.rs        # serial/UART platform driver glue
    │   └── net/
    │       ├── mod.rs           # net device driver registration
    │       ├── fxmac.rs         # FXMAC platform driver glue
    │       ├── ixgbe.rs         # ixgbe PCI driver glue
    │       └── ixgbe_hal.rs     # ixgbe HAL adapter
    └── tests/                   # block host tests (physically grouped)
        ├── block_completion.rs
        ├── block_completion_dispatch.rs
        ├── block_irq.rs
        ├── block_requests.rs
        └── virtio_block_activation.rs
```

## Architecture

When `block` is enabled, `HostBlockWaitProvider` implements the block host
contract: allocation-free context validation before submission and waiter
preparation only for an incomplete bio. `init_drivers` installs it before
discovery. `kruntime/fs` explicitly enables `kdriver/block`, so filesystem
consumers including devfs loop get this service without a physical block driver.
`HostSignals` owns one permanent
`kpoll::Completion`. `HostBlockWaiter` validates and retains the preparing task.
It creates a `PreparedTaskWait` and registers the terminal source only if the
completion is still pending when waited on. The check/register/recheck sequence
retains early signals; one registration spans all spurious wakes until the wait
returns. Signals clone the existing completion state. Only terminal waiting
remains; virtio completion refills its own pending
queue and needs no admission waiter. IRQ construction therefore takes no wait
provider. The host still prepares a waiter for explicit device-close drain.
ktask/kpoll dependencies are enabled only by the block feature and are not
propagated to concrete drivers; existing filesystems keep using the same
generic block interface.

Each `prepare_block_wait` allocates three metadata heap objects: the boxed
waiter, the HostSignals `Arc`, and its PollSet inner `Arc`. An already completed
bio skips all three; its only shared metadata allocation is the block-owned
result state. A blocking wait creates one
KWaker; its sole private registration fits kpoll's inline storage and cannot
exhaust registration IDs. No data buffers or data copies are added. The existing EEVDF wake
path may allocate, so this layer adding no wake bookkeeping allocations
must not be described as the whole wakeup chain being allocation-free.

```text
                        init_drivers()
                             │
                    discover_unified()
                             │
                    ┌────────┴────────┐
                    │   DeviceManager │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
         BusManager    EnumerationContext  DriverRegistrar
              │              │              │
    ┌─────────┴─────────┐    │     ┌────────┴────────┐
    │                   │    │     │                 │
 PlatformBackend    PciBackend  │  virtio drivers   platform drivers
    │                   │       │  (net/blk/gpu/    (ramdisk/AHCI/
    │                   │       │   input/vsock/9p)  sdmmc/fxmac/…)
    │                   │       │
    └───────┬───────────┘       │
            │ probe             │
            ▼                   │
     ┌──────────────┐           │
     │  kdevice core │◄─────────┘
     └──────┬───────┘
            │ publish
            ▼
     ┌──────────────┐
     │ kclass registries │
     │ net / block  │
     │ display      │
     │ input / vsock│
     │ char / 9p    │
     └──────────────┘
```

| Component | Responsibility |
|------|------|
| `DeviceManager` | Holds `BusManager` + `DriverRegistrar`; orchestrates the full discover → match → bind → activate pipeline |
| `BusManager` | Manages coexisting backend instances; owns the `early_init` → `enumerate` → `rescan` → `quiesce` → `remove` lifecycle |
| `BusBackend` | Bus-backend trait; implementers write `DeviceDesc`s through `EnumerationContext` |
| `EnumerationContext` | Descriptor buffer: records descriptors produced during discovery, then runs the unified probe |
| `DriverRegistrar` | Driver registration facade: registers `DeviceDriver` impls into the `kdevice` driver core |
| `PlatformBackend` | Unified platform bus: firmware-described (device-tree / ACPI) plus compile-time-known static devices |
| `PciBackend` | PCI bus: ECAM/MmioCam enumeration, BAR allocation, host-bridge / PCI-to-PCI-bridge adoption |
| `device-res-xkernel` | x-kernel resource provider implementation; bridges `memspace` (iomap), `kirq` (irq), `kdma` (dma) |
| VirtIO driver family | Each VirtIO device type yields two `DeviceDriver` descriptors (PCI/MMIO) sharing one activation path |
| Platform driver family | ramdisk, AHCI, bcm2835-sdhci, sdmmc, fxmac, and other platform device drivers |

The disabled AHCI integration only admits the SDK on an explicitly
hardware-coherent DMA host (currently x86_64). Its SDK `flush_dcache` callback
is an ordering barrier, not cache clean/invalidate. Non-coherent hosts are
rejected before SDK initialization; CPU alignment staging does not implement
that missing DMA/cache protocol. Production features/SDK/probe remain disabled.

## Block Device Completion Processing

`block_completion_dispatch` owns one `Arc<BlockIoReclaimer>` per device.
Probe creates it; IRQ calls its `mark_pending()`; task-context shutdown calls
its `stop_and_wait(&self, waiter)`. `BlockCompletionOperations` is implemented by the actual device and defines
how to reclaim completed requests, not host scheduling or request submission.

The reclaimer holds an intrusive pending_link, Weak device, ProcessingState
(phase, is_accepting, stop_notification), and a task-only stop mutex. One global
PENDING_DEVICES SpinNoIrq protects the per-CPU lists and all processing state.
Idle/Queued(cpu)/Running(cpu)/RunningAgain(cpu) are the processing phases. Detachment gives
the pinned CPU a finite batch; detached nodes remain Queued and cannot relink.
Callbacks and notifications run outside the global lock. RunningAgain requeues
only into the next batch. Enqueue and raise stay pinned across daemon wake.

Concurrent stop callers serialize on stop_lock through the terminal wait:
the single stop_notification cannot be overwritten. IRQ and softirq never take
this mutex, so it adds no lock acquisition to the I/O hot path. Stop takes a
fresh task-bound completion prepared before teardown; its sole lazy terminal
registration cannot fail after disabling processing. A later closer finds Idle
and returns.
The lock order is stop_lock -> PENDING_DEVICES, never the reverse.

Arc Drop does not stop processing. Activation must explicitly finish admitted
I/O, suppress device IRQ generation, release/synchronize its Irq, then call
stop_and_wait before destroying the transport. The owning CPU consumes disabled
queued entries without invoking the device. Stop waits for a running callback's
temporary device Arc to retire. Do not call stop from the device callback.
CPU unplug and forced callback cancellation remain out of scope.

Each device allocates one reclaimer Arc with an embedded stop mutex;
there is no per-mark allocation or data copy. Existing host scheduler wake costs
are unchanged. Request-local waiting remains separately owned in block_completion.

## Shared Block IRQ Lifecycle

### DMA And Execution Context

Ordinary block completion follows `complete_* -> pop_used -> Hal::unshare ->
device_res::unmap_streaming -> kdma::unmap_dma_buffer`. The existing provider
removes its mapping, copies reads back and returns bounce storage to the TLSF
pool. Pool creation is on map, not unmap; pool recycling and indirect descriptor
deallocation do not sleep in the audited implementation. Coherent virtqueue
destruction may perform different work and remains in task-context close.

This permits the Block softirq backend with the current provider. A provider
that needs sleepable reclamation must supply a task-context backend satisfying
the same block contracts. Neither hardirq nor softirq is sleepable.

No new block data copy is introduced, but the existing DMA bounce copies remain.
Reclaim cost depends on bytes as well as request count; finite batches and the
softirq outer-loop reschedule check do not bound a single pass in microseconds.
The existing scheduler's EEVDF enqueue can allocate, so allocation-free block
bookkeeping is not a claim of allocation-free end-to-end wakeup.

### Registration And Removal

`request_block_irq` returns the existing `device_res::Irq` directly. Its closure
holds a Weak device IRQ handler and the device's reclaimer Arc. It acknowledges
only that device, marks pending only for a claimed event, and preserves the
complete IrqEvent. An expired device returns NOT_HANDLED.

Each disk requests its own shared action through XKernelResourceProvider.
kirq owns fanout, capacity, compatibility checks and callback synchronization.
Provider errors propagate: no block bucket, capacity workaround, local callback
gate, extra registration owner, retry or polling fallback. In particular the
current four-action capacity can make setup fail; changing it belongs to kirq.

Dropping Irq in sleepable task context releases that action token. The existing
provider delegates to kirq, which waits for in-flight dispatches; currently this
is line-wide synchronization and can wait for a neighboring callback too.
The block adapter never masks the shared line. Activation must retain a strong
device reference through IRQ release and reclaimer stop. Callback snapshots
never own the activation object or its synchronous destructor.

IRQ adapter tests exercise per-device provider calls, token release, neighbor retention,
claimed/unclaimed dispatch, exact event propagation, expired targets and provider
failure. The mock has synchronous dispatch only; it does not prove concurrent
kernel release. That synchronization is covered by kirq's own regression tests.
driver_registry/virtio/block.rs owns DeviceId activation, rollback, publish-last
and hardware removal on top of the virtio request lifetime.
tests/block_requests.rs injects the real wait provider into that fake, blocks 24
callers across two CPUs, then acknowledges simulated IRQs and drives this
reclaimer through Block softirq. It does not publish a production IRQ disk.
Production PCI/MMIO block activation uses try_new_irq and the real host provider.
Driver initialization also installs that provider for block-core BioWait; disk
publication uses alloc_mq and the common Request data entry point.
Missing/failed IRQ leaves no published disk. One BlockActivation owns the strong
device, optional native Irq, reclaimer and optional Gendisk throughout setup and
close. A task-only map stores an owned FnOnce close action to erase heterogeneous
transport types; IRQ and request paths never consult that map. A local activation's Drop also handles
partial setup failure. Registry and devres cleanup ownership precede publication.

VirtioDriver::remove takes the unique close action before bus teardown; device
core's begin_removing serializes remove, and its later devres cleanup repeats
the now-idempotent lookup. Close prepares fresh task-bound drain/stop waiters,
marks the device Closing, removes owned block lookup, waits for admitted calls,
suppresses device interrupts, drops native Irq, stops the reclaimer, then
destroys the transport. No registry/class spinlock spans a wait. Retained disks
can query cached geometry but new I/O returns Io. Physical tests are grouped in
src/tests/virtio_block_activation.rs and included beneath their owning module.

Block host tests are physically grouped in src/tests/. Each unit test file is
included under its owning module with #[path], preserving private access and
the existing test namespace without a test-only directory per runtime module.

## Initialization Paths

Initialization is split into three deliberately separate paths:

### 1. Platform early init (`early_driver_init`)

Runs before the generic driver model and brings up the subsystems that must
work earliest: timer, IRQ, and the boot console. This stage does not go
through bus enumeration — the platform HAL starts it directly.

### 2. The descriptor-first path

This is the main path, driven by `init_drivers()` → `discover_unified()`:

1. **Bus enumeration**: `PlatformBackend` and `PciBackend` each enumerate
   devices, writing `DeviceDesc`s into the `kdevice` shared core through
   `EnumerationContext::register_device`.
2. **Driver matching**: `EnumerationContext::probe_pending()` runs
   `kdevice::probe_device_desc` for every descriptor, matching against
   registered `DeviceDriver`s by `bus_type` + `matcher`.
3. **Binding and activation**: on a match, `DeviceDriver::probe_device` is
   called; the driver performs hardware initialization and publishes the
   runtime device into the `kclass` registries.
4. **Unmatched devices**: descriptors with no matching driver land in the
   `unclaimed` list — logged, but never blocking boot.

### 3. The adoption path

Reserved for devices that already run from early init but lack runtime
device-model objects (such as the boot console).
`kdevice::adopt_active_device` registers the running device into the device
tree so later sysfs/device query paths can discover it.

## State Machine

### Device core states

Each `DeviceRecord` in `kdevice` moves through:

```text
                    ┌──────────┐
                    │Discovered│  ← bus enumeration create
                    └────┬─────┘
                         │ driver matched
                         ▼
                    ┌──────────┐
                    │ Matched  │
                    └────┬─────┘
                         │ bind
                         ▼
                    ┌──────────┐
                    │  Bound   │
                    └────┬─────┘
                         │ activate (probe success)
                         ▼
                    ┌──────────┐
                    │  Active  │  ← runtime device published to kclass
                    └────┬─────┘
                         │ remove
                         ▼
                    ┌──────────┐
                    │ Removing │
                    └────┬─────┘
                         │ cleanup done
                         ▼
                    ┌──────────┐
                    │ Removed  │
                    └──────────┘
```

| From | To | Trigger |
|----|----|----------|
| — | Discovered | bus backend `enumerate` calls `register_device` |
| Discovered | Matched | `probe_device_desc` finds a matching driver |
| Matched | Bound | binding succeeds; device-driver relation established |
| Bound | Active | `DeviceDriver::probe_device` returns `Ok` |
| Matched/Bound/Active | Removing | `remove_device_managed` is called |
| Removing | Removed | devres cleanup completes and the driver remove callback ran |

Unmatched devices stay `Discovered` but enter the `unclaimed` list and
never get a `DeviceObject`.

### Bus backend lifecycle

```text
BusManager::register(backend)
  │
  ▼
Registered ──► early_init() ──► enumerate() ──► probe_pending()
                   │                 │
                   │                 ├─ Activated (→ kclass publish)
                   │                 └─ Unclaimed (log + skip)
                   │
              rescan() ◄── hotplug / rescan
                   │
              quiesce() ──► suspend events (shutdown / suspend)
                   │
              remove() ──► bus teardown (orderly shutdown)
```

## Flows

### PCI bus enumeration

1. Pick ECAM or MmioCam access per `pci_cam_kind()`.
2. Open `PciBus` and obtain configuration-space access.
3. Adopt the host bridge: create a `pci-host` device on the platform bus as
   the root of the PCI device tree.
4. **Pass 1**: walk all BDFs on bus 0..bus_end, dispatching on `HeaderType`:
   - `Standard` → endpoint list;
   - `PciPciBridge` → bridge list, reading secondary/subordinate bus
     numbers;
   - others → skipped with a log entry.
5. **Pass 2**: call `adopt_active_device` per PCI-to-PCI bridge to create
   the `pci-bridge` device object, plus the corresponding secondary
   `BusInstance`.
6. **Pass 3**: walk the endpoint list; for each device:
   - call `configure_pci_device_if_needed` to assign unassigned BARs and
     enable the command register;
   - read BAR info and build the `ResourceSet` (MMIO / IO port / legacy
     INTx IRQ);
   - detect VirtIO-over-PCI devices and attach `TransportInfo::Virtio`;
   - register through `EnumerationContext::register_device_with_parent`,
     with the parent pointing at the host bridge or parent bridge.

### Platform bus enumeration

1. **Firmware stage**: walk firmware-described device nodes (DT compatible
   / ACPI).
   - For each node, query the registered `FirmwareMatchSpec`s (AHCI,
     sdmmc, fxmac, ...) for compatible matching.
   - VirtIO MMIO is special-cased: the `virtio,mmio` compatible is
     detected and the MMIO region mapped to probe the VirtIO device type.
   - Build the `ResourceSet` (MMIO + IRQ) and register through the
     `EnumerationContext`.
2. **UART/serial nodes**: UART nodes in the DT (including the stdout
   console) are enumerated here. The stdout UART is reused by the serial
   driver through `take_early_port` (same hardware, no re-map); other
   UARTs map their own ports and publish as independent char devices.
3. **Static device stage**: register compile-time-known platform devices
   (ramdisk is always registered; AHCI/sdmmc/bcm2835-sdhci only when no
   firmware description exists).

   > The ramdisk storage backend is controlled by `KFEAT_DRIVER_RAMDISK_STATIC`:
   > when off it is 16 MiB of zeroed heap memory (driver validation only); when
   > on it is zero-copy backed by a filesystem image embedded at build time (path
   > from the Makefile variable `RAMDISK_IMG`, format from `RAMDISK_IMG_FS`,
   > default ext4, an empty image generated by `make ramdisk_img`), so it can be
   > mounted as a real read-write root filesystem. See
   > `drivers/contracts/block/src/ramdisk_image.rs`.

### Driver matching and activation

1. `EnumerationContext::probe_pending` takes all pending descriptors.
2. For each descriptor it calls `kdevice::probe_device_desc`:
   - filter drivers by `bus_type`;
   - call `DeviceDriver::matcher().matches(identity)` to test the match;
   - on a match, run bind → activate.
3. Successfully activated devices have their runtime objects published to
   the matching `kclass` registries.
4. Unmatched (`Unclaimed`) or requeue-requested (`Requeue`) descriptors
   enter the `unclaimed` list.

### VirtIO device activation

VirtIO drivers share one activation entry across the PCI and MMIO
transports:

1. **PCI path**: extract the BDF from `DeviceLocation::Pci`, reopen
   `PciBus`, and run `probe_pci_device`, confirming the transport's
   reported `DeviceKind` matches the driver's declaration.
2. **MMIO path**: extract the physical address and size from
   `DeviceLocation::Mmio`, `iomap_mmio`, then run `probe_mmio_device`.
3. **Dispatch**: `dispatch_virtio_try_new` dispatches on `DeviceKind`:
   - `DeviceKind::Net` → `VirtIoNet::try_new` → `kclass::publish_net`
   - `DeviceKind::Block` → `block::activate` → IRQ setup → `kclass::publish_block`
   - `DeviceKind::Display` → `VirtIoGpu::try_new` → `kclass::publish_display`
   - `DeviceKind::Input` → local `virtio/input.rs` activation → devres IRQ registration → PCI INTx enable when applicable → `kclass::publish_input`
   - `DeviceKind::Vsock` → `VirtIoSocket::try_new` → `kclass::publish_vsock`
   - `DeviceKind::Fs9p` → `VirtIo9p::try_new` → `kclass::publish_virtio_9p`

### Resource management (devres)

`device-res-xkernel` provides x-kernel's implementation type for the
`device-res` provider contract; the driver-facing resource API is exposed
to drivers by `kdriver::resource` as `DeviceResourceExt`.
`kdriver::resource` holds the static `XKernelResourceProvider` and passes
it explicitly to `device_res::devm_*_with_provider()`; on release the RAII
handles return to the same provider that created them:

- **`device.devm_iomap`**: maps MMIO through `memspace::iomap_device`;
  automatically `iounmap`s on probe failure or device remove.
- **`device.devm_request_irq`**: adapts the devres IRQ handler into a
  `kirq` shared action registered on the `kirq` IRQ core action list.
  VirtIO input uses this path before class publication; a missing IRQ or failed
  registration aborts probe rather than publishing a polling-only device.
  The PCI path keeps INTx masked during queue construction and handler
  registration, adds a later cleanup that masks the source before IRQ release,
  and only then unmasks INTx and publishes the class device.
  `virtio/input.rs` owns this lifecycle and uses the framework's
  `DeviceResourceExt::devm_request_irq` helper. `glue.rs` only adapts the
  VirtIO HAL and other class constructors; it no longer selects an input IRQ
  provider or binds input devres.
- **`device.devm_request_threaded_irq` /
  `device.devm_request_threaded_irq_default`**: bind devres primary/thread
  handlers to the provider contract. The current mainline
  `device-res-xkernel` provider does not yet cover the threaded IRQ
  methods, so they return `DriverError::Unsupported`; a later kirq
  threadirq branch will supply the xkernel implementation.
  `device-res-xkernel` is the adaptation layer between devres IRQ
  resources and the kernel IRQ core, translating `device_res`
  trigger/controller/event/handler values into `kirq`'s own types; `kirq`
  does not depend back on devres.
  Future IRQ core capability extensions belong in `kirq`; the driver
  framework exposes kernel IRQ capability to drivers only through this
  adapter.
- **`device.devm_alloc_coherent`**: allocates a coherent DMA buffer via
  `kdma::allocate_dma_memory`; release calls
  `kdma::deallocate_dma_memory`.
- **`resource_provider().monotonic_time`**: only X-Kernel glue passes
  `device_res::TimeOp` into reusable drivers; concrete drivers must not
  call `khal::time` directly.

Release runs in reverse acquisition order (LIFO) so resource dependencies
are never inverted.

### IRQ dispatch mechanism

`device-res-xkernel::XKernelResourceProvider` keeps no local IRQ line
state. It converts `device_res::IrqResource` into `kirq::IrqSpec`, wraps
`device_res::IrqHandler` as `kirq::IrqHandler`, and registers with the IRQ
core per API:

1. `request_irq` calls `kirq::try_register_shared()`; `kirq` allocates a
   line-local `IrqActionToken` and stores the action identity; the provider
   returns `device_res::IrqHandlerToken::SharedAction(id)` to devres.
2. `request_threaded_irq` / `request_threaded_irq_default` currently reuse
   the `device_res::IrqOp` default implementations, returning
   `Unsupported` until a kirq threadirq provider override lands.
3. When an interrupt arrives, `kirq` snapshots the actions from
   `IrqDescRuntimeState`, invokes each handler in order, and passes the
   resolved `virq` as the handler argument. Each wrapper then converts the
   devres handler's `device_res::IrqEvent` into `kirq::IrqEvent`.
4. After the whole line's fanout, `kirq` aggregates the handled result.
   VirtIO input reports `NOT_HANDLED` for an empty device ISR and calls the
   input layer's notifier only for an event-queue interrupt. The input layer
   drains events in deferred work and wakes each evdev client's waiters when
   its queue receives an event; it does not register a physical IRQ waiter.
5. `release_irq` calls `kirq::free_irq_action()` for shared hardirq
   tokens, removing only the action belonging to this devres handler.

Registration and release paths may allocate; the IRQ dispatch path does
not allocate from the heap.

## Concurrency Model

- `DeviceManager::bus_mgr` uses `SpinNoPreempt`: bus enumeration, rescan,
  quiesce, and remove run in process context with mutual exclusion.
- `EnumerationContext` has no interior locking: it is a single-threaded
  bridge between bus backends and probe, filled only inside the `bus_mgr`
  lock.
- IRQ action fanout, threaded wake/ONESHOT, token teardown, and
  `in_flight` synchronization are owned by `kirq`; `device-res-xkernel`
  only stores the mapping from devres tokens to `kirq` release calls, and
  `kdriver` keeps no IRQ core state.
- `PCI_BAR_ALLOCATOR` uses `SpinNoPreempt`: BAR allocation happens only in
  process context (enumeration or probe).
- The `kdevice` shared core's internal locks are managed by the `kdevice`
  crate itself; `kdriver` never holds them directly.

## Design Decisions

### Descriptor-first instead of creating device objects directly

**Choice**: bus enumeration only produces `DeviceDesc` descriptors; it does
not directly create `DeviceObject`s.

**Trade-off**: one extra intermediate representation and a batched probe
step, in exchange for:

- unmatched descriptors never pay `DeviceObject` memory or devres costs;
- all descriptors are collected before probing, making unmatched-device
  logging and diagnosis a single summary;
- descriptor deduplication (e.g. ACPI and DT describing the same device)
  can be implemented before probe.

**Rejected alternative**: the Linux `device_register` model — register at
enumeration. It creates device objects during discovery, simplifying the
code path, but couples registration to matching in time (drivers must be
loaded before device registration). With all x-kernel drivers built in,
descriptor-first is simpler.

### Bus backends as a trait instead of compile-time branches

**Choice**: PCI and platform buses are separate implementations of the
`BusBackend` trait, registered in `default_bus_manager()`.

**Trade-off**: the minor dynamic-dispatch cost of `Box<dyn BusBackend>`, in
exchange for:

- one kernel image supports PCI and platform devices simultaneously — no
  compile-time either/or;
- a new bus type (USB, I2C, ...) is one `BusBackend` implementation plus
  registration, with no `BusManager` core changes;
- backend lifecycle management (init/enumerate/rescan/quiesce/remove) is
  unified in `BusManager`, removing per-backend scheduling boilerplate.

**Rejected alternative**: `cfg` branches hard-coding one bus type inside
`BusManager`. No dispatch overhead, but no multi-bus support, and every new
bus edits `BusManager` internals.

### PCI BARs configured once at enumeration

**Choice**: the PCI backend completes BAR allocation and command-register
configuration during enumeration.

**Trade-off**: slightly longer enumeration (the endpoint list is walked
twice — collect, then configure), in exchange for a pure-read activation
path:

- `configure_pci_device_if_needed` degrades to a no-op at activation since
  BARs are already non-zero;
- the activation path never re-derives the firmware MMIO window
  (`pci_bar_allocation_range`);
- a configuration failure skips the device at enumeration, so no
  unconfigured `DeviceDesc` is ever created.

**Rejected alternative**: deferring configuration — allocate BARs at
driver probe. It avoids useless work for unprobed devices but complicates
probe (which would hold the PCI bus lock and BAR allocator lock) and
requires rolling back a registered `DeviceObject` mid-probe on failure.

### VirtIO PCI/MMIO dual descriptors

**Choice**: each VirtIO device type yields two `DeviceDriver` descriptors
(PCI and MMIO), registered on their respective bus types.

**Trade-off**: twice the descriptor count (6 device types × 2 = 12), in
exchange for:

- the matcher (`VirtioTypeMatcher`) works on the VirtIO type code rather
  than PCI vendor/device ids or DT compatibles, so devices discovered via
  PCI and MMIO both match the same functional driver;
- the PCI vs MMIO activation differences are sealed inside
  `activate_virtio_pci` / `activate_virtio_mmio`; the upper
  `dispatch_virtio_try_new` is fully transport-agnostic.

**Rejected alternative**: one descriptor with runtime transport detection.
Fewer descriptors, but the matcher would compare bus type and VirtIO type
together, and `probe_device` would branch between two transport init
logics.

### IRQ handlers reach kirq through device-res-xkernel

**Choice**: `device-res-xkernel` is the `device_res` provider adapting
driver devres IRQ requests onto `kirq`; `kirq` keeps the shared action
list per IRQ line and manages devres handler lifetimes via tokens.

**Trade-off**: shared IRQ fanout moved from `kdriver` down into `kirq`,
making IRQ core action state more complex; but:

- shared, oneshot, threaded IRQ, per-action stats, and free/synchronize
  semantics all concentrate in the IRQ core;
- device handlers carry their own context — no per-slot trampolines and no
  global IRQ count limit;
- tokens allow freeing one shared handler while other devices on the line
  keep receiving interrupts;
- fixed-length action snapshots keep the dispatch path allocation-free;
- interrupt release removes a device handler by token; after the last
  handler goes, `kirq` masks and tears down the line.

**Rejected alternative**: a preallocated static `IrqSlot` array with
generated trampolines. That would need slot identity bookkeeping and a
hard-coded total IRQ limit.

## Drop / Resource Release

- devres resources are released in LIFO order in the `DeviceObject` remove
  path.
- `PciBackend` holds no persistent resources needing drop-time release
  (`PciBus` is released when `enumerate` returns).
- `PlatformBackend` holds only a `LocalIdAlloc` (a stack `u16`), with no
  explicit release.
- Interrupt release removes the device handler by token; once the last
  handler goes, the `kirq` handler is deregistered.
- A VirtIO input action is registered before class publication. PCI input adds
  its INTx-mask cleanup after the action, so LIFO device removal masks the
  source first, then releases and synchronizes the action before the detached
  class object and its shared transport state are finally dropped.
- Shared DMA buffers are reclaimed by `kdma::deallocate_dma_memory` after
  the last handle drops.

## Feature Gating

```text
virtio ────────► virtio-blk ──► block  + virtio + virtio/block
                virtio-net ──► net    + virtio + virtio/net
                virtio-gpu ──► display + virtio + virtio/gpu
                virtio-input ► input  + virtio + virtio/input
                virtio-socket► vsock  + virtio + virtio/socket
                virtio-9p ───► virtio + virtio/virtio-9p + kclass/virtio-9p

console ───────► console-pl011 / console-ns16550-mmio / console-ns16550-ioport
ramdisk ───────► block + block/ramdisk
ahci ──────────► any_firmware_driver + block
bcm2835-sdhci ─► any_firmware_driver + block
sdmmc ─────────► any_firmware_driver + block
fxmac ─────────► any_firmware_driver + net
ixgbe ─────────► net (placeholder)
```

`any_firmware_driver` is an umbrella feature: true whenever any DT/ACPI
platform driver is enabled; `cfg(feature = "any_firmware_driver")`
replaces the verbose `any(ahci, bcm2835-sdhci, sdmmc, fxmac)` condition.

Platform location IDs use a fallible monotonic allocator. Exhaustion returns
`DriverError::NoMemory`; firmware discovery records it in its existing first-error
result, and static discovery propagates it before registering another device.

## Hardware entropy completion provider

`rng_completion.rs` is the X-Kernel implementation of the driver-facing
`RngCompletion` capability. `wait_until` prepares the current task to sleep,
registers its device wake source with `PollSet`, checks the driver's readiness
predicate, and parks if the request is still pending.
`notify` wakes registered tasks so they can check readiness again. The
provider owns wait registrations; the concrete driver owns its request and DMA
buffer. The current entropy integration uses one worker per selected VirtIO RNG source,
issuing reads serially.

The `hwrng` feature compiles this provider for injection into concrete drivers.
