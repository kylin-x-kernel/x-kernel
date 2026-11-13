# devfs — Security and reliability

Native console callback allocation failure aborts devtmpfs preparation before
the root builder publishes devices. The console and DEVFS Once initializers
remain retryable; this path neither panics on NoMemory nor silently omits
`/dev/console`. Callback ownership uses NOWAIT to avoid reclaim re-entry, while
worker preparation still requires sleepable task context.

## Singleton allocation rollback

DTB snapshot allocation uses NOWAIT and propagates NoMemory before publishing
the static root, rather than silently omitting the firmware device. Capture is
serialized by DEVFS initialization; no additional concurrent initializer is
introduced. Local snapshot tests inject preparation failure, then verify retry,
copy contents, idempotence and absent-firmware behavior. They do not force
global allocator OOM; the existing guest test checks the published file bytes.

Private-owner allocation failure returns NoMemory without publishing a root or
internal mount. `try_call_once` resets failed initialization for retry rather
than caching an error or poisoning the singleton. Previously captured firmware
state is not rolled back; its capture remains idempotent. NOWAIT only covers the
new KBox owner; existing nested allocations retain their prior behavior.

## Fallible directory iterators

Directory iterator storage failure propagates as NoMemory instead of a fatal
Box allocation. Snapshot Vec/String allocation and lazy PTY number formatting
are not covered by this guarantee. Callers must satisfy SimpleDirOps' reclaim,
IO and filesystem re-entry constraints, not merely be outside an IRQ handler.

## Named character-device projection

Fixed VT nodes are materialized from KVFS registration handles; retired handles
return NoSuchDevice and are not registered a second time. TTY registers both
aliases or rolls the first back on failure. Node permissions do not authorize
VT configuration: ktty checks the current controlling-terminal identity and
subjective credential at ioctl time. Ordinary character construction continues
to propagate registration conflicts; this change does not fabricate hardware
DeviceObjects for virtual terminals.


## Analysis scope

This analysis covers the entire crate: `src/lib.rs`, `src/device_file.rs`,
`src/dynamic.rs`, `src/root.rs`, and every module under `src/nodes/`, including
feature-gated nodes, plus the guest fixtures in `ut-tests/`. The fixtures retain
their original private module boundaries under `cfg(unittest)`; event and
serial tests retain their node feature gates. Both PTS lifecycle and registration
cases remain serial guest tests. Extraction does not widen production access
or change fixture ownership and cleanup. No in-crate source module is excluded.
KVFS dispatch and
user-copy contracts, driver class handles, framebuffer storage, and DICE
providers remain responsible for the invariants documented by their own
interfaces.

## Trust model and external boundaries

Class handles come from `kclass`; node identities, device locations, names,
and numbers come from trusted `kdevice::DeviceNode` events.
User space can open, read, poll, mmap, and issue ioctls through the generated
character nodes. The projection never accepts user-selected device numbers or
kernel object identities and does not allocate identifiers itself.

`DeviceFile` supplies inode identity, not open-file dispatch. Both registered
character opens and PTMX master replacement use KVFS's single character adapter.

Character registration conflicts do not replace another device's backend or
publish a node/record for the failed registration. PTY nodes weakly reference
their original ktty pair, not a reusable device-number lookup. A numeric master
or slave alias has no such binding and is rejected by ktty's registered driver
with EIO. The persistent inode does not keep
the pair or its number allocated. ktty owns admission and reclamation; no devfs
table or per-pair driver registration retains closed endpoints.
VFS derives capabilities from the final backend and file open mode; poll results,
event masks and registration errors remain the backend's responsibility.
devfs supplies its own `SimpleFs` as the owner identity key to `open_ptmx` — ktty compares it (`Arc::ptr_eq`/`owned_by`) to scope which pty slaves devfs may project — plus an unpublished, weakly bound slave inode constructor;
ktty controls allocation, master opening and publication. PTMX installs and
opens its master once, without holding registry locks. Successful
open transfers release responsibility to the final file; a failed backend open
must roll back its own incomplete setup. Path permissions remain unchanged.

The evdev path copies user buffers through `UserPtr`; device events and
capability bitmaps originate from the backing input driver. The DRM user-memory
boundary is documented in `io/drmdevice/docs/security.md`.

`EVIOCGPROP` and `EVIOCGABS` write into a zeroed kernel scratch value and
copy at most the caller's requested length through `UserPtr`. A driver that
reports a short bitmap therefore exposes zeros, never stale kernel bytes.
`EVIOCGBIT` clips the requested length to the event type's declared bitmap
width before allocating its scratch buffer, so a user-controlled ioctl length
cannot force an oversized kernel allocation.
`EVIOCGPROP` returns the clipped copy length like Linux `bits_to_user`, so a
caller with a shorter buffer can detect truncation from the return value.
`EVIOCGABS` always returns 0 when it succeeds, matching Linux, even when the
caller's buffer is shorter than `struct input_absinfo`. Its `value` field is a
snapshot obtained from `inputdev`; static metadata comes from the driver and
is never copied directly from device config space by devfs.

For a live device, `EventDev::read` accepts an empty buffer as a successful
zero-byte read. A nonempty buffer shorter than one `InputEvent` is rejected
with `EINVAL`; larger buffers receive only complete event records. The exported
DICE `get_rand` entry treats a zero length as a successful no-op. For a nonzero
length it fills an owned kernel buffer and copies through `write_vm_mem`; an
invalid destination returns status `1`, while a successful checked copy
returns `0`.

`EVIOCSCLOCKID` reads one signed 32-bit clock id through `UserConstPtr`.
An invalid user pointer returns `EFAULT` before changing the client. Unsupported
ids return `EINVAL` without changing the selected clock or pending records.
Only the exact ioctl command is accepted. The client mutex serializes clock
selection, queue discard, enqueue, read, and detach; no global clock is changed.
A clock-change loss marker alone is hidden from both read and poll until an
actual synchronization report arrives. A peer open's queue is never cleared.

## Unsafe code and FFI inventory

The crate contains two unsafe memory operations, one unsafe timer start, and
two exported C ABI entry points:

- `src/nodes/fb.rs::FrameBuffer::as_mut_slice` uses
  `slice::from_raw_parts_mut`. The constructor obtains `base` and `size` from
  `fbdevice`; the required invariant is that `base..base + size` remains a
  mapped, writable shadow framebuffer for the kernel lifetime. Each kernel
  call keeps the slice local to one operation. Userspace mappings may modify
  the same device memory concurrently, so callers treat visibility and write
  races as legacy framebuffer semantics and must not retain the Rust slice.
- `src/nodes/loop.rs`, in the `LOOP_SET_STATUS` handler, uses
  `slice::from_raw_parts_mut` to expose exactly `size_of::<loop_info>()` bytes
  of a live, fully initialized POD `loop_info` value as
  `MaybeUninit<u8>`. `osvm::read_vm_bytes` initializes that exact destination
  range from userspace before the typed value is passed to `set_info`.
- `src/nodes/event.rs::EventClient::register` calls
  `unsafe { Timer::start(deadline) }`. The required invariants are that VFS poll
  registration runs only after the scheduler and timer subsystems are
  initialized, that the owned timer drains delivery on drop, and that the timer
  handler uses only IRQ-safe readiness/poll operations and never sleeps or
  reenters timer dispatch.
- `src/nodes/event.rs::ongkey` is a crate-visible
  `#[unsafe(no_mangle)] extern "C"` function exported by its unmangled symbol
  name. Its stable C ABI takes no arguments, accesses no caller memory, and
  only passes `()` through `core::hint::black_box`.
- `src/nodes/dice.rs::get_rand` is a crate-visible
  `#[unsafe(no_mangle)] extern "C"` function exported by its unmangled symbol
  name. The caller supplies a userspace output address and length. The
  function fills an owned `Vec<u8>` and copies it through the checked
  `write_vm_mem` boundary; an invalid destination returns status `1`, while a
  successful write or a zero length returns `0`.

All other user-memory access uses typed OSVM or `UserPtr` helpers. Safety of
driver trait objects and VFS character-device dispatch belongs to their
provider crates.

## Invariants

- A driver-core `DeviceId` has at most one filesystem projection record.
- Every `DeviceFile` uses the exact name and character-device number in its
  source `DeviceNode`.
- Removal detaches old file objects before the `kdevice` callback returns and
  before `kdevice` releases the class index for reuse.
- A stale remove event cannot delete a replacement record because removal
  compares the complete `DeviceNode` snapshot.
- Directory lookup returns only a file object still present in the projection
  at the time of lookup.
- Each open evdev file has a bounded queue and its own `PollSet`. The caller
  rechecks readiness after registering a waiter with that set.
- Evdev capability ioctls zero-fill their scratch buffer before the driver
  writes and never copy more than the caller's buffer length.

## Thread safety

`DeviceProjection` is protected by one `ksync::Mutex`. Per-open evdev operations
use an `EventClient` mutex; DRM uses its own locks. `kdevice` delivers owned node snapshots
without holding its registry lock, so no borrowed registry data escapes into
devfs. Removal extracts the record before running detach and `cdev_del`. The
implementation requires normal task context and may allocate. Hard-IRQ code
never acquires an `EventClient` mutex; the inputdev worker distributes frames
and wakes client-owned poll registrations.

## Threat analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Snapshot and callback create conflicting files | Medium — duplicate character-device registration could hide the valid node | An add callback waits while initialization applies the same snapshot | Projection insertion deduplicates under the mutex by driver-core id. |
| T-02 | Device removal redirects an old fd to a newly reused index | High — an old descriptor could operate on a different physical device | Hot-unplug followed by another device registration | Each node owns a distinct file object; removal detaches the old object, unregisters its number, then permits index reuse. |
| T-03 | Device event arrives before the class is active | Medium — node creation fails and userspace cannot access the device | Node publication is moved ahead of the `Activated` transition | `kclass` registers the node inside its activation callback; devfs then resolves an available class handle. |
| T-04 | A stale remove deletes a new projection | Medium — the replacement device disappears from `/dev` | A future device id or assignment is reused | Removal compares the complete recorded `DeviceNode` before deleting it. |
| T-05 | Removed device remains apparently readable | Medium — callers could mistake stale data or delayed errors for live hardware | An fd survives hot-unplug | evdev checks class availability before read/poll and returns `ENODEV` or poll error/hangup even before client detach; DRM checks `ClassDevice::is_available`. |
| T-06 | An input event arrives between readiness check and waiter registration | Medium — a reader could sleep despite queued input | The queue changes while a task is preparing to sleep | Registration precedes the caller's second readiness check; either the client `PollSet` wake or the recheck observes the event. |
| T-07 | One slow reader exhausts input buffers | Medium — another reader loses events or the VirtIO queue stalls | A process leaves its event file open without reading | Each file has a 256-event cap; overflow emits `SYN_DROPPED`; inputdev continues draining the hardware queue. |
| T-08 | Short driver capability reply exposes stale kernel memory | High — kernel stack/heap disclosure | A driver reports fewer bitmap bytes than the ioctl buffer | The ioctl zero-fills its scratch bitmap and abs value before querying; the driver can only overwrite initialized bytes, and the user copy is clipped to the requested length. |

Queued input records retain the clock selected when they were enqueued.
The timekeeper supplies a correlated pair at event receipt, preventing delayed
reads or subsequent wall-clock updates from mixing clock domains. Clock changes
flush that client's unread records and signal loss before new-domain events.
The client-to-timekeeper lock order has no reverse path: inputdev releases the
timekeeper and endpoint locks before invoking clients. The BOOTTIME mapping
currently lacks separate suspend accounting, matching `clock_gettime`; this
limitation must not be presented as a complete suspend-aware clock.

## Failure modes and effects

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Class index space is exhausted in kdevice | No add event reaches devfs | Existing devices remain usable | 3 | `kclass` logs the device-node assignment failure; allocation policy remains in kdevice. |
| F-02 | Character-device unregister races with lookup | Lookup either clones the old object or reports not found | An already opened old object reports removal | 3 | Registry serialization plus detached file semantics. |
| F-03 | Active class handle cannot be found for an add event | The `/dev` node is absent | sysfs may still expose the identity | 3 | Log the inconsistent activation ordering; `kclass` publishes nodes only from the active transition. |
| F-04 | Removal wins during initial snapshot insertion | The callback waits for initialization | No stale node survives | 4 | Initialization holds the projection mutex across subscription and snapshot insertion, then the queued remove callback deletes the record. |
| F-05 | Client poll registration is rejected | The current poll operation fails | The task does not sleep without a wake source | 3 | Propagate `PollRegisterError`. |
| F-06 | Driver fails an abs or property query | Both ioctls return `EIO` | Userspace cannot mistake a backend failure for an absent axis or property | 4 | Log the driver error; do not mask it as a zero-filled success. |
| F-07 | Userspace supplies a shorter abs/property buffer | Copy is clipped; `EVIOCGPROP` reports the clipped length, `EVIOCGABS` returns 0 | Caller can detect truncation for the bitmap but not the fixed-size abs record | 4 | Bound the copy by the requested length; match Linux's per-ioctl return convention. |

## Content files in writable directories

Writable devtmpfs directories accept plain file creation and hard links from
userspace, mirroring Linux ramfs-backed devtmpfs. Creation is gated by the
generic VFS namespace policy: the parent directory needs `MAY_WRITE |
MAY_EXEC` (`/dev` and `/dev/shm` are mode 0755/01777 root-owned), so only
root or a `CAP_DAC_OVERRIDE` holder can create content — the same population
that can already create device nodes there. Content files allocate memory
only through the caller-driven page cache and the per-inode metadata lock;
they expose no cross-task state, no hardware identity and no kernel-memory
backed reads (holes read as zeroes). `link` aliases an existing inode within
one directory and cannot cross superblocks (the VFS rejects cross-mount
links before the callback runs).

## Known limitations

- Only input and DRM primary `DeviceNodeKind` values have devfs adapters.
- An input driver must provide event notification for inputdev to create an
  endpoint and devfs to publish its event file.
- There are no DRM render nodes because the DRM implementation is KMS-only.
- `EVIOCGABS` succeeds with a zero-filled record for an axis the device has
  but does not report, and returns `EINVAL` only when the cached event bitmap
  lacks `EV_ABS`, matching Linux. A driver query failure is surfaced as
  `EIO` rather than Linux's per-device error code.
- A scalar axis position is only as fresh as the last corresponding `EV_ABS`
  event inputdev has drained; devfs does not poll hardware while serving the
  ioctl. Multi-touch payload axes read as zero and `ABS_MT_SLOT` as the last
  flushed slot, because Linux keeps per-slot values outside
  `input_absinfo.value`.

## Audit checklist

- Keep subscriptions installed before snapshots are read.
- Keep `kdevice` callback delivery outside its registry lock.
- Never allocate or reinterpret node names and numbers inside devfs.
- Preserve detached-fd behavior before enabling index reuse.
- Add a class adapter before accepting a new `DeviceNodeKind` in devfs.
- Register waiters only on the client `PollSet` and keep event draining in task
  context.

Firmware DTB capture precedes the first static root construction, preventing
mount order from permanently hiding the snapshot device. The owned byte copy
remains immutable for the filesystem lifetime; reads do not access user-selected
physical addresses. Platforms without a DTB publish no snapshot device.

## Loop ioctl authorization

The block dispatcher passes the current caller's CAP_SYS_ADMIN value to loop.
As in Linux `lo_ioctl`, SET_STATUS requires write-open mode or that capability
and returns EPERM before copying on denial. SET_FD/CLR_FD add no administrator
or owner check beyond device-node access; the default 0666 mode permits ordinary
callers to reach them. This permissive mode is for development, not a claim of
Linux distribution-equivalent access control. Production policy must restrict
node ownership/modes (for example root:disk 0660) rather than add blanket ioctl
administrator checks absent from Linux. SET_FD derives read-only state from both
open modes. Raw block mmap is rejected with ENODEV until a canonical block cache
and media-change invalidation protocol are implemented together.
Readahead controls belong to the generic block layer; the old loop-local value
was disconnected from I/O and has been removed.

## Loop request lifetime

A request retains its memory, device and captured backing file until synchronous
completion. Callbacks run outside media locks. Removing or replacing the backing
file does not invalidate a previously captured file reference. Execution uses
the submitting task and does not depend on shared worker-pool availability.

Loop passes borrowed request extents to one VFS iterator operation without a
payload-sized staging allocation. The iterator owns only cursor/borrow metadata;
completion returns the original memory even when backing I/O fails or is short.

## Idle input retry filtering

Poll registration, which calls the unsafe `Timer::start`, runs only after the
scheduler and timer subsystems are initialized; the owned timer drains delivery
on drop and its handler is IRQ-compatible. The retry callback uses only the
bounded driver readiness contract and IRQ-safe poll operations; it never takes
the sleepable input mutex or touches user pointers. A false hint suppresses task wakeups, not checks, and false positives
are allowed. Cancellation or detach may leave one harmless late retry. Timer
drop must remain outside the callback and the locks it acquires.
