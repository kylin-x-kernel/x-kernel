# kclass — Design

## Purpose

`kclass` is x-kernel's typed runtime device class layer. Built on top of
the `kdevice` device core, it provides type-safe publication, enumeration,
lookup, and availability-subscription interfaces for each device category
(net / block / char / hwrng / display / input / vsock / 9p).

After a successful probe, a driver publishes its runtime capability into
the matching class registry through `publish_<class>()`; subsystems (such
as `knet`, `fs_boot`, or the input subsystem) discover and use runtime
devices through `*_devices()` / `find_*_device()` /
`subscribe_*_available()`, without depending on probe order.

The intended audience is developers implementing device class adapters,
modifying class registry semantics, or adding new device categories.

## Out of Scope

- No framebuffer management: `/dev/fb0` emulation lives in `fbdevice`;
  kclass only delegates `DisplayDevice` calls (see the framebuffer path
  below).
- No IRQ handling: interrupt registration and dispatch belong to `kirq`
  and `device-res-xkernel`; kclass stores only the `irq` metadata value.
- No driver binding or probing: the discover → match → bind pipeline is
  owned by `kdevice` and `kdriver`; kclass consumes the already-bound
  device object at publish time.
- No device lifecycle or node-allocation ownership: hotplug state transitions,
  class indices, major/minor numbers, and `devname` templates belong to
  `kdevice`. kclass only supplies class-specific metadata at activation.
- No runtime dispatch policy between consumers: after publication the
  consumer holds `ClassDevice<T>` directly and kclass steps out of the
  call path.

## Background

The Linux kernel class mechanism (`/sys/class/`) provides a view of
devices organized by functional category, orthogonal to bus topology
(`/sys/bus/`). `kclass` fills the same role in x-kernel:

- **kdevice** owns bus attachment, lifecycle state, and driver binding;
- **kclass** adds the typed per-category view
  (net/block/char/hwrng/display/input/vsock/9p) on top;
- **kdriver** calls `publish_*()` after a successful probe to inject the
  runtime capability into the kclass registries.

This layering lets upper subsystems (network stack, filesystem) stay
agnostic of both the bus (PCI vs platform) and the probe order — they only
query the class registry for currently available devices or subscribe to
future availability notifications.

## Scope

```text
drivers/contracts/kclass/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs       # macro-driven class registries + device trait delegation + prelude
    └── generic.rs   # ClassDevice<T>, ClassRegistry<T> generic primitives
```

## Architecture

```text
                kdriver (probe success)
                     │
                     │ publish_net / publish_block / publish_display / ...
                     v
    ┌────────────────────────────────────────┐
    │ kclass                                  │
    │                                         │
    │  ┌─────────────────────────────────┐    │
    │  │     ACTIVATION_BRIDGE           │    │
    │  │  (kdevice event subscriber)     │    │
    │  │  Activated → notify_class_avail │    │
    │  │              + register node     │    │
    │  │  Removed   → remove_class_device│    │
    │  │              + unregister node   │    │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  Per-class registries           │    │
    │  │  (macro-generated)              │    │
    │  │                                 │    │
    │  │  NET_DEVICES    (SpinNoPreempt) │    │
    │  │  BLOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  CHAR_DEVICES   (SpinNoPreempt) │    │
    │  │  DISPLAY_DEVICES(SpinNoPreempt) │    │
    │  │  INPUT_DEVICES  (SpinNoPreempt) │    │
    │  │  VSOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  VIRTIO_9P_DEVICES(SpinNoPreempt)│   │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  ClassDevice<T>                 │    │
    │  │   Arc<ClassDeviceInner<T>>      │    │
    │  │   ├─ parent: Arc<DeviceObject>  │    │
    │  │   ├─ runtime: T (Box<dyn Trait>)│    │
    │  │   ├─ name, device_kind, irq     │    │
    │  │   └─ metadata (input identity)  │    │
    │  └─────────────────────────────────┘    │
    │                                         │
    │  Delegation impls:                      │
    │   ClassDevice<NI> : NetDevice           │
    │   ClassDevice<CI> : CharDevice          │
    │   ClassDevice<DI> : DisplayDevice       │
    │   ClassDevice<II> : InputDevice         │
    │   ClassDevice<VI> : VsockDevice         │
    │   ClassDevice<9I> : Virtio9pDevice      │
    └──────────────────┬─────────────────────┘
                       │
                       │ query / subscribe
                       v
         knet / fs_boot / input subsystem / ...
```

| Component | Responsibility |
|------|------|
| `ClassDevice<T>` | Typed runtime device handle; wraps `DeviceObject` + trait object and delegates trait method calls |
| `ClassDeviceInner<T>` | `ClassDevice` internal shared state: parent, runtime, name, kind, irq, metadata |
| `ClassRegistry<T>` | Typed registry: publish (unique publication), devices (enumerate available), find (lookup by id), subscribe (availability notification), remove (remove by id) |
| `ACTIVATION_BRIDGE` | Global event bridge: subscribes to `kdevice` `Activated` / `Removed` events and drives class-level notify/remove |
| `ClassRuntimeMetadata::device_node_class` | Supplies input capability metadata or the DRM primary class to `kdevice`; other runtime classes return no node request |
| `class_registries!` macro | Declaratively generates all 8 class registries and their companion functions |
| `ClassDeviceMetadata` | Optional class-specific metadata (currently only the input class carries physical_location / unique_id) |
| `prelude` module | Centralized re-export of all public types for publishers such as `kdriver` |
| Trait delegation impls | Implement the operation traits (e.g. `NetDevice`) for categories consumed through class handles, delegating to the inner runtime; the net class also forwards `NetRxScheduler` attach/detach, while block I/O goes only through the block core canonical `BlockDevice` |

## State Machine

### ClassDevice lifecycle

```text
                    driver probe_device()
                         │
                         │ publish_<class>(parent, runtime)
                         v
                    ┌──────────┐
                    │Published │  ← ClassDevice created, held by registry;
                    │(pending) │     parent.state != Active
                    └────┬─────┘
                         │ kdevice Activated event
                         ▼
                    ┌──────────┐
                    │Available │  ← parent.state == Active;
                    │(active)  │     DeviceNode assigned, then callbacks fired
                    └────┬─────┘
                         │ kdevice Removed event
                         ▼
                    ┌──────────┐
                    │ Removed  │  ← swap_remove from the registry
                    └──────────┘
```

| From | To | Trigger |
|----|----|----------|
| — | Published | Driver calls `publish_<class>(parent, runtime)` |
| Published | Available | `kdevice` dispatches `Activated`; `notify_class_available` registers any class node and then invokes subscriber callbacks |
| Published | Available (immediate) | `parent.state()` is already `Active` at publish time (desc-adoption path) |
| Available | Removed | `kdevice` dispatches the `Removed` event; `remove_class_device` removes the entry |
| Published | — | Publishing the same `DeviceId` again returns `AlreadyExists` |

### Registry publish semantics

`ClassRegistry::publish` rejects duplicate identity, as Linux device
registration does. Hotplug re-registration must first remove the old
object through the `Removed` lifecycle and then publish a new object; a
resident runtime is never silently replaced.

## Flows

### Publish flow

1. The driver creates the runtime device inside `DeviceDriver::probe_device`
   (e.g. `VirtIoNet::try_new`).
2. It calls `publish_<class>(parent, runtime)` (macro-generated).
3. `name()`, `device_kind()`, and `irq()` are extracted from the runtime.
4. The `device_kind` is checked against the registry type;
   a mismatch returns `InvalidInput`.
5. `class_metadata()` is extracted from the runtime (only the input class
   returns meaningful metadata).
6. `ClassDevice::try_new_with_class_metadata` is constructed:
   - the parent must have `driver_name()` and `driver_id()` (i.e. a bound
     driver), otherwise `BadState` is returned.
7. The registry publishes under the `SpinNoPreempt` lock:
   - if the same id already exists → `AlreadyExists`, and the
     class-specific publish already performed is undone;
   - otherwise → push.
8. If the device is already `Active`, `notify_class_available` runs
   synchronously. Input and display classes first register a `DeviceNode` in
   `kdevice`, then notify class subscribers.

### Device enumeration and lookup

1. `*_devices()`: take the registry lock, iterate all entries, filter by
   `is_available()` (parent.state == Active), and return clones.
2. `find_*_device(id)`: take the registry lock and find the entry matching
   the id and `is_available()`.
3. Both operations return clones of `ClassDevice<T>` (`Arc`-shared), so
   callers can safely use them across the lock boundary.

### Availability subscription

1. A subsystem calls `subscribe_<class>_available(callback)` during init.
2. The callback is stored as
   `ClassAvailabilityCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>`.
3. When a device becomes `Active`, `notify_class_available` invokes all
   node and class-subscriber callbacks outside the class-registry lock.
4. A subscriber may use the device immediately or cache the reference
   inside the callback.
5. Callbacks run outside the lock so reentrancy inside subscriber code
   cannot deadlock the registry.

### Event bridge

`ACTIVATION_BRIDGE` is lazily initialized on first class-registry access
(`ensure_event_bridge`):

1. A `DeviceEventKind::Activated` subscriber is registered: on each event
   it calls `notify_class_available(kind, id)`, dispatching on
   `DeviceKind` to the matching class notify function. Input and display
   notify functions capture their class metadata and call
   `kdevice::register_device_node` before availability callbacks.
2. A `DeviceEventKind::Removed` subscriber is registered: on each event it
   calls `remove_class_device(id)`, which walks every class registry, removes
   the id, unregisters its optional `DeviceNode`, and then runs class-specific
   unpublish cleanup.
3. The bridge completes inside `LazyInit::call_once`, guaranteeing a
   single registration.

### Device trait delegation

`ClassDevice<T>` implements the operation trait for categories whose
consumers hold class handles, delegating to the inner runtime. The block
class is the lifecycle publication entry point; I/O consumers obtain the
canonical `BlockDevice` from the block core, so the full set of block
operations is deliberately not re-implemented for
`ClassDevice<BlockDeviceImpl>`.
`BlockDeviceImpl` uses `block::GendiskRef` to keep the disk alive through
canonical part0. Class publication and removal only register or release that
owning reference; device references keep the complete disk alive after class
removal.

```rust
impl NetDevice for ClassDevice<NetDeviceImpl> {
    fn can_tx(&self) -> bool {
        self.with(|device| device.can_tx())
    }
    // ...
}
```

The `with()` method borrows the runtime shared through
`&self.inner.runtime` without taking a lock. Concurrency inside the
runtime is the driver's own responsibility (usually interior mutability).

### Display device framebuffer path

The `DisplayDevice` trait exposes only the resolution
(`DisplayInfo { width, height }`) and the scanout resource interface
(`create_scanout_resource` / `destroy_scanout_resource` /
`present_scanout_resource`). The kclass class adapter is a pure
delegation for those methods and holds no framebuffer raw pointers or
direct memory mappings.

The `/dev/fb0` framebuffer compatibility layer lives in the `fbdevice`
crate: at `fb_init` it allocates a shadow buffer against the primary
display device, binds it as a host-visible 2D resource through
`create_scanout_resource`, and pushes it to the scanout on demand via
`fb_present` (no background refresh task: a continuous
`present_scanout_resource` would race a DRM compositor for the single
physical scanout and cause flicker, so only `/dev/fb0` writes and the
`FBIOPAN_DISPLAY` ioctl trigger a present). This "fbdev emulation over
scanout" model applies uniformly to any `DisplayDevice`, so drivers never
need to expose a directly-mapped framebuffer, and kclass needs no
framebuffer special-casing or unsafe boundary.

## Hardware entropy class

The `hwrng` feature adds `HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>`
and the standard class publication and discovery APIs.
`ClassDevice<HwrngDeviceImpl>` forwards `read` and the terminal `is_failed`
query without holding the registry lock. Only the read operation may sleep. The host must prepare completion delivery
before publication; kclass does not implement waiting or entropy trust policy.

## Concurrency Model

- Each class registry is protected by `SpinNoPreempt<ClassRegistry<T>>`:
  publish / devices / find / subscribe / remove complete inside the lock.
- `notify_class_available` uses the "find device inside the lock, call
  callbacks outside" pattern:
  - find the device and clone the subscriber list under the lock;
  - walk subscribers and run callbacks outside the lock.
  This prevents reentrancy inside subscriber callbacks from deadlocking
  the registry.
- `ACTIVATION_BRIDGE` initialization is thread-safe through `LazyInit`.
- `ClassDevice<T>` is shared via `Arc`; `Clone` only bumps the reference
  count.
- Runtime (trait object) concurrency safety is guaranteed by the
  `Send + Sync` bounds of `Box<dyn Trait + Send + Sync>` plus each
  driver's interior locking.

## Design Decisions

### Macro-driven instead of hand-written per class

**Choice**: generate all code for the 8 device classes with the
`class_registries!` macro.

**Trade-off**: macro debugging is harder, in exchange for:

- identical publish/devices/find/subscribe/notify/remove logic per class;
  writing it 8 times by hand would duplicate heavily;
- adding a new class is one line at the `class_registries!` call site,
  with no repeated boilerplate;
- the expanded code is exactly what a hand-written version would be — no
  runtime overhead.

**Rejected alternative**: hand-writing each class in full. High
duplication, and new classes tend to miss steps (notify or
ensure_event_bridge calls); the macro enforces consistency.

### Device trait delegation instead of exposing the trait object

**Choice**: implement each operation trait for `ClassDevice<T>`,
delegating through `with()`.

**Trade-off**: each class needs 5-10 delegating methods, in exchange for:

- upper subsystems use `ClassDevice<NetDeviceImpl>` exactly as they would
  use `&dyn NetDevice`;
- `ClassDevice` can insert common logic around delegated calls (stats,
  logging, permission checks) without touching driver implementations;
- `DeviceObject` lifetime management is encapsulated; consumers never
  inspect device state.

**Rejected alternative**: expose `fn runtime(&self) -> &T` and let callers
operate directly. That simplifies delegation code but breaks
encapsulation — callers could bypass device state checks.

### Lazily initialized event bridge

**Choice**: register event listeners on first class-registry access via
`LazyInit` + `ensure_event_bridge()`.

**Trade-off**: a tiny one-time initialization cost on first access, in
exchange for:

- no explicit kclass init call inside `init_drivers`;
- kclass consumers need no init-order knowledge — only that kdevice is
  initialized;
- if no class feature is enabled in the build, the bridge is never
  registered.

**Rejected alternative**: registering events from a global constructor
(e.g. a dummy `LazyInit`). More "proactive", but it creates a hidden
init-order dependency — kdevice would have to be initialized whenever
kclass constructs.

### Input class carries runtime identity metadata

**Choice**: only the input class carries `physical_location` and
`unique_id` through `ClassDeviceMetadata`.

**Trade-off**: `ClassDeviceMetadata` is empty most of the time, in
exchange for:

- input subsystem device matching (evdev device identity) needs both
  fields;
- the `ClassRuntimeMetadata` trait's default implementation returns empty
  metadata, so other classes work with zero extra code;
- future classes can carry metadata by overriding `class_metadata()`.

**Rejected alternative**: adding `physical_location` / `unique_id` to the
`Device` trait. That would pollute the generic device abstraction with
input-specific concepts.

### Arc<DeviceObject> prevents use-after-remove

**Choice**: `ClassDevice` holds a strong `Arc<DeviceObject>`.

**Trade-off**: clones of a `ClassDevice` held outside the registry still
reach the trait methods after the device is removed from the registry.
This is necessary for:

- pollers / async tasks holding a device reference that must finish a last
  I/O after hot removal;
- references obtained inside subscriber callbacks that may outlive the
  removal.

**Rejected alternative**: `ClassDevice` holding `Weak<DeviceObject>`. That
would free the `DeviceObject` immediately on removal but requires
upgrading the weak pointer before every `with()` call, adding a failure
path (device already freed).

## Drop / Resource Release

- Dropping a `ClassDevice` only decrements the `Arc` count; it performs no
  device operations.
- Registry removal is a `Vec::swap_remove` of the `ClassDevice` entry.
- When the last `ClassDevice` and `DeviceObject` references are released,
  the `DeviceObject` devres (MMIO mappings, IRQs, DMA buffers) is cleaned
  up in LIFO order.
- The runtime trait object is dropped by its `Box<T>` when
  `ClassDeviceInner` drops.
