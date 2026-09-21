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
  calling `publish_<class>()`, with the parent's `driver_name()` /
  `driver_id()` set and the driver match validated by `kdevice`.
- `kclass` trusts the runtime trait object to be `Send + Sync` and
  internally concurrency-safe.
- `kclass` trusts `kdevice` to dispatch `Activated` / `Removed` events in
  the correct order.
- Upper subsystems trust that a `ClassDevice<T>` returned by `kclass`
  stays safely accessible after device removal (guaranteed by holding
  `Arc<DeviceObject>`).

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
by a `GlobalPage` RAII allocation that lives for the kernel lifetime).

## Memory-Safety Invariants

1. **ClassDevice Arc lifetime**: `ClassDevice` holds
   `Arc<ClassDeviceInner<T>>`, and `ClassDeviceInner` holds
   `Arc<DeviceObject>`. As long as any `ClassDevice` clone exists, the
   `DeviceObject` is not freed and its devres resources (MMIO mappings,
   IRQs, DMA buffers) stay valid.
2. **ClassDevice remains safe to use after removal**:
   `ClassRegistry::remove` only drops the registry entry; clones held
   outside stay valid, and their `with()` calls reach device resources
   through `Arc<DeviceObject>`. Trait methods may return errors once the
   device state is `Removing`/`Removed`, but no UB can occur.
3. **No callbacks under the registry lock**: `publish` and `remove`
   complete inside the `SpinNoPreempt` lock without calling external
   callbacks; callbacks run outside the lock.
4. **No reentrancy in event dispatch**: `notify_class_available` releases
   the registry lock before invoking callbacks, so a callback touching
   the same registry cannot deadlock.
5. **Publish identity uniqueness**: a second publish of the same
   `DeviceId` returns `AlreadyExists`; replacement must be expressed as a
   new publish after `Removed`.
6. **device_kind validation**: `publish_<class>()` verifies the runtime's
   `device_kind` against the registry type before constructing the
   `ClassDevice`; a mismatch returns `InvalidInput` without publishing.
7. **Node assignment follows activation**: kclass requests a `DeviceNode` only
   after observing `DeviceEvent::Activated`. It unregisters that node before
   class-specific cleanup on `DeviceEvent::Removed`.
8. **Node allocation failure does not hide the runtime**: a failed node
   registration is logged, but class availability callbacks still run. The
   typed runtime remains usable even when a Linux-compatible projection cannot
   be allocated.

## Thread Safety

| Type | Send condition | Sync condition |
|------|-----------|-----------|
| `ClassDevice<T>` | `Arc<ClassDeviceInner<T>>` is `Send` when `T: Send + Sync` | `Arc` provides shared access |
| `ClassDeviceInner<T>` | `Arc<DeviceObject>` + `T: Send` + metadata `Send` | `Arc<DeviceObject>` + `T: Sync` |
| `ClassRegistry<T>` | `Vec<ClassDevice<T>>` + `Vec<Callback>` are `Send` | interior mutability via `SpinNoPreempt` |
| `ClassAvailabilityCallback<T>` | `Arc<dyn Fn(...) + Send + Sync>` is `Send + Sync` | `Arc` provides shared access |
| `ACTIVATION_BRIDGE` | `LazyInit<()>` is a zero-sized type | `LazyInit` makes initialization thread-safe |

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-02 | Event bridge fires before `kdevice` is initialized | High | `ensure_event_bridge` called before `kdevice::init_device_registry` | kdriver runs `init_device_registry` before any `publish_*`; the `ACTIVATION_BRIDGE` lazy init fires at first publish |
| T-03 | Publish race loses or duplicates a device | Medium | Concurrent publish of the same device | `SpinNoPreempt` serializes; a second publish of the same id returns `AlreadyExists` |
| T-04 | A subscriber callback panics, skipping later subscribers | Medium | One callback panics inside the notification `for` loop | Callbacks run outside `catch_unwind`; currently no unwind protection — subscriber quality is relied upon |
| T-05 | `with()` on a removed `ClassDevice` reaches freed runtime | Medium | Runtime trait object dropped before `ClassDeviceInner` | All `ClassDeviceInner` fields (including runtime) drop together; `Arc` counting delays drop until all references are gone |
| T-06 | device_kind validation bypassed at publish | Medium | Driver misuses a publish function (e.g. `publish_block` for a net device) | Explicit `device_kind != $kind` check returns `InvalidInput` without publishing |
| T-07 | `find`/`devices` return non-`Active` devices | Low | `is_available()` filtering removed or broken | Both paths filter through `is_available()`; removed devices are swap_remove'd |
| T-08 | Reentrancy into the same registry from a subscriber callback deadlocks | Medium | Callback calls `publish_*` / `subscribe_*` | `notify_class_available` runs callbacks outside the lock, so reentrancy cannot deadlock (but may build long call chains) |
| T-09 | A node is visible before the underlying runtime is active | Medium — userspace could open a node whose class runtime cannot serve requests | Node registration happens during publication | Registration is driven only by the `Activated` event, after the class registry releases its lock |
| T-10 | Node allocation failure suppresses unrelated class consumers | Medium — kernel subsystems could miss an otherwise usable device | The class index pool is exhausted | The failure is logged and ordinary class availability callbacks continue |

Impact levels:

- High: UB, memory corruption, privilege escalation.
- Medium: panic, service unavailability, inconsistent state.
- Low: performance degradation, lost logs, degraded functionality.

## Failure Modes And Effects Analysis

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
|------|----------|----------|--------|------|----------|----------|
| F-01 | Publish with an unbound parent | `DeviceObject` did not complete bind before publish | publish returns `BadState` | Device not published | 3 | `try_new_with_class_metadata` checks `driver_name()` and `driver_id()` |
| F-02 | Publish with a mismatched device_kind | Driver passes a net device to `publish_block` | publish returns `InvalidInput` | Device not published | 4 | Kind check happens before `ClassDevice` construction |
| F-03 | Event bridge not registered | `ensure_event_bridge` never called (no class feature enabled) | No Activated/Removed dispatch | Device state changes do not reach class registries | 4 | `ensure_event_bridge` is called by every `*_registry_fn()` |
| F-04 | Subscriber callback panics | Callback bug causes unwinding | Later subscribers not notified | Some subsystems miss availability notices | 3 | Callbacks run in Vec order; no `catch_unwind` today; subscriber quality relied upon |
| F-06 | `devices()` returns a very large `Vec` | Many devices active at once | Allocation may fail | Caller gets an empty `Vec` (no OOM handling) | 4 | `Vec::collect` may fail; callers should handle empty results |
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
- `ClassDevice`'s `driver_name()` / `driver_id()` use `expect` — valid
  because publish validated them; tripping the `expect` indicates a bug
  in the publish path.
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
- Non-input classes have no `ClassDeviceMetadata` extension point; adding
  class-specific metadata requires a trait change.
- `ClassDevice` has no pre-removal notification (e.g. "device about to be
  removed").
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
- The publish path still validates `parent.driver_name()` /
  `parent.driver_id()`.
- Registry-lock critical sections still call no external callbacks (deadlock
  prevention).
- Device-node registration remains after `Activated` and before availability
  callbacks; removal unregisters the node before class cleanup.
- A node-allocation error must not suppress typed class availability callbacks.
- Framebuffer changes respect the `Arc<DeviceObject>` lifetime guarantee.
