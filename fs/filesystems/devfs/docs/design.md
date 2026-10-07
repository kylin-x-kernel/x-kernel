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

## Architecture

`DeviceFile` projects special-device identity and inode flags; it is not an
open-file operations adapter. KVFS owns character-device dispatch, including
capabilities, I/O, poll registration and open/release forwarding.
PTMX creates the pair and installs the master through `kvfs::open_character_device`,
the same adapter used by registry-based opens. It opens the master once during
the multiplexer callback; VFS does not reopen the replacement table. Final file
drop releases the selected master, not the multiplexer. Master device-number
registration and the slave PTS table retain their existing lifetime policy.

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

The optional Cargo feature `input` enables `kclass/input`; the top-level
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

Open files retain their concrete backend through KVFS dispatch after removal. `EventDev`
enters a terminal detached state and reports `ENODEV` plus
`POLLERR|POLLHUP`. `DrmCard` observes that its bound class handle is no longer
available and reports `ENODEV`. A later device that reuses the same index gets
a new file object and cannot reconnect an old file descriptor.

`EventDev::Inner.device` is its attachment state. `Some(handle)` means that
the file is attached to its original class device; `None` is terminal.
Explicit removal calls `EventDev::detach`. Reads, ioctls, and poll also call
`Inner::live_device`, which changes `Some` to `None` when the class handle is
unavailable and clears cached event/key state. A read after detach returns
`ENODEV`, including a zero-length read. If detach is observed after complete
event records were already copied, that read returns the copied byte count and
the next read returns `ENODEV`.

Variable-length evdev string ioctls follow Linux `str_to_user` behavior. The
terminating NUL is copied when it fits; a shorter user buffer receives the
requested string prefix without reserving one byte for a terminator.

## Execution context

Initialization and node callbacks run in normal task context. They allocate,
acquire a sleepable mutex, create VFS objects, and register character devices;
they are not valid in hard interrupt context. Event reads and ioctls run in
process syscall context.

## Concurrency model

One `Mutex<DeviceProjection>` serializes filesystem record changes and lookup.
The `kdevice` registry never invokes callbacks while holding its own lock.
Removal extracts the record before running the class-specific detach callback
or calling `cdev_del`, so those operations cannot re-enter the projection
mutex.

## Design decisions

- Device identity lives below filesystem projections. Mounting devfs is not a
  prerequisite for sysfs discovery or uevent delivery.
- Class directories disable dentry caching because device add/remove must be
  visible immediately.
- The projection stores the `DeviceNode` snapshot that created each file, so a
  stale remove event cannot delete a different replacement record.
- DRM instances bind one concrete display handle. Multiple display devices can
  therefore receive independent `cardN` nodes and state.
