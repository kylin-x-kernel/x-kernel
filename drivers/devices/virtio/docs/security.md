# VirtIO — Security And Reliability

## Scope

This analysis covers the complete VirtIO adapter crate, including device
probing, class-device adapters, virtual-queue and DMA ownership, interrupt
handling, resource cleanup, and the device-controlled data paths. No modules are
excluded.

## Trust Model

```text
Upper subsystems (block / network / display / input / vsock / 9p)
   │
   │ safe API: VirtIoBlkDev, VirtIoGpuDev, VirtIoInputDev,
   │          VirtIoNetDev, VirtIoVsockDev, VirtIo9pDev
   │          probe_pci_device()
   │ unsafe API: identify_mmio_device(), open_mmio_transport()
   │
   v
┌──────────────────────────────────────────────────────────────┐
│  virtio                                                      │
│                                                              │
│  ┌── unsafe boundary ──────────────────────────────────────┐ │
│  │ identify_mmio_device: read-only header reads            │ │
│  │ open_mmio_transport: MmioTransport::new()               │ │
│  │ net.rs: NetBuf::from_handle()                           │ │
│  │ net.rs: receive_begin() / receive_complete()            │ │
│  │ net.rs: transmit_begin() / transmit_complete()          │ │
│  │ contracts/vsock: TX buffer slices                       │ │
│  │ socket.rs: transmit begin/complete                      │ │
│  │ modules: unsafe impl Send/Sync                          │ │
│  └─────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

- **Callers of safe APIs:** The module is trusted to maintain its invariants,
  and device operations must not cause undefined behavior.
- **Callers of unsafe APIs:** The caller of `identify_mmio_device` and
  `open_mmio_transport` must provide a valid, properly aligned MMIO address.
  The caller of `probe_pci_device` must provide accessible PCI configuration
  space.

## External Boundaries And Attack Surface

The LoongArch PCI INTx fallback must pass its static PCH route through the IRQ
provider before exposing an IRQ number. Identity numbering alone does not
establish a host descriptor or validated flow. Mapping failure returns no IRQ;
the fallback neither registers a handler nor enables delivery. Unit tests use a
recording mapper to check route metadata, nonidentity virtual IRQs, and failure
without accessing PCI or interrupt-controller hardware.

PCI vsock currently has no ISR-acknowledgement path and relies on polling for
transmit and receive progress. Probing must therefore disable INTx output for
that PCI function to prevent repeated delivery of an unacknowledged
level-triggered interrupt. Removing only the software IRQ metadata does not
disable the hardware interrupt, while masking the shared line would also block
completion notifications for other devices, including blk. Command updates
preserve all other bits so memory access and DMA capability remain enabled.

The PCI display function enables INTx after registering its shared handler.
The handler returns `IrqEvent::NOT_HANDLED` when the device ISR is empty.
Only the submitting task recycles a used chain, after matching its token,
while its input and output buffers are still borrowed. A mismatched ID retires
both queues: the driver waits for device reset, detaches the submitted chain
using the original buffers, and rejects further commands.
Unbinding drains that task before withdrawing IRQ delivery. An unresponsive
device can stall an outstanding command or reset acknowledgement; a timeout alone would not make it
safe to release buffers still owned by the device.

`virtio` is a classic hardware or virtual-hardware boundary module. It directly
connects VirtIO devices exposed by the VMM, platform transports, and the
upper-level driver framework. The attack surface therefore extends beyond Rust
`unsafe` operations.

The module directly touches these boundaries:

- **MMIO and PCI configuration space:** device headers, BARs, capabilities, and
  IRQ routes.
- **DMA and shared-memory buffers:** virtqueue descriptors, network RX/TX
  buffers, block I/O buffers, GPU scanout-resource backing, and 9p
  request/response buffers.
- **Device-returned data and state:** VirtIO queue completion state, packet
  lengths, mount tags, input events, and vsock events.
- **VMM and virtual-device behavior:** A malicious or incorrect device may
  return malformed lengths, wrong tokens, protocol-invalid responses, or fail to
  generate an expected interrupt.

This module usually does not process user pointers directly, but it accepts
buffers, requests, and handles from upper subsystems. The boundary between
those subsystems and the device must therefore be included in threat analysis.

Threat analysis must specifically cover:

- Whether device register addresses and transport objects are valid.
- Whether DMA, virtqueues, tokens, and buffers remain one-to-one.
- Whether device-provided lengths, states, and responses can violate module
  assumptions.
- Whether IRQ, reclaim, and normal I/O paths can violate concurrency or lifetime
  invariants.

## IRQ Block Request Safety

The IRQ constructor rejects panic-unwind builds before creating a device. A
boxed protocol node retains the common request and all its memory owners. It is
converted to raw ownership before publishing DMA pointers; the token table
retains the unique allocation until descriptor reclamation. Box reconstruction
occurs only after the library retires the original descriptor chain. CPU access
to protocol and data bytes is excluded while hardware owns them. Pending
requests remain owned even if the caller drops its wait handle.

Scatter descriptors reference original extents. Transfer splitting advances the
common request only after successful retirement; original owners survive to the
terminal callback on either success or error. A sector bounce is used only when
device segment limits cannot represent that sector's fragments. Read
destinations are exclusive; borrowed synchronous write sources remain immutable.
Both constructors reject per-segment limits below 512 bytes before publishing
the device, preserving the adapter's single-segment sector-bounce invariant.

Unknown or out-of-range tokens and unproven descriptor reclamation panic before
normal request retirement. The pinned dependency's `IoError` and `Unsupported`
completion outcomes follow `pop_used`; `NotReady` can come from either pop or
status and is deliberately fatal. Fatal-error paths have been source-reviewed,
not injected into the shared QEMU test process. No device reset or retry is
invented.

Callbacks receive status and original owners only after all DMA references have
retired. They run outside the device lock and must not sleep. The host
serializes completion passes, keeps IRQ and reclamation active through drain,
and supplies DMA routines valid in completion context. Waiting resources are
prepared by the block caller's adapter, not by the transport.

Tests use an identity-mapped fake HAL and real dependency descriptors;
production uses the existing host bounce pool. Passing fake tests does not prove
physical-device DMA coherency, which remains a hardware-validation obligation.

Close counts accepted requests through terminal callbacks, including the pending
FIFO. `begin_close` rejects new submissions while keeping accepted requests
alive; its drain signal is retained until the last callback returns. Host close
serializes ownership, waits for drain, suppresses and acknowledges device IRQs,
synchronizes its native action, and stops the reclaimer before `finish_close`
takes the transport. Transport destruction and coherent queue deallocation occur
outside `SpinNoIrq` in the closing task. The device's immutable geometry and
read-only cache remain valid after close. No IRQ callback owns activation or can
run its synchronous destructor. Malformed device completion remains fatal;
graceful close gives no forced-DMA cancellation or timeout guarantee for a
nonresponsive device.

## VirtIO Input IRQ Safety

`VirtIoInputDev` and its IRQ handler share one `VirtIoInputState`. Every access
to the transport, configuration, and queues acquires that state's `SpinNoIrq`.
A local input interrupt therefore cannot preempt a task that holds the
transport. A remote CPU can spin only for a bounded configuration, queue, or ISR
access.

Before touching the transport, the handler validates the callback IRQ. It
returns `IrqEvent::NOT_HANDLED` when the ISR is empty, preserving shared-line
dispatch semantics. Queue or configuration interrupts return `HANDLED`. Only a
queue interrupt invokes the input layer's `InputEventNotifier` to schedule
deferred work. The handler does not allocate memory, access the VFS, copy
events, or traverse wake lists. The host registers and releases the handler
through `driver_os_api`; release waits for a running callback to finish before
dropping the handler's `Arc`.

After notification, the `inputdev` task-context worker calls
`pop_pending_event()`, reclaiming completed descriptors even when no file reader
exists. Untrusted event contents therefore never enter a userspace buffer in
hard-IRQ context. The input layer enforces frame boundaries and key or
absolute-axis code ranges; devfs owns a bounded queue for each open file. Axis
metadata is immutable after probing. `EVIOCGABS` reads that cache and combines
it with the live value from `inputdev`; it never re-parses device-supplied
configuration words on the ioctl path.

## Unsafe Code Inventory

### 1. `identify_mmio_device` and `open_mmio_transport` (`lib.rs`)

```rust
let info = unsafe { read_mmio_header(reg_base, reg_size) }?;  // read-only
// caller compares info.device_id with the driver's declared type
let transport = unsafe { open_mmio_transport(reg_base, reg_size) }?; // after match
```

**Invariant:** `reg_base` points to a valid, aligned VirtIO MMIO register
region of at least `reg_size` bytes. For `identify_mmio_device` the caller must
also have **exclusive access** to that region for the duration of the call,
because `read_mmio_header` creates a `UniqueMmioPointer` over the header; no
other mapping, transport or interrupt handler may access the same registers
while it exists. For `open_mmio_transport` the region must remain valid and
exclusive for the transport's lifetime.

**Why it is safe:** `read_mmio_header` checks alignment and the
`CONFIG_SPACE_OFFSET` minimum size, then reads only `magic`, `version` and
`device_id`. It constructs no transport, so `Drop` cannot reset the device and
zero/unknown ids are returned as ordinary values. `MmioTransport::new`
additionally validates the magic value, device id and version before handing
back a transport. The caller, the platform initialization code, guarantees the
MMIO address comes from firmware and remains mapped.

**Mapping ownership:** `OwnedMmioTransport<G, T = MmioTransport<'static>>`
wraps an opened transport and the guard `G` that owns its mapping and its range
claim. Its `transport` field is declared before the guard, so the transport is
dropped first (final register access and reset) and the mapping is released
only afterwards. The guard must hold the `iomap_device` reference; releasing it
is never decided from a dispatch result. The guard's `Drop` unmaps first and
releases the range claim afterwards. Releasing that final reference runs the
claim release, so it must happen in normal, sleepable task context with local
IRQs enabled and without a spin or preemption guard, even when it occurs
outside the remove function. The `T` parameter defaults to the production
transport; unit tests substitute a mock backend without changing the ownership
or drop order.

**Callers:**

- `kdriver::activate_virtio_mmio`, where firmware or the device tree
  guarantees a valid address and `iomap_mmio` established the mapping. Before
  the first register read it acquires an exclusive claim on
  `[base, base + size)` through `driver_os_api::MmioClaimOp`, implemented by
  the host provider. The table rejects every overlapping live claim,
  including one from a duplicate descriptor that `DeviceManager::rescan` may
  register while the previous transport is still alive, so a second probe
  cannot read or open the same physical range. The claim moves into the
  transport owner and is released only after the transport is dropped. The
  descriptor binding lock only serializes users of one descriptor and is not
  the physical-exclusivity proof. Only contract participants are arbitrated:
  other mapping entry points and drivers that do not claim are outside this
  guarantee.

### 2. VirtIO GPU Scanout Resource Backing (`gpu.rs:309-393`)

The `DisplayDevice` trait no longer exposes `fb()` or a `FrameBuffer` type, and
the driver no longer constructs slices from a raw framebuffer pointer. GPU pixel
backing is allocated by a DRM dumb buffer or fbdev shadow buffer and passed in a
`ScanoutWindow` to `create_scanout_resource()`. The virtio-gpu
driver only attaches that backing to a 2D resource and later submits
`TRANSFER_TO_HOST_2D`, `SET_SCANOUT`, and `RESOURCE_FLUSH` on page flip.

**Invariants:**

- The DRM layer obtains backing physical addresses from `PageRef` and keeps the
  requested byte length separate from the rounded allocation size.
- Backing remains valid from resource attachment through destruction.
- The DRM layer allocates resource IDs and performs detach/unref on destroy.
- The driver's internal lock serializes access to the virtio-gpu control queue.

**Callers:**

- DRM dumb-buffer lifetime management.

The `VirtIoGpuInner::scanout` field (`ScanoutBinding`) caches only the confirmed
resource ID and rectangle for scanout 0. It is protected by `GpuState::inner`,
the sleepable operation mutex reached through `VirtIoGpuDev.0`, together with
the control queues. A guest-rendered presentation submits `TRANSFER_TO_HOST_2D`
first; a host-rendered presentation skips that transfer. Both submit
`RESOURCE_FLUSH` last, and `SET_SCANOUT` is sent only when the resource or
rectangle changes. The driver publishes cached state only after `SET_SCANOUT`
succeeds, clears it on any presentation error, and clears a matching binding
after successful resource unref. A device error therefore cannot turn an
unconfirmed binding into an active binding or let a destroyed resource remain
eligible for reuse.

### 3. `receive_begin` In `VirtIoNetDev::try_new()` (`net.rs:263-278`)

```rust
let token = unsafe {
    dev.inner.lock().receive_begin(rx_buf.buffer_mut()).map_err(as_driver_error)?
};
```

**Invariant:** The supplied buffer remains valid and unmodified by other code
from the return of `receive_begin()` until `receive_complete()` is called.

**Why it is safe:** `rx_buf` owns the buffer. After `try_new()` finishes, the
buffer is stored in `rx_buffers[token]`, so its lifetime covers the device-use
interval.

**Callers:**

- `VirtIoNetDev::try_new()`, which stores the buffer in the `rx_buffers` array.

### 4. `from_handle` And `receive_begin` In `VirtIoNetDev::recycle_rx()` (`net.rs:366-388`)

```rust
let mut rx_buf = unsafe { NetBuf::from_handle(rx_buf) };
let new_token = unsafe {
    self.inner.lock().receive_begin(rx_buf.buffer_mut()).map_err(as_driver_error)?
};
```

**Invariants:**

- `from_handle`: The supplied handle is valid and not used by other code.
- `receive_begin`: The same invariant as item 3.

**Why it is safe:**

- `from_handle`: The network stack guarantees that the handle was returned by
  `recv()` and is consumed exactly once.
- `receive_begin`: The buffer is stored in `rx_buffers[new_token]`, preserving
  its lifetime.

**Callers:**

- The upper network stack, which guarantees handle uniqueness.

### 5. `transmit_complete` In `VirtIoNetDev::recycle_tx()` (`net.rs:390-420`)

```rust
unsafe {
    self.inner.lock().transmit_complete(token, tx_buf.frame()).map_err(as_driver_error)?;
}
```

**Invariant:** The buffer frame associated with `token` is the same frame as
`tx_buf.frame()`.

**Why it is safe:** `tx_buf` is removed from `tx_buffers[token]`, where `send()`
placed it, so the token-to-buffer correspondence is correct.

**Callers:**

- `VirtIoNetDev::recycle_tx()`, where the token and buffer come from the same
  array index.

### 6. `from_handle` And `transmit_begin` In `VirtIoNetDev::send()` (`net.rs:422-439`)

```rust
let tx_buf = unsafe { NetBuf::from_handle(tx_buf) };
let token = unsafe {
    self.inner.lock().transmit_begin(tx_buf.frame()).map_err(as_driver_error)?
};
```

**Invariants:**

- `from_handle`: The handle is valid and unused.
- `transmit_begin`: The buffer frame remains valid until transmission
  completes.

**Why it is safe:**

- `from_handle`: The caller guarantees that the handle came from
  `alloc_tx_buf()` and is used exactly once.
- `transmit_begin`: The buffer is stored in `tx_buffers[token]` until
  `recycle_tx()` reclaims it.

**Callers:**

- The upper network stack, which guarantees handle uniqueness.

### 7. `receive_complete` In `VirtIoNetDev::recv()` (`net.rs:441-474`)

```rust
let (hdr_len, pkt_len) = unsafe {
    self.inner.lock().receive_complete(token, rx_buf.buffer_mut()).map_err(as_driver_error)?
};
```

**Invariant:** The buffer associated with `token` is the same buffer as
`rx_buf.buffer_mut()`.

**Why it is safe:** `rx_buf` is removed from `rx_buffers[token]`, where
`try_new()` or `recycle_rx()` placed it, so the token-to-buffer correspondence
is correct.

**Callers:**

- `VirtIoNetDev::recv()`, where the token and buffer come from the same array
  index.

### 8. Vsock TX Buffer Slices And Descriptor Completion
(`contracts/vsock`, `socket.rs`)

`VsockTxBuf::header()`, `payload()`, `frame()`, and their mutable variants
construct slices from the buffer's fixed pool slot. The virtio adapter passes
`frame()` to `transmit_begin()`, and `reclaim_completed_tx_locked()` passes the
same frame to `transmit_complete()`.

**Invariants:**

- Each `VsockTxBuf` exclusively owns one fixed pool slot.
- The pool never reallocates its backing `Vec` after construction.
- Header and payload lengths are validated against the slot capacity before a
  slice is formed.
- The packet remains in `tx_bufs[token]` from submission until completion is
  reclaimed.
- The descriptor token used for completion is the same token returned by
  `transmit_begin()`, and its buffer is the packet stored at that index.

**Why it is safe:** Ownership prevents aliasing of the slot while a packet is
being encoded or transmitted. The in-flight table keeps the owning handle and
therefore its pool storage alive for the entire device-use interval. A failed
completion restores the packet to its token slot before returning the error.

**Callers:**

- `VsockTxBuf` slice accessors in `drivers/contracts/vsock`.
- `VirtIoVsockDev::queue_tx_buf()` and
  `VirtIoVsockDev::reclaim_completed_tx_locked()`.

### 9. `unsafe impl Send/Sync` In Device Modules (`blk.rs`, `input.rs`, `net.rs`, `socket.rs`, `virtio_9p.rs`)

```rust
unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkDev<H, T> {}
// Similarly for VirtIoInputState, VirtIoNetDev,
// VirtIoVsockDev, and VirtIo9pDev.
```

**Invariant:** The device type can be moved between threads (`Send`) and shared
by immutable reference (`Sync`) without violating its synchronization
contracts.

**Why it is safe:**

- blk, socket, and virtio_9p: every access to the underlying device goes
  through the adapter's internal `SpinNoIrq`, so shared `&self` access cannot race.
- gpu: The compiler derives `Send` and `Sync` from the transport and lock
  types. Task operations hold the sleepable mutex, and the IRQ handler
  accesses registers under `SpinNoIrq`.
- input: `VirtIoInputState` manually implements `Send` and `Sync`; every access
  to the underlying input device is serialized by its `SpinNoIrq`. The
  notification callback is protected by a separate `SpinNoIrq`, is invoked
  after releasing the transport lock, and may only schedule deferred work, not
  re-enter the driver.
- net: Internal `SpinNoIrq` state protects all shared access, making
  `Send`/`Sync` safe under the constructor and handle-lifetime contracts.
- vsock: The adapter protects `VirtIOSocket` with `SpinNoIrq` and its TX
  free/in-flight tables with `SpinNoPreempt`. Buffer slices come from a
  per-device fixed pool whose slots are exclusively owned by one `VsockTxBuf`.

### 10. VirtIO Queue Publication And Detach Contracts (`vendor/kvirtiodrivers/src/queue.rs`, `queue/owning.rs`, `device/socket/vsock.rs`)

```rust
pub unsafe fn add_notify_wait_pop_checked<'a>(
    &mut self,
    inputs: &'a [&'a [u8]],
    outputs: &'a mut [&'a mut [u8]],
    transport: &mut impl Transport,
) -> core::result::Result<u32, SyncError>;

pub unsafe fn detach_unused_buffer<'a>(&mut self, token: u16, ...) -> bool;
pub unsafe fn fill(&mut self) -> Result<()>;
pub unsafe fn detach_all_after_stop(&mut self);
```

**Invariant:** `add_notify_wait_pop_checked` may return while the queue still
owns a published descriptor chain and its DMA mappings. The caller must keep
the buffers valid until one of these holds: `Ok` (the queue unshared them);
`Err` with `token: Some(_)` after it stopped the device and detached that
token; or `Err` with `token: None` (nothing was published). After `fill`
returns `Ok`, or returns `Err` after publishing anything, the owning device
must reset before the queue is dropped. `detach_all_after_stop` and
`detach_unused_buffer` may only run after device-side stop, and the queue must
not be used afterwards.

**Why it is safe:** `VirtQueue::in_use` is the authoritative publication state;
a head is marked in use after `add` succeeds and cleared only by
`recycle_descriptors`. The checked API returns the token on any post-publication
failure. The vsock wrapper handles that case by resetting the transport before
touching the request, detaching the TX token with the original buffers, then
detaching all RX buffers, and setting a sticky `failed` flag; later `send` and
`poll` calls return `IoError` without queue access. `OwningQueue` clears each
buffer slot after detaching it, so `Drop` sees an empty list and cannot unshare
or free twice. `SyncError::token` is `None` for an `add` failure, so VSOCK
returns the original error without reset or poison. Constant
`[None; SIZE]` storage uses `NonNull<[u8; N]>` because the queue owns the
allocation, and `fill` only leaks a `Box` after `add` succeeded.

## Memory-Safety Invariants

1. **Buffer-token correspondence:** The buffers in `rx_buffers[token]` and
   `tx_buffers[token]` must match the buffers associated with the same token in
   the VirtIO queue. A violation makes the device read or write the wrong
   memory.
2. **MMIO address validity and exclusivity:** `reg_base` passed to
   `identify_mmio_device` must point to a valid MMIO register region for the
   duration of the call, with no other alias accessing it (its header read is
   an exclusive `UniqueMmioPointer`); the region passed to
   `open_mmio_transport` must remain mapped and exclusive for the device
   lifetime. The kdriver call site establishes exclusivity with the shared
   range-claim contract described above, not with the per-descriptor binding
   lock; non-participants are not arbitrated.
3. **NetBuf handle uniqueness:** Every caller of `NetBuf::from_handle` must
   provide a valid handle that has not already been consumed.

## Thread Safety

| Type | `Send` condition | `Sync` condition |
| --- | --- | --- |
| `VirtIoBlkDev<H, T>` | `H: Hal, T: Transport` (manual impl) | `H: Hal, T: Transport` (manual impl) |
| `VirtIoGpuDev<H, T, W>` | Derived from the shared GPU state (`T: Transport + Send`, `W: GpuHost`) | Derived from the operation mutex and IRQ-safe register lock |
| `VirtIoInputDev<H, T>` | `Arc<VirtIoInputState<H, T>>`; state manually implements `Send` | `Arc<VirtIoInputState<H, T>>`; state manually implements `Sync` |
| `VirtIoNetDev<H, T, QS>` | `H: Hal, T: Transport` (manual impl) | `H: Hal, T: Transport` (manual impl) |
| `VirtIoVsockDev<H, T, RX_BUF_SIZE>` | `H: Hal, T: Transport` (manual impl) | `H: Hal, T: Transport` (manual impl) |
| `VirtIo9pDev<H, T>` | `H: Hal, T: Transport` (manual impl) | `H: Hal, T: Transport` (manual impl) |

All manual `Send` and `Sync` implementations rely on these guarantees:

- blk, input, gpu, vsock and 9p serialize device access with an internal
  `SpinNoIrq`. Input capability bitmaps and axis metadata are immutable after
  probing.
- Network devices protect the shared `InnerDev` with `SpinNoIrq`, making the
  IRQ callback mutually exclusive with normal paths.
- The vsock adapter protects `VirtIOSocket` with `SpinNoIrq` and its TX packet
  free/in-flight tables with `SpinNoPreempt`. Packet slices come from a
  per-device fixed pool whose slots are exclusively owned by one `VsockTxBuf`
  handle at a time; the pool storage is never resized after construction.

## Threat Analysis

| ID | Threat | Impact | Trigger | Mitigation |
| --- | --- | --- | --- | --- |
| T-01 | `identify_mmio_device` / `open_mmio_transport` access invalid memory because the caller supplied a bad MMIO address | High | The caller supplies an unmapped or incorrect physical address | Platform initialization validates the address; identification checks alignment, minimum size and magic before reading; `MmioTransport::new` validates magic, device id and version. |
| T-02 | A network buffer token mismatch makes the device DMA into the wrong memory | High | `rx_buffers` or `tx_buffers` indices do not match VirtIO queue tokens | `try_new()` checks initial mappings with `assert_eq!(token, i as u16)`; `recycle_rx` and `recycle_tx` verify `is_some()`. |
| T-03 | `NetBuf::from_handle` receives an invalid or duplicate handle | High | The network stack reuses a consumed handle | The caller guarantees handle uniqueness; `from_handle` is unsafe and transfers the proof obligation to the caller. |
| T-04 | Scanout-resource backing dangles | High | Backing is released before `destroy_scanout_resource` | DRM/fbdev retain the backing through `PageRef` from attachment through destruction; destruction failures are logged. |
| T-05 | IRQ context and normal paths concurrently access a network device | Medium | An IRQ callback runs concurrently with `send` or `recv` | `InnerDev` is protected by `SpinNoIrq`; every access is mutually exclusive. |
| T-06 | The device is used after IRQ registration fails | Medium | `register_virtio_net_irq` returns `ResourceBusy` | `try_new()` propagates the error and does not create the device. |
| T-07 | `free_tx_bufs` is exhausted and transmission fails | Low | All TX buffers are in flight | `alloc_tx_buf` returns `DriverError::WouldBlock`; the upper layer waits for `recycle_tx` and retries. |
| T-08 | A rejected MMIO candidate is reset or otherwise modified during identification | Medium | Every enabled MMIO driver probes each `virtio,mmio` slot, including slots owned by another driver | `identify_mmio_device` uses `read_mmio_header`, which performs no write and constructs no transport; `open_mmio_transport` (whose `Drop` resets the device) runs only after an exact id match. |
| T-09 | A malicious VMM returns an oversized `pkt_len` through a VirtIO device | High | `receive_complete` returns a packet length larger than the buffer capacity | `recv()` checks `hdr_len + pkt_len <= capacity` before writing `NetBuf` metadata and returns `DriverError::InvalidInput` on overflow; `NetBuf::set_payload_len` has a development-time `debug_assert`. |
| T-10 | A malicious VMM returns a forged 9p response | Medium | Data returned by `request()` violates the 9p protocol | The upper 9p filesystem validates response format; this module does not parse response content. |
| T-11 | A malicious VMM returns an oversized `mount_tag` | Medium | `VirtIO9p::mount_tag()` returns an overlong string | `virtio-drivers` reads it from configuration space, whose field length is bounded by the device configuration contract. |
| T-12 | The block layer does not enforce VirtIO read-only capability | Medium | The VMM negotiates `VIRTIO_BLK_F_RO`, but the block core treats the disk as writable | `VirtIoBlkDev` propagates inherent read-only capability to the block core and rejects writes again at the lower write entry. |
| T-13 | The input handler claims a shared IRQ belonging to another device | Medium | The shared line fires while the input device's ISR is empty | The handler validates the configured IRQ, acknowledges only this device's ISR, and returns `NOT_HANDLED` when the status is empty. |
| T-14 | A vsock TX completion reports a different descriptor chain than the driver submitted | High | A malicious or broken device corrupts the used-ring id | `add_notify_wait_pop_checked` reports `WrongToken` with the published token; VSOCK resets the device, detaches the TX token with its original buffers, detaches all RX buffers, and poisons the socket so later queue operations return `IoError`; `Drop` cannot unshare them twice. |
| T-15 | A DMA mapping or ring allocation leaks during destroy, construction failure or partial RX publication | Medium | An error path returns while a published buffer or ring is still owned | The device owner resets before releasing queues; `OwningQueue::fill` states the reset-before-drop obligation; detach clears slots so `Drop` is idempotent; tests compare share/unshare, alloc/dealloc and provider/bounce baselines. |

## Failure Modes And Effects Analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Device initialization fails | VirtIO feature negotiation or DMA allocation fails | Device unavailable | The corresponding subsystem cannot use the device | 2 | `try_new()` returns an error; the upper layer may select another device or degrade. |
| F-02 | Block read or write fails | The device returns an I/O error | One I/O operation fails | The filesystem may become read-only | 3 | Return `DriverError::Io`; the filesystem handles the error. |
| F-03 | Network packet loss | The VirtIO queue is full or buffers are unavailable | Packet dropped | Reduced network quality | 4 | Return `WouldBlock` or `NoMemory`; the protocol stack retransmits. |
| F-04 | Network IRQ is not serviced | IRQ registration is absent or hardware fails | Data is not received promptly | Network latency increases or connectivity is lost | 2 | Polling mode acts as fallback; interrupt state is logged. |
| F-05 | GPU framebuffer refresh fails | Device flush returns an error | Display content is not updated | Display failure | 3 | Return `DriverError`; the upper layer may retry. |
| F-06 | Vsock connection is lost | Peer close or transport error | Connection disappears | Vsock communication stops | 3 | `poll_event` returns `Disconnected`; the upper layer reconnects. |
| F-07 | 9p request times out | The device does not respond | File operation hangs | Filesystem unavailable | 2 | No timeout exists in this layer; timeout handling depends on the upper layer. |
| F-08 | MMIO identification fails | Region smaller than `0x100`, misaligned base, bad magic or unsupported version | The candidate returns `Io`/`InvalidInput`; no transport is built and the device is not reset | The device stays unbound; other candidates are unaffected | 3 | `read_mmio_header` reports `MmioRegionTooSmall`, `Misaligned`, `BadMagic` or `UnsupportedVersion`; the caller logs and continues. |
| F-09 | Network-buffer token mismatch | Internal state inconsistency | DMA targets the wrong address | Memory corruption | 1 | Use `assert_eq!` and `is_some()` checks; return `BadState` where recoverable. |
| F-10 | Network RX event is lost | Both IRQ and task context call `ack_interrupt()`, and task context clears a newly arrived event | VirtIO RX stalls and the unhandled IRQ retriggers | TCP receive window drops to zero and host-to-guest traffic times out | 2 | When an IRQ is registered, only the handler calls `ack_interrupt()`; `recv()` consumes completed queue entries. Polling mode acknowledges from `recv()`. |
| F-11 | A write is issued to an inherently read-only block device | An upper layer bypasses or has not yet established the block-device view | The write is rejected | The filesystem remains read-only and media is unmodified | 3 | Both polling and IRQ `write_block()` paths return `DriverError::ReadOnly` before submission. |
| F-12 | Input IRQ registration fails | IRQ is absent or invalid, or the shared-action limit is reached | The input device cannot wake readers | Keyboard or mouse events are not published | 3 | Host activation propagates registration failure; VirtIO input provides no polling fallback. |
| F-13 | Vsock TX packet pool exhausted | More data packets are in flight than the fixed pool can hold | `alloc_tx_buf()` returns `WouldBlock` until completion reclamation frees a slot; control packets are enqueued in `pending_controls` instead of being dropped | The socket layer applies backpressure or waits; control operations never lose their response | 3 | One slot and descriptor are reserved for control packets; `recycle_tx()` flushes pending controls and returns completed slots to the free list, waking TX waiters. |

## Failure Management

- **Error codes:** All device operations map `virtio_drivers::Error` through
  `as_driver_error()` into `DriverError`, giving upper layers one error model.
- **Panic policy:**
  - `identify_mmio_device` and `open_mmio_transport` do not panic; invalid
    regions and headers are returned as `MmioError`.
  - `try_new()` panics when `receive_begin` returns an unexpected token,
    indicating an internal consistency failure.
  - The IRQ block path panics on unknown or out-of-range tokens, unproven
    descriptor reclamation, and `NotReady` completion outcomes (see IRQ Block
    Request Safety). Other paths propagate errors through `DriverResult`.
  - Some other paths, for example in socket `poll_event`, also contain `expect`
    calls.
- **Recovery:**
  - Network devices can continue after `recycle_tx` or `recycle_rx` reclaims
    buffers.
  - Other devices may retry after a single operation fails; `virtio-drivers`
    owns their device state.
- **Input-name sanitization:** `VirtIoInputDev::try_new` applies
  `trim_end_matches('\0')` before storing a device-provided name. This removes
  VirtIO configuration padding so trailing NUL bytes cannot leak into class
  metadata, sysfs attributes, or uevent properties.

## Privacy Analysis

This module does not process user data directly. VirtIO devices are transport
channels for block I/O, network packets, display frames, input events, vsock
data, and 9p file requests; the upper subsystems own data-privacy policy. The
VirtIO channel between the device and VMM must be protected by the VMM's
isolation mechanisms against observation by another virtual machine.

## Known Limitations

1. **MSI-X release scope:** x86_64 network devices use MSI-X where available;
   other classes retain legacy IRQs. A failed configuration retains its
   disabled allocation. Explicit release retains that owner on rejection and
   permits retry only after the caller satisfies source-stop and action-drain
   obligations. Terminal Drop and rollback use the infallible provider contract
   and fail-stop rather than silently discarding a rejected allocation. They do
   not provide recoverable hot-unplug or automatic retry. Configuration
   masking is not evidence of DMA or posted-message completion.
2. **No timeout:** The 9p device's `request()` waits synchronously for a
   response and has no timeout. A malicious or broken device can block the
   caller indefinitely.
3. **Hard-coded input physical location:** `physical_location()` returns the
   fixed string `"virtio0/input0"` and does not reflect the actual device
   topology.
4. **Reset has no timeout:** `Transport::reset` busy-polls the status readback.
   The vsock TX path may hold `SpinNoIrq`, so a non-responsive device can cause
   an unbounded IRQs-disabled wait. The teardown contract does not claim to
   solve availability or to recover from such a device.
5. **Reset is device-side evidence only:** a status readback that observes the
   empty status does not prove that host IRQ callbacks already in flight have
   returned. VSOCK is polled, registers no IRQ callback and suppresses INTx
   during probe, so it has no host callback to synchronize. IRQ-driven
   adapters must establish host-side quiescence themselves.
6. **Other drivers still use the unchecked wait:** `add_notify_wait_pop`
   retains its previous busy-wait/no-timeout behavior and reports post-
   publication errors without a token. This change converts only the vsock TX
   path to the checked API; a wrong token in another caller can leave a mapping
   unreleased until that caller resets and tears the queue down. Treating those
   consumers as fixed would be inaccurate.
7. **Publication is an unsafe contract, not a type invariant:** `fill`,
   `add_notify_wait_pop_checked` and the detach APIs rely on the owner resetting
   before drop. The type system cannot enforce it; VSOCK and sound uphold it.
8. **Test evidence is software-only:** the counting HAL, recording transport
   and mock MMIO transport verify ordering, counts and failure injection. They
   do not prove hardware cache maintenance, IOMMU behavior, or that a real
   device stops DMA on reset.
9. **Provider counters are process-wide:** the `kdma` test snapshots read
   global allocator state. Scenarios which compare them run as `serial` tests,
   compare only the fields they own, and tolerate the one-time bounce-pool
   initialization region.

## Audit Checklist

When modifying this crate, verify:

- [ ] Every `unsafe` block has a `SAFETY:` comment.
- [ ] New state transitions match the state machines in `design.md`.
- [ ] New atomic operations use orderings at least as strong as existing
  requirements.
- [ ] Drop handling covers every state. Network devices implement custom Drop;
  vsock and sound rely on the vendor owner's `queue_unset` → `reset` →
  detach/unshare/free order; other devices rely on the dependency's Drop
  behavior.
- [ ] Every new panic path has a corresponding panic guard or equivalent
  protection.
- [ ] The input IRQ handler acknowledges only this device's nonempty ISR and
  never consumes events in hard-IRQ context.

## IRQ-Driven RNG Safety Boundary

`rng.rs::RngRequest` confines unsafe submission and completion to an exclusively
borrowed request. The original Box address, length, and live token are preserved
until completion; `Pending` forbids CPU consumption and duplicate submission.
Queue and buffer are retained together if Drop observes `Pending`. Successful
completion validates `1..=256` before making bytes readable. A malformed token
or terminal error cannot release storage still owned by DMA.

The unsafe `Send` implementation requires HAL DMA/MMIO virtual mappings that
remain valid across CPUs and threads, stable backing allocations, and `Send`
HAL and transport types. Shared access is separately serialized by the device's
`SpinNoIrq`; the request itself does not implement `Sync`. The host must
preserve transport mappings and IRQ resources for the published device
lifetime.

The IRQ callback holds a `Weak` reference and upgrades it only for the call.
IRQ registration precedes enabling interrupts; registration failure drops an
idle request. Notification runs after releasing the request lock. Host wait
errors do not imply a device fault or revoke live DMA. `is_failed` reports only
the sticky device fault. Empty used rings (`NotReady`) retain `Pending` without
poisoning the source; mismatched tokens remain terminal and cannot justify
freeing or resubmitting storage whose DMA ownership is unresolved.

The VMM controls bytes, completion length, and timing. Length validation
protects CPU copies but cannot establish entropy quality or force device
progress. There is no completion timeout and no reset or hotplug protocol. With
the X-Kernel HAL, reclaim competes for the global kdma lock, which is also used
by other DMA operations and page-table changes. Lock-wait latency in hard-IRQ
context is an accepted limitation with no latency guarantee from this driver.

`DeviceDmaBackend` is an `unsafe` bridge for exactly that reason: it cannot
prove that the supplied capability is legal under the RNG request lock and in
hard-IRQ context, that it is exclusive to this device, or that map/unmap
preserves original bytes. The constructor documents those caller obligations;
the adapter itself only enforces exact release pairing and interval or overflow
checks. Queue-ring memory has no per-descriptor cache-maintenance hook, so the
capability must provide coherent ring access, not one-time flushing. A duplicate
coherent record is returned through `free_coherent` rather than dropped,
because the provider's `DmaAllocation` token has no destructor.

## Conservative Input Readiness

Input-notification probing never pops or requeues descriptors, because HAL
streaming DMA mapping can allocate. The IRQ handler exclusively acknowledges the
transport; the timer-side probe only reads the atomic readiness hint. Pending
input is consumed only through normal `read_event`. False-positive wakes are
harmless because callers drain and recheck.
