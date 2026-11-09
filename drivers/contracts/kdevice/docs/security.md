# kdevice — Security And Reliability

## Scope

This analysis covers the entire `kdevice` crate: `src/lib.rs`,
`src/bus/`, `src/device/`, `src/device_node.rs`, `src/driver/`,
`src/lifecycle/`, `src/registry/`, `src/topology/`, and `ut-tests/`. No modules are excluded; all
state machines and locks are reachable from the lifecycle API audited
below.


## Trust Model

```text
    kdriver (bus enumeration,    kclass / knet / ...
    probe, adoption, remove)         │
         │                           │ subscribe_event
         │ lifecycle APIs            │ query / snapshot
         ▼                           ▼
┌─────────────────────────────────────────────┐
│ kdevice                                     │
│                                             │
│ all-safe Rust — zero unsafe blocks          │
│                                             │
│ Safety comes from:                          │
│  ├─ SpinNoPreempt locks on all mutable      │
│  │  shared state                            │
│  ├─ AtomicU8 + CAS device lifecycle state   │
│  ├─ Arc reference counts keeping objects    │
│  │  alive across lock boundaries            │
│  ├─ BTreeMap preventing ID collisions       │
│  └─ strict lock-ordering rules preventing   │
│     deadlock                                │
└──────────────┬──────────────────────────────┘
               │
               │ DeviceEvent dispatch
               ▼
    kclass (event subscriber)
```

- `kdevice` trusts callers (`kdriver`) to implement the `DeviceDriver`
  trait correctly — in particular that `probe_device` finishes device
  initialization on success and leaks nothing on failure.
- `kdevice` trusts callers to follow the lock-ordering rules (no registry
  callbacks while holding a per-object lock).
- `kdevice` trusts subscriber callbacks not to call driver-core mutators
  (subscriber contract).
- `kdevice` trusts `kclass` to register device nodes only for active runtime
  capabilities and to provide bounded, driver-derived class metadata.
- `kdevice` trusts the firmware backends behind `FwNodeOps<'static>` to keep
  their storage alive for the kernel lifetime and to answer only for handles
  they issued. In fallible queries an unknown handle must surface as
  `FwError::InvalidNode`, not as memory unsafety; infallible accessors such as
  `name` and `parent` return `None`. The handle itself is opaque to
  `kdevice`.
- `kdevice` is called only from process/task context (it uses no
  IRQ-safe locks). Firmware-node queries are also normal-context operations;
  the contract does not make backends interrupt-safe.

## External Boundaries / Attack Surface

`kdevice` is a pure in-memory data-structure layer that never touches
hardware, firmware, or network input. Its safety boundaries are:

- **Concurrency correctness**: multiple CPUs reach DeviceRegistry /
  BusInstance / DeviceObject simultaneously through kdriver paths. A lock-
  ordering violation can deadlock; a CAS logic error can corrupt the
  state machine.
- **State machine integrity**: `DeviceState` transition correctness
  underpins the whole device model. An illegal transition (Removing →
  Active) could produce use-after-free.
- **Subscriber callback safety**: subscribers run outside kdevice locks,
  but a callback violating the contract (calling a driver-core mutator)
  could deadlock or corrupt state.
- **Device-node projection integrity**: devfs and sysfs trust one
  `DeviceNode` snapshot for class index, major/minor, `devname`, driver name,
  topology, and input metadata. A mismatched or prematurely reused assignment
  would create inconsistent user-visible device identity.
- **ID allocator overflow**: the `AtomicU64` id counters can theoretically
  overflow (~1.8×10^19 allocations), after which they panic.
- **Arc reference cycles**: DeviceObject parent→child and child→parent
  `Arc`s are broken explicitly by detach in the remove path; missing that
  leaks memory.

The threat analysis should focus on:

- whether lock-ordering violations are possible (audit every callback
  path taken while a registry guard is held);
- DeviceState CAS loop correctness under concurrency;
- whether races between `begin_removing` and `try_acquire` can produce
  use-after-free;
- whether a subscriber callback panic affects other subscribers or the
  driver core;
- whether `DeviceRegistry::find_bus_type`'s panic-on-missing can trigger
  on an abnormal path.

## Unsafe Code Inventory

**kdevice contains no unsafe code.**

The whole crate (~3500 lines) is 100% safe Rust. All concurrency control
uses reviewed safe abstractions: the descriptor's `kmutex::Mutex`, and
`SpinNoPreempt`, `AtomicU8`, `AtomicU64`, `AtomicUsize`, and `Arc` for the
rest.

## Memory-Safety Invariants

1. **Lock-ordering rule**: descriptor binding lock → Registry → (drop guard)
   → per-object (`bus.devices` → `bus.drivers`; `bus.devices` →
   `DeviceObject` state). The binding lock is a sleepable `kmutex` and must
   be acquired outside every spinlock. Every caller must follow this order.
   `DeviceRegistry::find_bus_type` documents its panic-on-missing behavior
   and requires bus types to be registered at init time.
2. **DeviceObject::begin_removing atomicity**: the CAS loop is the only
   `→Removing` commit point. After a successful CAS the transition is
   irreversible, and the usage count must be checked to be zero.
   Violating this can cause use-after-free.
3. **DeviceUse RAII guard**: `try_acquire` uses `fetch_add` + a state
   check; `begin_removing` checks the usage count after its CAS. Together
   they guarantee: either `begin_removing` is rejected while a `DeviceUse`
   exists, or after `begin_removing` succeeds, `try_acquire` sees a
   non-Active state and rolls the count back.
4. **DeviceDesc Probing mutual exclusion**: only the holder of the
   descriptor's binding lock may call `mark_device_desc_probing` (which
   accepts `Pending → Probing` only) or requeue it. Every binder acquires
   that lock and re-reads the state, so concurrent probes serialize and a
   waiter never trusts a pre-wait snapshot. Same-context recursion is
   rejected with `ProbeOutcome::Busy` before the recursive `kmutex`
   acquisition.
5. **Subscriber contract**: subscriber callbacks run without registry or
   per-object spinlocks, but the emitting path still holds the transitioning
   descriptor's binding lock; callbacks must not call mutators. Violations
   can deadlock or corrupt re-entrant state.
6. **Arc lifetime**: every object crossing a lock boundary is shared via
   `Arc`. Objects stay alive after the registry guard drops, preventing
   use-after-free.
7. **ID uniqueness**: `AtomicU64` ids increase monotonically and are
   never reused. Even with wraparound, stale BTreeMap entries cannot
   collide with new ids (extremely improbable but theoretically
   possible).
8. **devres LIFO order**: `run_cleanups` pops from a `Vec` (LIFO), so the
   last-acquired resource is released first.
9. **Remove is irreversible**: after `begin_removing`'s CAS succeeds, a
   failing `driver.remove()` or `bus_type.remove()` never rolls back to
   Active. A partially cleaned device must never look "available".
10. **Parent/child detach**: the remove path removes the child from the
    parent's children list and clears the child's parent pointer,
    preventing stale parent references.
11. **Device-node assignment consistency**: class index, `DeviceNumber`, and
    `devname` are created together under the node-registry lock and stored in
    one immutable `DeviceNode` publication with shared atomic lifecycle state.
12. **No premature node replacement or index reuse**: removal marks the node
    unavailable but keeps it reserved through `Removing`, host remove
    publication, and `Removed` callbacks. The same core ID returns
    `ResourceBusy`, and the class index is released only after the sequence.
13. **Lifecycle event ordering**: projection add precedes host add publication;
    projection quiesce precedes host remove publication, which precedes
    projection deletion. A removal request that overlaps add publication is
    retained as `RemovePending` and completed by the add-publishing thread, so
    callbacks cannot observe remove before add.
14. **Firmware-node handles are borrowed, not owned**: `FwNode<'static>` is a
    `Copy` handle containing the backend `FwNodeOps` reference and a
    backend-local id. `kdevice` copies it descriptor → object and never
    dereferences backend storage. The `'static` bound prevents storing a
    short-lived backend borrow, but it does not detect a deliberately leaked
    runtime backend; publication policy must restrict nodes to boot-static
    storage. An invalid handle is a typed backend error, and a missing node is
    `None`, never a default handle.
15. **Publish/remove binding window**: the binding metadata (Matched/Bound) is
    prepared before a device becomes visible, and the binding lock is held
    from before registry publication until after `Activated` (probe/adoption)
    or until after `Removed` and all relation teardown (remove). A remover
    therefore never observes a half-published device, and a waiter never
    rebinds a half-removed one. Bus publication attaches the device and its
    driver in one `devices → drivers` critical section; bus detachment keeps
    the shared driver entry while any remaining device still references it.

## Thread Safety

| Type | Send condition | Sync condition |
|------|-----------|-----------|
| `DeviceRegistry` | fields are `Send` | `SpinNoPreempt` provides interior mutability |
| `DeviceNodeRegistry` | maps, index sets, and callbacks are `Send` | a separate `SpinNoPreempt` serializes assignment and snapshots |
| `DeviceObject` | `Arc<DeviceObject>` is `Send + Sync` | `AtomicU8` + `SpinNoPreempt` + `AtomicUsize` |
| `BusInstance` | `Arc<BusInstance>` is `Send + Sync` | internal `SpinNoPreempt` guards all mutable fields |
| `BusTypeObject` | `Arc<BusTypeObject>` is `Send + Sync` | internal `SpinNoPreempt` |
| `DriverObject` | `Arc<DriverObject>` is `Send + Sync` | internal `SpinNoPreempt` + `AtomicU64` |
| Descriptor binding lock | `Arc<kmutex::Mutex<()>>` is `Send + Sync` | sleepable lock; acquired only outside spinlocks |
| `DeviceTopology` | plain `Vec` data, `Send` | immutable snapshot, `Sync` |
| `DeviceEventSubscribers` | `Vec<Arc<dyn Fn>>` is `Send` | serialized through `DeviceRegistry`'s `SpinNoPreempt` |

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-01 | Lock-ordering violation deadlocks | High | Calling `device_registry()` while holding a per-object lock | Lock-ordering rules documented; `BusInstance`/`DeviceObject` methods are `pub(crate)` to limit direct use |
| T-02 | `begin_removing` vs `try_acquire` race causes use-after-free | High | Timing: `try_acquire` fetch_add lands, then `begin_removing` CAS succeeds before the state check | `try_acquire` does `fetch_add` then checks state, rolling back with `fetch_sub` if not Active; `begin_removing` checks usage after its CAS. The two are mutually exclusive |
| T-03 | Same descriptor probed concurrently | Medium | Two tasks probe the same `DeviceDescId` at once | Both serialize on the descriptor's binding lock; the waiter re-reads the state and returns `Skipped` for a terminal binding or probes a `Pending` one. Same-task recursion returns `Busy` without re-locking. |
| T-04 | Subscriber callback panic blocks later subscribers | Medium | One callback panics; later ones are skipped | No `catch_unwind` today; callbacks run in iteration order; subscriber quality relied upon |
| T-05 | Subscriber callback calls a mutator, deadlocking | High | A subscriber calls `probe_device_desc` and friends | Not enforced by code: the subscriber contract is documentation-only (`lifecycle/subscribers.rs` has no runtime check), so prevention relies on auditing subscriber implementations — recorded here as an accepted, audit-enforced rule. |
| T-06 | ID counter overflow | High — a kernel panic prevents identity reuse after exhaustion | `AtomicU64` reaches `u64::MAX` | `fetch_add_u64` panics instead of returning a duplicate id; exhaustion is practically unreachable at a 64-bit width. |
| T-07 | `find_bus_type` panics on a missing bus type | Medium | A bus instance is created before `register_bus_type` | `default_bus_manager` fixes the order: register_bus_type before register_bus_instance |
| T-08 | Driver probe failure leaves devres uncleaned | Medium | Driver registers devres then panics during probe (unwinding skips cleanup) | x-kernel is `#![no_std]` with no unwinding today; probe failure cleans up explicitly via `run_cleanups` |
| T-09 | Parent/child relations survive device removal | Medium | The remove path forgets to detach | `remove_device_from_index` removes the child from the parent's list and clears children's parent pointers |
| T-10 | `DeviceRecord` snapshot inconsistent with the live object | Low | Snapshot built under one lock, fields read under another | `record_snapshot()` reads the `AtomicU8` lifecycle first, then takes the per-object lock for state fields |
| T-11 | Arc reference cycles leak device/bus objects | Medium | parent↔child `Arc`s never broken | Remove path runs explicit `detach_child` + `set_parent(None)`; controller↔bus `set_child_bus`/`set_controller` are managed by backends |
| T-12 | A class index or core ID is reused before projections remove the old node | Medium — stale files or operations could collide with a replacement node | Hot-unplug races a new device in the same class | Removing nodes remain in the registry and unavailable; same-ID registration returns `ResourceBusy`, and the index is released only after `Removed` callbacks return |
| T-13 | Filesystem projections disagree about device identity | Medium — `/dev` and `/sys` could describe different devices | A consumer independently derives names or numbers | `DeviceNode` is the single immutable source consumed by both devfs and sysfs; allocation templates are private to kdevice |
| T-14 | Concurrent registration and removal invert add/remove events | Medium — listeners retain a ghost node after observing remove followed by add | Removal begins after registry insertion but before add callbacks finish | Publishing transitions to `RemovePending`; the add-publishing thread emits add first, then performs the complete ordered removal sequence |
| T-15 | A stale or invalid firmware-node handle reaches a probe | Medium — probe reads another node or fails late | Backend-local ids are reused or a descriptor outlives its backend | `FwNode` equality includes the backend pointer; fallible backend queries must return `FwError::InvalidNode` for unknown ids (infallible name/parent accessors return `None`); the `'static` type bound keeps boot backends alive for the kernel lifetime |
| T-16 | Remove races a probe/adoption and tears down the new identity | High — new device loses its bus/driver relations or a half-removed object is rebound | Remove waits for the binding lock, then a binder acquires it before teardown completes | Remove holds the descriptor binding lock from the commit point through `Removed`; a binder waits, rechecks, and only then sees `Pending` |
| T-17 | Shared driver entry removed while a concurrent publish still uses it | Medium — bus driver list loses an active driver | Last-device remove overlaps a publish on the same bus | `detach_device_and_orphan_driver` checks remaining `driver_id()` values under the same `bus.devices` guard that removes the device, so it never removes a driver another device still references |

Impact levels:

- High: UB, memory corruption, deadlock.
- Medium: panic, resource leak, inconsistent state.
- Low: performance degradation, lost logs, degraded functionality.

## Failure Modes And Effects Analysis

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
|------|----------|----------|--------|------|----------|----------|
| F-01 | Bus type not registered before driver registration | `register_bus_type` called after `register_driver_object`/`register_bus_instance` | `find_bus_type` panics | Kernel boot aborts | 1 | `default_bus_manager` registers bus types first; init order is fixed in code |
| F-02 | Probe cannot create a DeviceObject | `Arc::new` / `Vec` allocation fails (OOM) | Probe fails; descriptor requeued | Device may activate on a later reprobe | 3 | OOM is not handled today (`#![no_std]`); an OOM hook may come later |
| F-03 | `begin_removing` CAS loop starves | Other threads keep mutating lifecycle; CAS keeps failing | That remove is delayed | Device removal delayed | 4 | `compare_exchange_weak` + retry loop; the race window is tiny |
| F-04 | `driver.remove()` panics in the remove path | Driver remove implementation bug | Removal aborted mid-way | Device stuck in Removing, never fully cleaned | 2 | No `catch_unwind` today; driver remove quality relied upon |
| F-05 | Allocation fails inside a subscriber callback | `Arc::new` OOM in callback | Depends on the callback | Possible panic or silently lost event | 3 | kdevice allocates nothing in callbacks; OOM handling belongs to the subscriber |
| F-06 | `device_records_snapshot` memory pressure with many devices | Thousands of active devices require a large `Vec` | Slower snapshot; possible OOM | Affects snapshot consumers (e.g. /proc reads) | 4 | The BTreeMap iterator yields records in id order; callers can paginate |
| F-07 | Same DeviceId added twice | Two adoptions reusing an id | The second `add_device` replaces the first entry | The old DeviceObject may drop while external `Arc` holders persist | 4 | The id allocator is monotonic; adoption allocates a fresh id every time |
| F-08 | `desc.parent` points at a removed parent | Parent device removed before the child is published | `attach_device_parent` runs after publish; parent may be gone | Child has no parent; device tree incomplete | 4 | `attach_device_parent` failure logs a warning and does not block child activation |
| F-09 | BusInstance.devices inconsistent with DeviceRegistry.devices | One path updated only one side | Snapshots may miss or include stale devices | Inconsistent query results | 3 | Publish adds to the registry and then to the bus under the binding lock; remove deletes from the registry and atomically detaches from the bus under the same lock, then fires `Removed` |
| F-10 | Same-task recursive probe self-deadlocks the binding lock | A probe calls `probe_device_desc` for its own descriptor | `kmutex` would panic on recursive locking | Probe path aborts | 3 | `probe_device_desc` checks `is_owned_by_current()` and returns `Busy`; the registration scan skips local re-entry with a warning |
| F-11 | Device-node class index range is exhausted | More than 32 input or 64 DRM primary devices are active | New node registration returns `ResourceBusy` | The runtime device remains active but has no Linux node projection | 3 | Existing assignments remain unchanged; kclass logs the failure |
| F-12 | Device-node subscriber panics | Projection callback has a kernel bug | Later callbacks do not run | One or more filesystem projections may miss the transition before panic aborts | 2 | Callbacks run without registry locks; panic isolation is unavailable in the current panic=abort kernel |

Severity levels:

- 1: fatal — system crash, data loss.
- 2: serious — function unavailable until restart.
- 3: moderate — degraded function, self-recoverable.
- 4: minor — limited impact, tolerable.

## Failure Management

- `DeviceRegistry::find_bus_type` panics when the bus type is missing —
  that is an init-order bug and must not happen at runtime.
- `DeviceObject::state()` returns `DeviceState::Removed` (rather than
  panicking) when `from_u8` decoding fails, so memory corruption cannot
  take down hot paths.
- All lifecycle APIs return `Result<_, DriverError>` (`InvalidInput` /
  `BadState` / `ResourceBusy` / `Unsupported`, ...).
- `ResourceBusy` means the call did not commit a new binding or teardown:
  another remover may already have won, or references are still outstanding,
  so this call performed no partial transition. A caller may retry later, but
  the interface guarantees no eventual completion and runs no automatic retry.
- Errors from `driver.remove()` and `bus_type.remove()` inside
  `remove_device_managed` are only logged and never stop the removal
  ("remove never fails" semantics).
- A candidate probe failure runs the full rollback (`run_cleanups` +
  `detach_driver` + `mark_removed`) and continues with the next candidate;
  when the round ends the descriptor is requeued. A partially initialized
  device is never leaked and never receives lifecycle events.
- The descriptor binding lock is released only after the terminal lifecycle
  step (`Activated`, or `Removed` plus relation teardown), so a waiting
  binder never observes an intermediate publication/removal state.
- Subscriber callback panics are unprotected, but x-kernel runs
  `#![no_std]` without unwinding (panic = abort), so later subscribers
  cannot be silently skipped.

## Privacy Analysis

`kdevice` handles device identity data (`DeviceIdentity`: PCI
vendor/device ID/class, platform alias/firmware_id), bus topology
(`DeviceLocation`, `BusInfo`), device metadata (`DeviceRecord`), and
Linux-facing device-node identity and input capability metadata, visible
through `Debug` output and snapshot functions.

The module processes no user data, persists nothing, and logs nothing
itself (all logging belongs to upper layers such as `kdriver`).

## Known Limitations

- No `catch_unwind` protection: panics in subscriber callbacks or
  `driver.remove()` would leave inconsistent state (moot under the
  current panic=abort configuration).
- The id allocator uses 64-bit counters that can theoretically overflow
  (practically impossible; no compile-time or runtime check).
- `DeviceRegistry` uses `BTreeMap` instead of `HashMap`: O(log n)
  insert/lookup instead of O(1), in exchange for no hashing dependency.
- Subscribers cannot unsubscribe: once registered, a callback lives as
  long as the registry.
- Device-node allocation currently supports only input event nodes and DRM
  primary nodes, with Linux ABI-sized pools of 32 and 64 entries.
- `DeviceTopology::snapshot()` allocates a fresh `Vec` on every call; no
  caching.
- Parent/child relations are single-level (no cross-bus hierarchy); a
  multi-level device tree would need a recursive children structure.
- No IOMMU or device-isolation type abstractions; those belong to kernel
  adaptation layers.
- Firmware-node association supports boot-static backends only. There is no
  ownership, refcount or teardown protocol for a dynamically unloaded backend,
  so such a backend must not be published as `FwNode<'static>`.

## Audit Checklist

When modifying this module, verify:

- No new `unsafe` blocks are added (currently zero unsafe; keep it that
  way).
- New per-object locks do not violate the lock-ordering rule
  (Registry → per-object).
- New lifecycle transitions update the `DeviceState` `as_u8`/`from_u8`
  and `repr(u8)` mapping.
- New subscriber event kinds update `DeviceEventKind::COUNT` and
  `index()`.
- New registry queries never hold a per-object lock.
- Discovery fields are carried by `DeviceDescSpec` and copied descriptor →
  object → probe explicitly; do not reintroduce a second registration entry or
  a re-lookup of firmware data inside probe.
- Device-node names and numbers remain private allocation policy in kdevice;
  projection crates consume `DeviceNode` without reassigning identity.
- Node removal keeps the class index reserved until every projection callback
  returns.
- Changes to `DeviceObject::begin_removing`'s CAS logic get a concurrency
  review.
- New `DeviceDriver` methods keep the `DriverObject` delegation path
  complete.
- New `DeviceMatcher` implementations keep `matches()` pure (no side
  effects, no lock acquisition).

## OS capability ownership after device-res retirement

ResourceDesc/ResourceSet belong to device::resource. device::managed acquires
driver-os-api guards through explicit providers and registers them directly with
DeviceObject::add_cleanup. DeviceResource and the unused ResourceProvider
aggregate are removed without a replacement facade.
Cleanup remains framework-triggered, LIFO and outside the devres lock.
Failed acquisition registers nothing; repeated drains are empty. Raw MMIO/DMA
views expire at cleanup. Hardware/users must be quiesced first and release must
run in the provider's permitted context. Allocation policy and legacy
remove-error handling are unchanged.
