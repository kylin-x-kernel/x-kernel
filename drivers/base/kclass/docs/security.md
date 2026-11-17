# kclass — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (macro-generated
class registries, delegation impls, event bridge, prelude), `src/generic.rs`
(`ClassPublication<T>`, `ClassDevice<T>`, publication state/registry) and the
`cfg(unittest)` `src/retirement_hooks.rs` pause seam. No modules are excluded; the
crate contains no unsafe code and every registry is reachable from the
documented publish/query/subscribe API.


## Trust Model

```text
    kdriver (probe success)
       │
       │ trusted: parent DeviceObject (bound driver verified),
       │          runtime trait object (driver-constructed)
       v
┌─────────────────────────────┐
│ kclass                      │
│                             │
│ safe boundary               │
│  ├─ ClassRegistry publish/  │
│  │   devices/find/subscribe │
│  ├─ ClassDevice::with()     │
│  │   trait delegation       │
│  ├─ Event bridge dispatch   │
│  └─ DeviceNode metadata     │
│      handoff to kdevice     │
│                             │
│ (no unsafe boundary in      │
│  kclass — see below)        │
└──────────────┬──────────────┘
               │
               │ query / subscribe / trait calls
               v
    knet / fs_boot / input subsystem / ...
```

- `kclass` trusts `kdriver` to have finished the driver probe before
  calling `publish_<class>()`, with a live descriptor binding and the driver
  match validated by `kdevice`. Publication captures that exact
  `BindingRef` through the `ProbeContext`; the capture alone
  is not a publication permission, and the caller must already hold the
  descriptor binding lock because it is in the probe write context.
- `kclass` trusts the runtime trait object to be `Send + Sync` and
  internally concurrency-safe.
- `kclass` trusts `kdevice` to dispatch `Activated` / `Removed` /
  `BindingRetired` events in the correct order, and to report node ownership
  truthfully through `NodeRemoval`. `BindingRetired` names only the device ID;
  `kclass` matches the exact captured binding itself and never retires an
  entry of a different instance.
- Upper subsystems trust that a `ClassDevice<T>` returned by `kclass`
  stays safely accessible after device removal: it retains both
  `Arc<DeviceObject>` and the creating binding's `BindingRef`. Driver
  identity remains readable from the snapshot, while `is_available()` turns
  false; the handle never grants hardware access by itself and is never
  re-admitted to a replacement binding.
- `ClassPublication<T>` is a non-Clone revocation credential. `ClassDevice<T>`
  is the cloneable consumer handle and has no revoke method. A stale
  credential can only address its own `PublicationId`; it cannot delete a
  newer publication of the same device/binding. Dropping the credential does
  not revoke and must not be treated as a cleanup signal.

## Publication revocation and availability

- A revoke request under the class-registry lock closes availability
  immediately. During `Activating`/`Notifying` it stores
  `revoke_requested` (Release) and returns `Deferred`; `is_available()`,
  `find_*_publication()` and enumeration read it with Acquire and hide the
  publication. Cleanup may lag; availability does not.
- `Revoked` is exposed only after node silence, backend `unpublish_class` and
  removed callbacks complete. A concurrent second revoke observes `Revoking`
  and gets `Deferred`, never a premature `AlreadyRevoked`.
- A node `NotOwner`/unrecoverable failure enters `RevokeFailed`; the
  publication is hidden and never resurrects. A later exact revoke or binding
  retirement must CAS `RevokeFailed -> Revoking` to retry. `RevokeFailed` must
  not be reported as `Deferred`.
- Node removal, unpublish, callbacks and last-`Arc` drop run outside the
  class-registry spinlock. Activation never holds that lock while invoking a
  subscriber, so subscriber reentrant revoke returns `Deferred` rather than
  deadlocking.
- A committed available callback may finish after a revoke request, but the
  handle it received is already unavailable; callback code must not use it to
  reopen availability.

## External Boundaries / Attack Surface

`kclass` is a typed runtime device-capability registry layer that never
touches hardware or external input directly. Its attack surface comes
from:

- **kdriver publish input**: the driver-supplied
  `parent: Arc<DeviceObject>` and `runtime: T`. kclass assumes `parent`
  went through the full `kdevice` probe pipeline — driver matching
  validated, `parent.state` transitions protected by `kdevice` internal
  locks.
- **Event bridge**: the arrival order of `Activated` / `Removed` /
  `BindingRetired` events dispatched by `kdevice`. kclass trusts kdevice to
  dispatch only after state transitions complete; it does not trust the ID
  alone and re-checks the captured binding.
- **Device-node metadata**: active input and display runtimes supply immutable
  class metadata to `kdevice`. The node registry validates class capacity and
  owns index, major/minor, and `devname` assignment. Binding-owned nodes are
  registered and revoked by exact `BindingRef`, so a replacement binding
  cannot revoke the previous instance's node.
- **ClassDevice trait delegation**: when upper subsystems call trait
  methods through `ClassDevice<T>`, calls delegate to the runtime trait
  object. kclass itself performs no argument validation — it trusts each
  subsystem and driver to validate.

The threat analysis should focus on:

- whether event-bridge callbacks can fire in the wrong device state;
- whether the `ClassDevice` `Arc` lifetime can dangle after device
  removal;
- whether duplicate publishes are rejected and class-specific publish
  steps are rolled back correctly;
- whether a stale binding/publication can retire a replacement or sibling
  class entry/node;
- whether a deferred node callback can double-unpublish or remove a newer
  publication.

## Unsafe Code Inventory

kclass contains no `unsafe` code blocks. Historically
`DisplayDevice::fb()` built a framebuffer reference from a raw vaddr via
`display::FrameBuffer::from_raw_parts_mut`; that path was removed along
with the directly-mapped framebuffer abstraction. `/dev/fb0` is now
implemented by `fbdevice`'s fbdev emulation (shadow buffer + scanout
resource), so the framebuffer raw-pointer unsafe boundary is gone and its
safety responsibilities moved to `fbdevice` (the shadow buffer is managed
by a `PageRef` RAII allocation that lives for the kernel lifetime).

## Memory-Safety Invariants

1. **ClassDevice Arc lifetime**: `ClassDevice`/`ClassPublication` hold
   `Arc<ClassDeviceInner<T>>`; `ClassDeviceInner` holds
   `Arc<DeviceObject>` and the creating binding's `BindingRef`. As long as
   any `ClassDevice` clone exists, the `DeviceObject` allocation and the
   captured binding runtime are not freed, so object memory, runtime storage
   and lifecycle reads remain valid. This does **not** keep the device's
   devres resources (MMIO mappings, IRQs, DMA buffers) active: removal and
   probe failure run the explicit cleanup batch regardless of retained
   handles.
2. **ClassDevice remains safe to query after removal**:
   retirement only removes the registry entry; clones held outside stay
   valid. `driver_name()`/`driver_id()` read the immutable snapshot and
   cannot panic after detachment; `is_available()` reads the captured runtime
   and turns false when the binding is retired. `with()` returns a borrow of
   the runtime object whose storage is retained by the handle, but it is not
   an operation admission: hardware access remains safe only under the
   concrete runtime's resource-ownership, quiesce, and admission contract.
   Trait methods may return errors once the binding is `Removing`/`Removed`;
   this layer guarantees memory validity, not hardware availability.
3. **Binding references do not admit hardware use**: a `ClassDevice` and
   its clones retain a `BindingRef`, which does not increment usage and
   cannot mutate the runtime. Retaining class handles therefore cannot make
   removal return `ResourceBusy`. Availability is an instantaneous
   observation, not permission for later hardware access.
4. **No callbacks or class teardown under the registry lock**: registry
   mutation completes inside the `SpinNoPreempt` lock; class-specific
   `publish_class` / `unpublish_class`, node registration/revocation and all
   subscriber callbacks run outside it.
5. **No reentrancy in event dispatch**: `notify_class_available` releases
   the registry lock before invoking callbacks, so a callback touching
   the same registry cannot deadlock.
6. **Publication identity uniqueness**: the registry rejects a duplicate
   `PublicationId`, but permits multiple publications for the same stable
   `DeviceId` and binding. A stale credential can only address its own
   `PublicationId`; it cannot evict a sibling or replacement.
7. **device_kind validation**: `publish_<class>()` verifies the runtime's
   `device_kind` against the registry type before constructing the
   `ClassDevice`; a mismatch returns `InvalidInput` without publishing.
8. **Node assignment follows activation**: kclass requests a `DeviceNode` only
   after observing `DeviceEvent::Activated`, keyed by the entry's captured
   binding. It revokes that exact node before class-specific cleanup on
   `Removed` / `BindingRetired`.
9. **Node allocation failure does not hide the runtime**: a failed node
   registration is logged, but class availability callbacks still run. The
   typed runtime remains usable even when a Linux-compatible projection cannot
   be allocated.
10. **Revocation is publication-exact**: `ClassPublication::revoke` and
    binding-retirement scans use `PublicationId` plus the captured
    `BindingRef`. Retiring an old publication cannot evict a newer or sibling
    publication of the same device.
11. **Availability closes at request time**: a revoke request under the
    registry lock sets `revoke_requested` (Release); `is_available()`, public
    `find_*_publication` and device-level enumeration read it with Acquire and
    hide the publication immediately. `Deferred` means cleanup is pending, not
    that the publication remains usable.
12. **Deferred node completion resumes the exact executor**: a deferred node
    callback calls `finish_revoke` for the same `PublicationId`; a stale
    callback finds no entry and is a no-op. A node `NotOwner` or unrecoverable
    failure enters `RevokeFailed`, never silently unpublishes another owner's
    runtime.
13. **Revoked is completion, not request**: the registry entry is removed and
    `Revoked` store happens only after node silence, backend unpublish and
    removed callbacks. A concurrent second revoke observes `Revoking` and gets
    `Deferred`, never premature `AlreadyRevoked`.
14. **No wait under the source/class lock**: activation/revocation callbacks,
    unpublish and last-`Arc` drop run outside the registry spinlock. Entropy
    source removal takes the source out under its mutex and only then stops the
    worker, so a blocked worker cannot deadlock the selector.

## Thread Safety

| Type | Send condition | Sync condition |
|------|-----------|-----------|
| `ClassDevice<T>` | `Arc<ClassDeviceInner<T>>` is `Send` when `T: Send + Sync` | `Arc` provides shared access |
| `ClassDeviceInner<T>` | `Arc<DeviceObject>` + `BindingRef` + `PublicationId` + atomics + `T: Send` | `Arc<DeviceObject>` + `BindingRef` + `T: Sync` |
| `ClassRegistry<T>` | `Vec<ClassDevice<T>>` + callback vectors are `Send` when `T` is | interior mutability via `SpinNoPreempt`; exact `PublicationId` bounds the unlocked cleanup |
| `ClassDeviceInner<T>` | `Arc<DeviceObject>` + `BindingRef` + `PublicationId` + atomics + `T: Send` | shared state is atomic or registry-locked |
| `test_hooks::ClassPause` | `cfg(unittest)` only | pause points run outside registry/node locks |
| `ClassAvailabilityCallback<T>` | `Arc<dyn Fn(...) + Send + Sync>` is `Send + Sync` | `Arc` provides shared access |
| `ACTIVATION_BRIDGE` | `LazyInit<()>` is a zero-sized type | `LazyInit` makes initialization thread-safe |

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-02 | Event bridge fires before `kdevice` is initialized | High | `ensure_event_bridge` called before `kdevice::init_device_registry` | kdriver runs `init_device_registry` before any `publish_*`; the `ACTIVATION_BRIDGE` lazy init fires at first publish |
| T-03 | Publish race loses or duplicates a device | Medium | Concurrent publish of the same device/binding | `SpinNoPreempt` serializes by `PublicationId`; same device siblings are allowed and independently revocable |
| T-04 | A subscriber callback panics, skipping later subscribers | Medium | One callback panics inside the notification `for` loop | Callbacks run outside `catch_unwind`; currently no unwind protection — subscriber quality is relied upon |
| T-05 | `with()` on a removed `ClassDevice` reaches freed runtime | Medium | Runtime trait object dropped before `ClassDeviceInner` | All `ClassDeviceInner` fields (including runtime) drop together; `Arc` counting delays drop until all references are gone |
| T-06 | device_kind validation bypassed at publish | Medium | Driver misuses a publish function (e.g. `publish_block` for a net device) | Explicit `device_kind != $kind` check returns `InvalidInput` without publishing |
| T-07 | `find`/`devices` return non-`Active` devices | Low | `is_available()` filtering removed or broken | Both paths filter through `is_available()` on the captured binding; terminal entries are retired |
| T-08 | Reentrancy into the same registry from a subscriber callback deadlocks | Medium | Callback calls `publish_*` / `subscribe_*` | `notify_class_available` runs callbacks outside the lock, so reentrancy cannot deadlock (but may build long call chains) |
| T-09 | A node is visible before the underlying runtime is active | Medium — userspace could open a node whose class runtime cannot serve requests | Node registration happens during publication | Registration is driven only by the `Activated` event, after the class registry releases its lock |
| T-10 | Node allocation failure suppresses unrelated class consumers | Medium — kernel subsystems could miss an otherwise usable device | The class index pool is exhausted | The failure is logged and ordinary class availability callbacks continue |
| T-11 | A retained class handle prevents device removal | Medium — device cannot be unbound while any subsystem holds a clone | A subsystem caches `ClassDevice` indefinitely | `BindingRef` retains the runtime but does not increment usage; availability reads the captured lifecycle, so removal can commit and later reports `false` |
| T-12 | A stale removal retires a replacement binding's entry | Medium — a fast rebind loses its class view | Old `Removed`/`BindingRetired` event or stale callback processed after the new entry is published | Claims match `same_binding` and require a terminal binding; the ID alone never selects an entry |
| T-13 | Deferred node completion double-revokes a publication | Duplicated teardown | Node finishes after another executor | Completion removes only the exact `PublicationId`; a stale callback finds no entry and is a no-op |
| T-14 | Sibling publication is deleted by a stale credential | Medium — capability loss | DeviceId matches, PublicationId differs | Exact lookup by `PublicationId`; sibling stays available |
| T-15 | Node owner is foreign | Medium — wrong `/dev` projection is removed | Key matches but binding differs | `NotOwner` enters retryable `RevokeFailed`; no foreign node is touched |

Impact levels:

- High: UB, memory corruption, privilege escalation.
- Medium: panic, service unavailability, inconsistent state.
- Low: performance degradation, lost logs, degraded functionality.

## Failure Modes And Effects Analysis

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
|------|----------|----------|--------|------|----------|----------|
| F-01 | Publish with an unbound parent | `DeviceObject` did not complete bind before publish, or its binding is already terminal/detached | publish returns `BadState` | Device not published | 3 | `publish_<class>` requires a `ProbeContext`; `try_new_with_binding` also rejects an observed `Removing`/`Removed` |
| F-02 | Publish with a mismatched device_kind | Driver passes a net device to `publish_block` | publish returns `InvalidInput` | Device not published | 4 | Kind check happens before `ClassDevice` construction |
| F-03 | Event bridge not registered | `ensure_event_bridge` never called (no class feature enabled) | No Activated/Removed dispatch | Device state changes do not reach class registries | 4 | `ensure_event_bridge` is called by every `*_registry_fn()` |
| F-04 | Subscriber callback panics | Callback bug causes unwinding | Later subscribers not notified | Some subsystems miss availability notices | 3 | Callbacks run in Vec order; no `catch_unwind` today; subscriber quality relied upon |
| F-06 | `devices()` returns a very large `Vec` | Many devices active at once | Allocation may fail | Allocation failure reaches the global allocation error handler instead of returning (no OOM handling) | 4 | `collect()` is infallible; kclass has no recovery path |
| F-07 | Duplicate publish | Driver bypasses the normal remove/add lifecycle | publish returns `AlreadyExists` | New runtime not visible | 4 | Class-specific publish is rolled back; the resident object is kept |
| F-08 | Missing input metadata | Non-input class does not override `class_metadata` | `physical_location()` / `unique_id()` return empty strings | Input device identity incomplete | 4 | `ClassRuntimeMetadata` default returns `empty()`; the input class overrides explicitly |
| F-09 | Device-node allocation fails | The fixed class index range is exhausted | No `/dev` or `/sys` projection is created for the runtime | Linux-compatible discovery is unavailable for that device | 3 | kdevice returns an error; kclass logs it and still notifies typed class consumers |
| F-10 | Deferred node removal never completes | Node implementation loses/drops callback | Publication stays `Revoking` | No wrong publication is published; second revoke returns Deferred | 2 | Callback is registered before `Deferred`; failure is logged |
| F-11 | Revocation executor fails on foreign node | `NodeRemoval::NotOwner` | `RevokeFailed` | Publication hidden; retry possible | 3 | No unpublish/remove on failure; state transitions to retryable `RevokeFailed` |
| F-12 | Foreign node owner during removal | A stale binding tries to revoke a node owned by the new binding | `NotOwner` | The new binding keeps its projection; the stale entry is not retired by this call | 3 | kclass logs `NotOwner` and returns `BadState`; it never falls back to ID-only revocation |
| F-13 | Class unpublish panics after a claim | Driver `unpublish_class` implementation panics | Reservation is not finished | Same-ID publication stays blocked | 3 | No unwind protection; class unpublish implementations must not panic; the panic itself is the primary fault |

Severity levels:

- 1: fatal — system crash, data loss.
- 2: serious — function unavailable until restart.
- 3: moderate — degraded function, self-recoverable.
- 4: minor — limited impact, tolerable.

## Failure Management

- Publish validation failures return `DriverError` (`BadState`,
  `InvalidInput`, `AlreadyExists`, `ResourceBusy`); nothing panics.
- devices, publication lookup and subscribe are infallible. Publish reports a
  duplicate `PublicationId`; multiple `DeviceId` siblings are valid.
- Removal of one publication is a sequence: exact node revocation → unpublish
  outside the lock → removed callbacks → terminal entry removal. `Deferred`
  means the node/activation executor owns the rest; `NotOwner` enters
  retryable `RevokeFailed`; no error force-clears a sibling.
- Device-node allocation failures are isolated from the typed class registry;
  the runtime remains available to direct class consumers.
- A subscriber callback panic currently has no unwind protection;
  subscriber implementation quality is relied upon. A panic in
  `unpublish_class` after a claim may leave the publication in `Revoking`;
  this is a fail-closed outcome, not a silent sibling deletion.
- `ClassDevice`'s `driver_name()` / `driver_id()` read the captured
  `BindingRef` snapshot and cannot fail after construction; an unbound or
  terminal parent is rejected at construction with `BadState` instead.
- Availability is evaluated against the captured binding instance. A
  handle built while the device is `Active` observes the binding go
  `Removing`; only a successful removal commit is terminal for the captured
  binding, and a later rebind is a different instance that the old handle
  never follows.
- Test cleanup (`remove_class_device_for_tests`) requests exact revocation
  for every entry and must only run after tasks that could hold an activation
  or completion are joined.
- kclass contains no `unsafe` blocks, so class adapter logic cannot cause
  UB; the historical framebuffer raw-pointer path moved to `fbdevice`'s
  fbdev emulation.

## Privacy Analysis

`kclass` processes no user data. Device metadata (name, device_kind,
driver_name, irq) is logged at debug level and contains no user-process
data or device payloads. The input class's `physical_location` and
`unique_id` are device identity strings, not user input data.

The module persists nothing; all state lives in the in-memory class
registries and `ClassDevice` objects.

## Known Limitations

- Subscriber callback panics have no `catch_unwind` protection and may
  skip later subscribers.
- `devices()` allocates a fresh `Vec` per call; high-frequency polling
  adds allocation pressure.
- The registry supports no predicate filtering (e.g. "list devices with
  feature X"); callers filter themselves.
- `ClassDeviceMetadata` currently only carries input identity
  (`input_identity`); adding class-specific metadata for other classes
  requires extending the `ClassDeviceMetadata` struct in addition to
  overriding `class_metadata()`.
- `ClassDevice` has no pre-removal notification (e.g. "device about to be
  removed").
- `is_available()` is an instantaneous observation of the captured binding,
  not an admission or reservation: it neither increments usage nor
  guarantees that a subsequent hardware operation succeeds. Retained class
  handles never block removal.
- Class handles are not re-admitted to a later binding of the same device;
  availability follows the binding captured at construction.
- Device-node templates currently cover only input event devices and DRM
  primary nodes. Other classes remain discoverable through kclass without a
  Linux-compatible node projection.
- A revoke request closes availability immediately; cleanup may remain
  `Revoking` or `RevokeFailed` until the node/backend completes or a retry is
  made. kclass does not time out a deferred node.
- `RevokeFailed` has no automatic retry loop; a later exact revoke or binding
  retirement must retry it.
- `PublicationId` is monotonic `u64`; at `u64::MAX` new publication returns
  `ResourceBusy` and no id is reused.

## Audit Checklist

When modifying this module, verify:

- Every `unsafe` block carries a `SAFETY:` comment.
- A new class is added at the `class_registries!` macro call site, not by
  hand-copying logic.
- The new class's runtime type alias (e.g. `FooDeviceImpl`) is declared
  in lib.rs.
- The new class's trait delegation impls cover all required trait
  methods.
- The new class is re-exported in the `prelude` module.
- Production publish captures the exact binding through
  `ProbeContext` and constructs with
  `try_new_with_binding`; it returns `BadState` when capture fails. The
  slot-reading `try_new_with_class_metadata` remains fixture-only.
- `driver_name()` / `driver_id()` read the captured snapshot, never the
  parent's live binding slot.
- No path treats `BindingRef` or `is_available()` as usage/admission or
  mutates binding state through the class handle.
- Registry-lock critical sections still call no external callbacks and no
  class-specific publish/unpublish (deadlock and sleeping prevention).
- Device-node registration remains after `Activated` and before availability
  callbacks; removal revokes the node by exact binding before class cleanup.
- A node-allocation error must not suppress typed class availability callbacks.
- Retirement matches id and `same_binding`, only claims terminal bindings,
  unpublishes outside the lock, and finishes with the matching token.
- Deferred node callbacks re-enter the same retirement path; no new
  reconciliation path is added that keys only on `DeviceId`.
- Same-ID publication is rejected while a reservation is in flight.
- Token allocation uses `checked_add`; exhaustion returns `BadState` before
  mutation.
- Test cleanup preserves deferred entries and does not force-clear live
  claims.
- Framebuffer changes respect the `Arc<DeviceObject>` lifetime guarantee.

## Input readiness forwarding

Readiness forwarding preserves the input trait's IRQ contract. Availability can
race removal; retained ownership keeps the runtime storage valid, and consumers
still recheck detach/wake state in task context.
