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
├── ut-tests/                    # unit-test fixtures included through path_mod_test
└── src/
    ├── lib.rs                   # crate entry, init_drivers, ownership summary API
    ├── power.rs                 # owned system-sleep participants and PCI state
    ├── manager.rs               # DeviceManager and the unified discovery pipeline
    ├── enumeration.rs           # EnumerationContext, descriptor buffer bus backends write
    ├── resource.rs              # provider holder and DriverResult wrappers
    ├── firmware_dma.rs          # firmware DMA facts composed into one device DMA view
    ├── block_completion.rs      # block wait/signals host provider
    ├── block_completion_dispatch.rs  # block completion reclaimer (softirq batches)
    ├── block_irq.rs             # shared block IRQ request adapter
    ├── rng_completion.rs        # hwrng completion wait provider
    ├── bus/
    │   ├── mod.rs               # bus module root
    │   ├── acpi_owner.rs        # ACPI INTx ownership for the PCI backend (x86)
    │   ├── backend.rs           # BusBackend trait
    │   ├── intx.rs              # PCI legacy INTx route resolution (x86)
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
    │   │   ├── glue.rs          # VirtIoHal implementation over the resource provider
    │   │   ├── block.rs         # VirtIO block activation and close
    │   │   └── input.rs         # VirtIO input activation and IRQ lifecycle
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
```

## Architecture

When `block` is enabled, `HostBlockWaitProvider` implements the block host
contract: allocation-free context validation before submission and waiter
preparation only for an incomplete bio. `init_drivers` installs it before
discovery. `kruntime/fs` explicitly enables `kdriver/block`, so filesystem
consumers including devfs loop get this service without a physical block driver.
HostSignals owns one permanent `kcompletion::Completion`.
`HostBlockWaiter` uses `ktask::completion::CompletionWait` to retain and
validate the preparing task. Scoped stack registration retains early signals
and rechecks completion after spurious wakes, without heap allocation or a
recoverable registration error. Only terminal waiting remains; virtio refills
its own pending queue. Explicit device-close drain prepares its waiter before
teardown. Concrete drivers depend only on the portable contract.

Each `prepare_block_wait` allocates two metadata heap objects: the boxed waiter
and HostSignals Arc. An already completed bio skips both. Main's BioWait still
prepares these objects lazily after submission under the infallible OOM policy;
this migration does not make the complete bio lifecycle allocation-free.
After preparation, waiting and notification allocate no wait resources.
The scheduler uses intrusive ready queues; default unfiltered scheduler tracing
uses preallocated capture storage. Arbitrary tracing callbacks/filters are
outside that guarantee.

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
| `driver-os-api-xkernel` | x-kernel resource provider implementation; bridges `memspace` (iomap), `kirq` (irq), `kdma` (dma) |
| VirtIO driver family | Each VirtIO device type yields two `DeviceDriver` descriptors (PCI/MMIO) sharing one activation path |
| Platform driver family | ramdisk, AHCI, bcm2835-sdhci, sdmmc, fxmac, and other platform device drivers |

`DriverRegistrar` delegates to `kdevice::register_driver_object`, which
immediately scans the descriptors that still await a binding — both `Pending`
and `Probing` — instead of only those that were pending at registration time.
The scan takes each descriptor's binding lock and rechecks its state after any
wait, so a driver registered while a probe is in flight observes the final
outcome rather than a stale snapshot. A descriptor currently held by the
registering context itself (registration from inside that descriptor's probe)
is logged and skipped; it is not retried automatically. Cross-descriptor
re-entry, where two tasks each hold a different descriptor lock and register,
is not detected at runtime; auditing production call sites is the current
gate (see `security.md`).

A probe may register child descriptors with `device_desc_add`, which does not
take a binding lock. That call guarantees only that the descriptor was
recorded; automatic discovery and binding of the new child by the outer
pipeline is not yet proven. Transports that create child devices must add an
explicit trigger rather than assuming the child will be probed.

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
Only the Idle and RunningAgain transitions to Queued link a reclaimer, and it is
unlinked then, so the intrusive-collections relink panic is unreachable.
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
I/O, suppress device IRQ generation, release/synchronize its DevIrq, then call
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
driver_os_api::unmap_streaming -> kdma::unmap_dma_buffer`. The existing provider
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

`request_block_irq` returns the existing `driver_os_api::DevIrq` directly. Its closure
holds a Weak device IRQ handler and the device's reclaimer Arc. It acknowledges
only that device, marks pending only for a claimed event, and preserves the
complete IrqEvent. An expired device returns NOT_HANDLED.

Each disk requests its own shared action through XKernelResourceProvider.
kirq owns fanout, capacity, compatibility checks and callback synchronization.
Provider errors propagate: no block bucket, capacity workaround, local callback
gate, extra registration owner, retry or polling fallback. In particular the
current four-action capacity can make setup fail; changing it belongs to kirq.

Dropping DevIrq in sleepable task context releases that action token. The existing
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
ut-tests/block_requests_unittest.rs injects the real wait provider into that fake, blocks 24
callers across two CPUs, then acknowledges simulated IRQs and drives this
reclaimer through Block softirq. It does not publish a production IRQ disk.
Production PCI/MMIO block activation uses try_new_irq and the real host provider.
Driver initialization also installs that provider for block-core BioWait; disk
publication uses alloc_mq and the common Request data entry point.
Missing/failed IRQ leaves no published disk. One BlockActivation owns the strong
device, optional native DevIrq, reclaimer and optional Gendisk throughout setup and
close. A task-only map stores an owned FnOnce close action to erase heterogeneous
transport types; IRQ and request paths never consult that map. A local activation's Drop also handles
partial setup failure. Registry and devres cleanup ownership precede publication.

VirtioDriver::remove takes the unique close action before bus teardown; device
core's begin_removing serializes remove, and its later devres cleanup repeats
the now-idempotent lookup. Close prepares fresh task-bound drain/stop waiters,
marks the device Closing, removes owned block lookup, waits for admitted calls,
suppresses device interrupts, drops native DevIrq, stops the reclaimer, then
destroys the transport. No registry/class spinlock spans a wait. Retained disks
can query cached geometry but new I/O returns Io. Physical tests are grouped in
ut-tests/virtio_block_activation_unittest.rs and included beneath their owning module.

Host tests are physically grouped in ut-tests/. Each unit test file is
included under its owning module with `unittest::path_mod_test!`, preserving private access and
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
   registered `DeviceDriver`s by `bus_type` + `matcher`. Candidates are tried
   in registration order and the first successful probe wins; later
   registration cannot overtake a binding that is already in flight.
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
   - map each discovered INTx route through `driver_os_api::IrqOp::map_irq`
     before publishing its resource. Preserve the firmware controller and
     trigger, and use the provider's logical IRQ rather than the raw number.
     Mapping failure is logged and omits INTx; discovery may still proceed
     for devices using MSI/MSI-X;
   - detect VirtIO-over-PCI devices and attach `TransportInfo::Virtio`;
   - register through `EnumerationContext::register_device` with a
     `DeviceDescSpec` whose `parent` points at the host bridge or parent
     bridge.

A PCI INTx line is level-triggered and may be shared by several functions.
Once a VirtIO transport is created the device has seen `DRIVER_OK` and may
assert that line at any moment. A device kind whose activation installs its
interrupt owner later — input, vsock, and display — must therefore have INTx
masked while the action is still absent: on a shared line an unrouted
assertion is never acknowledged, so the interrupt never deasserts and the CPU
spins in the handler forever, starving the very activation that would have
installed the owner. `virtio::probe_pci_device` masks INTx for those kinds, and
activation unmasks it only after registering the handler; a devres cleanup
re-masks it before the action is released. Input and display use the shared
`set_pci_intx_disabled` helper; vsock never re-enables INTx because it does not
use it.

### Platform bus enumeration

1. **Firmware stage**: walk firmware-described device nodes (DT compatible
   / ACPI).
   - For each node, query the registered `FirmwareMatchSpec`s (AHCI,
     sdmmc, fxmac, ...) for compatible matching.
   - VirtIO MMIO is special-cased: every `virtio,mmio` node is registered
     from its description alone. A node with a first MMIO range gets a
     `DeviceLocation::Mmio`; one without it gets a `FirmwareNode` location so
     probe can reject it. Neither path records a `TransportInfo` or reads a
     register. Empty slots and unknown device ids are not filtered here.
   - Build the `ResourceSet` (MMIO + IRQ) and register through the
     `EnumerationContext`.
     After selecting a supported device, map its firmware declaration through
     `IrqOp::map_fw_irq`, passing the controller node and complete binding
     cells unchanged. Failure — including an unreadable or malformed
     declaration, an unready controller, or a conflict — skips that device and
     records an enumeration error; no unmapped numeric fallback is published.
     Because empty and unknown `virtio,mmio` slots are no longer filtered at
     enumeration, their IRQ declarations are mapped like any other firmware
     device; the mapping stays attached to the `Pending` descriptor and is
     unused (and not released) until generic device teardown handles it. This
     is the accepted cost of describing devices before probing them.
   - `PlatformBackend::register_firmware_device` is the single per-device
     helper used by production enumeration and by the integration tests. It
     preserves the firmware node already carried by `FirmwareDevice` and stores
     it as `DeviceDescSpec::firmware_node = Some(dev.node)`; it never searches
     the tree again by compatible or name. The helper also keeps the existing
     skip-vs-failure split: a deliberate skip (unsupported/empty virtio slot,
     missing MMIO for the virtio transport) returns `Ok(())`, while allocation,
     IRQ-mapping and registration failures return an error that
     `enumerate_firmware` aggregates as the first error while continuing with
     the remaining nodes.
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

### Firmware-node association and test coverage

The node stored on a platform descriptor is the one firmware discovery already
resolved. `kdriver` does not own the backend: the handle is a copyable borrow
into boot-static firmware storage, and the descriptor and runtime object share
that borrow. Probe consumes `DeviceObject::firmware_node()` and must not
re-query the tree by compatible or name; `None` means the device has no
firmware description, never "look it up elsewhere".

The paths that deliberately register without a node are:

- platform-static devices (`PlatformBackend::register_static_device`) — known
  at compile time, so there is nothing to associate;
- PCI endpoints and adopted PCI bridges/host bridges — PCI identity and INTx
  routing are carried separately, and this change does not attach a per-device
  firmware node;
- the adoption path — a boot/runtime handoff, not firmware discovery.

Coverage boundary: `ut-tests/platform_firmware_node.rs` A1 proves real
`DtBackend` → production helper → descriptor → probe property/phandle reads, but
does not prove global `for_each_compatible` traversal or candidate filtering.
A2 drives a source-neutral fake backend through `device_desc_add` and the same
consumer; A4 covers failed probe, retry and removal; A5 separates missing,
malformed and unknown-handle results; A6 covers the static and adoption `None`
paths. PCI has no unit-testable enumeration seam in this tree; its explicit
`None` is covered by static review only, not by a runtime test.

Platform enumeration passes `FirmwareDevice.irq` straight to the host
provider's `map_fw_irq`; the provider selects the domain by exact firmware-node
identity, the controller's translator interprets the cells, and the IRQ core
owns the resulting descriptor and logical number. kdriver neither decodes
cells nor guesses a controller family. Mapping does not configure or enable
delivery. Domain associations belong to platform/Domain lifetime, not to one
handler or probe attempt; handler release does not dispose those mappings. No
KIRQ dependency is added to reusable concrete drivers or firmware parsers.

PCI legacy INTx keeps two explicit routes. `bus::map_pci_legacy_irq`
dispatches on the route kind: a `Gsi` calls `IrqOp::map_gsi`, while a DT
compatibility `Hwirq` still converts the `of` layer's decoded
controller/hwirq/trigger into `IrqRouteDesc` and calls `map_irq`. The helper
returns the provider's `ResError`, so the x86 resolver can log the exact
mapping failure. A GSI never enters `IrqRouteDesc.hwirq`; the platform resolves
it to an instance domain and local pin. That DT path is explicitly legacy and
must migrate with PCI firmware routing.

On x86 the firmware route query is `bus::acpi_owner`, not `pci`: one static
`OwnerState` owns the retained AML context and the published `PrtTable`, loaded
once with `\_PIC(1)` and the root `_PRT` during enumeration. The repository
answers `route(bdf, pin)` from that table only; it never reads PCI config,
maps an IRQ, evaluates AML, or locks the context. The PCI integration seam is
`bus::intx::resolve`: it reads `pci::interrupt_pin` (`0x3D`), calls the owner
only for `DiscoveryOrigin::Acpi`, maps `Gsi` through the provider, and returns
either the mapped resource or a driver-neutral `kdevice::IntxRouteStatus`
(`Routed`/`NotConnected`/`Failed`). `bus::pci_backend` attaches the `Irq`
resource and stores that status on the descriptor; it does not keep a global
BDF table and does not clear records at enumeration start. The kdriver VirtIO
integration converts `PciProbeError::NoInterrupt` to a `DriverError` using the
device's stored status, so an INTx-only device fails visibly instead of
polling, while a transport failure is never blamed on INTx. Non-x86 keeps the
`pci::legacy_interrupt_route` + OF static fallback path.

`ut-tests/irq_mapping.rs` checks that unreadable firmware declarations map to
input/unsupported enumeration errors and never look like an absent IRQ, and
that `Gsi`/`Hwirq` each reach only their own provider entry point.
Provider tests cover mapping error classification and authoritative descriptor
retention.

### Firmware DMA configuration

`firmware_dma` composes the neutral firmware facts carried by
`FirmwareDevice` into one `driver_os_api::DmaInfo` and hands it to the device
core through the `DeviceDescSpec` passed to `EnumerationContext::register_device`
(or `kdevice::device_desc_add`); descriptors, records, and device objects then
expose the same immutable view. The composition is the kdriver
half of Linux's split: the firmware backend owns DT encoding, the DMA-parent
relationship, whole-window translation and coherency inheritance; fwnode
exposes those results as neutral queries; kdriver applies device/bus policy;
and `kdma` still owns allocation, mapping, and synchronization.

The same registration call also carries an optional, driver-neutral
`kdevice::IntxRouteStatus` from discovery. It travels with the descriptor into
the runtime object and record, so an activation decision reads the discovery
result from its own device instead of a global side table. `None` means the
backend recorded no INTx fact; it never means "no interrupt".

The backend answers `FwNode::dma_window()` and
`FwNode::dma_effective_coherency()`; this module contains no DT property names,
ancestor walk or address translation.

Rules enforced by `from_firmware`:

- The device mask is an explicit argument from the device or bus policy. It is
  never derived from the firmware window, and a firmware window is never
  applied as a mask.
- A missing resolved window or coherency declaration is an error, never an
  unlimited/identity window or an invented default. Absence is carried by the
  neutral contract as `Ok(None)`; a firmware read error is never converted
  into absence, and absence is never converted into an error by the backend.
- The backend reports multi-window declarations, ambiguous ancestor windows,
  a declaration at the root boundary, `interconnects`/`interconnect-names`
  DMA-parent redirection and `iommus`/`iommu-map` as typed `fwnode::FwDmaError`
  variants. They are mapped one-to-one to configuration errors instead of being
  reduced to a guessed window.
- The full `[start, start + size)` interval is validated when the neutral
  window is converted into `driver_os_api::DmaWindow`, including empty and
  overflowing windows. The backend already validated containment in every
  ancestor window against the whole interval.

`from_firmware_with_qemu_virt_mmio_policy` adds one narrow platform exception
for the QEMU `virt` machines that place `virtio,mmio` directly in CPU physical
space: an available child of the `linux,dummy-virt` (Arm) or `riscv-virtio`
(RISC-V) root, crossing at most available `simple-bus` containers, may use the
single firmware-described RAM interval as an identity window when no
`dma-ranges` exists. Explicit firmware facts and errors take precedence, and an
ambiguous RAM description is an error.

Coherency remains firmware-derived except on that same scoped RISC-V machine.
Linux selects `CONFIG_ARCH_DMA_DEFAULT_COHERENT` for RISC-V, so
`of_dma_is_coherent()` falls back to coherent when no declaration exists;
`from_firmware_with_qemu_virt_mmio_policy` mirrors that machine/architecture
default and nothing broader. Arm64 does not select it, so a missing coherency
declaration remains an error there. Other nodes get no view at all; `None` on a
device means "no resolved view", never unlimited addressing or implicit
coherency.

`ut-tests/firmware_dma.rs` exercises composition only — mask independence,
missing facts, error mapping, window composition bounds and platform scope —
against a backend that returns already-resolved facts. The DMA-parent walk,
ancestor translation, topology rejection and coherency inheritance are tested
in `rs_fdtree`'s own host tests (`tests_dma.rs` and `tests_fwnode.rs`).

### Driver matching and activation

1. `EnumerationContext::probe_pending` takes all pending descriptors.
2. For each descriptor it calls `kdevice::probe_device_desc`, which:
   - takes the descriptor's binding lock (cross-task contention blocks and
     rechecks; same-task recursion returns `Busy`);
   - snapshots the bus type's drivers and walks them in registration order;
   - filters by `bus_type`, then calls `DeviceDriver::matcher().matches(identity)`;
   - on a match, probes that driver and stops at the first success.
3. Successfully activated devices have their runtime objects published to
   the matching `kclass` registries.
4. `Skipped` descriptors are already bound/terminal and are not reported.
   `Unmatched` (no candidate matched), `Unclaimed` (all matching candidates
   failed), `Busy` (same-thread recursion; remains `Pending`) and `Err`
   (core preparation failure; remains `Pending`) descriptors enter the
   `unclaimed` list for logging. A later explicit probe or driver
   registration can retry them.

### VirtIO device activation

VirtIO drivers share one activation entry across the PCI and MMIO
transports:

1. **PCI path**: extract the BDF from `DeviceLocation::Pci`, reopen
   `PciBus`, and run `probe_pci_device`, confirming the transport's
   reported `DeviceKind` matches the driver's declaration.
2. **MMIO path**: extract the physical address and size from
   `DeviceLocation::Mmio`, claim that physical range through
   `resource::claim_mmio_region` (the shared `driver_os_api::MmioClaimOp`
   contract, implemented by the X-Kernel provider), then `iomap_mmio`.
   The claim is acquired
   before any register access and stays owned by the mapping guard, so a
   duplicate descriptor registered by a rescan cannot identify or open a
   range that a live transport still owns; an overlapping claim fails with
   `ResourceBusy` before any register access. Claim coverage is limited to
   participants of the shared contract: `memspace::iomap_device`,
   `kdriver::iomap_mmio`, `devm_iomap` and non-VirtIO drivers do not join it,
   so this is not system-wide MMIO exclusivity. `virtio::identify_mmio_device`
   then validates the header and returns the raw device id **read-only** —
   it constructs no transport and writes no status register. The raw id is
   compared with the driver's declared type: an empty slot (id 0) or any
   other type fails with `Unsupported` before any device state exists. Only
   on an exact match does `virtio::open_mmio_transport` construct the
   transport and hand it to the dispatcher. `iomap_mmio` takes one mapping
   reference per call (cached hits included). The reference moves into
   `virtio::OwnedMmioTransport`, whose transport field is dropped before its
   mapping guard, so the region is unmapped only after the transport's last
   access and `Drop` reset, and the claim is released after the unmap;
   failed candidates drop the guard on return. Unmapping is not decided from
   the dispatch result. Removing a published device withdraws its class entry;
   in-flight readers may retain the runtime until their last handle is dropped.
   On final runtime drop, a VSOCK or sound device owner unsets its queues, resets the
   device, and releases RX mappings and queue rings before the transport (and
   therefore the claim) is dropped. GPU devres cleanup drains the active
   command, detaches the queues from callers and resets the device before
   withdrawing its IRQ handler. The teardown order and its evidence are described
   in `drivers/devices/virtio/docs/design.md`.

   MMIO fixture tests serialize global registry/HAL-counter resets with a
   sleepable mutex. Withdrawal alone is not a teardown barrier: the VSOCK
   subscriber can leave a poller holding a runtime reference. Fixtures signal
   a completion after their transport and real MMIO claim have dropped; tests
   wait for it before checking exact DMA balances or resetting counters for
   the next scenario. Cleanup also drains this completion on assertion-failure
   exits. A retained-reader regression pins final release to the test CPU to
   exercise progress without disabling preemption across the wait.
3. **Dispatch**: `dispatch_virtio_try_new` dispatches on `DeviceKind`:
   - `DeviceKind::Net` → `VirtIoNet::try_new` → `kclass::publish_net`
   - `DeviceKind::Block` → `block::activate` → IRQ setup → `kclass::publish_block`
   - `DeviceKind::Display` → `gpu::activate` → devres IRQ registration → PCI INTx enable when applicable → `gpu.start()` → `kclass::publish_display`.
     Activation captures the current `BindingRef` before acquiring resources;
     IRQ registration, INTx masking and GPU close callbacks all belong to that
     binding, including probe-failure rollback.
   - `DeviceKind::Input` → local `virtio/input.rs` activation → devres IRQ registration → PCI INTx enable when applicable → `kclass::publish_input`
   - `DeviceKind::Vsock` → `VirtIoSocket::try_new` → `kclass::publish_vsock`
   - `DeviceKind::Fs9p` → `VirtIo9p::try_new` → `kclass::publish_virtio_9p`
   - `DeviceKind::Hwrng` (platform/MMIO only) → `VirtIoRng::try_new_with_dma`.
     The firmware device's resolved `DmaInfo` is wrapped in
     `XKernelDeviceDma` and `virtio::DeviceDmaBackend`, so every queue ring
     and request buffer is allocated or mapped through that device's capability.
     Because the vendor backend requires the same addresses to work whether or
     not `VIRTIO_F_ACCESS_PLATFORM` is negotiated, this wiring admits only an
     identity window (the QEMU case) and fails others with `Unsupported`; that
     is a scoped consumer restriction, not a judgment about the window itself.
     A platform Hwrng device without a resolved view fails the probe; it never
     falls back to the static HAL. PCI Hwrng, whose firmware DMA description is
     out of scope, keeps `VirtIoRng::try_new` and the static HAL.

`DeviceDmaBackend` (in `drivers/devices/virtio/src/device_dma.rs`) is the
private bridge between `driver_os_api::DeviceDma` and the vendor's raw
`DmaBackend` tuple ABI. It stores the provider's `DmaAllocation` and
`StreamingMapping` tokens under exact release keys so allocation and mapping
release stay paired with the operation that created them. Queue setup requires
zero-filled allocations; that is part of the `DeviceDma::alloc_coherent`
contract and implemented by `XKernelDeviceDma`, not assumed from the page
allocator. The vendor queue and RNG types are generic over the backend, with
`StaticHal` as the default so existing drivers and tests are unchanged.

### Resource management (devres)

`driver-os-api-xkernel` provides x-kernel's implementation type for the
`driver-os-api` provider contract; the driver-facing resource API is exposed
to drivers by `kdriver::resource` as `DeviceResourceExt`.
`kdriver::resource` holds the static `XKernelResourceProvider` and passes
it explicitly to `kdevice::device::managed::devm_*_with_provider()`; on release the RAII
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
  `driver-os-api-xkernel` provider does not yet cover the threaded IRQ
  methods, so they return `DriverError::Unsupported`; a later kirq
  threadirq branch will supply the xkernel implementation.
  `driver-os-api-xkernel` is the adaptation layer between devres IRQ
  resources and the kernel IRQ core, translating `driver_os_api`
  trigger/controller/event/handler values into `kirq`'s own types; `kirq`
  does not depend back on devres.
  Future IRQ core capability extensions belong in `kirq`; the driver
  framework exposes kernel IRQ capability to drivers only through this
  adapter.
- **`device.devm_alloc_coherent`**: allocates a coherent DMA buffer via
  `kdma::allocate_dma_memory`; release calls
  `kdma::deallocate_dma_memory`.
- **`resource_provider().monotonic_time`**: only X-Kernel glue passes
  `driver_os_api::TimeOp` into reusable drivers; concrete drivers must not
  call `khal::time` directly.

Release runs in reverse acquisition order (LIFO) so resource dependencies
are never inverted.

### IRQ dispatch mechanism

`driver-os-api-xkernel::XKernelResourceProvider` keeps no local IRQ line
state. It converts `driver_os_api::IrqResource` into `kirq::IrqSpec`, wraps
`driver_os_api::IrqHandler` as `kirq::IrqHandler`, and registers with the IRQ
core per API:

1. `request_irq` calls `kirq::try_register_shared()`; `kirq` allocates a
   line-local `IrqActionToken` and stores the action identity; the provider
   returns `driver_os_api::IrqHandlerToken::SharedAction(id)` to devres.
2. `request_threaded_irq` / `request_threaded_irq_default` currently reuse
   the `driver_os_api::IrqOp` default implementations, returning
   `Unsupported` until a kirq threadirq provider override lands.
3. When an interrupt arrives, `kirq` snapshots the actions from
   `IrqDescRuntimeState`, invokes each handler in order, and passes the
   resolved `virq` as the handler argument. Each wrapper then converts the
   devres handler's `driver_os_api::IrqEvent` into `kirq::IrqEvent`.
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
  `in_flight` synchronization are owned by `kirq`; `driver-os-api-xkernel`
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

- PCI descriptors carry the VirtIO type in `TransportInfo`, so the PCI
  driver uses `VirtioTypeMatcher` on the type code rather than PCI
  vendor/device ids;
- platform descriptors only carry the `virtio,mmio` firmware identity, so
  the MMIO driver uses a `FirmwareMatchSpec` for that compatible and decides
  the concrete type during probe from the read-only header. Every enabled
  MMIO function driver therefore attempts every `virtio,mmio` descriptor;
  wrong candidates fail read-only and the descriptor stays `Pending` for the
  next candidate;
- the PCI vs MMIO activation differences are sealed inside
  `activate_virtio_pci` / `activate_virtio_mmio`; the upper
  `dispatch_virtio_try_new` is fully transport-agnostic.

**Rejected alternative**: one descriptor with runtime transport detection.
Fewer descriptors, but the matcher would compare bus type and VirtIO type
together, and `probe_device` would branch between two transport init
logics.

### IRQ handlers reach kirq through driver-os-api-xkernel

**Choice**: `driver-os-api-xkernel` is the `driver_os_api` provider adapting
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
registers a scoped stack node with `kcompletion::WaitQueue`, checks the driver's readiness
predicate, and parks if the request is still pending.
`notify` wakes registered tasks so they can check readiness again. The
provider owns wait registrations; the concrete driver owns its request and DMA
buffer. The current entropy integration uses one worker per selected VirtIO RNG source,
issuing reads serially.

The `hwrng` feature compiles this provider for injection into concrete drivers.

## Block I/O statistics clock

init_drivers installs resource_provider() as the block core's TimeOp before
discovery or consumer I/O. The X-Kernel provider reads khal's monotonic clock;
reusable block code imports only driver_os_api. Clock samples may occur under a
short IRQ-safe accounting lock in task or completion IRQ/softirq context and
must not allocate or sleep.

## Guest system sleep

`power::PowerDevices` snapshots exact bound driver/device Arcs, validates explicit
`supports_suspend` contracts, and orders children before parents. It owns PCI
endpoint BAR/command/INTx-line state; nonzero segments and bridge headers refuse
preparation. Device callbacks run outside registry locks. A successful prefix
is recovered in reverse order, followed by original PCI bus-master admission.
The system coordinator separately owns task/work/IRQ exclusion and CPU join.

The ACPI owner also publishes a copied `_S3` type pair at boot; PM performs no
AML evaluation while tasks or devices are frozen. Current callbacks implement
ISA serial, the synthetic PCI host and VirtIO block only. Other driver kinds
inherit a false capability even if legacy callbacks default to no-op.

The VirtIO block driver uses the activation owned by the current binding.
The device lifecycle lock serializes each callback with removal. The PM
dispatcher checks binding identity and restores driver data after the callback,
including when the driver returns an error.
Discovery descriptors, including those without runtime device objects, are
checked for PCI display-class devices before bound participants are captured.
These devices are rejected even if unbound: firmware VGA option-ROM
recovery is not implemented. The supported QEMU profile requires `-vga none`.
