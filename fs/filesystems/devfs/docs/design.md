# devfs — Design

## Purpose

`devfs` provides X-Kernel's singleton `devtmpfs` instance. It projects
device-node identities assigned by `kdevice` into `/dev` character nodes and
adapts input and display class handles to VFS file operations.

## Responsibilities

- Create `/dev/input/eventN` and `/dev/dri/cardN` from `kdevice::DeviceNode`
  events.
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
├── root.rs            # root directory assembly
└── nodes/
    ├── dri.rs         # dynamic /dev/dri directory
    └── event.rs       # dynamic /dev/input directory and evdev operations
```

## Guest unit-test layout

Guest tests live in `ut-tests/{cpu_dma_latency,dtb,event,loop,null_zero_full,pts,random,serial}.rs`.
Each file is mounted by `unittest::path_mod_test!` as the original private
`nodes::<name>::tests` module under `cfg(unittest)`, preserving access to
node-local fixtures and implementation details. The 33 cases include 11 event
tests gated by the `input` feature and one UART numbering test gated by
`serial`; the other 21 cases have no additional feature gate.

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
The PTMX node delegates creation to `ktty::tty::open_ptmx`, supplying only a
slave inode constructor. ktty controls allocation, opening, publication and
rollback, and installs the master through `kvfs::open_character_device`,
the same adapter used by registry-based opens. It opens the master once during
the multiplexer callback; VFS does not reopen the replacement table. Final file
drop releases the selected master, not the multiplexer. ktty owns PTY allocation
and open-file lifetime; devfs does not keep an allocation table or per-pair cdev
registrations. Slave lookup returns one persistent inode per original pair,
preserving chmod/chown state. Its opener holds only a weak original-pair reference
and uses the same KVFS adapter after successful admission.

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
       └── /dev/dri/cardN -> DrmCard
```

`DeviceProjection` owns only filesystem-specific state: the `SimpleFs`
instance, `DeviceFile`, removal callback, and the `DeviceNode` snapshot used
for lookup. `DeviceNode` remains the identity authority and is shared directly
by other projections such as sysfs.

`DynamicDeviceDir` is the non-caching `SimpleDirOps` implementation behind
`/dev/input` and `/dev/dri`. It filters projection records by
`DeviceNodeKind`, derives the final path component from `DeviceNode::devname`,
and clones the matching `DeviceFile` for lookup.

## Configuration and dependencies

The optional Cargo feature `input` enables `kclass/input` and `inputdev`; the top-level
`kfeat/input` feature selects it through `devfs/input`. When enabled, devfs can
adapt input node assignments to `EventDev`. Display support is unconditional
through `kclass/display`, so DRM primary nodes remain available independently.

`devfs` depends on `kdevice` and `kclass`. `sysfs` depends directly on
`kdevice`; there is no dependency edge from sysfs to devfs.

## Initialization and lifecycle

1. `new_devfs` creates the singleton superblock and private root mount.
2. `dynamic::initialize` creates `DeviceProjection`, subscribes to
   `kdevice::DeviceNodeEvent`, and receives the current snapshot atomically.
3. Initialization holds the projection mutex while installing the subscriber
   and applying the snapshot. A concurrent callback waits and therefore cannot
   make an older snapshot overwrite a newer removal.
4. An add event looks up the already-active class handle by `DeviceId`, creates
   `EventDev` or `DrmCard`, and registers the exact `DeviceNumber` assigned by
   `kdevice`.
5. Dynamic lookup reads the current projection, so add/remove is visible
   without rebuilding the directory or retaining cached dentries.
6. A remove event deletes the projection record, detaches the file object,
   unregisters the character-device number, and returns. `kdevice` releases
   the class index only after all projection callbacks finish.

Open files retain their concrete backend through KVFS dispatch after removal.
Read and poll check class availability even before the removal callback detaches
clients. Their
`EventClient` objects enter a terminal detached state and report `ENODEV` plus
`POLLERR|POLLHUP`. `DrmCard` observes that its bound class handle is no longer
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
  therefore receive independent `cardN` nodes and state.

Loop advances the owning `Gendisk::diskseq` after successful SET_FD/CLR_FD
under the existing media-change locks; failed changes retain the sequence.

## Firmware snapshot publication

The first devtmpfs superblock construction captures the firmware DTB before
calling the static root builder. `firmware-dtb` is therefore included even when
the bootstrap namespace mounts `/dev` before virtual filesystem initialization.
Capture and static node publication belong to this single initialization stage;
there is no separate late capture API. Additional mounts share the same root
and immutable snapshot. Platforms without a firmware
DTB omit the node. The node exports firmware data as supplied; it does not
validate device functionality or prove clock accuracy.
