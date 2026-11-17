# kclass — Design

## Purpose

`kclass` is x-kernel's typed runtime device class layer, located in
`drivers/base/`. Built on top of the `kdevice` device core, it provides
type-safe publication, enumeration, lookup, and availability-subscription
interfaces for each device category (net / block / char / hwrng / display /
input / vsock / 9p). It lives in `drivers/base/` because it owns the typed
class registries and the activation bridge from `kdevice`, not only the class
trait declarations.

The `virtio-9p` feature still depends on and re-exports the concrete
`virtio::Virtio9pDevice`. That is known coupling debt (see
`drivers/base/README.md`); the directory move did not change it.

After a successful probe, a driver publishes its runtime capability into
the matching class registry through `publish_<class>()`; subsystems (such
as `knet`, `fs_boot`, or the input subsystem) discover and use runtime
devices through `*_devices()` / `find_*_publication()` /
`*_publications_for_device()` / `subscribe_*_available()`, without depending on
probe order. The deprecated single-value `find_*_device(DeviceId)` lookup is
gone because one stable device/binding may publish several capabilities of the
same class.

Publication is binding-scoped and has its own `PublicationId`. The parent
`DeviceObject` is stable across rebinds, but each publication captures the
exact `BindingRef` that published it. `publish_*()` returns a non-Clone
`ClassPublication<T>` credential; `ClassDevice<T>` is the cloneable consumer
handle. Dropping the credential does not revoke; a publisher that needs an
explicit lifetime must retain it and call `revoke(&self)`.

Publication state is `Published -> Activating -> Notifying -> Available`, with
`Revoking`, retryable `RevokeFailed`, and terminal `Revoked`. `is_available()`
is false as soon as a revoke request is accepted, even while activation or
callback cleanup is still pending. A revoke during `Activating`/`Notifying`
sets `revoke_requested` and returns `Deferred`; the activation executor owns
the handoff. `Revoked` is set only after backend unpublish and removed
callbacks complete. Node removal, unpublish and callbacks never run under the
class registry spinlock.

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
drivers/base/kclass/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
├── ut-tests/        # publication/revoke tests mounted as cfg(unittest)
└── src/
    ├── lib.rs       # macro-driven class registries + event bridge + delegation + prelude
    ├── generic.rs   # ClassPublication<T>, ClassDevice<T>, publication state/registry
    └── retirement_hooks.rs # cfg(unittest) publication pause seams
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
    │  │  Removed / BindingRetired       │    │
    │  │            → remove_class_device│    │
    │  │              + retire by binding │    │
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
    │  │  HWRNG_DEVICES  (SpinNoPreempt) │    │
    │  │  VSOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  VIRTIO_9P_DEVICES(SpinNoPreempt)│   │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  ClassPublication<T>            │    │
    │  │   └─ ClassDevice<T>             │    │
    │  │      Arc<ClassDeviceInner<T>>   │    │
    │  │   ├─ parent: Arc<DeviceObject>  │    │
    │  │   ├─ binding: BindingRef        │    │
    │  │   ├─ runtime: T (Box<dyn Trait>)│    │
    │  │   ├─ name, device_kind, irq     │    │
    │  │   └─ metadata (input identity)  │    │
    │  └─────────────────────────────────┘    │
    │                                         │
    │  Delegation impls:                      │
    │   ClassDevice<NI> : NetDevice           │
    │   ClassDevice<CI> : CharDevice          │
    │   ClassDevice<HI> : HwrngDevice         │
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
| `ClassPublication<T>` | Non-Clone publisher credential; owns explicit `revoke(&self)` and wraps the consumer handle. Drop does not revoke |
| `ClassDevice<T>` | Cloneable typed consumer handle; wraps `DeviceObject`, the captured binding reference, `PublicationId`, runtime and publication state; delegates trait calls |
| `ClassDeviceInner<T>` | Shared state: parent, captured `BindingRef`, immutable `PublicationId`, atomic state/`revoke_requested`, runtime, name, kind, irq, metadata |
| `ClassRegistry<T>` | Typed registry: publish exact publications, enumerate available devices, lookup by `PublicationId`, list by `DeviceId`, subscribe to availability/removal, exact revocation orchestration |
| `RevocationOutcome` | `Completed` (unpublish and removed callbacks done), `Deferred` (a live activation/revocation executor owns completion), `AlreadyRevoked` |
| `ACTIVATION_BRIDGE` | Global event bridge: subscribes to `kdevice` `Activated` / `Removed` / `BindingRetired` events and drives class-level notify/remove; removable for tests via `reinstall_event_bridge_for_tests` |
| `ClassPublication::revoke` | Exact `PublicationId` request; `Deferred` means a live activation/revoke executor owns cleanup, not that the publication remains available |
| `ClassRuntimeMetadata::device_node_class` | Supplies input capability metadata or the DRM primary class to `kdevice`; other runtime classes return no node request |
| `class_registries!` macro | Declaratively generates all 8 class registries and their companion functions |
| `ClassDeviceMetadata` | Optional class-specific metadata (currently only the input class carries physical_location / unique_id) |
| `prelude` module | Centralized re-export of all public types for publishers such as `kdriver` |
| Trait delegation impls | Implement the operation traits (e.g. `NetDevice`) for categories consumed through class handles, delegating to the inner runtime; the net class also forwards `NetRxScheduler` attach/detach, while block I/O goes only through the block core canonical `BlockDevice` |

## State Machine

### Publication lifecycle

```text
Published --Activated--> Activating --callback commit--> Notifying --done--> Available
    |                         |                              |
    | revoke                  | revoke request               | revoke request
    v                         v                              v
 Revoking <----------- Activating/Notifying (activation owns handoff)
 Revoking --node NotOwner/unrecoverable--> RevokeFailed --retry--> Revoking
 Revoking --unpublish + removed callbacks--> Revoked
```

- `Published` is registry-visible only for internal activation; `is_available()`
  is false.
- `Activating`/`Notifying` are activated but callback delivery is not final.
  A revoke request stores `revoke_requested` with Release ordering and closes
  `is_available()` immediately; activation owns node cleanup/unpublish.
- `Available` is set only after callbacks complete. It is capability
  availability, not proof that node projection succeeded; a node registration
  failure is logged and leaves the publication available with no node.
- `Revoking` keeps the entry hidden while unpublish/removed callbacks run.
  A second revoke returns `Deferred`, never a premature `AlreadyRevoked`.
- `RevokeFailed` means the executor exited and nobody owns completion. It is
  not available and never resurrects; a later revoke/binding retirement may
  CAS it back to `Revoking`.
- Node removal, `unpublish_class`, callbacks and last-Arc drop never run under
  the class registry spinlock.

### Captured binding reference

`ClassDeviceInner` captures a `kdevice::BindingRef` at construction:

- driver identity is the immutable historical identity of the binding that
  created this class device; after unbind/removal it stays readable and never
  falls back to the parent's current metadata;
- `is_available()` observes only that captured runtime. It is a lock-free,
  instantaneous availability check, not operation admission. If the binding is
  replaced by a rebind, an old class handle keeps reporting unavailable and can
  never start following the replacement;
- the reference retains the runtime but does not increment `usage`, so a
  resident class handle or clone cannot make removal return `ResourceBusy`;
- construction succeeds for a live binding (Bound stage used by in-probe
  publication, or Active); terminal or detached bindings return `BadState`;
- capture is not a publication permission. Publication serialization with
  removal still comes from the descriptor binding lock, and removal is matched
  back to the entry with `BindingRef::same_binding`, never by device ID alone.

### Device enumeration and lookup

1. `*_devices()`: lock the registry, iterate entries, filter by
   `is_available()` and return `ClassDevice<T>` clones.
2. `find_*_publication(PublicationId)`: exact lookup, filtered by
   `is_available()`; a revoke-requested publication is immediately absent.
3. `*_publications_for_device(DeviceId)`: public diagnostic enumeration of
   currently available publications for a stable device; unavailable entries
   are hidden. Internal activation/retirement scans use the private
   `all_publications_for_device` snapshot.
4. All operations return clones of `ClassDevice<T>` (`Arc`-shared); the
   non-Clone `ClassPublication<T>` is never handed out by a query.

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
   `kdevice::register_device_node_for_publication` for the entry's exact
   `PublicationId` and captured binding before availability callbacks.
2. `DeviceEventKind::Removed` and `DeviceEventKind::BindingRetired`
   subscribers are registered: on each event they call
   `remove_class_device(id)`, which walks every class registry and retires
   terminal entries whose captured binding is the instance being removed.
   Unbinding (which keeps the stable device object) and retirement of a failed
   candidate that already published a runtime both use this path; the event
   name does not select an entry, the exact binding reference does.
3. The bridge completes inside `LazyInit::call_once`, guaranteeing a
   single registration. `reinstall_event_bridge_for_tests` restores the
   subscriptions after a test reset clears the subscriber buckets.

### Exact publication retirement

Retirement never removes a publication with a live binding and never lets a
replacement binding inherit an old one.

1. Binding retirement scans the internal all-state snapshot for entries whose
   captured binding is `Removed`; the public device-level enumeration only
   sees available entries.
2. For each exact publication, the optional node is revoked with
   `unregister_device_node_for_publication(id, publication, &binding,
   on_deferred)`. `Removed`/`NotFound` continues, `Deferred` keeps the
   publication in `Revoking` and the node completion callback resumes the same
   exact publication, and `NotOwner` enters retryable `RevokeFailed` without
   touching another owner's node.
3. The backend `unpublish_class` and removed callbacks run outside the
   registry lock. Only after both complete is the registry entry removed and
   `Revoked` exposed.
4. A second revoke during `Revoking` returns `Deferred`; a stale callback is
   matched by `PublicationId`, never by `DeviceId`.
5. Test pause hooks expose activation claim, node registration, callback
   commit, available finalize, unpublish, removed callback and final Revoked
   points. They run outside registry/node locks.

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

The `DisplayDevice` trait exposes the flush hooks (`need_flush` / `flush`), the resolution
(`DisplayInfo { width, height }`), display IRQ acknowledgement (`ack_irq`) and the scanout resource interface
(`create_scanout_resource` / `destroy_scanout_resource` /
`present_scanout_resource` / `flush_scanout_resource`). The kclass class adapter is a pure
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

- Each class registry is protected by `SpinNoPreempt<ClassRegistry<T>>`.
  Membership and publication state handoff change inside the lock;
  class-specific `publish_class` / `unpublish_class`, node work and subscriber
  callbacks always run outside it.
- An activation owns `Activating -> Notifying`; a revoke request during that
  window sets `revoke_requested` (Release) under the registry lock and returns
  `Deferred`. The activation executor performs the handoff to `Revoking` and
  exact node cleanup. `is_available`/find/device enumeration read the flag
  with Acquire and hide the publication immediately.
- Revocation has two lock regions (claim, terminal removal) with node removal,
  unpublish and removed callbacks in between. A second revoke during
  `Revoking` returns `Deferred`; `Revoked` is exposed only after callbacks.
- Spin-lock sections never sleep. Deferred node callbacks may run later in a
  different context; they resume the exact `PublicationId` and binding.
- Driver probes and removals call the registry while holding the descriptor
  binding lock, which is a sleeping `kmutex::Mutex`. The two are never held in
  the opposite order, and no class-registry `SpinNoPreempt` is held while
  taking the descriptor lock.
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

### Exact publication identity replaces same-ID retirement reservation

**Choice**: every publication has a globally unique `PublicationId` allocated
by `kdevice`, and node ownership is keyed by
`DeviceNodeKey(DeviceId, PublicationId)`. kclass owns the publication state
machine and credential; kdevice only owns the identity space required by the
node registry.

**Trade-off**: one identity allocation and a slightly larger
`ClassDeviceInner`, in exchange for:

- two capabilities published by the same device and binding can coexist and
  revoke independently;
- a stale credential cannot match or delete a newer publication;
- binding retirement can revoke every publication whose captured binding is
  exactly terminal without a device-ID sweep;
- node removal is exact and cannot erase a sibling publication's node.

**Rejected alternative**: keying only by `DeviceId`. Rebind and multi-publish
both make a device ID ambiguous; stale events would delete a newer entry.

**Rejected alternative**: holding the registry spinlock across
`unpublish_class`. That violates the no-sleep rule and runs subscriber-adjacent
teardown in atomic context.

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

**Choice**: `ClassDevice` holds a strong `Arc<DeviceObject>` plus the
`BindingRef` captured at construction.

**Trade-off**: clones of a `ClassDevice` held outside the registry still
reach the trait methods after the device is removed from the registry. This is
necessary for:

- pollers / async tasks holding a device reference that must finish a last
  I/O after hot removal;
- references obtained inside subscriber callbacks that may outlive the
  removal.

The strong parent keeps descriptor facts (id, name, location) readable. It
does **not** make the handle current: `is_available()` reads the captured
binding, so after unbind or rebind an old handle stays unavailable and cannot
follow the new instance.

**Rejected alternative**: `ClassDevice` holding `Weak<DeviceObject>`. That
would free the `DeviceObject` immediately on removal but requires
upgrading the weak pointer before every `with()` call, adding a failure
path (device already freed).

## Drop / Resource Release

- Dropping `ClassDevice` or `ClassPublication` only decrements an `Arc`; it
  performs no device operation and releases no binding. Drop is not revoke.
- The unique executor first removes the exact publication node, then calls
  `unpublish_class` outside the registry lock, then invokes removed callbacks,
  and only then removes the registry entry and sets `Revoked`.
- A deferred node removal keeps the publication in `Revoking`; the node
  completion callback resumes the same executor. A second revoke returns
  `Deferred`. A node `NotOwner`/unrecoverable failure enters `RevokeFailed`
  instead of silently unpublishing another owner's node.
- When the last `Arc` (consumer, credential, registry or node callback) is
  released, the runtime and `DeviceObject` references are dropped. The last
  drop never runs under the class registry spinlock.

## Input readiness forwarding

The input class adapter forwards the optional readiness hint without taking a
class lock or allocating. The devfs consumer separately checks device
availability before probing, and a retained class handle keeps the underlying
driver storage alive.

## Probe-context publication

`publish_*` accepts the kdevice `ProbeContext` rather than a raw stable object.
The class handle captures the context's exact `BindingRef`; publication never
queries the current binding slot itself. Remove/PM contexts cannot call the
publish API, so class publication cannot be smuggled into teardown.
