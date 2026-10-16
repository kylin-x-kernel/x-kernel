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
- `kdevice` is called only from process/task context (it uses no
  IRQ-safe locks).

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
uses the standard safe abstractions: `SpinNoPreempt`, `AtomicU8`,
`AtomicU64`, `AtomicUsize`, and `Arc`.

## Memory-Safety Invariants

1. **Lock-ordering rule**: Registry → (drop guard) → per-object. Every
   caller must follow it. `DeviceRegistry::find_bus_type` documents its
   panic-on-missing behavior and requires bus types to be registered at
   init time.
2. **DeviceObject::begin_removing atomicity**: the CAS loop is the only
   `→Removing` commit point. After a successful CAS the transition is
   irreversible, and the usage count must be checked to be zero.
   Violating this can cause use-after-free.
3. **DeviceUse RAII guard**: `try_acquire` uses `fetch_add` + a state
   check; `begin_removing` checks the usage count after its CAS. Together
   they guarantee: either `begin_removing` is rejected while a `DeviceUse`
   exists, or after `begin_removing` succeeds, `try_acquire` sees a
   non-Active state and rolls the count back.
4. **DeviceDesc Probing mutual exclusion**: `mark_device_desc_probing`
   CASes `Pending → Probing` only, preventing concurrent probes of the
   same descriptor.
5. **Subscriber contract**: subscriber callbacks run without driver-core
   locks and must not call mutators. Violations can deadlock or corrupt
   re-entrant state.
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

## Thread Safety

| Type | Send condition | Sync condition |
|------|-----------|-----------|
| `DeviceRegistry` | fields are `Send` | `SpinNoPreempt` provides interior mutability |
| `DeviceNodeRegistry` | maps, index sets, and callbacks are `Send` | a separate `SpinNoPreempt` serializes assignment and snapshots |
| `DeviceObject` | `Arc<DeviceObject>` is `Send + Sync` | `AtomicU8` + `SpinNoPreempt` + `AtomicUsize` |
| `BusInstance` | `Arc<BusInstance>` is `Send + Sync` | internal `SpinNoPreempt` guards all mutable fields |
| `BusTypeObject` | `Arc<BusTypeObject>` is `Send + Sync` | internal `SpinNoPreempt` |
| `DriverObject` | `Arc<DriverObject>` is `Send + Sync` | internal `SpinNoPreempt` + `AtomicU64` |
| `DeviceTopology` | plain `Vec` data, `Send` | immutable snapshot, `Sync` |
| `DeviceEventSubscribers` | `Vec<Arc<dyn Fn>>` is `Send` | serialized through `DeviceRegistry`'s `SpinNoPreempt` |

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-01 | Lock-ordering violation deadlocks | High | Calling `device_registry()` while holding a per-object lock | Lock-ordering rules documented; `BusInstance`/`DeviceObject` methods are `pub(crate)` to limit direct use |
| T-02 | `begin_removing` vs `try_acquire` race causes use-after-free | High | Timing: `try_acquire` fetch_add lands, then `begin_removing` CAS succeeds before the state check | `try_acquire` does `fetch_add` then checks state, rolling back with `fetch_sub` if not Active; `begin_removing` checks usage after its CAS. The two are mutually exclusive |
| T-03 | Same descriptor probed concurrently | Medium | Two paths probe the same `DeviceDescId` at once | `mark_device_desc_probing` succeeds only on Pending→Probing; the concurrent loser sees Probing and returns Requeue |
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
| F-09 | BusInstance.devices inconsistent with DeviceRegistry.devices | One path updated only one side | Snapshots may miss or include stale devices | Inconsistent query results | 3 | `add_device` updates both inside the registry lock; remove likewise |
| F-10 | Descriptor `attempted` list grows without bound | Many drivers fail to match one descriptor | attempted `Vec` grows | Per-descriptor memory grows | 4 | Device removal clears `attempted` on requeue; the list is bounded by the driver count |
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
- Errors from `driver.remove()` and `bus_type.remove()` inside
  `remove_device_managed` are only logged and never stop the removal
  ("remove never fails" semantics).
- A probe failure runs the full rollback (`run_cleanups` +
  `detach_driver` + requeue), so a partially initialized device is never
  leaked.
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
