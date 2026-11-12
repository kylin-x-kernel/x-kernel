# kclass — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (macro-generated
class registries, delegation impls, event bridge, prelude) and
`src/generic.rs` (`ClassDevice<T>`, `ClassRegistry<T>`). No modules are
excluded; the crate contains no unsafe code and every registry is
reachable from the documented publish/query/subscribe API.


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
  calling `publish_<class>()`, with complete binding metadata available and
  the driver match validated by `kdevice`. The captured `BindingRef` assumes
  the normal probe/adoption publication path holds the descriptor binding
  lock; a capture alone is not a publication permission.
- `kclass` trusts the runtime trait object to be `Send + Sync` and
  internally concurrency-safe.
- `kclass` trusts `kdevice` to dispatch `Activated` / `Removed` events in
  the correct order.
- Upper subsystems trust that a `ClassDevice<T>` returned by `kclass`
  stays safely accessible after device removal: it retains both
  `Arc<DeviceObject>` and the creating binding's `BindingRef`. Driver
  identity remains readable from the snapshot, while `is_available()` turns
  false; the handle never grants hardware access by itself.

## External Boundaries / Attack Surface

`kclass` is a typed runtime device-capability registry layer that never
touches hardware or external input directly. Its attack surface comes
from:

- **kdriver publish input**: the driver-supplied
  `parent: Arc<DeviceObject>` and `runtime: T`. kclass assumes `parent`
  went through the full `kdevice` probe pipeline — driver matching
  validated, `parent.state` transitions protected by `kdevice` internal
  locks.
- **Event bridge**: the arrival order of `Activated` / `Removed` events
  dispatched by `kdevice`. kclass trusts kdevice to dispatch only after
  state transitions complete.
- **Device-node metadata**: active input and display runtimes supply immutable
  class metadata to `kdevice`. The node registry validates class capacity and
  owns index, major/minor, and `devname` assignment.
- **ClassDevice trait delegation**: when upper subsystems call trait
  methods through `ClassDevice<T>`, calls delegate to the runtime trait
  object. kclass itself performs no argument validation — it trusts each
  subsystem and driver to validate.

The threat analysis should focus on:

- whether event-bridge callbacks can fire in the wrong device state;
- whether the `ClassDevice` `Arc` lifetime can dangle after device
  removal;
- whether duplicate publishes are rejected and class-specific publish
  steps are rolled back correctly.

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

1. **ClassDevice Arc lifetime**: `ClassDevice` holds
   `Arc<ClassDeviceInner<T>>`; `ClassDeviceInner` holds
   `Arc<DeviceObject>` and the creating binding's `BindingRef`. As long as
   any `ClassDevice` clone exists, the `DeviceObject` allocation and the
   captured binding runtime are not freed, so object memory, runtime storage
   and lifecycle reads remain valid. This does **not** keep the device's
   devres resources (MMIO mappings, IRQs, DMA buffers) active: removal and
   probe failure run the explicit cleanup batch regardless of retained
   handles.
2. **ClassDevice remains safe to query after removal**:
   `ClassRegistry::remove` only drops the registry entry; clones held
   outside stay valid. `driver_name()`/`driver_id()` read the immutable
   snapshot and cannot panic after detachment; `is_available()` reads the
   captured runtime and turns false when removal commits. `with()` returns a
   borrow of the runtime object whose storage is retained by the handle, but
   it is not an operation admission: hardware access remains safe only under
   the concrete runtime's resource-ownership, quiesce, and admission
   contract. Trait methods may return errors once the device state is
   `Removing`/`Removed`; this layer guarantees memory validity, not hardware
   availability.
3. **Binding references do not admit hardware use**: a `ClassDevice` and
   its clones retain a `BindingRef`, which does not increment usage and
   cannot mutate the runtime. Retaining class handles therefore cannot make
   removal return `ResourceBusy`. Availability is an instantaneous
   observation, not permission for later hardware access.
4. **No callbacks under the registry lock**: `publish` and `remove`
   complete inside the `SpinNoPreempt` lock without calling external
   callbacks; callbacks run outside the lock.
5. **No reentrancy in event dispatch**: `notify_class_available` releases
   the registry lock before invoking callbacks, so a callback touching
   the same registry cannot deadlock.
6. **Publish identity uniqueness**: a second publish of the same
   `DeviceId` returns `AlreadyExists`; replacement must be expressed as a
   new publish after `Removed`.
7. **device_kind validation**: `publish_<class>()` verifies the runtime's
   `device_kind` against the registry type before constructing the
   `ClassDevice`; a mismatch returns `InvalidInput` without publishing.
8. **Node assignment follows activation**: kclass requests a `DeviceNode` only
   after observing `DeviceEvent::Activated`. It unregisters that node before
   class-specific cleanup on `DeviceEvent::Removed`.
9. **Node allocation failure does not hide the runtime**: a failed node
   registration is logged, but class availability callbacks still run. The
   typed runtime remains usable even when a Linux-compatible projection cannot
   be allocated.

## Thread Safety

| Type | Send condition | Sync condition |
|------|-----------|-----------|
| `ClassDevice<T>` | `Arc<ClassDeviceInner<T>>` is `Send` when `T: Send + Sync` | `Arc` provides shared access |
| `ClassDeviceInner<T>` | `Arc<DeviceObject>` + `BindingRef` + `T: Send` + metadata `Send` | `Arc<DeviceObject>` + `BindingRef` + `T: Sync` |
| `ClassRegistry<T>` | `Vec<ClassDevice<T>>` + `Vec<Callback>` are `Send` | interior mutability via `SpinNoPreempt` |
| `ClassAvailabilityCallback<T>` | `Arc<dyn Fn(...) + Send + Sync>` is `Send + Sync` | `Arc` provides shared access |
| `ACTIVATION_BRIDGE` | `LazyInit<()>` is a zero-sized type | `LazyInit` makes initialization thread-safe |

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-02 | Event bridge fires before `kdevice` is initialized | High | `ensure_event_bridge` called before `kdevice::init_device_registry` | kdriver runs `init_device_registry` before any `publish_*`; the `ACTIVATION_BRIDGE` lazy init fires at first class-registry access (publish, query, or subscribe) |
| T-03 | Publish race loses or duplicates a device | Medium | Concurrent publish of the same device | `SpinNoPreempt` serializes; a second publish of the same id returns `AlreadyExists` |
| T-04 | A subscriber callback panics, skipping later subscribers | Medium | One callback panics inside the notification `for` loop | Callbacks run outside `catch_unwind`; currently no unwind protection — subscriber quality is relied upon |
| T-05 | `with()` on a removed `ClassDevice` reaches freed runtime | Medium | Runtime trait object dropped before `ClassDeviceInner` | All `ClassDeviceInner` fields (including runtime) drop together; `Arc` counting delays drop until all references are gone |
| T-06 | device_kind validation bypassed at publish | Medium | Driver misuses a publish function (e.g. `publish_block` for a net device) | Explicit `device_kind != $kind` check returns `InvalidInput` without publishing |
| T-07 | `find`/`devices` return non-`Active` devices | Low | `is_available()` filtering removed or broken | Both paths filter through `is_available()`; removed devices are swap_remove'd |
| T-08 | Reentrancy into the same registry from a subscriber callback deadlocks | Medium | Callback calls `publish_*` / `subscribe_*` | `notify_class_available` runs callbacks outside the lock, so reentrancy cannot deadlock (but may build long call chains) |
| T-09 | A node is visible before the underlying runtime is active | Medium — userspace could open a node whose class runtime cannot serve requests | Node registration happens during publication | Registration is driven only by the `Activated` event, after the class registry releases its lock |
| T-10 | Node allocation failure suppresses unrelated class consumers | Medium — kernel subsystems could miss an otherwise usable device | The class index pool is exhausted | The failure is logged and ordinary class availability callbacks continue |
| T-11 | A retained class handle prevents device removal | Medium — device cannot be unbound while any subsystem holds a clone | A subsystem caches `ClassDevice` indefinitely | `BindingRef` retains the runtime but does not increment usage; availability reads the captured lifecycle, so removal can commit and later reports `false` |

Impact levels:

- High: UB, memory corruption, privilege escalation.
- Medium: panic, service unavailability, inconsistent state.
- Low: performance degradation, lost logs, degraded functionality.

## Failure Modes And Effects Analysis

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
|------|----------|----------|--------|------|----------|----------|
| F-01 | Publish with an unbound parent | `DeviceObject` did not complete bind before publish, or its binding is already terminal/detached | publish returns `BadState` | Device not published | 3 | `try_new_with_class_metadata` calls `binding_ref()`, which rejects missing metadata and observed `Removing`/`Removed` |
| F-02 | Publish with a mismatched device_kind | Driver passes a net device to `publish_block` | publish returns `InvalidInput` | Device not published | 4 | Kind check happens before `ClassDevice` construction |
| F-03 | Event bridge not registered | `ensure_event_bridge` never called (no class feature enabled) | No Activated/Removed dispatch | Device state changes do not reach class registries | 4 | `ensure_event_bridge` is called by every `*_registry_fn()` |
| F-04 | Subscriber callback panics | Callback bug causes unwinding | Later subscribers not notified | Some subsystems miss availability notices | 3 | Callbacks run in Vec order; no `catch_unwind` today; subscriber quality relied upon |
| F-06 | `devices()` returns a very large `Vec` | Many devices active at once | Allocation may fail | Allocation failure reaches the global allocation error handler instead of returning (no OOM handling) | 4 | `collect()` is infallible; kclass has no recovery path |
| F-07 | Duplicate publish | Driver bypasses the normal remove/add lifecycle | publish returns `AlreadyExists` | New runtime not visible | 4 | Class-specific publish is rolled back; the resident object is kept |
| F-08 | Missing input metadata | Non-input class does not override `class_metadata` | `physical_location()` / `unique_id()` return empty strings | Input device identity incomplete | 4 | `ClassRuntimeMetadata` default returns `empty()`; the input class overrides explicitly |
| F-09 | Device-node allocation fails | The fixed class index range is exhausted | No `/dev` or `/sys` projection is created for the runtime | Linux-compatible discovery is unavailable for that device | 3 | kdevice returns an error; kclass logs it and still notifies typed class consumers |

Severity levels:

- 1: fatal — system crash, data loss.
- 2: serious — function unavailable until restart.
- 3: moderate — degraded function, self-recoverable.
- 4: minor — limited impact, tolerable.

## Failure Management

- Publish validation failures return `DriverError` (`BadState`,
  `InvalidInput`, `AlreadyExists`); nothing panics.
- devices, find, subscribe, and remove are infallible; publish reports
  duplicate identity explicitly.
- Device-node allocation failures are isolated from the typed class registry;
  the runtime remains available to direct class consumers.
- A subscriber callback panic currently has no unwind protection;
  subscriber implementation quality is relied upon.
- `ClassDevice`'s `driver_name()` / `driver_id()` read the captured
  `BindingRef` snapshot and cannot fail after construction; an unbound or
  terminal parent is rejected at construction with `BadState` instead.
- Availability is evaluated against the captured binding instance. A
  handle built while the device is `Active` can observe the binding go
  `Removing` and return to `Active` if that removal attempt fails with
  `ResourceBusy`; only a successful removal commit is terminal for the
  captured binding.
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
- The publish path still captures a `BindingRef` at construction and
  returns `BadState` when `binding_ref()` is unavailable.
- `driver_name()` / `driver_id()` read the captured snapshot, never the
  parent's live binding slot.
- No path treats `BindingRef` or `is_available()` as usage/admission or
  mutates binding state through the class handle.
- Registry-lock critical sections still call no external callbacks (deadlock
  prevention).
- Device-node registration remains after `Activated` and before availability
  callbacks; removal unregisters the node before class cleanup.
- A node-allocation error must not suppress typed class availability callbacks.
- Framebuffer changes respect the `Arc<DeviceObject>` lifetime guarantee.

## Input readiness forwarding

Readiness forwarding preserves the input trait's IRQ contract. Availability can
race removal; retained ownership keeps the runtime storage valid, and consumers
still recheck detach/wake state in task context.
