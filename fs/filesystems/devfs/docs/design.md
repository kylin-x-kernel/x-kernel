# devfs — Design

Serial and loop guest fixtures retain narrow standard Box lint exceptions at
the `CharDeviceImpl` and `BioVec` ownership boundaries. These production driver
interfaces remain unchanged; the exceptions do not permit unrelated test owners.

Native console preparation now precedes the infallible static root builder.
Its fallible `native_tty()` uses NOWAIT callback ownership inside the singleton
initializer. Failure propagates from devtmpfs creation before dynamic-root or
device-node publication and leaves DEVFS retryable. The prepared console owner
is passed into the root builder rather than initialized by a lazy dereference.

## Fallible singleton mount state

Firmware capture owns an immutable `KBox<[u8]>` prepared with NOWAIT inside
the DEVFS initializer, avoiding reclaim re-entry into that singleton. Copy
failure propagates before root construction and leaves capture retryable;
missing firmware is still a successful no-op. A completed snapshot survives
later mount-preparation errors and is reused on retry. The temporary Vec used
only to create the former boxed slice is no longer needed.

The devtmpfs singleton uses `Once::try_call_once` and SimpleFs's explicit NOWAIT
private-owner policy. Reclaim during Once initialization could re-enter the same
singleton, so default KERNEL is not permitted here. The caller is still a
sleepable task for firmware capture, root construction and internal mounting.
Successful calls continue to share one superblock and internal mount.

## Fallible directory iterators

Dynamic device and devpts child_names return KBox iterators using the default
KERNEL policy under SimpleDirOps' sleepable-task contract. Device projection
locks are released at the snapshot statement boundary; pty_slave_numbers_for
returns an owned snapshot after releasing its registry lock, and each listed
number is re-validated by the owner-checked visibility lookup. Neither iterator
retains a registry guard across allocation or iteration.

## Purpose

`devfs` provides X-Kernel's singleton `devtmpfs` instance. It projects
device-node identities assigned by `kdevice` into `/dev` character nodes and
adapts input and display class handles to VFS file operations.

## Responsibilities

- Create `/dev/input/eventN`, `/dev/dri/cardN` and `/dev/dri/renderD{128+N}`
  from `kdevice::DeviceNode` events.
- Register and unregister the assigned character-device numbers with KVFS.
- Resolve dynamic directory enumeration and lookup to the current file-object
  projection.
- Detach open input and DRM file objects when their backing device is removed.

## Non-responsibilities

`devfs` does not discover hardware, bind drivers, assign class indices,
allocate major/minor numbers, choose `devname` values, or publish facts to
sysfs. `kdevice` owns those identities and their lifecycle; `kclass` publishes
the runtime input and display capabilities. KVFS owns generic pathname,
permission, inode, mount, and character-device dispatch semantics.

## Scope

```text
fs/filesystems/devfs/src/
├── lib.rs             # filesystem registration and singleton superblock
├── dynamic.rs         # DeviceNode -> DeviceFile projection
├── device_file.rs     # special-device inode projection
├── content.rs         # memory-backed regular files for writable directories
├── root.rs            # root directory assembly
└── nodes/
    ├── dri.rs         # dynamic /dev/dri directory
    ├── event.rs       # dynamic /dev/input directory and evdev operations
    └── shm.rs         # /dev/shm directory entry
```

## Guest unit-test layout

Guest tests live in `ut-tests/{cpu_dma_latency,dtb,event,loop,null_zero_full,pts,random,serial}.rs`.
Each file is mounted by `unittest::path_mod_test!` as the original private
`nodes::<name>::tests` module under `cfg(unittest)`, preserving access to
node-local fixtures and implementation details. The 46 cases include 16 event
tests gated by the `input` feature and one UART numbering test gated by
`serial`; the other 29 cases have no additional feature gate.

`ut-tests/pts.rs` retains the two serial cases previously included from
`src/tests/pts.rs` by `nodes::pts::tests`. They remain registered guest tests
for retained-slave lifetime and character-device registration conflicts.
The DTB snapshot case returns `Ignored` when firmware supplies no DTB.
These cases run through the configured kernel unittest runner, not host Cargo
tests; their source presence alone does not establish execution coverage.

## Architecture

`nodes/tty_nodes.rs` asks ktty to register fixed VT aliases once, then projects
its returned named-cdev identities through `DeviceFile::from_registration`.
The registry owns each name, class and device number; devfs does not repeat
those facts or register them again. Both aliases select the independent
VT_CONSOLE terminal. On pair registration failure ktty rolls back the first
alias, and devfs publishes neither node; procfs/sysfs observe the same result.

`nodes/serial.rs` keeps auxiliary UART names `/dev/serial/serialN` indexed from zero,
but assigns major 4 minors starting at 64. This leaves the fixed VT range 4:0
and 4:1 available while keeping the custom serial node names unchanged. The
stdout UART remains omitted because it is already available as `/dev/console`.

`DeviceFile` projects special-device identity and inode flags; it is not an
open-file operations adapter. KVFS owns character-device dispatch, including
capabilities, I/O, poll registration and open/release forwarding.

devfs publishes `/dev/ptmx` as a symlink to `pts/ptmx`, the conventional
userspace layout. The target resolves to the devpts instance mounted at
`/dev/pts` when present, and to devtmpfs's own `pts/ptmx` before such a mount.
The symlink is published even when the fallback node cannot be registered, so
a later devpts mount still provides `/dev/ptmx`. The fallback PTMX node
delegates creation to `ktty::tty::open_ptmx`, supplying its own `SimpleFs` as
the owner identity key (so pairs allocated through the fallback are owned by —
and projected only in — devfs) plus a slave inode constructor. ktty controls
allocation, opening, publication and rollback, and installs the master through
`kvfs::open_character_device`, the same adapter used by registry-based opens.
It opens the master once during the multiplexer callback; VFS does not reopen
the replacement table. Final file drop releases the selected master, not the
multiplexer. ktty owns PTY allocation and open-file lifetime; devfs does not
keep an allocation table or per-pair cdev registrations. Slave lookup returns
one persistent inode per original pair, preserving chmod/chown state. Its
opener holds only a weak original-pair reference and uses the same KVFS adapter
after successful admission.

Device-node construction propagates character registration conflicts instead
of replacing an existing driver. Static directory construction logs and omits
failed nodes; dynamic projection publishes a record only after registration
succeeds. Device removal unregisters the full single-device range. Unix98 master
and slave drivers each register one numeric range through ktty at devfs
initialization. The ranges and numeric-alias rejection policy belong to ktty.
Independently created numbered nodes have no PTY binding and fail open with EIO;
ordinary symlinks to a live slave resolve its bound inode. Final master close
hides the node and prevents old inode opens; existing slave files can outlive
the name. Pair reservation and builder-held master use own failure rollback.
The pts directory disables both positive and negative dentry caching: each name
lookup consults current pair visibility, while retained dentries keep old identity.
This deliberately pays for a lookup and a new dentry on each path resolution.
Unlike Linux devpts, which explicitly publishes and removes dentries with PTY
lifecycle changes, this projection does not maintain a synchronized cache
invalidation protocol. Enabling caching requires such a protocol for creation,
retirement and number reuse, including concurrent lookups; invalidation alone
must not introduce a stale positive or negative entry.

Unix98 range registration is a required, one-time boot invariant. Failure stops
initialization before publishing PTMX, rather than exposing a multiplexer without
its numeric-alias rejection ranges. This matches Linux's fatal registration
failure policy in `drivers/tty/pty.c:unix98_pty_init`; it is distinct from omitting
an optional device node whose registration conflicts with an existing owner.

```text
kclass input/display activation
               │
               │ register_device_node
               ▼
      kdevice DeviceNode registry
      ├── index and dev_t assignment
      ├── devname templates
      └── add/remove notifications
               │
               │ DeviceNodeEvent
               ▼
       devfs DeviceProjection
       ├── DeviceId -> DeviceNodeRecord
       ├── /dev/input/eventN -> EventDev
       ├── /dev/dri/cardN -> DrmMinor(Primary)
       └── /dev/dri/renderD{128+N} -> DrmMinor(Render)
```

`DeviceProjection` owns only filesystem-specific state: the `SimpleFs`
instance, the projected files (the primary `DeviceFile` plus, for a DRM node,
the render minor's file), the removal callback, and the `DeviceNode` snapshot
used for lookup. Both `DeviceFile`s of a DRM node share one `DrmDevice`; the
render file is removed with the primary one. `DeviceNode` remains the identity authority and is shared directly
by other projections such as sysfs.

`DynamicDeviceDir` is the non-caching `SimpleDirOps` implementation behind
`/dev/input` and `/dev/dri`. It filters projection records by
`DeviceNodeKind`, derives the final path component from `DeviceNode::devname` (primary node) or `DeviceNode::render_devname` (render node, only while the directory kind is DRM),
and clones the matching `DeviceFile` for lookup.

## Configuration and dependencies

The optional Cargo feature `input` enables `kclass/input` and `inputdev`; the top-level
`kfeat/input` feature selects it through `devfs/input`. When enabled, devfs can
adapt input node assignments to `EventDev`. Display support is unconditional
through `kclass/display`, so DRM primary nodes remain available independently.

`devfs` depends on `kdevice`, `kclass` and `kdevice_uevent`. The last one is
how a DRM card publishes the Linux `change`/`HOTPLUG=1` notification when the
display backend reports a new size: devfs owns the `DeviceNode`, so it installs
the sink that turns the DRM event into that uevent. `sysfs` depends directly on
`kdevice`; there is no dependency edge from sysfs to devfs.

## Initialization and lifecycle

1. `new_devfs` creates the singleton superblock and private root mount.
2. `dynamic::initialize` creates `DeviceProjection`, subscribes to
   `kdevice::DeviceNodeEvent`, and receives the current snapshot atomically.
3. Initialization holds the projection mutex while installing the subscriber
   and applying the snapshot. A concurrent callback waits and therefore cannot
   make an older snapshot overwrite a newer removal.
4. An add event looks up the already-active class handle by `DeviceId`, creates
   `EventDev` or `DrmDevice`, and registers the exact `DeviceNumber` assigned by
   `kdevice`. For a DRM node it registers the primary minor first, then the
   render minor from `DeviceNode::render_device_number()` over the same
   `DrmDevice`. If render registration fails, the primary character device is
   deleted before returning, so no unremovable half-projection is recorded.
5. Dynamic lookup reads the current projection, so add/remove is visible
   without rebuilding the directory or retaining cached dentries.
6. A remove event deletes the projection record, detaches the file object,
   unregisters both DRM character-device numbers (primary and render), and
   returns. `kdevice` releases the class index only after all projection
   callbacks finish.

Open files retain their concrete backend through KVFS dispatch after removal.
Read and poll check class availability even before the removal callback detaches
clients. Their
`EventClient` objects enter a terminal detached state and report `ENODEV` plus
`POLLERR|POLLHUP`. `DrmDevice` observes that its bound class handle is no longer
available and reports `ENODEV`. A later device that reuses the same index gets
a new file object and cannot reconnect an old file descriptor.

`EventDev` owns node-level identity and capability data. Each independent
`open()` creates an `EventClient` in the `VfsFile` private data; duplicated
descriptors share that open-file client. The client owns a bounded event
queue and `PollSet`. `inputdev::InputEndpoint` drains the device independently
of userspace and copies each completed frame to every subscribed client.
When a client's 256-event queue overflows, it drops the buffered events and
reports `EV_SYN/SYN_DROPPED` plus `SYN_REPORT`. One slow client cannot consume
another client's events or hold the hardware queue. Explicit removal calls
`EventDev::detach`, which stops the endpoint and detaches its clients. A read
after detach returns `ENODEV`, including a zero-length read.

`EVIOCSCLOCKID` selects `CLOCK_REALTIME` (the default), `CLOCK_MONOTONIC`,
or `CLOCK_BOOTTIME` for that open-file client. Duplicated descriptors share the
selection and queue; independent opens retain separate choices. `inputdev`
provides a capture-time realtime/monotonic pair, and devfs selects and converts
it when enqueueing each `InputEvent`. Reads only copy those cached records.
BOOTTIME follows this kernel's existing `clock_gettime` behavior: it currently
shares the monotonic origin because separate suspend accounting is absent.
The selections remain distinct; suspend accounting must be updated consistently
with the system clock API if added later.

Changing to a different clock clears a nonempty client queue and inserts one
`SYN_DROPPED` timestamped in the new domain. That marker alone is not readable
and does not wake waiters. `is_waiting_for_report` represents only this state:
inputdev already delivers complete frames, so the next actual frame's
`SYN_REPORT` makes the queued marker and new frame readable together. The
existing overflow-generated `SYN_DROPPED`/`SYN_REPORT` pair also completes it.
Repeating the same clock or changing an empty queue emits no marker. Normal
frame delivery, partial reads, the 256-event cap, and removal retain their
previous semantics. This follows Linux v6.12 `evdev_set_clk_type` and its
pending-packet behavior without introducing a general packet-buffer rewrite.

Variable-length evdev string ioctls follow Linux `str_to_user` behavior. The
terminating NUL is copied when it fits; a shorter user buffer receives the
requested string prefix without reserving one byte for a terminator.

`EVIOCGPROP` returns the device's `INPUT_PROP_*` bitmap as a
`usize`-rounded, zero-padded byte array (8 bytes on a 64-bit target), clipped
to the caller's buffer. It returns the clipped copy length, matching Linux
`bits_to_user`; a device that reports no property bitmap yields all zeros.
`EVIOCGABS(code)` accepts request numbers `0x40..=0x7f` (axis codes
`0..0x3f`) and reconstructs the Linux
`struct input_absinfo` (`value`, `min`, `max`, `fuzz`, `flat`, `res`; six
`__s32`, 24 bytes). The static `min`/`max`/`fuzz`/`flat`/`res` fields come
from the driver's `AbsInfo`; `value` is the scalar axis position tracked by
`inputdev`, so the reply does not depend on the driver caching event state.
Multi-touch payload axes report zero because Linux keeps them per slot;
`ABS_MT_SLOT` reports the last slot flushed by MT payload. All six fields are
already signed and are copied without casts. The copy is clipped to 24 bytes
and returns 0 like Linux `evdev_do_ioctl`.

The `EV_ABS` capability is read from the event bitmap cached when the evdev
node is created. A device without that capability returns `EINVAL`, matching
Linux's unallocated `dev->absinfo`, even for a zero-length request. With
`EV_ABS` present, a zero-length request succeeds with 0, an axis the device
has but does not advertise succeeds with a fully zeroed descriptor (including
`value`), and a failed driver query returns `EIO`.

Reads honor the open-file blocking mode. In blocking mode an empty queue
waits on the client's `PollSet` through `poll_file_io`; a signal that leaves
the queue empty returns `EINTR` (`KError::IoWaitInterrupted`) instead of
spinning, and `O_NONBLOCK` reports `EAGAIN`. Zero-length and undersized
buffers keep their immediate results and never wait.

For readable waits, `EventClient::register` attaches the waiter to the
client's `PollSet`. Enqueue and detach both wake that set. The caller rechecks
the queue after registration, closing the event-arrival race. No physical IRQ
identity is passed to devfs.

## Execution context

Initialization and node callbacks run in normal task context. They allocate,
acquire a sleepable mutex, create VFS objects, and register character devices;
they are not valid in hard interrupt context. Event reads and ioctls run in
process syscall context. The input layer delivers frames from a task-context
worker. Hard-IRQ dispatch does not enter `EventDev` or take a client mutex.

## Concurrency model

One `Mutex<DeviceProjection>` serializes filesystem record changes and lookup.
The `kdevice` registry never invokes callbacks while holding its own lock.
Removal extracts the record before running the class-specific detach callback
or calling `cdev_del`, so those operations cannot re-enter the projection
mutex.

The input driver's IRQ handler and `EventClient` do not share a lock. The
handler schedules inputdev, whose worker drains events and calls each client
outside the endpoint lock. A client releases its mutex before waking poll
waiters. Clients never read the VirtIO queue or register IRQ-line waiters.

## Design decisions

- Device identity lives below filesystem projections. Mounting devfs is not a
  prerequisite for sysfs discovery or uevent delivery.
- Class directories disable dentry caching because device add/remove must be
  visible immediately.
- The projection stores the `DeviceNode` snapshot that created each file, so a
  stale remove event cannot delete a different replacement record.
- DRM instances bind one concrete display handle. Multiple display devices can
  therefore receive independent `cardN` nodes and state. The render node is a
  second projection of that same handle, not a second display instance.
- Primary and render registration form one transaction at projection level:
  a render failure rolls the primary cdev back instead of publishing a record
  that removal could never reach.
- Writable devtmpfs directories accept plain file creation and hard links.
  Linux devtmpfs is ramfs-backed, so userspace treats `/dev` entries as
  creatable files; POSIX shared memory relies on this. When an init grafts a
  fresh devtmpfs mount over `/dev` (systemd's `fsmount` + `move_mount` setup),
  the boot-time `/dev/shm` tmpfs is shadowed and `sem_open` creates its
  semaphore files directly in the devtmpfs `/dev/shm` directory. `content.rs`
  provides that page-cache-backed regular-file inode, and the writable
  directories (`/dev` root and `/dev/shm`) opt in through the kvfs
  `SimpleDirOps::create_file` / `install_link` hooks. Pseudo filesystems
  without a backing store keep the rejecting defaults.

Loop advances the owning `Gendisk::diskseq` after successful SET_FD/CLR_FD
under the existing media-change locks; failed changes retain the sequence.
Loop accepts the common block Request, snapshots its backing file, and performs
I/O in the submitting task outside the media lock. Completion follows the final
backing-file operation. No per-device work list or background worker is created.

## Firmware snapshot publication

The first devtmpfs superblock construction captures the firmware DTB before
calling the static root builder. `firmware-dtb` is therefore included even when
the bootstrap namespace mounts `/dev` before virtual filesystem initialization.
Capture and static node publication belong to this single initialization stage;
there is no separate late capture API. Additional mounts share the same root
and immutable snapshot. Platforms without a firmware
DTB omit the node. The node exports firmware data as supplied; it does not
validate device functionality or prove clock accuracy.

## Idle input retry filtering

Each evdev client owns one bounded 10 ms retry timer. On expiry it checks its
poll registration snapshot and the driver's optional IRQ-safe readiness hint.
A definitely idle device rearms without waking a task; ready, detached or
legacy devices wake the poll set for normal task-context polling. Unsupported
drivers keep the previous wake fallback. The timer only runs while a waiter is
registered.
