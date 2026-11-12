# VirtIO — Design

## Purpose

This crate is X-Kernel's VirtIO driver adaptation layer. It wraps VirtIO devices
provided by the `virtio-drivers` crate in implementations of the `driver_base`
trait family. Block, network, display, input, vsock, and 9p subsystems use this
crate to access VirtIO virtual devices.

The crate is a reusable concrete-driver layer and cannot depend directly on
X-Kernel host APIs. DMA, IRQ mapping, and time capabilities must be supplied
through `virtio_drivers::Hal` or `driver_os_api` provider contracts.

## Background

X-Kernel runs in a bare-metal `no_std` environment and uses the VirtIO protocol
to communicate with a virtual machine monitor (VMM). The `virtio-drivers` crate
provides the base VirtIO implementation, including queue management and
configuration-space access, but its interfaces do not match X-Kernel's
`driver_base` trait family. This crate bridges those interfaces so upper layers
do not need to depend on VirtIO protocol details.

## Scope

The documented source files are:

```text
drivers/devices/virtio/
├── src/
│   ├── lib.rs          # device probing, error conversion, public re-exports
│   ├── blk.rs          # VirtIO Block adapter
│   ├── blk/requests.rs # Block request queue and token ownership
│   ├── device_dma.rs   # held device DMA capability adapted to the vendor allocation ABI
│   ├── rng.rs          # VirtIO RNG adapter
│   ├── pci.rs          # PCI transport and probing
│   ├── pci/x86_64.rs   # x86_64 MSI-X setup
│   ├── gpu.rs          # VirtIO GPU adapter
│   ├── input.rs        # VirtIO Input adapter
│   ├── net.rs          # VirtIO Net adapter with IRQ management
│   ├── socket.rs       # VirtIO Vsock adapter
│   ├── virtio_9p.rs    # VirtIO 9p adapter
│   └── mock_virtio.rs  # unit-test HAL and transport mocks
└── Cargo.toml
```

Feature flags select the device adapters compiled into the crate:

| Feature | Enabled device | Additional dependencies |
| --- | --- | --- |
| `block` | blk | `block` crate |
| `gpu` | gpu | `alloc`, `display` crate |
| `input` | input | `alloc`, `input` crate |
| `net` | net | `alloc`, `net` crate |
| `rng` | rng | `alloc`, `hwrng` crate |
| `socket` | socket | `alloc`, `vsock` crate |
| `virtio-9p` | virtio_9p | `alloc` |

## Architecture

```text
┌──────────────────────────────────────────────────────────────────────┐
│                           Upper subsystems                           │
│       (block / network / display / input / vsock / 9p)              │
└───────┬──────────┬──────────┬──────────┬──────────┬──────────┬───────┘
        │          │          │          │          │          │
      Block      Display    Input       Net       Vsock      9p API
      Device     Device     Device      Device    Device     Device
        │          │          │          │          │          │
┌───────┴──────────┴──────────┴──────────┴──────────┴──────────┴───────┐
│                          virtio (this crate)                         │
│                                                                      │
│  ┌───────┐  ┌───────┐  ┌───────┐  ┌───────┐  ┌───────┐  ┌───────┐    │
│  │  Blk  │  │  Gpu  │  │ Input │  │  Net  │  │ Sock  │  │  9p   │    │
│  │  Dev  │  │  Dev  │  │  Dev  │  │  Dev  │  │  Dev  │  │  Dev  │    │
│  └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘    │
│      │          │          │          │          │          │        │
│  ┌───┴──────────┴──────────┴──────────┴──────────┴──────────┴───────┐│
│  │                   lib.rs (probe / error / re-export)             ││
│  └───────────────────────────────┬──────────────────────────────────┘│
└──────────────────────────────────┼───────────────────────────────────┘
                                   │
                          virtio-drivers crate
                                   │
                          VirtIO Transport (MMIO / PCI)
```

| Component | Responsibility |
| --- | --- |
| `lib.rs` | MMIO identification and transport construction (`identify_mmio_device` / `open_mmio_transport`), PCI probing (`probe_pci_device`), raw device-id to `DeviceKind` mapping, VirtIO-error to `DriverError` conversion, driver-neutral `PciProbeError`, and public re-exports |
| `blk.rs` / `blk/requests.rs` | `VirtIoBlkDev` implements `BlockDeviceOperations` and manages admission, requests, and DMA reclamation |
| `gpu.rs` | Wraps `VirtIOGpu` in `VirtIoGpuDev`, which implements `DisplayDevice` |
| `input.rs` | Wraps `VirtIOInput` in `VirtIoInputDev`, which implements `InputDevice`; probes and caches static capability bitmaps and axis metadata, acknowledges the ISR in IRQ context, and asks the input layer to drain events in task context |
| `net.rs` | Wraps `VirtIONetRaw` in `VirtIoNetDev`, which implements `NetDevice` and manages RX/TX buffers, IRQ acknowledgement, and network RX scheduling |
| `socket.rs` | Wraps `VirtIOSocket` in the raw packet adapter `VirtIoVsockDev`, which implements `VsockDevice`; connection state is owned by `VSOCK_CONN_MANAGER` in `net/knet` |
| `virtio_9p.rs` | Wraps `VirtIO9p` in `VirtIo9pDev`, which implements `Virtio9pDevice` and provides `mount_tag()` and `request()` |
| `mock_virtio.rs` | Provides `MockHal` and `MockTransport` for unit tests |

## Calling Constraints And Execution Context

`virtio` sits between platform transports and upper subsystems. Callers must
satisfy the following constraints:

- **The platform must have completed device enumeration and mapping.**
  `identify_mmio_device` and `open_mmio_transport` require a valid mapped
  MMIO register region, and the caller must have exclusive access to it:
  identification creates a `UniqueMmioPointer` over the header and the
  transport owns the registers for its lifetime. kdriver establishes this
  with an exclusive claim from the shared `driver_os_api::MmioClaimOp`
  contract, acquired before the first register read and held until the
  transport is dropped; a duplicate descriptor registered by a rescan cannot
  alias a live transport. The contract arbitrates only its participants, so
  it is not system-wide MMIO ownership.
  Identification reads only
  the header fields (`magic`, `version`, `device_id`) and never writes device
  status; `open_mmio_transport` constructs the transport only after the
  caller has confirmed the type. `probe_pci_device` requires accessible PCI
  configuration space, BARs, and IRQ routes.
- **Initialization depends on the early boot environment.** Device probing,
  feature negotiation, virtqueue allocation, and IRQ registration normally run
  during boot or driver registration and require memory allocation, DMA
  support, and interrupt facilities to be available.
- **PCI vsock is polled.** Before driver initialization, `probe_pci_device`
  sets `Command::INTERRUPT_DISABLE` on the PCI function to suppress INTx.
  Transmit and receive progress through the vsock polling path. This does not
  mask the shared IRQ at the interrupt controller and does not affect devices
  such as blk. `VirtIoVsockDev` does not report an IRQ number. The policy only
  covers PCI INTx and is not a generic interrupt-disable mechanism for MMIO or
  future MSI-X vsock devices.
- **Ordinary device operations must not run in an inappropriate interrupt
  context.** Synchronous block I/O, 9p requests, GPU flushes, and network
  buffer reclamation may hold locks or wait for device completion. They must
  not be assumed safe in arbitrary non-sleepable contexts.
- **Network IRQ and normal-path concurrency rely on `SpinNoIrq`.** Callers must
  not bypass the existing wrappers and access `InnerDev` concurrently.
- **The VirtIO-net IRQ handler performs only device-level fast work.** It
  filters device handles by resolved IRQ, acknowledges the device interrupt,
  and calls only the RX scheduling capability attached by the network layer
  through `driver_net::NetRxScheduler`. It does not run the protocol stack or
  socket wake logic directly.
- **The VirtIO input IRQ handler only acknowledges interrupts.**
  `VirtIoInputIrqHandler` may run in hard-IRQ context. It acquires the device's
  `SpinNoIrq`, reads and clears the VirtIO ISR, and returns `NOT_HANDLED` when
  the shared line reports no interrupt for this device. It does not consume
  input events, allocate memory, or enter the VFS. Queue consumption remains in
  task context.
- **Shared safety depends on trait bounds and internal encapsulation.** blk
  uses internal locks for queues and request state. input, gpu, vsock and 9p
  serialize device access with an internal `SpinNoIrq`. Network devices rely
  on internal locking and token-buffer correspondence.
- **Upper layers must respect handle lifetimes.** For example, TX/RX handles
  returned by the network device must be reclaimed exactly as specified and
  must not be consumed twice or mixed across devices.

## State Machines

### Device Lifecycle

```text
Uninitialized ──probe──> Probed ──try_new──> Ready ──ops──> Active
                              │                  │
                              │   try_new error  │  device error
                              └──────Error───────> Failed
```

| From | To | Trigger |
| --- | --- | --- |
| Uninitialized | Probed | `identify_mmio_device` confirms the MMIO device type and `open_mmio_transport` provides the transport, or `probe_pci_device` produces one |
| Probed | Ready | `try_new()` negotiates features and allocates queues |
| Probed | Failed | `try_new()` fails because the device is unsupported or DMA allocation fails |
| Ready | Active | The first driver operation, such as `read_block` or `send`, is invoked |
| Active | Failed | The device returns an unrecoverable error |

### Block Request State

The host uses `try_new_irq`. `try_new` remains the explicit early-boot polling
constructor and is never a fallback after IRQ setup fails. Both constructors
use the same owned request queue. IRQ construction disables queue interrupts
until activation is ready.

The pending FIFO and submitted-token ownership retain a boxed `VirtioRequest`
containing one common `block::Request`, the protocol header/response, and queue
links. There is no driver-local waiter or second result state machine. The
request owns the transfer cursor; the private node owns only
protocol/transport resources. Terminal callbacks run after descriptor and DMA
retirement, outside the device lock.

### GPU Scanout Resource State

```text
Uninit ──probe/query resolution──> Ready
Ready ──create_scanout_resource──> ResourceAttached
ResourceAttached ──present_scanout_resource──> ScanningOut
ScanningOut ──present_scanout_resource(same resource/geometry)──> ScanningOut
ScanningOut ──present_scanout_resource(new resource/geometry)──> ScanningOut
ResourceAttached / ScanningOut ──destroy_scanout_resource──> Ready
Ready / ResourceAttached / ScanningOut ──device error──> Failed
```

| From | To | Trigger |
| --- | --- | --- |
| Uninit | Ready | `try_new()` negotiates features, initializes virtqueues, and queries display resolution |
| Ready | ResourceAttached | The DRM dumb-buffer backing is created and attached to a virtio-gpu 2D resource |
| ResourceAttached | ScanningOut | A DRM modeset/page flip issues transfer-to-host, set-scanout, and flush |
| ScanningOut | ScanningOut | Repeated presentation of the same resource and geometry reuses the existing scanout binding; another resource or geometry reprograms the binding |
| ResourceAttached / ScanningOut | Ready | The dumb buffer is destroyed and the resource is detached/unreferenced |
| Ready / ResourceAttached / ScanningOut | Failed | The device returns an unrecoverable error |

> The GPU driver no longer creates a fixed framebuffer in `try_new()`. Pixel
> memory is allocated by DRM dumb buffers and mmaped to userspace; the
> virtio-gpu resource attaches that backing directly. A page flip only submits
> transfer-to-host, set-scanout, and resource-flush commands and performs no CPU
> framebuffer copy.

### Input Event Queue State

```text
Empty ──device event──> HasEvent ──read_event──> Empty
                          │                         │
                          │ read_event (no event)   │ return WouldBlock
                          └──> WouldBlock ──────────┘
```

| From | To | Trigger |
| --- | --- | --- |
| Empty | HasEvent | The device produces an input event and the VirtIO queue contains a pending event |
| HasEvent | Empty | `read_event()` successfully pops an event |
| Empty | WouldBlock | `read_event()` finds no queued event and returns `DriverError::WouldBlock` |
| WouldBlock | HasEvent | The device produces a new event |

> Input events are notification-driven: the IRQ handler acks the ISR and calls `InputEventNotifier`, after which `inputdev` calls `read_event()` in task context. `read_event()` pops events from the VirtIO queue without blocking and returns `WouldBlock` when none are pending.

### Network Buffer State

```text
Free ──alloc_tx_buf──> Allocated ──send──> InFlight ──recycle_tx──> Free
  │                                                               │
  └───────────────────── recv + recycle_rx ────────────────────────┘
```

| From | To | Trigger |
| --- | --- | --- |
| Free | Allocated | `alloc_tx_buf()` removes a buffer from `free_tx_bufs` |
| Allocated | InFlight | `send()` submits the buffer to the VirtIO queue |
| InFlight | Free | `recycle_tx()` reclaims a completed transmit buffer |
| Free (RX) | InFlight (RX) | `try_new()` calls `receive_begin()` to submit RX buffers |
| InFlight (RX) | Free (RX) | `recv()` and `recycle_rx()` reclaim and resubmit an RX buffer |

### Vsock Connection State

This adapter keeps no vsock connection state; `listen()` and `unlisten()` are no-ops. The connection state machine (listen, connect, accept, close) is owned by `VSOCK_CONN_MANAGER` in `net/knet`, which drives this adapter's packet-level methods (`connect`, `accept`, `send`, `shutdown`, `force_close`, `credit_update`).

> Vsock connection state is owned by `VSOCK_CONN_MANAGER` in `net/knet`, not by this module. This module only fetches raw events through `poll_event()` and translates them into `VsockTransportEvent`.

### 9p Request State

```text
Idle ──request──> Waiting ──device response──> ResponseReady ──return──> Idle
                       │
                       │  device error
                       └──> Failed
```

| From | To | Trigger |
| --- | --- | --- |
| Idle | Waiting | `request()` submits a 9p request to the VirtIO queue |
| Waiting | ResponseReady | The device returns response data |
| ResponseReady | Idle | `request()` copies the response into `resp` and returns the byte count |
| Waiting | Failed | The device returns an error or the response buffer is too small |

> The 9p device uses a synchronous request/response model. `request()` blocks
> for a device response and has no timeout. `mount_tag()` may be called at any
> time and is independent of request state.

## Algorithms

### PCI Device Probe

1. Read PCI configuration space to obtain device type information.
2. Use `virtio_device_type()` to identify a VirtIO device and `as_device_kind()`
   to map it into `DeviceKind`.
3. `probe_pci_device` receives the INTx resource mapped during discovery as an
   `Option<IrqResource>` and does not read Interrupt Line itself.
   - On x86_64, network devices first try to establish MSI-X through `IrqOp`
     and use the provided INTx resource if MSI-X is unavailable. If neither is
     available, probing returns the driver-neutral
     `PciProbeError::NoInterrupt`. The host integration layer chooses the
     failure policy using the device's discovery-time INTx state; the driver
     layer never interprets a register value as a GSI.
   - Other device kinds use only the provided INTx resource. There is no x86
     Interrupt Line fallback.
   - Transport-construction failure returns `PciProbeError::Transport`, which
     remains distinct from interrupt unavailability and is not misclassified
     as an INTx routing failure.
   - A device that is not recognized as VirtIO returns
     `PciProbeError::NotVirtio`.
4. On other architectures, prefer the firmware route mapped during discovery
   and retain compatibility fallbacks when it is absent.
   - aarch64: `pci::legacy_interrupt_route()` obtains the device-tree `Hwirq`
     route and builds a GIC descriptor using the declared trigger type.
   - riscv64: map the route to a PLIC interrupt.
   - loongarch64: the static INTx fallback maps `0x10 + device % 4` through
     `IrqOp::map_irq` as `LoongArchExtioi` / `LevelHigh` without a domain. It
     returns the provider's virtual IRQ, not the raw hardware number; mapping
     failure leaves the IRQ absent.
5. Mask legacy INTx for input devices and the currently polled vsock path, then
   return `Result<(DeviceKind, PciTransport, IrqResource), PciProbeError>`. The
   host integration layer unmasks input INTx only after its shared interrupt
   handler is registered; vsock INTx remains masked.

### MMIO Device Probe

1. Convert the memory base address into a `VirtIOHeader` pointer.
   `NonNull::new` rejects null.
2. Create `MmioTransport` and validate the magic value (`0x74726976`) and
   version (only version 2 is supported).
3. Map the device type to `DeviceKind` through `as_device_kind()`.
4. Return `(DeviceKind, MmioTransport)`.

### Block Reads, Writes, And Flushes

`BlkMqOperations::queue_rq` accepts the common `Request`, prepares its protocol
node, and enqueues it. `QueueFull` retains the node in FIFO order. Completion
reclaims a finite used batch and refills from pending requests without caller
intervention. A successful transfer prefix advances the request cursor. Only
the final prefix or an error completes the common request. A no-op flush
completes without a token.

The library's scatter submission and completion APIs preserve byte ranges
through virtqueue descriptors. Segment count and size are negotiated; requests
exceeding a chain's limits continue from the common cursor. An otherwise
indescribable fragmented sector uses a preallocated 512-byte bounce buffer, not
a request-sized copy. The adapter rejects a per-segment limit below 512 bytes
with `Unsupported` during initialization because the fallback requires one
segment. The library's SG API only requires sector alignment of the combined
request length. Descriptor arrays are temporary stack views, not another
owner/request registry. The HAL still performs device-specific DMA mapping,
including its existing bounce pool; indirect-descriptor allocation remains a
transport cost.

Unknown or out-of-range tokens and unproven descriptor reclamation are fatal
invariant violations. `IO_ERR` and `UNSUPPORTED` after `pop_used` become
ordinary driver errors. `NotReady` is ambiguous and fails closed. The IRQ
constructor rejects panic-unwind builds. The host supplies non-sleeping
DMA/completion capabilities and serializes completion passes; caller-side waits
use only the common block adapter.

### GPU Scanout Resources

**Initialization:**

1. `try_new(transport)` negotiates features and allocates the control and cursor
   virtqueues.
2. `GET_DISPLAY_INFO` queries display resolution `(width, height)`.
3. The driver stores `width` and `height` in `DisplayInfo { width, height }`.
   Only `refresh_info()` updates the cached value afterward.

The virtio-gpu device is scanout-only and exposes no direct framebuffer
mapping. The `DisplayDevice` trait has no `fb()` or direct-mapping field. All
pixel output uses the scanout-resource path shared by `fbdevice` emulation and
`drmdevice`.

**Resource-backed display flow:**

1. DRM `CREATE_DUMB` allocates contiguous guest pages and calls
   `create_scanout_resource()`.
2. virtio-gpu sends `RESOURCE_CREATE_2D` and `RESOURCE_ATTACH_BACKING`.
3. Userspace mmaps the dumb buffer and writes pixels.
4. A DRM modeset/page flip calls `present_scanout_resource()`.
5. virtio-gpu sends `TRANSFER_TO_HOST_2D`, `SET_SCANOUT`, and
   `RESOURCE_FLUSH`.
6. DRM `DESTROY_DUMB` calls `destroy_scanout_resource()` to detach and
   unreference the resource.

> DRM allocates resource IDs and releases them with the dumb-buffer lifetime.
> The virtio-gpu driver serializes these resource commands on the control
> virtqueue.

### Display Size Changes

When the host changes the console size, for example by resizing the QEMU
window, it sets `VIRTIO_GPU_EVENT_DISPLAY` in the configuration-space
`events_read` word. `pci.rs` keeps Display INTx enabled; the IRQ handler that
`kdriver` installs acknowledges the interrupt through `ack_irq()` and, on a
configuration change, queues task-context work that calls `refresh_info()`.
The query path also refreshes the size:

1. `DisplayDevice::refresh_info()` reads `events_read`, acknowledges consumed
   bits by writing `events_clear`, and issues `GET_DISPLAY_INFO` again.
2. If the size differs from the cached value, it updates `DisplayInfo` and
   returns `Some(new)`; otherwise it returns `None`.
3. `drmdevice` calls `refresh_info()` before `GETCONNECTOR`, so the new size
   appears as a `<width>x<height>` connector mode, and `GETRESOURCES` reports
   the capability envelope rather than the current size.

`refresh_info()` issues control-virtqueue requests and therefore may run only in
process context, never from interrupt handling.

### Input Event Reads

**Initialization:**

1. `try_new(transport, irq)` calls `InnerDev::new(transport)`, which negotiates
   features and allocates VirtIO queues.
2. `name()` reads the device-name string and uses `"<unknown>"` on failure.
3. `ids()` reads bustype, vendor, product, and version into `InputDeviceId`.

**Event-read flow:**

1. `read_event()` calls `inner.pop_pending_event()`.
2. If an event is pending, return `Event { event_type, code, value }`.
3. If the queue is empty, return `DriverError::WouldBlock`. `inputdev` performs
   deferred event retrieval after device notification.

**Capability query:**

1. `get_event_bits(ty, out)` calls
   `inner.query_config_select(EvBits, ty, out)`.
2. The device writes its event bitmap into `out`.
3. The call returns `true` when data is available and `false` otherwise.
4. `try_new` also probes `EvBits(EV_ABS)`, `AbsInfo`, and `PROP_BITS`. It
   decodes the five `__s32` fields of every advertised axis into immutable
   `AbsInfo` values and copies the property bitmap into a fixed-length cache.
   Later `EVIOCGABS` and `EVIOCGPROP` calls do not touch configuration space.
5. `abs_info(code)` returns only cached static axis metadata. `inputdev`
   maintains scalar axis position from the `EV_ABS` stream; the driver does not
   duplicate event state.

A failed configuration query is treated as "not advertised": probing still
completes, `abs_info` returns `None`, and `inputdev` applies Linux
zero-filled `input_absinfo` semantics for the unadvertised axis. The event
bitmap decides whether `EV_ABS` is supported, so devices without absolute axes
never enter the per-axis query path.

### Network Transmit And Receive

**Initialization:**

1. `try_new(transport, irq)` calls `InnerDev::new(transport)` to negotiate
   features and allocate VirtIO queues.
2. Create `NetBufPool` with capacity `2 * QS` and 1526-byte buffers.
3. Fill every RX buffer by calling `receive_begin()` for each
   `rx_buffers[i]`, verifying `token == i`.
4. Preallocate every TX buffer by filling its frame header with
   `fill_buffer_header()` and placing it in `free_tx_bufs`.
5. If `irq` is provided, call `register_virtio_net_irq()` to register the
   interrupt callback.

**Transmit:**

1. `alloc_tx_buf(size)` pops a buffer from `free_tx_bufs` and sets
   `payload_len`.
2. After the upper layer fills the payload, `send()` calls
   `transmit_begin(frame)` to submit it to the VirtIO TX queue.
3. Store the `NetBuf` in `tx_buffers[token]` so it remains alive while the
   device owns it.
4. On interrupt, `recycle_tx()` loops over completed tokens from
   `poll_transmit()`, calls `transmit_complete()` to release each buffer, and
   returns it to `free_tx_bufs`.

**Receive:**

1. In IRQ mode, the handler calls `ack_interrupt()` and schedules RX; task
   context calls `recv()` and obtains a completed token through
   `poll_receive()`. In polling mode there is no IRQ handler; `recv()` calls
   `ack_interrupt()` and then queries the receive queue.
2. Take the buffer from `rx_buffers[token]` and call `receive_complete()` to
   obtain the header length and packet length.
3. Return a `NetBufHandle` carrying `hdr_len` and `payload_len`.
4. After upper-layer processing, `recycle_rx()` calls `receive_begin()` to
   resubmit the buffer and stores it back in `rx_buffers[new_token]`.

**IRQ handling:**

1. `register_virtio_net_irq(irq_provider, irq, inner, rx_scheduler)` stores a
   `VirtIoNetIrqHandle` in global `NET_IRQ_HANDLES`.
2. On the first registration for an IRQ, `REGISTERED_NET_IRQS` deduplicates it
   and `DevIrq::request_with(irq_provider, resource,
   Arc::new(handle_virtio_net_irq))` registers the action. The returned guard is
   stored in `NET_IRQ_GUARDS`.
3. `handle_virtio_net_irq()` iterates over handles, calls `ack_interrupt()`, and
   schedules RX work when a handle acknowledges the interrupt. It returns
   `IrqEvent::HANDLED` if a handle acknowledged the interrupt and
   `IrqEvent::NOT_HANDLED` otherwise.
4. With a registered IRQ, only the IRQ handler calls `ack_interrupt()`. This
   prevents the RX task from clearing a newly arrived, not-yet-scheduled device
   event. Without a registered IRQ, `recv()` acknowledges interrupts in the
   polling path.

### VirtIO Input Interrupt Flow

1. `VirtIoInputDev::try_new(transport, irq)` initializes both input queues,
   caches device identity, and records the resolved IRQ.
2. The host integration layer obtains `irq_handler()`, registers it through the
   `driver_os_api` contract, and only then publishes the class device.
3. The hard-IRQ handler validates the callback IRQ, acknowledges the transport
   ISR under the shared `SpinNoIrq`, and claims the interrupt only when the ISR
   is nonempty. Only queue interrupts invoke the installed
   `InputEventNotifier`; configuration interrupts never fabricate input-event
   notifications.
4. `inputdev` calls `read_event()` in task context, removes completed events
   with `pop_pending_event()`, resubmits descriptors, and distributes input
   frames to device files.

The IRQ handler and `VirtIoInputDev` share one
`Arc<VirtIoInputState>`. The transport and notifier each use `SpinNoIrq`, and
the notifier is called without holding the transport lock. The transport is
destroyed only after both the published input device and the registered IRQ
action release their references. The reusable driver does not depend on the
host IRQ core or a workqueue.

### Vsock Raw Transport Adapter

`socket.rs` does not maintain connection tables, listen sets, receive buffers,
or other connection state. It wraps `VirtIOSocket` in `VirtIoVsockDev`, which
implements X-Kernel's `VsockDevice` trait and provides packet-level
virtio-vsock operations:

- `connect` / `accept`: send connection-request or response control packets.
- `send` / `shutdown` / `force_close`: send data, perform graceful shutdown, or
  send a forced reset.
- `credit_update`: send a credit update.
- `poll_event`: remove one raw event from the RX virtqueue and pass its event
  type and payload to the caller.
- `guest_cid`: return the configured guest CID.

`VsockDevice` is a low-level packet-transport trait. `kdriver` publishes
`VirtIoVsockDev` through the existing `VsockDevice` class registry.
`VSOCK_CONN_MANAGER` in `net/knet` consumes the trait directly and owns all
connection, listening, buffer, and credit semantics.

RX, TX, and event queues use 128 entries, matching QEMU 8.2.4's
`vhost-vsock` capability and the depth used by the Linux guest driver. Each RX
buffer intentionally remains 8192 bytes instead of using Linux's 4 KiB RX SKB
budget. The vsock-TIPC bridge currently derives TIPC message boundaries from RX
events, and a 4 KiB buffer increases the number of vhost splits for records
larger than 4 KiB. The 8 KiB buffer preserves the current message-boundary
behavior. The buffer includes the 44-byte virtio-vsock header, leaving a
maximum payload of 8148 bytes.

**Initialization:**

1. `try_new(transport)` calls `VirtIOSocket::new(transport)` to negotiate
   features and allocate VirtIO queues.
2. It returns `VirtIoVsockDev`, which uses `SpinNoIrq` to serialize access to
   `VirtIOSocket`.

**Packet paths:**

1. Transmit: `send(info, buf)` converts `VsockConnectionInfo` into the lower
   layer's `ConnectionInfo` and calls `inner.send(buf, &mut ci)`.
2. Receive: `poll_event(handler)` calls `inner.poll()`, translates the
   `VsockEvent` into a `VsockTransportEvent` plus payload, and invokes
   `handler`.
3. Control actions: while handling an event, `handler` may call `VsockDevice`
   methods such as `accept`, `force_close`, and `credit_update` to send response
   packets. The callback runs after the virtqueue lock is released.

**Connection-state ownership:**

- This crate does not maintain a connection table.
- `VSOCK_CONN_MANAGER` in `net/knet` owns connection state.
- This crate only submits packets to or removes packets from virtqueues.
  `VsockConnectionInfo`, including `tx_cnt`, is supplied by the manager.

**Close paths:**

1. Graceful close: `shutdown(cid)` sends `VIRTIO_VSOCK_OP_SHUTDOWN`.
2. Forced close or rejection: `force_close(cid)` sends `VIRTIO_VSOCK_OP_RST`.

### 9p Requests

**Initialization:**

1. `try_new(transport)` calls `InnerDev::new(transport)` to negotiate features
   and allocate VirtIO queues.
2. Once the device is ready, `mount_tag()` returns the mount tag.

**Request/response flow:**

1. `request(req, resp)` calls `inner.request(req, resp)`.
2. The 9p request bytes are written to the VirtIO queue.
3. The call blocks for the device response.
4. Response bytes are copied into `resp`, and the written byte count is
   returned.

> The 9p device uses a synchronous request/response model. `request()` blocks
> for a device response and has no timeout. `mount_tag()` may be called at any
> time and is independent of request state.

## Concurrency Model

- **blk:** Submission and completion use one device `SpinNoIrq` for queue and
  state. Caller waits and terminal callbacks occur outside it. IRQ
  acknowledgement uses the same lock, and the host serializes per-device
  completion passes.
- **gpu / socket / virtio_9p:** Each adapter serializes access to its underlying
  device with an internal `SpinNoIrq`; trait methods take `&self`.
- **input:** `VirtIoInputState` wraps the lower device in `Arc<SpinNoIrq<_>>`.
  IRQ acknowledgement in hard-IRQ context is mutually exclusive with capability
  queries and event access in task context. Tasks never hold the lock while
  waiting.
- **net:** `InnerDev` is wrapped in `Arc<SpinNoIrq<...>>` because IRQ callbacks
  access the device through `ack_interrupt()` concurrently with normal
  transmit and receive paths. The global `NET_IRQ_HANDLES`, `NET_IRQ_GUARDS`,
  and `REGISTERED_NET_IRQS` state is also protected by `SpinNoIrq`.
- **IRQ registration:** `register_virtio_net_irq()` stores callback handles in
  `NET_IRQ_HANDLES` and registers each IRQ only once through
  `REGISTERED_NET_IRQS`. The `DevIrq` guard returned by
  `DevIrq::request_with()` is stored in `NET_IRQ_GUARDS` and unregistered when
  the device is dropped.

## Design Decisions

### Adapter Pattern Instead Of Direct Trait Implementations

The `virtio-drivers` device types, such as `VirtIOBlk<H, T>`, are generic
structs. Implementing the `driver_base` traits directly for them would violate
the orphan rule. Each submodule therefore defines a newtype adapter.

### Manual `Send` And `Sync` Implementations

`virtio-drivers` device types contain markers such as `PhantomData`, so the
compiler cannot infer `Send` and `Sync` automatically. X-Kernel does allow
VirtIO devices to move across threads and share references under the documented
locking rules, so the adapters provide manual `unsafe impl Send/Sync` blocks
with explicit safety arguments.

### Global State For VirtIO-net IRQs

The `handle_virtio_net_irq` callback must return `IrqEvent` and must also find
all network-device instances. `NET_IRQ_HANDLES` stores every network-device IRQ
callback handle. When an interrupt arrives, the handler iterates over the
handles and calls `ack_interrupt()`. `NET_IRQ_GUARDS` retains the `DevIrq`
guards so the interrupt action is unregistered when the device is dropped.

## Drop And Resource Release

### MSI-X Allocation Ownership

`PciMsixState` retains the exact `MsiResource` returned by its provider. A
configuration or vector failure disables MSI-X and moves that owner into
`PciIrqState::Failed(Some(...))`; it does not release a vector that may still
have an installed action. Repeated failure cannot discard the owner.

The explicit x86 `PciTransport::try_release_msix` entry permanently closes
MSI-X delivery before asking `IrqOp::try_free_msix` to release the allocation.
Success consumes the allocation and leaves a terminal failed/closed transport.
Rejection returns the original resource to the disabled transport for retry;
it cannot resume configuration or queue-vector setup or reactivate MSI-X.
Repeated release after success is a no-op. Legacy transports are unchanged.

The caller must first stop submissions, establish device/message quiescence,
and detach/drain all IRQ actions in sleepable task context. MSI-X masking alone
does not establish DMA or outstanding-message quiescence. The current network
Drop path removes its registration before dropping transport fields, but this is
not a general hot-unplug or reset protocol for every device class.

Transport Drop and initialization rollback have no error-return channel. They
use the provider's infallible `free_msix` contract, which does not return after
a rejected release. The default wrapper is fail-stop, not a recovery queue or
a promise of eventual cleanup. Callers requiring recovery must retain their
transport and use explicit release before Drop. Raw transport construction and
table-programming rollback occur before action registration; their normal
release still requires the provider's valid task context.

`ut-tests/msix_release.rs` checks configuration failure, repeated failure,
rejected release, and retry against a private fake provider and RAM-backed
configuration space. It does not claim hardware message-drain coverage.

### Device-Specific Drop

`VirtIoNetDev` implements `Drop` to call `unregister_virtio_net_irq()` and
release the corresponding guard from `NET_IRQ_GUARDS`.

`VirtIoVsockDev` and `VirtIoSoundDev` rely on the vendor device owner's `Drop`
(`VirtIOSocket` / `VirtIOSound`) for queue teardown. That owner performs, in
order:

1. detach every queue from the transport (`queue_unset`);
2. reset the device (`Transport::reset`, which writes the empty status and
   returns only after a status read observes it);
3. drop its fields in declaration order: RX buffers (`OwningQueue` detaches
   every outstanding token, unshares it and frees the allocation), the
   remaining queue rings, and finally the transport.

`transport` is the last declared field, so the final device-visible register
access (the reset) precedes unmapping and range-claim release. `OwningQueue`
never resets the device itself: its `Drop` only detaches and frees after its
owner established device-side stop.

Initialization rollback follows the same rule. Once `OwningQueue::fill` has
published any RX buffer, any later failure resets the device before the
partially constructed queue and transport are dropped. A failure inside `fill`
also resets before the already published buffers are released; earlier
publications are not rolled back inside the queue.

Other device adapters (block, net, GPU, 9p, input, RNG) keep their existing
release paths; this change does not convert them.

### Teardown Consumer Audit

| Consumer | Stop precondition | Release path | This round | Remaining risk | Verification |
| --- | --- | --- | --- | --- | --- |
| VSOCK `VirtIOSocket` | Owner `Drop` runs `queue_unset` ×3 then `reset`; a fatal published TX failure resets before detaching; a failed `fill` resets before its locals drop. | `OwningQueue::Drop` detaches every outstanding token, unshares and frees; remaining rings, then transport (last field). | Field order, owner reset, checked TX token, `failed` poison, `fill` rollback. | Reset busy-polls with no timeout and may hold `SpinNoIrq`; it is device-side evidence only. VSOCK is polled and registers no IRQ callback. | Vendor UTs: `drop_stops_device_before_releasing_unconsumed_rx_buffers`, `completed_rx_buffer_is_reused_and_released_once`, `rx_fill_failure_resets_before_releasing_published_buffers`, `indirect_tx_unshares_every_mapping_once`, `wrong_token_stops_device_and_releases_every_outstanding_buffer`, `unpublished_tx_failure_does_not_stop_the_device`, `ring_allocation_failure_releases_partial_queue`, `next_queue_allocation_failure_releases_existing_queues_after_reset`; the kdriver MMIO integration tests assert HAL and provider baselines. |
| sound `VirtIOSound` | Owner `Drop` runs `queue_unset` ×4 then `reset`; queue creation and config-read failures reset before return; an `OwningQueue::fill` failure resets. | `event_queue` `OwningQueue::Drop` detaches, unshares and frees; other rings, then transport (last field). | Field order, owner reset, `try_or_reset!` around creation and config reads, deferred `fill`. | No behavioral UT covers the sound teardown path; the reset limitations above apply. | Compile-time contract only this round; no sound failure-injection test. |
| Unchecked `add_notify_wait_pop` callers: vendor `console`, `blk`, `rtc`, `rng`, `gpu`, `virtio_9p`, `net/dev_raw`, sound control queue, and the driver GPU wrapper | Each caller's existing owner/transport `Drop`; only some transports reset in `Drop` (`MmioTransport`, `PciTransport`), and `HypPciTransport` requires an explicit reset. | Success recycles descriptors; a post-publication error returns no token, so the chain stays published until the queue and transport are destroyed. | None: the old helper keeps its signature and semantics. | A wrong token or post-publication error can leave a mapping and buffer unreleased until teardown; these paths were not converted or individually audited. | Existing per-device tests unchanged; no new wrong-token coverage for these consumers. |
| Transport wrappers: `SomeTransport`, `OwnedMmioTransport`, driver `PciTransport`, `HypPciTransport`, mock/test transports | A wrapper must forward rather than reimplement reset. | Each wrapper reaches its inner transport, which performs the status write and readback poll. | Explicit forwarding added to `SomeTransport`, `OwnedMmioTransport` and driver `PciTransport`; explicit reset added to `HypPciTransport`. | A wrapper that failed to forward would lose an inner override; `HypPciTransport` still depends on its owner calling reset before release. | The vsock recording transport observes the wrapper call; the MMIO integration tests observe the reset write through the mock transport. |

### Reset Semantics And Evidence

`Transport::reset` writes the empty status and busy-polls the status field
until it reads empty. For a compliant device this is device-side evidence that
it stopped accessing the old queues and buffers; it is the precondition for
`queue_unset`, detach, unshare and freeing. It is not host-side evidence:
returning from the status read does not prove that a host IRQ callback already
in flight has returned, because the readback synchronizes device state, not
host execution. Adapters which use IRQ callbacks must establish host-side
quiescence separately; block does so through admitted-I/O drain, IRQ
suppression and `DevIrq` release. The vsock adapter is polled and registers no
IRQ callback; probing also suppresses its INTx, so it has no host callback to
synchronize.

The poll has no timeout. If the caller runs with IRQs disabled — the vsock TX
path may hold `SpinNoIrq` — a non-responsive device produces an unbounded
IRQs-off busy wait. This is an accepted availability limitation; the teardown
contract does not claim to detect or recover from it.

Input-device IRQs are registered by the host through devres. PCI input devices
also add a devres cleanup action that masks INTx, so LIFO destruction first
stops new device interrupts and then releases the IRQ action. Releasing the
action waits for any running callback to finish and drops the shared-state
reference held by the handler. The published class device retains another
reference; after withdrawal and release of that reference, the lower transport
and queues are cleaned up.

For IRQ block instances, host activation keeps the device alive through
admitted-I/O drain, device IRQ suppression, native `DevIrq` release, and
reclaimer stop. `BlockState` represents:

- Live as `is_accepting` with `Some(transport)`.
- Closing as `!is_accepting` with `Some(transport)`.
- Closed as `!is_accepting` with `None`.

Active-request accounting spans admission through the terminal callback,
including requests awaiting descriptors. Closing rejects new submissions with
`Io` while the completion path continues to drain accepted requests.
`begin_close` receives a fresh closing-task signal. `finish_close` requires
drained requests and stopped callbacks, takes `InnerDev` under the lock, and
drops it outside the lock in task context. Retained `Gendisk` references own
only inert state; cached geometry and read-only queries never require a live
transport. No concrete driver imports `ktask` or `kirq`.

Request tests live in `ut-tests/blk_requests_unittest.rs` and remain a child of
the requests module. They cover queue refill, retained memory, scatter
addresses, transfer limits, errors, and close drain using real dependency
descriptors.

## IRQ-Driven RNG Adapter

`rng.rs` provides `VirtIoRngDev`, which implements `HwrngDevice` through the
nonblocking request and completion APIs in `kvirtiodrivers` 0.13.5. Production
probing registers the completion IRQ before publishing the device in the Hwrng
class. The old Char adapter and polling timeout have been removed.

Only VirtIO RNG devices with a usable IRQ are supported. Missing IRQ resources
or failure to register the completion IRQ cause probing to fail, and the device
is not published in the Hwrng class. This intentionally narrows compatibility
relative to the old polling adapter; there is no polling fallback.
`driver_virtio_rng` enables Hwrng through `kdriver/virtio-rng`; neither its
Cargo feature nor `KFEAT_DRIVER_VIRTIO_RNG` requires the Char subsystem.

The driver is disabled globally by default. QEMU and crosvm defconfigs enable
it explicitly; bare-metal configurations such as RK3588 do not inherit it. The
driver switch does not enable the entropy consumer. Select
`KFEAT_ENTROPY_VIRTIO_RNG` separately to connect Hwrng devices to the pool;
Kconfig requires `KFEAT_DRIVER_VIRTIO_RNG` for this source.

`RngRequest` owns a 256-byte `Box` and its `VirtIORng` queue together. Its state
moves from `Idle` to `Pending(token)`, then to
`Readable(remaining: Range<usize>)` after reclaim, and back to `Idle` when
cached bytes are consumed. `NotReady` preserves `Pending` without a fault or
reader notification; a later completion IRQ can reclaim the same submission.
`WrongToken` is terminal: with one outstanding request it represents an invalid
completion, not an out-of-order result to retry. Submission errors for a full
queue or an invalid fixed buffer likewise indicate broken invariants.

A stored fault is terminal and exposed through `HwrngDevice::is_failed`. Host
wait errors do not set it. Subsequent reads can join the same pending request
or consume bytes already reclaimed by IRQ after a wait error. Only `read`
submits, and readers never poll the used ring. The hard-IRQ handler
acknowledges status, attempts at most one completion for a queue interrupt,
drops the request lock, and then notifies waiters. Copying cached bytes to the
caller and submitting the next request happen in task context.

The host injects `RngCompletion` and an IRQ provider. `try_new_irq` installs a
weak callback before enabling completion interrupts. The device owns the IRQ
guard; the weak callback avoids an ownership cycle. `SpinNoIrq` serializes
readers with completion. No device lock spans host waiting or notification.
Empty reads perform no I/O; wait failures leave submitted DMA storage owned by
the driver. Short reads retain their unused cached suffix.

### Per-Device DMA Backend

`device_dma.rs` provides `DeviceDmaBackend`, an implementation of the vendor's
`DmaBackend` over a `driver_os_api::DeviceDma` capability. `VirtQueue` and
`VirtIORng` are generic over `B: DmaBackend` (`StaticHal<H>` by default), so a
caller with a resolved device view can route ring allocation and request
sharing through that device instead of the static HAL.
`VirtIoRngDev::try_new_irq_with_dma` supports this case; `try_new_irq` retains
the static path.

The adapter stores the provider's `DmaAllocation` and `StreamingMapping` tokens
in exact-key maps and removes them under the record lock, releasing each token
after the lock is dropped. A rejected duplicate share is released immediately;
a rejected duplicate coherent allocation is returned through `free_coherent`
because `DmaAllocation` is a plain record without a drop guard. Allocation
validates the complete page-rounded interval, both addresses' page alignment,
and overflow before publishing the tuple. The constructor is `unsafe` because
the provider's operation context, exclusivity, and byte-preservation guarantees
cannot be checked here.

`ut-tests/device_dma_unittest.rs` covers allocation release, streaming release,
duplicate rejection, and empty-buffer rejection with a counting fake provider.

Completion includes HAL unshare in hard-IRQ context. The backend must be
IRQ-safe and non-sleeping. X-Kernel's backend takes the global kdma allocator
lock during unmap; one-request reclamation does not imply bounded IRQ latency.
This design preserves inline reclamation and introduces no softirq path.

Device removal and rebinding are unsupported. Dropping a `Pending` request
retains its queue and buffer rather than freeing live DMA storage; normal idle
and readable requests are released. Tests emulate ring completion and IRQ
delivery to check reclaim-before-wake, short reads, spurious interrupts,
malformed completions, wait failures, and retention of outstanding storage
without real-device timing.

## Conservative Input Readiness

VirtIO input keeps a sticky readiness hint in an atomic. The IRQ handler
acknowledges this device's interrupt status and sets the hint. The timer-side
readiness probe only reads the hint and never touches the ISR. Neither path
pops or recycles descriptors. Event reads set the hint from the actual dequeue
result, so an empty queue clears it only after a task observes it.
