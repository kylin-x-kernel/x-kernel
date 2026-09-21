# kdevice — Design

## Purpose

`kdevice` is x-kernel's shared device-model type crate. It provides the
stable type definitions for the bus / device / driver triple, the global
object registries, the lifecycle state machine, Linux-compatible device-node
identity assignment, and event distribution infrastructure.

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
- **Explicit ownership**: `DeviceObject` owns its devres cleanup entries;
  `DeviceRegistry` owns the global indexes; lifecycle transitions are
  atomic through `AtomicU8` + CAS;
- **Event-driven**: lifecycle events (Published/Matched/Bound/Activated/
  Removed) are delivered to observers such as `kclass` through bucketed
  subscribers.

## Scope

```text
drivers/contracts/kdevice/
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
    │   ├── desc.rs               # DeviceDesc, DeviceId, DeviceLocation,
    │   │                         #   DeviceIdentity, DeviceState, DeviceRecord
    │   ├── object.rs             # DeviceObject (core runtime object),
    │   │                         #   DeviceUse (RAII usage-count guard)
    │   ├── handles.rs            # BusHandle, DeviceCore, DriverCore
    │   └── resource.rs           # resource type re-exports
    ├── device_node.rs            # DeviceNode identity, class allocators,
    │                             # snapshots and lifecycle subscribers
    ├── driver/
    │   └── mod.rs                # DriverId, DriverInfo, DriverObject,
    │                             #   DeviceDriver trait, DeviceMatcher impls,
    │                             #   ProbeStats, priority module
    ├── lifecycle/
    │   ├── mod.rs                # lifecycle orchestration:
    │   │                         #   register_bus_instance, register_driver_object,
    │   │                         #   probe_device_desc, adopt_active_device,
    │   │                         #   remove_device_managed, parent attach/detach
    │   ├── dispatch.rs           # event distribution and state transition notify
    │   ├── event.rs              # DeviceEvent, DeviceEventKind
    │   └── subscribers.rs        # DeviceEventSubscribers (bucketed storage)
    ├── registry/
    │   └── mod.rs                # DeviceRegistry (global BTreeMap indexes),
    │                             #   query/snapshot/test helpers
    └── topology/
        └── mod.rs                # DeviceTopology (read-only topology snapshot),
                                  #   BusView, DeviceCoreView, DriverCoreView
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
    │  │   mark_matched → bind_to_driver →     │    │
    │  │   activate → dispatch_event           │    │
    │  └──────────────┬───────────────────────┘    │
    │                 │                             │
    │  ┌──────────────┴───────────────────────┐    │
    │  │   lifecycle::subscribers              │    │
    │  │   buckets: [Published][Matched]       │    │
    │  │            [Bound][Activated][Removed] │    │
    │  └──────────────────────────────────────┘    │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   device_node::DeviceNodeRegistry     │    │
    │  │   DeviceId -> DeviceNode              │    │
    │  │   eventN/cardN allocation + events    │    │
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
    │  │   devices:     BTreeMap<DeviceId, ...>│    │
    │  │   buses:       BTreeMap<BusId, ...>   │    │
    │  │   bus_types:   Vec<BusTypeObject>     │    │
    │  │   drivers:     BTreeMap<DriverId, ...>│    │
    │  │   subscribers: DeviceEventSubscribers │    │
    │  └──────────────────────────────────────┘    │
    │                                              │
    │  ┌──────────┐ ┌───────────┐ ┌────────────┐  │
    │  │BusInstance│ │DeviceObject│ │DriverObject│  │
    │  │(per-bus  │ │(per-device│ │(per-driver │  │
    │  │SpinLock) │ │SpinLock + │ │SpinLock +  │  │
    │  │          │ │AtomicU8)  │ │AtomicU64)  │  │
    │  └──────────┘ └───────────┘ └────────────┘  │
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
| `DeviceObject` | Core runtime device object; `AtomicU8` lifecycle state + CAS transitions; devres cleanup list; usage count |
| `DeviceUse` | RAII guard; blocks device removal while held |
| `BusInstance` | Runtime bus instance; controller / devices / drivers lists; probe statistics |
| `BusTypeObject` | Bus matching domain; manages the pending descriptor queue and registered drivers; delegates matching to the `BusType` trait |
| `DriverObject` | Registered driver object; bound-device list; probe statistics |
| `DeviceDesc` | Discovery-phase descriptor; carries bus_id/location/identity/transport/resources |
| `DeviceDriver` trait | Driver interface: name/device_kind/bus_types/matcher/probe_device/remove/suspend/resume/shutdown |
| `DeviceMatcher` trait | Open matcher; built-ins PciIdsMatcher/VirtioTypeMatcher/CompatibleAliasMatcher/FirmwareMatchSpec/NeverMatcher |
| `DeviceEvent` / `DeviceEventKind` | The 5 lifecycle events; bucketed subscribers |
| `DeviceTopology` | Read-only topology snapshot; bus/driver-filtered iterators |
| Lifecycle API | `register_bus_instance`, `register_driver_object`, `probe_device_desc`, `adopt_active_device`, `remove_device_managed` |

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

Device-node callbacks run outside the registry lock and may allocate or enter
filesystem code, but they must run in normal task context. The host lifecycle
publisher must be installed before the first device node; late installation is
rejected instead of backfilling synthetic add events.

## State Machine

### DeviceRecord lifecycle

```text
                         ┌────────────┐
                         │ Discovered │  ← device_desc_add / adopt_active_device
                         └─────┬──────┘
                               │ probe_device_desc finds a matching driver
                               ▼
                         ┌────────────┐
                         │  Matched   │  ← mark_device_matched()
                         └─────┬──────┘
                               │ bind_device_to_driver()
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
                               │ remove_device_managed()
                               ▼
                         ┌────────────┐
                         │  Removing  │  ← begin_removing() CAS commit point;
                         │            │     driver.remove() + bus_type.remove()
                         │            │     + run_cleanups()
                         └─────┬──────┘
                               │ remove_device_from_index()
                               ▼
                         ┌────────────┐
                         │  Removed   │  ← Removed event fired
                         └────────────┘
```

| From | To | Trigger |
|----|----|----------|
| — | Discovered | `device_desc_add` or `adopt_active_device` creates the DeviceObject |
| Discovered | Matched | `probe_device_desc` finds the best-matching driver |
| Matched | Bound | `bind_device_to_driver` records driver_id/name/kind |
| Bound | Active | `DeviceDriver::probe_device` returns `Ok(())` |
| Bound | Discovered (requeue) | probe failed; desc requeued, device object dropped |
| Active/MBound/Bound | Removing | `remove_device_managed` CAS succeeds |
| Removing | Removed | driver.remove + bus_type.remove + run_cleanups complete |

The `begin_removing` CAS loop uses `compare_exchange_weak`:

- only `Active`/`Bound`/`Matched`/`Discovered` may transition to `Removing`;
- an existing `Removing` or `Removed` state is rejected (re-entry guard);
- a non-zero usage count rolls back and returns `ResourceBusy`;
- after a successful CAS the transition is irreversible.

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
              │Bound(id) │ │ Pending  │◄┘ requeue
              └──────────┘ └──────────┘
                    │
                    │ remove_device_from_index
                    ▼
              ┌──────────┐
              │ Pending  │  ← device removed, descriptor re-opened
              └──────────┘
```

Key semantics:

- the `Probing` state prevents concurrent probes of the same descriptor;
- the `attempted` list records failed drivers so reprobe skips them and
  avoids infinite retries;
- when a device is removed, its descriptor returns to `Pending` with
  `attempted` cleared, giving new drivers a chance.

## Flows

### Probe pipeline

`probe_device_desc(id)` → `probe_device_desc_with_drivers(desc, candidates)`:

1. Check `desc_probe_outcome`: if the descriptor is already `Bound` and the
   device is terminal (Active/Removing/Removed), return Skipped.
2. CAS the descriptor to `Probing` (concurrency guard).
3. Get the candidate driver list from the `bus_type`.
4. Match with `match_desc`, selecting the highest-priority driver
   (excluding drivers already in `attempted`).
5. No matching driver → requeue and return `Requeue`.
6. Create the `DeviceObject` from the descriptor (allocate DeviceId,
   construct the object).
7. Run the standard event sequence: `mark_matched` → `bind_to_driver`.
8. Call `driver.ops().probe_device(device)`:
   - success → `publish_desc_device`: write the global index, add to the
     BusInstance, attach parent, fire Published + Activated events.
   - failure → `device.run_cleanups()` (devres LIFO) + `detach_driver()` +
     record the failed driver in `attempted` + requeue.

### Adoption path

`adopt_active_device(adoption)`: for devices initialized before the core
runs, such as the boot console or the PCI host bridge.

1. Verify the target bus exists and the driver's `bus_types` include the
   target bus's `bus_type`.
2. Allocate `desc_id` and `device_id`.
3. Construct `DeviceDesc` + `DeviceObject` (skipping match/probe).
4. Run the same standard event sequence as probe:
   Matched → Bound → Published → Activated.
5. Attach the parent if the adoption names one.

### Remove path

`remove_device_managed(id)`:

1. Look up device, driver, and bus_type from the registry.
2. `device.begin_removing()` — the single commit point:
   - CAS into `Removing`; rejects `Removing`/`Removed` states and non-zero
     usage.
3. Call `driver.ops().remove(device)` — best-effort; errors are logged and
   do not stop the removal.
4. Call `bus_type.remove(device)` — best-effort.
5. `device.run_cleanups()` — devres cleanup in LIFO order.
6. `remove_device_from_index(id)` — remove from the registry, update
   BusInstance/DriverObject, detach parent/child, fire the Removed event.

### Descriptor rescan after driver registration

`register_driver_object` immediately scans all pending descriptors in the
new driver's bus_types:

1. Collect pending descriptor ids from each `BusTypeObject`.
2. Call `probe_device_desc_with_drivers` per descriptor (candidates
   limited to the newly registered driver).
3. This removes driver load-order problems: a driver registered later
   still finds already-discovered devices.

## Lock Ordering Rules

The driver core uses multiple `SpinNoPreempt`-protected objects. To
prevent deadlock, every path must follow:

```text
1. Registry (DeviceRegistry)     ← always acquired first
2. BusInstance / BusTypeObject   ← after snapshotting Arcs, drop the
                                    registry guard before accessing
3. DeviceObject / DriverObject   ← innermost
```

**Rules**:

- take the registry guard → snapshot the needed `Arc` handles →
  **drop the registry guard** → access per-object locks.
- **Forbidden**: calling back into `device_registry()` while holding a
  per-object lock.
- **Forbidden**: calling driver-core mutators from a lifecycle subscriber
  callback.
- The device-node registry is independent of `DeviceRegistry`. Callers must
  finish the core lifecycle transition and release all driver-core locks before
  registering or unregistering a `DeviceNode`; `kclass` performs this from the
  lock-free event callback path.

The `DeviceObject::begin_removing` CAS loop needs no per-object spinlock —
the lifecycle field is an `AtomicU8`, allowing lock-free reads plus a CAS
write.

## Concurrency Model

- **`DeviceRegistry`**: one global `SpinNoPreempt`; all lookup/add/remove
  happen under it.
- **`DeviceNodeRegistry`**: a separate global `SpinNoPreempt` protects node
  records, class index sets, and subscriber lists. Callbacks receive owned
  clones after the lock is released.
- **`BusInstance`**: internal device/driver/controller lists each under
  `SpinNoPreempt`.
- **`DeviceObject`**: `lifecycle` is an `AtomicU8` + CAS (lock-free read
  path); `state` (parent/children/driver binding) uses `SpinNoPreempt`;
  `usage` uses `AtomicUsize`.
- **`DriverObject`**: `bound_devices` under `SpinNoPreempt`; probe
  statistics in `AtomicU64`.
- **`BusTypeObject`**: buses/pending_descriptors/drivers lists each under
  `SpinNoPreempt`.
- **`ProbeCounters`**: `AtomicU64` with `Relaxed` ordering (statistics
  only).
- **Id allocators**: `AtomicU64` + `Relaxed`, outside the registry lock.

All locks are `SpinNoPreempt` (preemption disabled, interrupts enabled), so
**none of this may be called from interrupt context**.

## Design Decisions

### Descriptor-first design

**Choice**: device discovery produces a `DeviceDesc` descriptor; it does
not directly create a `DeviceObject`. The two lifecycles are decoupled.

**Trade-off**: an extra layer of abstraction and state management, in
exchange for:

- descriptors can match repeatedly (after device removal the descriptor
  returns to Pending, letting a new driver take over);
- the descriptor's `attempted` list tracks failed drivers and avoids
  repeated probes;
- a `DeviceObject` is created only at probe time; unmatched devices never
  pay for a runtime object.

**Rejected alternative**: creating the `DeviceObject` at discovery. Simpler
code, but loses the descriptor's independent lifecycle management.

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
`DeviceObject::lifecycle` as an `AtomicU8`.

**Trade-off**: a CAS loop is more complex than a plain spinlock, in
exchange for:

- hot-path `state()` reads take no per-object lock;
- `begin_removing` is a lock-free CAS commit point, avoiding complex
  teardown decisions while holding a spinlock;
- `try_acquire` uses `fetch_add` + state check, forming a correct
  concurrency protocol with `begin_removing`.

**Rejected alternative**: doing all lifecycle operations under a
`SpinNoPreempt`. Simpler concurrency, but `state()` becomes a hot-path
bottleneck (read on every ISR/poll path).

### Bucketed event subscribers instead of a global broadcast

**Choice**: subscribers are stored in 5 buckets keyed by
`DeviceEventKind`.

**Trade-off**: 5 `Vec`s cost slightly more memory than one, in exchange
for:

- dispatching an `Activated` event never scans `Removed` subscribers;
- per-event dispatch cost is proportional to that kind's subscriber count.

**Rejected alternative**: a single subscriber list with per-event
filtering. Simpler, but dispatch would walk every subscriber and filter by
event kind.

### Probe failure rollback: devres LIFO + detach_driver

**Choice**: on probe failure:

1. `device.run_cleanups()` — run driver-registered devres (LIFO);
2. `device.detach_driver()` — clear the driver binding;
3. record the failed driver in `attempted`;
4. requeue the descriptor.

**Trade-off**: every probe failure pays a full rollback, in exchange for:

- partially initialized devices never leak resources (devres guarantees
  cleanup);
- the attempted list prevents retrying the same driver forever;
- the requeue lets the next reprobe try a different driver.

**Rejected alternative**: dropping the descriptor on probe failure. That
cannot support "several drivers may match one device, tried by priority".

### Registry + Arc snapshot pattern (instead of nested locks)

**Choice**: take `Arc` handles under the registry lock, drop the registry
guard, then access per-object data.

**Trade-off**: every cross-object access takes and drops the registry
guard (extra boilerplate), in exchange for:

- no deadlock risk (registry lock and per-object locks never nest);
- `Arc` keeps objects alive after the registry guard drops (reference
  counting).

**Rejected alternative**: one global big lock or nested locking. A global
lock is simple but slow; nesting deadlocks easily.

## Drop / Resource Release

- `DeviceUse::drop` decrements `DeviceObject::usage`; `begin_removing` may
  proceed once the count reaches zero.
- `DeviceObject` does not implement `Drop` — cleanup is driven explicitly
  by `remove_device_managed` (devres LIFO).
- `DeviceRegistry` does not implement `Drop` — it is a global static.
- `DeviceTopology` is a plain data snapshot; dropping it frees the `Vec`s.
