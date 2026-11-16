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
devices through `*_devices()` / `find_*_device()` /
`subscribe_*_available()`, without depending on probe order.

Publication is binding-scoped. The parent `DeviceObject` is stable across
rebinds, but each class entry captures the exact `BindingRef` that published
it. Removal of a class entry is therefore a two-step protocol: a terminal
entry is reserved under the registry lock, then its device node is revoked
and its class-specific runtime is unpublished outside the lock. A deferred
node removal completes the same protocol later, still keyed by the exact
binding instance.

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
├── ut-tests/        # registry/retirement tests mounted as cfg(unittest)
└── src/
    ├── lib.rs       # macro-driven class registries + event bridge + delegation + prelude
    ├── generic.rs   # ClassDevice<T>, ClassRegistry<T>, retirement claim protocol
    └── retirement_hooks.rs # cfg(unittest) retirement-claim pause seam
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
    │  │  ClassDevice<T>                 │    │
    │  │   Arc<ClassDeviceInner<T>>      │    │
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
| `ClassDevice<T>` | Typed runtime device handle; wraps `DeviceObject`, the captured binding reference and the trait object, and delegates trait method calls |
| `ClassDeviceInner<T>` | `ClassDevice` internal shared state: parent, captured `BindingRef`, runtime, name, kind, irq, metadata. Driver identity is the binding snapshot; availability reads the captured binding |
| `ClassRegistry<T>` | Typed registry: publish (unique publication), devices (enumerate available), find (lookup by id), subscribe (availability notification), terminal-entry discovery, two-phase retirement claim/finish |
| `ClaimOutcome` | Result of reserving terminal entries: `Claimed { id, token, removed }`, `Retiring` (a reservation for the id is already in flight), or `Nothing` |
| `ACTIVATION_BRIDGE` | Global event bridge: subscribes to `kdevice` `Activated` / `Removed` / `BindingRetired` events and drives class-level notify/remove; removable for tests via `reinstall_event_bridge_for_tests` |
| Retirement token | Monotonic `u64` private cleanup credential. It authorizes `finish_retirement` for the exact claim; it is not binding identity and is never reused. Exhaustion (`u64::MAX`) returns `BadState` before touching registry state |
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
                         │ publish_<class>(ctx, runtime)
                         v
                    ┌──────────┐
                    │Published │  ← ClassDevice created, held by registry;
                    │(pending) │     captured binding not Active
                    └────┬─────┘
                         │ kdevice Activated event
                         ▼
                    ┌──────────┐
                    │Available │  ← captured binding is Active;
                    │(active)  │     DeviceNode assigned, then callbacks fired
                    └────┬─────┘
                         │ Removed / BindingRetired event
                         ▼
                    ┌──────────┐
                    │ Terminal │  ← captured binding is no longer Active
                    │ (reserved)│    registry id is claimed under the lock
                    └────┬─────┘
                         │ node revoked (or deferred) → unpublish outside
                         │ the lock → finish_retirement(token)
                         ▼
                    ┌──────────┐
                    │ Removed  │  ← entry removed; reservation released
                    └──────────┘
```

| From | To | Trigger |
|----|----|----------|
| — | Published | Driver calls `publish_<class>(ctx, runtime)` from a probe context |
| Published | Available | `kdevice` dispatches `Activated`; `notify_class_available` registers any class node and then invokes subscriber callbacks |
| Available/Published | Terminal | `kdevice` dispatches `Removed` (unbind/unregister) or `BindingRetired` (failed candidate); the captured binding is terminal |
| Terminal | Removed | `evict_*_terminal`/`retire_*_binding` claims the id, revokes the node, unpublishes the class runtime, then finishes the claim |
| Terminal | Terminal (deferred) | The node is mid-removal; the claim is not taken, a deferred callback is registered, and the finishing node path retries the same retirement |
| Published/Available | — | Publishing the same `DeviceId` while a retiring reservation exists returns `ResourceBusy`; a live non-terminal entry returns `AlreadyExists` |

### Registry publish semantics

`ClassRegistry::publish` rejects duplicate identity, as Linux device
registration does. Hotplug re-registration must first remove the old
object through the `Removed` lifecycle and then publish a new object; a
resident runtime is never silently replaced.

Publication also respects an in-flight same-ID retirement. A retiring
reservation is installed in the same critical section that removes the
terminal entries, and it is released only after the class runtime has been
unpublished outside the registry lock. Until then a same-ID publish returns
`ResourceBusy`, so a replacement device created by a fast rebind cannot become
visible before the previous instance's unpublish has finished. A live
non-terminal entry is reported as `AlreadyExists` instead.

Publishing is all-or-nothing from the driver's point of view: a driver that
returns an error from `probe_device` after calling `publish_<class>` must
revoke every capability it already published before returning. The framework
performs no rollback of class publications after the hook returns, and
residue that is merely invisible to callers (for example because the parent
is not yet active) is not a rollback. Every successful publish must therefore
be undone explicitly on the error path.

## Flows

### Publish flow

1. The driver creates the runtime device inside `DeviceDriver::probe_device`
   (e.g. `VirtIoNet::try_new`).
2. It calls `publish_<class>(ctx, runtime)` (macro-generated).
3. `name()`, `device_kind()`, and `irq()` are extracted from the runtime.
4. The `device_kind` is checked against the registry type;
   a mismatch returns `InvalidInput`.
5. `class_metadata()` is extracted from the runtime (only the input class
   returns meaningful metadata).
6. The macro calls `ctx.binding()` to capture
   the exact binding being published. `ClassDevice::try_new_with_binding`
   rejects a binding observed as `Removing`/`Removed` with `BadState`;
   `driver_name()`/`driver_id()` become the immutable identity snapshot, and
   `is_available()` reads the captured runtime. Only the fixture/test
   constructor `try_new_with_class_metadata` reads the parent's current slot.
   A successful capture is **not** a publish permission: the caller must
   already hold the descriptor binding lock because it is in the probe write
   context.
7. The registry is inspected before any class resource is touched:
   an in-flight retirement → `ResourceBusy`; a live same-ID entry →
   `AlreadyExists`; terminal same-ID entries are evicted in the fixed order
   node revocation → unpublish → reservation release.
8. `publish_class` runs the class-specific registration. If the subsequent
   registry `publish` fails (for example a racing `AlreadyExists`), the
   class-specific publish is undone and the error is returned.
9. Successful publication pushes the entry under the `SpinNoPreempt` lock.
   A production probe publishes while the binding is still `Bound`, so
   availability is delivered by the later `Activated` event; publication does
   not synthesize an immediate availability transition. Only a test/fixture
   context that captures an already-`Active` binding takes the synchronous
   path: input and display classes first register a `DeviceNode` for the exact
   binding in `kdevice`, then notify class subscribers.

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

1. `*_devices()`: take the registry lock, iterate all entries, filter by
   `is_available()` (the captured binding is Active), and return clones.
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
   `kdevice::register_device_node_for_binding` for the entry's captured
   binding before availability callbacks.
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

### Terminal retirement protocol

Retirement never removes an entry with a live binding and never lets a
replacement binding inherit the entry.

1. `terminal_entries_for_retirement(id)` snapshots entries whose captured
   binding is `Removed`. An in-flight reservation for the id yields
   `ResourceBusy`; a live non-terminal entry yields `AlreadyExists`; a
   `Removing` (transient) entry is not terminal and is left alone.
2. For each terminal entry, the optional `DeviceNode` is revoked with
   `unregister_device_node_for_binding(id, &binding, on_deferred)`. The node
   implementation compares the owning binding instance:
   - `Removed`/`NotFound`: continue;
   - `Deferred`: the node is mid-removal; the callback is registered and the
     caller returns `ResourceBusy` without unpublishing. The finishing node
     path re-enters the same retirement with the same binding;
   - `NotOwner`: the node belongs to another binding instance; this is a
     `BadState`, never a reason to remove someone else's node.
3. `claim_retirement(id, [binding])` removes exactly the matching terminal
   entries from the registry and installs a same-ID reservation in one
   critical section. Only a `Claimed` outcome may unpublish; `Retiring` means
   another executor already owns the sequence.
4. The class runtime is unpublished outside the registry lock (input and
   display release class resources; block releases its owning disk
   reference).
5. `finish_retirement(&claim)` releases only the reservation whose id and
   token match the claim. A delayed finisher cannot clear a later executor's
   reservation, and a `Retiring`/`Nothing` claim is a no-op.
6. Test-only pause hooks (`test_hooks::ClassPause::RetirementClaimed` and
   `RetirementUnpublished`) can hold a claim between steps 3 and 5, and after
   step 4 respectively, to exercise same-ID publication and deferred-retirement
   interleavings. Both run outside the registry lock.

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
  Registry membership, the retirement reservation list and the token
  allocator change inside the lock; class-specific `publish_class` /
  `unpublish_class` and subscriber callbacks always run outside it.
- The retirement sequence has two lock regions (claim, then finish) with the
  unpublish in between. The same-ID reservation is what makes that gap safe:
  it is installed in the claim's critical section and cleared only by the
  matching token, so publication cannot slip into the gap.
- Spin-lock sections never sleep. Deferred node callbacks may run later in a
  different context; they re-validate the exact binding before claiming.
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

### Binding-scoped entries with a two-phase retirement claim

**Choice**: every class entry captures the exact `BindingRef` that published
it, and retirement is a claim/finish protocol with an opaque monotonic token
and a same-ID reservation, rather than an immediate `swap_remove` on the
`Removed` event.

**Trade-off**: one extra reservation entry and one token allocation per
retirement, in exchange for:

- the stable `DeviceObject` can be rebound without an old class entry being
  mistaken for the new binding's entry;
- `unpublish_class` (which may take class-specific locks or release a disk
  reference) can run outside the registry spin lock without allowing a
  same-ID publish to resurrect the device in the gap;
- a deferred `DeviceNode` removal can complete later and still retire exactly
  the instance that requested it, instead of guessing by device ID;
- removing an entry without owning its claim is impossible, because
  `finish_retirement` matches the private token.

**Rejected alternative**: key removal by `DeviceId` only. Under stable device
objects a rebind reuses the ID, so an old `Removed` event or a stale deferred
callback could evict the new binding's entry.

**Rejected alternative**: hold the registry spin lock across `unpublish_class`.
That violates the spin-lock no-sleeping rule for class unpublish paths and
would run subscriber-adjacent teardown inside an atomic context.

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

- Dropping a `ClassDevice` only decrements the `Arc` count; it performs no
  device operations and releases no binding.
- Registry removal is a `Vec::swap_remove` inside `claim_retirement`, before
  the class runtime is unpublished outside the lock.
- A binding-owned `DeviceNode` is revoked through
  `unregister_device_node_for_binding` before the claim, so a replacement
  binding can register its own node only after the old owner is gone. A node
  that is still in use defers, and the node completion callback finishes the
  same claim.
- When the last `ClassDevice` and `DeviceObject` references are released,
  the `DeviceObject` devres (MMIO mappings, IRQs, DMA buffers) is cleaned
  up in LIFO order.
- The runtime trait object is dropped by its `Box<T>` when
  `ClassDeviceInner` drops; the class-specific `unpublish_class` hook runs
  first, in the retirement path.

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
