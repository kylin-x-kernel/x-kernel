# sysfs — Design

## Fallible directory iterators

SysfsDirectory::child_names returns a KBox iterator with default KERNEL policy.
The projection guard is explicitly dropped before collecting the final name
vector and allocating the iterator. The returned iterator owns its snapshot,
not a projection lock, and requires SimpleDirOps' sleepable-task context.

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

Named KVFS cdev registrations use the same device-projection tree as hardware
nodes, at `devices/virtual/<class>/<name>` with class/dev links and read-only
identity attributes. Name, class, dev_t and availability come from the cdev
registration handle; there is no ConsoleAttribute or hardcoded tty0/tty1 path.
Sysfs subscribes atomically with a registry snapshot, and named add/remove
callbacks rebuild/invalidate the projection after registry locks are released.
Read closures recheck registration availability. These virtual identity uevent
attributes are read-only; hardware DeviceNode uevent behavior is unchanged.

## Purpose

`sysfs` provides a Linux-compatible, read-mostly device discovery filesystem.
It projects `kdevice::DeviceNode` facts into `/sys/devices`, `/sys/class`,
`/sys/dev/char`, and `/sys/bus`, and exposes `uevent` attributes for standard
device-management clients.

## Responsibilities

- Project device-core node assignments into Linux-compatible device, class,
  bus, driver, and character-number paths.
- Render read-only attributes and relative symlinks from owned snapshots.
- Validate `uevent` action writes and pass them to the shared kobject event
  publisher.
- Apply Linux-compatible permissions: ordinary attributes are `0444` and
  writable `uevent` attributes are `0644`.

## Non-responsibilities

`sysfs` does not discover hardware, bind drivers, assign class indices,
allocate device numbers, create `/dev` nodes, or publish automatic lifecycle
events. `kdevice` owns node identity and lifecycle ordering,
`kdevice-uevent` publishes automatic events, `kclass` publishes runtime
capabilities, and devfs independently projects the same facts into `/dev`.
KVFS owns generic pathname, inode, mount, and permission enforcement. This
crate does not parse firmware, access hardware, or implement userspace udev
policy.

## Scope

The production implementation is in `src/lib.rs`. It owns filesystem registration,
the singleton superblock, device projection, dynamic tree lookup, symlink
construction, input attributes, and user-triggered uevent files.

## Architecture

```text
kclass input/display activation
              │
              ▼
    kdevice DeviceNode registry
              │ Added / Removing / Removed + snapshot
              ▼
 DEVICE_PROJECTION: Mutex<nodes + cached tree>
              │ per-node RwLock access gate
              │ rebuild + targeted dentry invalidation
              ▼
/sys/devices/<bus path>/...       canonical objects
/sys/class/{input,drm}/...        class links
/sys/dev/char/<major>:<minor>     number links
/sys/bus/<bus>/drivers/<driver>   bus/driver projection
              │ write action to uevent
              ▼
kdevice-uevent -> NETLINK_KOBJECT_UEVENT group 1
```

The physical path is derived from `kdevice::DeviceLocation`. PCI devices use
`pciSSSS:BB/SSSS:BB:DD.F`; MMIO and other platform devices use stable platform
paths. The allocated class index, `DeviceNumber`, `devname`, driver identity,
and input metadata all come from the same `DeviceNode` snapshot.

## Filesystem layout

For an input device the canonical object is:

```text
/sys/devices/<physical>/input/inputN/
├── name, phys, uniq, properties
├── id/{bustype,vendor,product,version}
├── capabilities/{ev,key,rel,abs,msc,sw,led,snd,ff}
├── subsystem -> /sys/class/input (relative link)
├── uevent
└── eventN/
    ├── dev
    ├── device -> ..
    ├── subsystem
    └── uevent
```

DRM uses `/sys/devices/<physical>/drm/cardN` with `dev`, `device`, `subsystem`,
and `uevent`. Class and character-number entries are relative links to these
canonical objects. `relative_symlink` computes links from source and target
components, so link depth follows the actual paths.

The root also supplies `/sys/fs/cgroup` and `/sys/fs/bpf` mountpoints required
by boot filesystem setup.

## Initialization and state

Boot initializes the projection before driver discovery, independently of the
first sysfs mount. Initialization subscribes to `kdevice::DeviceNodeEvent` and
receives the current node snapshot atomically. It never turns that snapshot
into synthetic add events. Mount-time initialization is only an idempotent
fallback for nonstandard callers.

An add callback inserts the projection before the device lifecycle coordinator
publishes its add event. Removal first changes the shared node state to
unavailable. The `Removing` callback takes the projected device's access gate
exclusively, which waits for in-flight attribute reads or uevent writes. The
coordinator then publishes remove while the path still exists, and `Removed`
finally deletes the projection. A removing node remains reserved in `kdevice`,
so a replacement with the same core ID cannot interleave with this sequence.

Each add or remove rebuilds the generated path tree once. A structural diff
identifies the highest changed paths and invalidates only those cached dentries.
Each live directory also owns a persistent inode reused across rebuilds and
cache races. Unchanged directories therefore retain their VFS identity; in
particular `/sys/fs`, `/sys/fs/cgroup`, and `/sys/fs/bpf` remain stable
mountpoints.

## Uevent protocol

Writing one of `add`, `remove`, `change`, `move`, `online`, `offline`, `bind`,
or `unbind` to a device `uevent` file publishes a NUL-delimited kernel message.
Every message carries `ACTION`, `DEVPATH`, `SUBSYSTEM`, and a netlink sequence
number. Character-device events also carry `MAJOR`, `MINOR`, and `DEVNAME`;
DRM events carry `DEVTYPE=drm_minor`. Input parent events carry product,
identity, and capability properties. `NAME`, `PHYS`, and `UNIQ` replace control
characters before framing, so driver text cannot create additional fields.

Ordinary generated attributes use mode `0444`. `uevent` uses `0644`, matching
Linux's root-writable, globally readable convention. VFS permission checks
therefore reject writes to identity and capability attributes before their
read-only callbacks run.

## Execution context and concurrency

Boot initialization, directory lookup, uevent file I/O, and device callbacks
run in normal task context. They may allocate and acquire sleepable locks and
are not suitable for interrupt context. One mutex protects both the device map
and generated path tree. Each projected device also has a reader-writer access
gate shared by every attribute callback. A lookup clones only the selected
entry, then releases the projection mutex before VFS objects perform user I/O.

Dynamic directories use normal dentry caching to preserve inode and mountpoint
identity. Lifecycle changes call KVFS targeted invalidation for changed paths,
including cached negative entries. Existing open attribute files keep their
inode object but return `ENODEV` after removal starts. One event mutex keeps a
tree update and its cache invalidation ordered against other device callbacks.

## Design decisions

- sysfs consumes `kdevice` facts directly and is a peer of devfs. Boot installs
  the sysfs projection before discovery, so device events do not depend on
  filesystem mount order.
- One device-core allocation authority keeps `/dev` and `/sys` indices,
  numbers, and names identical.
- A generated tree keeps the implementation small while preserving dynamic
  semantics. It is rebuilt only for lifecycle changes, so repeated path lookup
  does not reformat every device attribute. Device counts are bounded by the
  kdevice node-class pools.
- Relative links are computed rather than encoded with fixed `../` counts.
- Capability files use Linux's high-word-first hexadecimal format.
- Uevent files use a command-file operation so one node supports readable
  properties and validated action replay.
