# kdevice — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose

`kdevice` is x-kernel's device core, located in `drivers/base/`. It provides
the stable type definitions for the bus / device / driver triple, the global
object registries, the lifecycle state machine, Linux-compatible device-node
identity assignment, and event distribution infrastructure. It lives in
`drivers/base/` rather than `drivers/contracts/` because it owns registries
and binding/removal orchestration, not only interface declarations.

`kdevice` is the bottom of the device-driver stack — `kdriver` (driver
orchestration) and `kclass` (the typed class layer) are built on top of
it, but `kdevice` itself depends on neither. All persistent device
topology information (bus instances, device objects, driver objects,
discovery descriptors) and shared device-node identity (class index,
major/minor, and `devname`) is managed here.

The intended audience is developers implementing new bus matchers,
modifying the device lifecycle state machine, or extending global registry
queries.

## Background

The Linux kernel device model provides complete
discover → bind → activate → remove lifecycle management through its three
core structures (`device`, `device_driver`, `bus_type`) plus the devres
(device resource) mechanism. x-kernel's `kdevice` lands that mechanism in
Rust's type system and ownership model:

- **Type safety**: bus instances, device objects, and driver objects are
  separate Rust types, shared via `Arc`, with mutable state protected by
  `SpinNoPreempt`;
- **Stable object, replaceable binding**: `device_desc_add` publishes one
  stable `DeviceObject`/`DeviceId` that survives probe failures, unbind and
  rebind; only unregistering the descriptor ends it. The object holds its
  discovery facts immutably and a slot containing at most one current
  `BindingRuntime`. Every binding attempt installs a brand-new runtime, and a
  retired runtime is never reset or reused. Read-only `BindingRef` handles and
  `DeviceUse` guards retain the exact instance they were created from, so they
  cannot follow the object into a later binding. `DeviceRegistry` owns the
  global indexes; binding transitions are atomic through `AtomicU8` + CAS;
- **Event-driven**: lifecycle events (Published/Matched/Bound/Activated/
  Removed and the instance-scoped BindingRetired) are delivered to observers
  such as `kclass` through bucketed subscribers.

## Scope

```text
drivers/base/kdevice/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs                    # crate entry, re-exports all public types
    ├── bus/
    │   ├── mod.rs                # BusId, BusInfo, BusInstance
    │   └── bus_type.rs           # BusTypeId, BusType trait, BusTypeObject,
    │                             #   PciBusTypeMatcher, PlatformBusTypeMatcher
    ├── device/
    │   ├── mod.rs                # device submodule root
    │   ├── binding.rs            # private BindingRuntime (lifecycle, usage,
    │   │                         #   metadata, devres) + public BindingRef
    │   ├── desc.rs               # DeviceDesc, DeviceId, DeviceLocation,
    │   │                         #   DeviceIdentity, DeviceState, DeviceRecord
    │   ├── object.rs             # DeviceObject (stable facts + relations +
    │   │                         #   registration/binding slot), DeviceUse guard
    │   ├── handles.rs            # BusHandle, DeviceCore, DriverCore
    │   └── resource.rs           # resource type re-exports
    ├── device_node.rs            # DeviceNode identity, class allocators,
    │                             # snapshots and lifecycle subscribers
    ├── driver/
    │   └── mod.rs                # DriverId, DriverInfo, DriverObject,
    │                             #   DeviceDriver trait, DeviceMatcher impls,
    │                             #   MatchResult, ProbeStats
    ├── lifecycle/
    │   ├── mod.rs                # lifecycle orchestration:
    │   │                         #   register_bus_instance, register_driver_object,
    │   │                         #   probe_device_desc, adopt_active_device,
    │   │                         #   remove_device_managed, parent attach/detach
    │   ├── binding_hooks.rs      # cfg(unittest) pause/ownership seams
    │   ├── dispatch.rs           # event distribution and state transition notify
    │   ├── event.rs              # DeviceEvent, DeviceEventKind
    │   └── subscribers.rs        # DeviceEventSubscribers (bucketed storage)
    ├── registry/
    │   └── mod.rs                # DeviceRegistry (global BTreeMap indexes),
    │                             #   descriptor/object state, query/snapshot helpers
    └── topology/
        ├── mod.rs                # DeviceTopology (read-only topology snapshot),
        │                         #   BusView, DeviceCoreView, DriverCoreView
        └── relations.rs          # serialized attach/detach, bounded ancestor walk,
                                  #   ordered relation locks, registration cleanup
```

## Architecture

```text
    kdriver (enumerate, probe)    kclass (event subscriber)
         │                              │
         │ register_bus_instance        │ subscribe_device_event_kind
         │ register_driver_object       │
         │ device_desc_add              │
         │ probe_device_desc            │
         │ adopt_active_device          │
         │ remove_device_managed        │
         ▼                              ▼
    ┌─────────────────────────────────────────────┐
    │ kdevice                                      │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   lifecycle::mod                      │    │
    │  │   ├─ probe pipeline (desc → publish)  │    │
    │  │   ├─ adoption (early device handoff)  │    │
    │  │   ├─ remove (managed teardown)        │    │
    │  │   └─ parent/child attach/detach       │    │
    │  └──────────────┬───────────────────────┘    │
    │                 │                             │
    │  ┌──────────────┴───────────────────────┐    │
    │  │   lifecycle::dispatch                 │    │
    │  │   mark_matched → bind_driver →        │    │
    │  │   activate → dispatch_event           │    │
    │  └──────────────┬───────────────────────┘    │
    │                 │                             │
    │  ┌──────────────┴───────────────────────┐    │
    │  │   lifecycle::subscribers              │    │
    │  │   buckets: [Published][Matched]       │    │
    │  │   [Bound][Activated][Removed]         │    │
    │  │   [BindingRetired]                    │    │
    │  └──────────────────────────────────────┘    │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   device_node::DeviceNodeRegistry     │    │
    │  │   DeviceId -> DeviceNode              │    │
    │  │   eventN/cardN/renderD + events       │    │
    │  └──────────────┬───────────────────────┘    │
    │                 │                             │
    │                 ├──────────► devfs projection │
    │                 └──────────► sysfs projection │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   registry::DeviceRegistry            │    │
    │  │   (SpinNoPreempt global lock)          │    │
    │  │                                       │    │
    │  │   descriptors: BTreeMap<DescId, ...>  │    │
    │  │     state Pending|Probing|Bound       │    │
    │  │     + shared binding lock Arc          │    │
    │  │   devices:     BTreeMap<DeviceId, ...>│    │
    │  │   buses:       BTreeMap<BusId, ...>   │    │
    │  │   bus_types:   Vec<BusTypeObject>     │    │
    │  │   drivers:     BTreeMap<DriverId, ...>│    │
    │  │   subscribers: DeviceEventSubscribers │    │
    │  └──────────────────────────────────────┘    │
    │                                              │
    │  ┌───────────┐ ┌────────────┐ ┌────────────┐  │
    │  │BusInstance│ │DeviceObject│ │DriverObject│  │
    │  │(per-bus   │ │(facts +    │ │(per-driver │  │
    │  │SpinLock)  │ │relations + │ │SpinLock +  │  │
    │  │           │ │binding slot│ │AtomicU64)  │  │
    │  └───────────┘ └────────────┘ └────────────┘  │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   topology::DeviceTopology            │    │
    │  │   read-only snapshot, bus/driver       │    │
    │  │   filtering                            │    │
    │  └──────────────────────────────────────┘    │
    └─────────────────────────────────────────────┘
```

| Component | Responsibility |
|------|------|
| `DeviceRegistry` | Global object index; BTreeMaps for descriptors/devices/buses/drivers; id allocation |
| `DeviceNodeRegistry` | Linux-compatible class index, device number, `devname`, and node lifecycle authority; orders projection callbacks around one host lifecycle publisher |
| `DeviceObject` | Stable object published at registration: immutable discovery facts, topology relations under `SpinNoPreempt`, and a slot holding the registration state plus at most one current `Arc<BindingRuntime>` |
| `BindingRuntime` (private) | Per-binding state owner: `AtomicU8` lifecycle + CAS, usage count, driver metadata under `SpinNoPreempt`, devres cleanup list. A new runtime is allocated for every candidate/rebind; a retired one is never reset |
| `BindingRef` | Read-only handle to one exact binding instance: retains that runtime and snapshots driver identity; does not count usage, expose mutation, or follow the object into a later binding |
| `DeviceUse` | RAII guard; retains the admitted binding instance and blocks device removal while held |
| `BusInstance` | Runtime bus instance; controller / devices / drivers lists; probe statistics |
| `BusTypeObject` | Bus matching domain; tracks bus instances and registered drivers and delegates matching to the `BusType` trait. Descriptors awaiting a driver are discovered from the registry's `Pending`/`Probing` state, not from a second queue |
| `DriverObject` | Registered driver object; bound-device list; probe statistics |
| `DeviceDesc` | Discovery-phase descriptor; carries bus_id/location/identity/transport/resources, the stable `DeviceId`, an optional resolved DMA view, and an optional firmware-node handle |
| `DeviceDescSpec` | Finalized registration aggregate consumed by `device_desc_add`; the only public registration input |
| `DeviceDriver` trait | Driver interface: name/device_kind/bus_types/matcher/probe_device/remove/suspend/resume/shutdown |
| `DeviceMatcher` trait | Open matcher; built-ins PciIdsMatcher/VirtioTypeMatcher/CompatibleAliasMatcher/FirmwareMatchSpec/NeverMatcher |
| `DeviceEvent` / `DeviceEventKind` | The 6 lifecycle events, including instance-scoped `BindingRetired`; bucketed subscribers |
| `DeviceTopology` | Read-only topology snapshot; bus/driver-filtered iterators |
| Lifecycle API | `register_bus_instance`, `register_driver_object`, `probe_device_desc`, `adopt_active_device`, `remove_device_managed` (unbind/requeue), `unregister_device_desc` (remove registration), `attach_device_parent`/`detach_device_parent` |

## Stable object and binding ownership

A `DeviceObject` separates long-lived registration facts from one replaceable
driver binding:

| Owner | State |
| --- | --- |
| `DeviceObject` | `id`, bus/location/origin/identity/transport, resources, DMA view, INTx route, firmware node (all fixed after construction); registration membership |
| `DeviceObject.relations` | `parent`, `children`, `child_bus` under one `SpinNoPreempt`; managed by the lifecycle/topology layer, not by `BindingRuntime`. Unbind preserves it; only unregister clears it |
| `DeviceObject.slot` | `Registration` (`Detached` for fixtures, `Registered`, `Ended`) plus `Option<Arc<BindingRuntime>>` for the current binding, under one leaf `SpinNoPreempt` |
| `BindingRuntime` | `lifecycle` (`AtomicU8`), `usage` (`AtomicUsize`), driver metadata (`driver_id`/`driver_name`/`device_kind`) under `SpinNoPreempt`, devres cleanup `Vec` under `SpinNoPreempt` |

`BindingRuntime` remains private with no public reset/into-inner API. The
difference from the descriptor-first model is where it lives: a `DeviceObject`
now owns a slot whose current runtime is replaced, instead of owning exactly
one runtime for its lifetime.

- `device_desc_add` mints one `DeviceId` and publishes the stable object in
  `Discovered` state in the same registry critical section as the descriptor.
  Probe failures, unbind and rebind reuse that object; unregister is the only
  operation that ends it.
- Every candidate in a probe round, and every rebind after an unbind, gets a
  **fresh** `BindingRuntime` installed into the empty slot. A terminal runtime
  is never reset, reused, or installed a second time. The slot must be empty
  before `install_fresh_binding`, and the caller must hold the descriptor
  binding lock.
- `BindingRef` is the public read-only handle to one exact instance. It keeps
  that runtime alive and snapshots driver id/name at capture, but never
  increments `usage`, never blocks removal, and cannot be redirected to the
  object's later binding. `same_binding` compares instance identity.
- `DeviceUse` stores both the `Arc<DeviceObject>` (for `device()`/`Deref`) and
  the `Arc<BindingRuntime>` admitted by `try_acquire`; `Drop` releases that
  same instance. It is the only consumer that gates teardown. A guard held on
  a retired binding can never decrement a newer binding's usage.
- `BindingRef::try_acquire(&DeviceObject)` revalidates that the captured
  runtime is still the object's current binding; an old handle cannot obtain
  admission from a replacement binding. `add_cleanup(&DeviceObject, ...)`
  performs the same exact-instance check, so a stale handle cannot attach
  resources to its successor.

`DeviceObject::state()` is a projection, not a per-object state variable:
`Ended` → `Removed`; registered with no current binding → `Discovered`;
otherwise the current runtime's state. `is_registered()` reports the
registration membership.

`DeviceObject::binding_ref()` rejects a capture that observes
`Removing`/`Removed` or lacks identity metadata. `binding_ref_for_lifecycle_write()`
additionally requires that the caller holds the descriptor binding lock for a
registered object (fixtures skip that check); probe, adoption and removal use
it to capture the exact binding they are writing. Neither is a publication
permission: publication still relies on the descriptor binding lock to
serialize with removal. An independent capture followed by a publish after
removal can leave a class entry whose `is_available()` is false (queries
filter it out); this layer adds no publication protocol or rollback.
Conversely, `Removing` can be transient: when `begin_removing` finds
outstanding `usage` it restores the prior state, so existing references can
observe `Active` again. Only a successful `begin_removing` commit is
irreversible.

`device_records_snapshot` collects object `Arc`s under the registry lock,
drops that lock, then calls `record_snapshot` on each. `record_snapshot`
returns a **grouped snapshot**, not one atomic snapshot. The current binding
and its driver metadata (`driver_id`/`driver_name`/`device_kind`) are sampled
together from the slot/`binding.meta`; `parent`/`child_bus` are sampled under
the relations lock as another group; the projected lifecycle state is a
separate read. Each group is internally consistent, but the samples may come
from different instants. Immutable device facts are copied from the object.
Records exist for unbound registrations too (`Discovered`, no driver).
Consumers use records for diagnostics, enumeration and filtering, not as a
cross-group transaction.

Cleanup ownership moves with the binding. `run_cleanups` takes the current
`Vec` under the devres lock, releases the lock, then runs items LIFO; each item
in that batch runs once. A cleanup registered by a running callback enters the
next batch and is not run by the same call, so only a call with no new
registrations is a no-op. Explicit lifecycle orchestration still owns teardown;
`BindingRuntime` has no `Drop` side effects and does not replace
`driver.remove`/`bus_type.remove` ordering. Retaining a `BindingRef` keeps only
the runtime allocation and its identity snapshot alive; it does not keep
hardware usable and does not delay or re-run the explicit cleanup batch.

## Device-node identity lifecycle

`kclass` calls `register_device_node` from the `Activated` event path for
runtime classes that require Linux character nodes. The caller supplies the
driver-core id, class-specific metadata, runtime name, driver name, and bus
location. `kdevice` assigns the lowest free class index and derives both the
Linux `DeviceNumber` and path below `/dev` from one class template:

| Kind | Names | Device numbers | Capacity |
|------|-------|----------------|----------|
| input | `input/eventN` | major 13, minor `64 + N` | 32 |
| DRM primary | `dri/cardN` | major 226, minor `N` | 64 |
| DRM render | `dri/renderD{128+N}` | major 226, minor `128 + N` | 64 (shares the card index) |

A DRM node owns two identities derived from one class index: the primary
`dri/cardN` (`226:N`) and the render `dri/renderD{128+N}` (`226:128+N`).
`render_device_number()` and `render_devname()` are the only derivation sites;
`DeviceNodeRegistry` still allocates the index and lifecycles both nodes
together. The render identity exists even when the bound driver has no render
backend, matching Linux, which registers the minor unconditionally and rejects
unsupported commands per ioctl.

Registration inserts the complete `DeviceNode` in Publishing state before
delivering `Added` to subscribers. `subscribe_device_nodes` installs a
callback and returns the current snapshot under one lock, allowing devfs and
sysfs to initialize without a subscribe/snapshot gap. After projection
subscribers return from `Added`, the single host lifecycle publisher receives
the matching add event and the node becomes Published.

Removal atomically changes the node from Published to Removing while it remains
reserved in the registry. If removal arrives while add callbacks are still
running, the state instead becomes RemovePending; the add-publishing thread
finishes the add event before advancing it to Removing. Projection subscribers
then receive `Removing` so they can reject new operations and wait for
in-flight work. The host lifecycle publisher receives remove while paths still
exist. Projection subscribers finally receive `Removed`; only after those
callbacks return does the registry mark the node Removed, delete it, and
release the class index. Registration of the same core ID returns
`ResourceBusy` during this sequence.

Completing publication and dispatching removal are separate steps.
`finish_publish` only advances the lifecycle state and reports that removal was
requested; the caller dispatches the removal callbacks outside the registry
lock and then calls `finish_remove`, which re-checks the node identity and the
expected binding owner before deleting the node and releasing its index. The
placeholder and index therefore stay reserved for the whole removal
notification, so a newly discovered device cannot take the same class index
while consumers still observe the old removal.

Device-node callbacks run outside the registry lock and may allocate or enter
filesystem code, but they must run in normal task context. The host lifecycle
publisher must be installed before the first device node; late installation is
rejected instead of backfilling synthetic add events.

### Binding-owned nodes and deferred retirement

A class entry captures a `BindingRef`, so a node must be removable only by the
binding that created it. `register_device_node_for_binding` records
`(core_id, BindingRef)`; `unregister_device_node_for_binding` returns one of:

- `Removed` — the node was this binding's and its removal callbacks ran;
- `NotFound` — no node is registered for that id;
- `Deferred` — removal is already in flight; this call's callback is stored
  (one per id, first one wins) and runs exactly once when the in-flight
  sequence ends;
- `DeferredExisting` — another request already owns completion; this call's
  callback was not stored and is dropped after the registry lock is released;
- `NotOwner` — the node belongs to another binding; nothing is touched.

Callbacks that are not registered by `begin_remove_for_binding` are kept
outside the registry guard by the public wrapper and dropped only after the
guard is released. Their captured resources can therefore take locks or query
the node registry in `Drop` without running under a registry spinlock.

The legacy `register_device_node`/`unregister_device_node` pair remains for
callers that have no binding handle. A node created by the legacy path has no
owner, so the legacy unregister removes it; a node owned by a binding is
rejected with `ResourceBusy`/`BadState` rather than silently stolen. This is
an explicit compatibility boundary, not a second lifecycle.

`DeferredNodeRetirement` carries the owner check through the deferred path:
when the finishing task runs the callback it calls back into the
binding-scoped retire function, which claims the exact terminal class entry by
`(id, binding)`. A deferred callback therefore cannot retire a replacement
binding's entry for the same device id.

## State Machine

### Registration and binding lifecycle

The stable object's projected state combines registration membership with the
current binding's state. A single binding instance walks:

```text
                         ┌────────────┐
                         │ Discovered │  ← device_desc_add: object published,
                         │ (no binding│     slot empty
                         │  installed)│
                         └─────┬──────┘
                               │ probe round installs a fresh runtime + metadata
                               ▼
                         ┌────────────┐
                         │  Matched   │  ← DeviceObject::mark_matched()
                         └─────┬──────┘
                               │ DeviceObject::bind_driver()
                               ▼
                         ┌────────────┐
                         │   Bound    │  ← driver_id/name/kind recorded
                         └─────┬──────┘
                               │ probe_device() returns Ok
                               ▼
                         ┌────────────┐
                         │   Active   │  ← activate_device();
                         │            │     Activated event fired
                         └─────┬──────┘
                               │ remove_device_managed() / unregister_device_desc()
                               ▼
                         ┌────────────┐
                         │  Removing  │  ← begin_removing() closes admission;
                         │            │     driver.remove() + bus_type.remove()
                         │            │     + run_cleanups()
                         └─────┬──────┘
                               │ Removed dispatched while this runtime is still
                               │ in the slot; then the slot is cleared
                               ▼
                         ┌────────────┐
                         │  Removed   │  ← binding instance is terminal forever
                         └────────────┘
              unbind: object returns to Discovered (same DeviceId, topology kept)
              unregister: registration ended; object projects Removed
```

| From | To | Trigger |
|----|----|----------|
| — | Discovered (projection) | `device_desc_add` or `adopt_active_device` publishes the stable object with an empty or fresh slot |
| Discovered | Matched | a probe round installs a fresh runtime and marks it matched |
| Matched | Bound | the publish path records driver_id/name/kind before visibility |
| Bound | Active | `DeviceDriver::probe_device` returns `Ok(())` |
| Matched/Bound | Removed | candidate probe failed; this runtime is retired and the slot cleared; the object returns to `Discovered` |
| Active/Matched/Bound | Removing | `begin_removing` closes admission and sees zero usage |
| Removing | Removed | driver.remove + bus_type.remove + run_cleanups complete; `Removed` fires, then unbind clears the slot / unregister marks the registration ended |

`adopt_active_device` installs its fresh binding and prepares Matched/Bound
metadata before publication, so its object is never observable in
`Discovered`.

The `begin_removing` CAS loop uses `compare_exchange_weak`:

- only `Active`/`Bound`/`Matched`/`Discovered` may transition to `Removing`;
- an existing `Removing` or `Removed` state is rejected (re-entry guard);
- the CAS into `Removing` only **closes admission** for new `DeviceUse`
  references; it is not the teardown commit point;
- a non-zero usage count restores the prior state, returns `ResourceBusy`, and
  commits no teardown — the caller may retry;
- only `begin_removing() == true` (CAS succeeded and usage was zero) gates the
  lifecycle layer's irreversible teardown.

### DeviceDesc lifecycle (descriptor-first path)

```text
                         ┌──────────┐
                         │ Pending  │  ← device_desc_add
                         └────┬─────┘
                              │ mark_device_desc_probing
                              ▼
                         ┌──────────┐
                         │ Probing  │  ← held by probe_device_desc
                         └────┬─────┘
                    ┌─────────┼─────────┐
                    │ probe   │ probe   │
                    │ success │ failure │
                    ▼         ▼         │
              ┌──────────┐ ┌──────────┐ │
              │  Bound   │ │ Pending  │◄┘ requeue (object stays)
              └──────────┘ └──────────┘
                    │
          ┌─────────┴─────────┐
          │                   │
          │ unbind: requeue   │ unregister: delete
          ▼                   ▼
    ┌──────────┐        ┌────────────┐
    │ Pending  │        │ (absent)   │
    └──────────┘        └────────────┘
```

Key semantics:

- each descriptor registration owns one stable sleepable `kmutex`, created by
  `device_desc_add`/`adopt_active_device` and shared with its stable
  `DeviceObject`. It serializes every binding install/clear, cleanup
  registration, removal and topology entry point, and it outlives candidate
  rebuilds, failed probes, requeue, unbind and rebind.
- registration holds the fresh lock through publication and the initial
  topology link with `try_lock`, because it may run under the enumeration
  spinlock. Adoption does the same under the bus-manager spinlock.
- only the binding-lock holder may move `Pending` to `Probing` or back, so the
  state can be trusted by a waiter that acquires the lock and re-reads it.
- `Bound` means the descriptor's stable object exists and the slot holds its
  current binding. A concurrent `probe_device_desc` that finds the projected
  object state Active/Removing/Removed returns `Skipped` after recheck; a
  `Bound` descriptor whose object is missing or non-terminal without being
  registered is a broken invariant (`BadState`).
- `remove_device_managed` unbinds: the descriptor returns to `Pending` and
  the same stable `DeviceId` can be bound again; the object keeps its
  discovery facts and topology.
- `unregister_device_desc` is final: it deletes the descriptor registration
  and the stable object (tearing down a bound device through the same remove
  sequence first). A `Pending` unregister also deletes the never-bound object
  without emitting an event. After it returns, the old `DeviceDescId` and
  `DeviceId` are absent, so `probe_device_desc` reports `InvalidInput` and
  scan snapshots skip it; re-discovery must allocate new ids. No tombstone or
  second identity is kept.

A descriptor and the device object created from it carry an optional
`DmaInfo` resolved by the discovering backend. `None` records that no view was
established; it never means unlimited addressing or coherent DMA. The view is
immutable once registered, and the probe/adoption paths copy it verbatim, so
firmware ancestry translation and platform policy stay in the bus backend
rather than in the device core.

### Firmware-node association

`DeviceDescSpec` is the single registration input. It carries
`firmware_node: Option<FwNode<'static>>`; `DeviceDesc` stores the handle
privately and exposes it only through the crate-internal `firmware_node()`
getter. Probe copies the stored handle into the `DeviceObject` it constructs,
and `DeviceObject::firmware_node()` is the only public read access. The core
never searches firmware again, re-matches a compatible string, or re-parses a
property to rebuild the association. A failed probe keeps the descriptor and
its handle, so every retry attempt for that descriptor observes the same node.

Association ownership follows `FwNode` semantics: it is a `Copy` borrowed
handle (backend operations pointer + backend-local id), not an `Arc`, refcount
or owned firmware object. `'static` in the handle type means the backend must
outlive the kernel device model; this design supports boot-static firmware
backends only. The bound does not by itself forbid deliberately leaking a
dynamic backend, so publication policy must still restrict nodes to boot-static
storage. A backend cannot be unloaded, and a node handle must not be retained
past backend destruction. Consumers query nodes in normal task/process context
during discovery or probe; `kdevice` holds no backend lock and never calls
backend operations itself, and the contract does not declare backends
interrupt-safe.

The node is descriptive association, not identity: matching still uses
`DeviceIdentity` plus `DiscoveryOrigin`. Platform-static devices and PCI
endpoints pass `firmware_node: None` explicitly, and adoption also stores
`None` because it is a boot/runtime handoff rather than firmware discovery.
Absence is a normal state: a descriptor without a node binds and probes exactly
as before, and a probe must not substitute a default node.

Only one registration entry is kept. `DeviceDescSpec` replaced the previous
`DeviceDesc::new`/`new_with_parent`/`with_intx_route`/`with_dma` builders and
the split `device_desc_add_with_dma`/`device_desc_add_with_parent` functions.
Keeping those would have preserved two ways to register and a second place to
forget `firmware_node`.

Only `device_descs_snapshot` retains the private descriptor handle; the
`DeviceRecord` snapshot and the topology snapshot are metadata views and do not
carry a firmware node. Cloning a descriptor snapshot copies the handle but not
the backend, so the copy still points at the original boot-static backend.
Registry and per-object locks protect the copied handle, not backend queries.
`kdevice` unit tests cover descriptor/object/probe copying, retry, absence and
adoption; the production platform-discovery wiring is covered in `kdriver`.

## Flows

### Probe pipeline

`probe_device_desc(id)` binds at most once and reports one `ProbeOutcome`:

1. Fetch the descriptor's binding-lock handle. A missing handle means the
   registration does not exist (never registered or already unregistered) →
   `Err(InvalidInput)`. If this task already owns the lock, return `Busy`
   immediately (same-context recursion); a different task never sees `Busy`.
2. Acquire the binding lock, blocking in sleepable task context, then re-read
   the descriptor state:
   - missing descriptor (unregistered while this task waited) → `Skipped`;
   - `Pending` → claim it (set `Probing`);
   - terminal bound device (Active/Removing/Removed) → `Skipped`;
   - `Probing` or non-terminal bound device → `Err(BadState)` (invariant
     violation; only the lock holder may set `Probing`).
3. Snapshot the bus type's candidate drivers once for this round, then walk
   them in registration order:
   - `match_desc` returns `NoMatch` → skip (no probe attempt);
   - install a **fresh** binding runtime into the stable object's empty slot,
     silently prepare Matched/Bound metadata, account the attempt, and call
     `probe_device`;
   - success → publish and activate (below), stop the round → `Activated`;
   - failure → `detach_driver`, `mark_removed`, `run_cleanups`, dispatch the
     instance-scoped `BindingRetired`, then clear the slot without discarding
     the object; continue with the next candidate. Every candidate is tried
     at most once per round.
4. A candidate construction/core-preparation failure requeues the descriptor
   and returns `Err`; ordinary probe failures do not.
5. No matching driver at all → requeue, `Unmatched`. At least one matching
   driver was probed and all failed → requeue, `Unclaimed`. In both cases the
   stable object remains registered and unbound (`Discovered`).

On success the order is fixed: registry descriptor bind, bus
`attach_bound_device` (devices then drivers, with binding metadata already
set), parent attach, `driver.attach_device`, Matched/Bound/Published events,
then `activate_device`. The descriptor stays `Bound`; the object holds the
same runtime throughout.

### Adoption path

`adopt_active_device(adoption)`: for devices initialized before the core
runs, such as the boot console or the PCI host bridge.

1. Verify the target bus exists and the driver's `bus_types` include the
   target bus's `bus_type`.
2. Allocate `desc_id` and `device_id`, then construct `DeviceDesc` + the
   stable `DeviceObject` (skipping match/probe), with `firmware_node = None`
   on both: adoption is not firmware discovery.
3. Create a fresh binding lock and `try_lock` it, then install one fresh
   binding runtime and prepare Matched/Bound metadata while the object is
   still unpublished. Adoption runs inside the bus-manager spinlock, so it
   must never block; a failed `try_lock` on this unpublished entry is a
   broken invariant (`BadState`).
4. Publish the descriptor/device, attach the bus association, parent and
   driver, announce Matched/Bound/Published, activate, then drop the lock.
   A concurrent remove blocks on the lock until the publication window is
   closed. The same stable object and id carry every later unbind/rebind.

### Remove path

`remove_device_managed(id)` serializes with binding through the unique
descriptor's binding lock:

1. Before waiting: the device must exist (`InvalidInput`) and exactly one
   descriptor must be bound to it (`BadState` otherwise). Snapshot that
   descriptor's binding lock.
2. Acquire the binding lock (blocking). Then re-read:
   - device disappeared → `ResourceBusy` (another remover won);
   - descriptor no longer bound to this device → `BadState`;
   - snapshot driver/bus type.
3. `device.begin_removing()` — admission gate and lifecycle commit condition.
   It runs after the lock is acquired, closes admission by entering
   `Removing`, and rejects `Removing`/`Removed` states and non-zero usage with
   `ResourceBusy`. On `ResourceBusy` the prior state is restored and no
   teardown is committed; only a `true` return lets the steps below proceed.
4. Under the lock: `driver.ops().remove(device)` and
   `bus_type.remove(device)` (best-effort; errors logged, removal proceeds),
   then `device.run_cleanups()` (devres LIFO).
5. `remove_device_from_index`: requeue the descriptor only if it is still
   bound to this device and snapshot bus/driver/parent/children while the
   registry owns the entry. Detach the bus association and the shared driver
   entry atomically (the driver is kept if another device on that bus still
   references it), detach the driver metadata and mark the binding
   `Removed`. **The device index entry, the stable object and its topology
   stay.** `Removed` is dispatched while the slot still holds the terminal
   binding, so subscribers revoke handles captured from that exact instance.
6. Only after `Removed` returns does the lifecycle layer clear the slot; the
   object then projects `Discovered`. Drop the binding lock last. A blocked
   binder rechecks, sees `Pending`, and may bind a fresh runtime to the same
   stable `DeviceId`.

### Unregister path

`unregister_device_desc(desc_id)` is the transport/owner-side operation that
retracts a discovery registration. It shares the removal sequence with
`remove_device_managed` but deletes the descriptor instead of requeueing it:

1. Fetch the binding-lock handle; missing → `InvalidInput`. If this task
   already holds it, return `ResourceBusy` before blocking (the recursive
   `kmutex` acquisition would panic).
2. Acquire the lock, then read state and backing-device existence in one
   registry critical section. The binding lock excludes every writer, so the
   decision is stable until deletion.
   - absent (unregistered by another task while waiting) → `Ok(())`;
   - `Pending` → delete the descriptor and its never-bound stable object with
     a state recheck in the delete critical section; a mismatch returns
     `BadState` and emits/hooks nothing;
   - `Probing` → `BadState` (only the lock holder may leave it `Probing`);
   - `Bound` → require the stable object to exist and to be the unique object
     owned by this descriptor, otherwise `BadState`.
3. For `Bound`, run the same commit/teardown as the remove path
   (`begin_removing`, driver/bus remove, cleanups, bus/driver detach, one
   `Removed`) but with the `Delete` disposition: the shared index step then
   deletes both the device entry and the descriptor, marks the registration
   ended, conditionally clears the object's topology relations, and reports
   a terminal `Removed` projection. All of this happens before the binding
   lock is released.
4. `begin_removing` failure returns `ResourceBusy` with the descriptor,
   binding and cleanup state untouched, so the caller may retry. Once
   `begin_removing()` returns `true`, a delete miss is reported as `BadState`;
   the teardown still completes and is never rolled back to `Active`.

There is no window in which the descriptor is written back to `Pending` and
can be reclaimed by a competitor. A `Pending` unregister deletes the
never-bound stable object without emitting a lifecycle event (no binding was
ever published); a `Bound` one emits exactly one `Removed`.

### Descriptor rescan after driver registration

`register_driver_object` immediately scans descriptors that still await a
binding in the new driver's bus types:

1. Snapshot `(desc_id, binding lock)` pairs under the registry lock. The
   snapshot includes both `Pending` and `Probing` descriptors.
2. For each pair: skip only descriptors currently being bound by the same
   context (`is_owned_by_current`, logged); otherwise acquire the binding
   lock and recheck. Descriptors unregistered after the snapshot and
   terminal-bind descriptors are skipped; `Pending` ones are claimed and
   probed with candidates limited to the new driver.
3. Acquiring the lock may wait for an in-flight probe. Rechecking afterwards
   is what makes a later registration see the final state instead of a stale
   snapshot. This removes driver load-order problems: a driver registered
   later still finds already-discovered devices.

## Lock Ordering Rules

The driver core uses one sleepable binding lock per descriptor plus multiple
`SpinNoPreempt`-protected objects. To prevent deadlock, every path must
follow:

```text
1. Descriptor binding lock (kmutex)  ← outermost; taken outside spinlocks
2. Registry (DeviceRegistry)         ← may be taken while holding (1)
3. BusInstance                       ← devices → drivers
4. BusTypeObject / DriverObject /    ← DeviceObject leaves:
   DeviceObject leaves               slot; OR relations; OR binding.meta;
                                     OR binding.devres
   relation locks (several)          ← ascending DeviceId order only
```

**Rules**:

- acquire the descriptor binding lock only in sleepable task context and only
  while holding no spinlock. Holding it may take the registry and per-object
  locks; the reverse order is forbidden. Registration and adoption use
  `try_lock` because they run under a spinlock, and release it only after the
  initial topology link.
- take the registry guard → snapshot the needed `Arc` handles →
  **drop the registry guard** before taking a `BusInstance` lock; do not hold
  the registry while blocking.
- per-object order is `bus.devices` → `bus.drivers`, and `bus.devices` →
  one `DeviceObject` leaf lock. The slot, `binding.meta` and `binding.devres`
  locks are separate leaves and are never nested.
- topology writes are the one place that holds several `relations` locks.
  They take exactly the locks for the child, new/old parent and every
  ancestor, **sorted by ascending `DeviceId`**, independent of parent/child
  role. That ordering rules out ABBA deadlocks between an attach in one
  direction and a detach in the other.
- the ancestor walk is bounded: `MAX_PARENT_DEPTH` edges, repeated-node
  detection, and `MAX_TOPOLOGY_RETRIES` snapshot/validate attempts. A
  missing ancestor, a depth overflow or a continuously changing topology
  yields `ResourceBusy`; a cycle yields `InvalidInput`. No unbounded loop
  runs while holding a relation lock.
- the only path from a `DeviceObject` lock back into the registry is the
  documented unregister snapshot order `registry → relations`: the registry
  entry is removed first, then the topology snapshots are cleared. The
  reverse (`relations` → registry) never happens.
- **Forbidden**: calling back into `device_registry()` while holding a
  per-object lock, other than the documented unregister snapshot.
- **Forbidden**: running a callback, doing I/O, or releasing a resource while
  holding `relations`, `slot`, `binding.meta` or `binding.devres`.
- **Forbidden**: calling driver-core mutators from a lifecycle subscriber
  callback.
- The device-node registry is independent of `DeviceRegistry`. Callers must
  finish the core lifecycle transition and release all driver-core locks before
  registering or unregistering a `DeviceNode`; `kclass` performs this from the
  lock-free event callback path.

The `BindingRuntime::begin_removing` CAS loop needs no per-object spinlock —
the lifecycle field is an `AtomicU8`, allowing lock-free reads plus a CAS
write. The metadata and devres locks are separate leaf locks in the same
binding runtime; sharing a runtime does not combine them into one critical
section.

## Concurrency Model

- **Descriptor binding lock**: one `kmutex::Mutex<()>` per registration,
  shared by the descriptor entry and its stable `DeviceObject`. It is stable
  across candidate rebuilds/failures/requeue/rebind/removal and serializes
  probe, driver-registration rescans, adoption publication, unbind, unregister
  and public topology entry points. The registry only clones its `Arc`;
  blocking acquisition happens outside the registry lock.
- **`DeviceRegistry`**: one global `SpinNoPreempt`; all lookup/add/remove
  happen under it.
- **`DeviceNodeRegistry`**: a separate global `SpinNoPreempt` protects node
  records, class index sets, subscriber lists and binding ownership. Callbacks
  receive owned clones after the lock is released.
- **`BusInstance`**: internal device/driver/controller lists each under
  `SpinNoPreempt`.
- **`DeviceObject`**: immutable facts, `relations` (`parent`/`children`/
  `child_bus`) under one `SpinNoPreempt`, and the registration/binding `slot`
  under another leaf `SpinNoPreempt`.
- **`BindingRuntime`**: `lifecycle` is an `AtomicU8` + CAS (lock-free read
  path); `usage` is `AtomicUsize`; driver metadata (`meta`) and the devres
  list each use their own `SpinNoPreempt`. A device object therefore has four
  leaf spinlocks; no path nests them. `BindingRef` capture takes only `meta`
  plus an atomic lifecycle read; its other getters are lock-free.
- **`DriverObject`**: `bound_devices` under `SpinNoPreempt`; probe
  statistics in `AtomicU64`.
- **`BusTypeObject`**: buses/drivers lists each under `SpinNoPreempt`.
- **`ProbeCounters`**: `AtomicU64` with `Relaxed` ordering (statistics
  only).
- **Id allocators**: `AtomicU64` + `Relaxed`, outside the registry lock.

Except for the descriptor binding lock, all locks are `SpinNoPreempt`
(preemption disabled, interrupts enabled). The binding lock is a sleepable
`kmutex`, so binding, adoption and removal require task context; waiting on it
may block. **None of this may be called from interrupt context.**

## Design Decisions

### Descriptor-first design

**Choice**: device discovery produces a `DeviceDesc` descriptor whose state
(`Pending`/`Probing`/`Bound`) owns matching and binding; the stable
`DeviceObject` is published with it, but the driver binding runtime is created
only when a candidate is probed.

**Trade-off**: an extra layer of abstraction and state management, in
exchange for:

- descriptors can match repeatedly (after unbind the descriptor returns to
  Pending, letting a new driver take over) while the object identity and
  topology stay fixed;
- each binding round tries every candidate in registration order once and
  stops at the first successful probe, so a later registration cannot
  overtake a binding that is in flight;
- unmatched or unclaimed registrations cost a device object but no binding
  runtime, and their `Discovered` object is a legitimate enumeration result.

**Rejected alternative**: a separate binding state machine on the object
alone. It would have to duplicate the descriptor's registration/scan
lifecycle and the per-descriptor lock that serializes probe with unregister.

### Stable device object with replaceable bindings

**Choice**: `device_desc_add` publishes one `DeviceObject`/`DeviceId` that
lives until unregister. Probe failures and unbind clear a binding slot; the
next attempt installs a brand-new `BindingRuntime` into the same object.

**Trade-off**: the object outlives its binding, so every reference type must
carry instance identity, in exchange for:

- device identity (id, topology, discovery facts) is stable across rebind,
  which is what user-visible identity and parent/child relations need;
- old `BindingRef`/`DeviceUse`/class handles stay tied to the retired
  runtime and cannot follow the object into a replacement binding;
- the descriptor alone decides whether the registration exists, so a
  concurrent probe and unregister still serialize on one lock.

**Rejected alternative**: allocating a new object/id per binding (the
previous model). Simpler reference semantics, but the id and topology change
on every rebind, and old handles either dangle or force callers to
re-discover the device.

### Ordered relation locks instead of parent→child order

**Choice**: a topology write locks the child, both parents and every
ancestor by ascending `DeviceId`, not parent-before-child.

**Trade-off**: each write must build and sort a node set and may retry a
bounded number of times, in exchange for:

- no ABBA deadlock between concurrent attach and detach that traverse the
  same edge from opposite roles;
- bounded walking (`MAX_PARENT_DEPTH`), with a repeated-node check turning
  an existing cycle into `InvalidInput` and a changing topology into
  `ResourceBusy` instead of an unbounded loop under spinlocks.

**Rejected alternative**: role-based ordering. It is shorter but two
operations can take the same pair in opposite order when the roles swap.

### Open DeviceMatcher trait instead of a closed enum

**Choice**: `DeviceMatcher` is a trait; built-in implementations
(PciIdsMatcher, VirtioTypeMatcher, ...) are peers of external ones.

**Trade-off**: the minor dynamic-dispatch cost of `&dyn DeviceMatcher`, in
exchange for:

- external crates can define custom matchers;
- `kdevice` never enumerates all possible matcher kinds;
- matchers can carry arbitrary state (e.g. FirmwareMatchSpec implements
  both `DeviceMatcher` and a `firmware_spec()` extension method).

**Rejected alternative**: a `MatchTable` enum. Every new matcher would
require editing `kdevice`, and externally defined matching logic would be
impossible.

### AtomicU8 lifecycle + CAS transitions

**Choice**: `DeviceState` is a `#[repr(u8)]` enum stored in
`BindingRuntime::lifecycle` as an `AtomicU8`.

**Trade-off**: a CAS loop is more complex than a plain spinlock, in
exchange for:

- hot-path `state()` reads take no per-object lock;
- `begin_removing` closes admission lock-free and avoids teardown decisions
  while holding a spinlock; a non-zero usage count reverts the CAS, and only
  the successful `true` return gates teardown;
- `try_acquire` uses `fetch_add` + state check, forming a correct
  concurrency protocol with `begin_removing`.

**Rejected alternative**: doing all lifecycle operations under a
`SpinNoPreempt`. Simpler concurrency, but `state()` becomes a hot-path
bottleneck (read on every ISR/poll path).

### Bucketed event subscribers instead of a global broadcast

**Choice**: subscribers are stored in 6 buckets keyed by
`DeviceEventKind`.

**Trade-off**: 6 `Vec`s cost slightly more memory than one, in exchange
for:

- dispatching an `Activated` event never scans `Removed` subscribers;
- per-event dispatch cost is proportional to that kind's subscriber count.

**Rejected alternative**: a single subscriber list with per-event
filtering. Simpler, but dispatch would walk every subscriber and filter by
event kind.

### Per-descriptor binding lock instead of priority/all-or-nothing

**Choice**: one stable sleepable lock per descriptor serializes every binder.
A round walks the candidate snapshot in registration order, stops at the first
successful probe, and rolls each failed candidate back before trying the next.

**Trade-off**: cross-thread contention blocks a task instead of failing fast,
in exchange for:

- no priority or "already attempted" bookkeeping that could starve a driver
  or make the winner depend on timing;
- a waiter always re-reads the descriptor after the lock, so it never
  double-binds or observes a half-published device;
- same-context recursion is still rejected (`Busy`) instead of self-
  deadlocking `kmutex`.

**Rejected alternatives**: priority ordering (ties and late registration make
the outcome timing-dependent) and returning `Busy` on cross-thread contention
(callers would have to invent a retry loop).

### Probe failure rollback: devres LIFO + detach_driver

**Choice**: on a candidate's probe failure:

1. `device.detach_driver()` — clear the driver metadata on this binding;
2. `device.mark_removed()` — make this binding instance terminal;
3. `device.run_cleanups()` — run driver-registered devres (LIFO);
4. dispatch `BindingRetired { id }` — instance-scoped, so class-layer
   subscribers can revoke handles published during this failed attempt;
5. `device.clear_binding()` — empty the slot; the stable object stays;
6. continue with the next candidate; when all fail, requeue the descriptor.

**Trade-off**: every probe failure pays a full rollback, in exchange for:

- partially initialized devices never leak resources (devres guarantees
  cleanup);
- a candidate that published a class runtime before failing is revoked
  through the same exact-binding retirement path, while the object itself is
  never discarded or re-identified;
- subscribers can distinguish a failed attempt (`BindingRetired`) from a
  registration being removed (`Removed`);
- the requeue lets the next reprobe try the same candidate set again on a
  fresh runtime.

**Rejected alternative**: keeping the failed driver in an `attempted` list.
That avoids retrying one driver, but also suppresses legitimate retries after
transient probe failures and complicates removal/requeue semantics.

### Registry + Arc snapshot pattern (instead of nested locks)

**Choice**: take `Arc` handles under the registry lock, drop the registry
guard, then access per-object data.

**Trade-off**: every cross-object access takes and drops the registry
guard (extra boilerplate), in exchange for:

- no deadlock risk: a per-object lock never calls back into the registry,
  and the per-object leaf locks never nest;
- `Arc` keeps objects alive after the registry guard drops (reference
  counting).

**Rejected alternative**: one global big lock or nested locking. A global
lock is simple but slow; nesting deadlocks easily.

## Drop / Resource Release

- `DeviceUse::drop` decrements the `BindingRuntime::usage` of the exact
  instance admitted by `try_acquire`; `begin_removing` may proceed once the
  count reaches zero. A read-only `BindingRef` never changes this count, so
  class or identity handles cannot keep a device busy.
- Unbind clears the slot only after `Removed`; the retired runtime stays alive
  for as long as a `BindingRef` or `DeviceUse` retains it, but it can never be
  re-installed.
- `DeviceObject` does not implement `Drop` — cleanup is driven explicitly
  by `remove_device_managed`/`unregister_device_desc` (devres LIFO).
- `BindingRuntime` does not implement `Drop` either. Dropping it only frees
  the remaining callback boxes; it is not a substitute for explicit teardown
  and does not imply hardware is quiesced.
- `DeviceRegistry` does not implement `Drop` — it is a global static.
- `DeviceTopology` is a plain data snapshot; dropping it frees the `Vec`s.

## OS capability ownership after device-res retirement

ResourceDesc/ResourceSet belong to device::resource. device::managed acquires
driver-os-api guards through explicit providers and registers them through
`BindingRef::add_cleanup(&DeviceObject, ...)`, which verifies that the captured
runtime is still the object's current binding before queuing the callback.
DeviceResource and the unused ResourceProvider aggregate are removed without a
replacement facade.
Cleanup remains framework-triggered, LIFO and outside the binding's devres
lock; the batch belongs to that binding's runtime. Failed acquisition
registers nothing. A second drain with no new registrations is empty; cleanup
registered by a running callback waits for the next drain. Raw MMIO/DMA
views expire at cleanup. Hardware/users must be quiesced first and release must
run in the provider's permitted context. Allocation policy and legacy
remove-error handling are unchanged.
