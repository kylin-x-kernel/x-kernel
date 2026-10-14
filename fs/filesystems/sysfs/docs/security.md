# sysfs — Security and reliability

Named character identity attributes retain the cdev registration lifetime,
not merely copied bytes. Retirement marks snapshots unavailable before the
registry notifies sysfs, so held attribute reads return NoSuchDevice after
removal. Registry snapshots and subscribers are captured under one lock;
callbacks run outside it and serialize through sysfs's event/projection locks.
Failed registrations publish no record. Names/classes are validated path
components by the cdev registration API; virtual uevent attributes are read-only.

## Analysis scope

This analysis covers the entire crate, which currently consists of
`src/lib.rs`; no source module or test-only path is excluded. `kdevice` owns
device-node identity, numbering, state, and ordered lifecycle callbacks;
`kdevice-uevent` owns formatting and netlink delivery; KVFS owns filesystem
dispatch, user-copy, mount, and permission contracts at the external
boundaries used here.

## Trust model

Device descriptions are trusted kernel data copied from `kdevice`. Paths,
attributes, and uevents are visible to unprivileged user space. User space can
supply only the action string written to a `uevent` file; it cannot select the
message path, subsystem, device number, or properties.

## External boundaries

- VFS path lookup exposes generated directories, files, and symlinks.
- `uevent` writes accept user bytes and publish messages to
  `NETLINK_KOBJECT_UEVENT` listeners.
- Driver-provided names, locations, ids, and bitmaps are rendered as sysfs text
  and uevent properties.
- The crate performs no MMIO, DMA, FFI, raw user-pointer access, or firmware
  parsing.

## Unsafe code and memory safety

The crate contains no unsafe blocks. Cached tree entries own their strings, byte
vectors, and cloned `DeviceNode` values. No reference into the device
projection escapes its mutex. VFS command callbacks capture owned values.

## Invariants

- A projected device exists at one canonical `/sys/devices` path and every
  class/number link resolves to that object.
- Link targets are computed relative to the link's parent.
- Add callbacks insert before automatic publication; removal quiesces all
  attributes before automatic publication and deletes paths afterwards.
- `DEVPATH`, subsystem, number, and name come from the same registered node.
- User-written actions are accepted only from the fixed Linux action set.
- Every open device attribute rechecks the captured node's atomic lifecycle
  while holding its projected-device access gate. After removal starts, reads
  and writes return `ENODEV`.
- Ordinary attributes are created with mode `0444`; only `uevent` is created
  with mode `0644` and receives write operations.

## Thread safety

The device map and generated tree are protected by one projection mutex.
`kdevice` invokes node callbacks outside its registry lock. Initialization
holds the sysfs projection lock while installing the callback and inserting the
atomic snapshot; concurrent callbacks wait, so a remove event cannot be
overtaken by stale snapshot insertion. A separate event mutex serializes each
tree update with its targeted VFS cache invalidation, preventing concurrent
device callbacks from invalidating a newer projection state. Removal uses a
per-projected-device reader-writer gate outside the projection mutex: ordinary
attribute operations hold a read guard, while `Removing` takes the write guard
after `kdevice` marks the node unavailable. This waits for old operations and
prevents new ones from passing their state check without creating a
registry/projection lock cycle.

## Threat analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Crafted uevent write injects arbitrary properties | Medium — forged properties could misclassify a device or trigger unintended user-space policy | User writes binary or `KEY=VALUE` data | Only a trimmed action from the fixed action set is accepted; all properties are kernel-generated. |
| T-02 | Broken symlink redirects discovery outside sysfs | Medium — consumers could resolve the wrong device or escape the intended projection | A new path has a different nesting depth | `relative_symlink` derives the target from both component lists and has unit coverage for class and device links. |
| T-03 | Removal event races a stale attribute operation | Medium — an add/change event or stale snapshot could appear after remove | Hot-unplug races an open sysfs file | Atomic node state blocks new operations; the `Removing` write guard waits for in-flight operations before automatic remove publication. |
| T-04 | Driver text injects extra uevent fields | Medium — spoofed kernel metadata could change udev matching and device permissions | A device name contains control bytes | Control characters are replaced before building the NUL-delimited netlink payload. |
| T-05 | Lifecycle cache invalidation breaks a stable mountpoint | High — a mount on `/sys/fs/cgroup` or `/sys/fs/bpf` becomes unreachable through a replacement dentry | Device add/remove rebuilds the generated tree | Live directories retain persistent inodes, dynamic directories use dentry caching, and structural diff invalidates only highest changed paths, leaving unchanged static mountpoint ancestors intact. |

## Failure modes and effects

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Unknown path after device removal | Lookup returns `ENOENT`; an already open attribute returns `ENODEV` | User space rescans | 4 | Targeted positive/negative dentry invalidation plus per-node lifecycle checks. |
| F-02 | Invalid uevent action or non-UTF-8 data | Write returns `EINVAL` | No event is sent | 4 | Parse and allowlist before publication. |
| F-03 | Netlink listener queue is full | That listener drops an event | User space may need a rescan | 3 | knet bounds each queue; sysfs remains the authoritative current snapshot. |
| F-04 | Device add occurs during initial snapshot | Duplicate callback/snapshot record | None | 4 | Map insertion is idempotent by exact node publication; boot normally subscribes before discovery. |

## Privacy

The filesystem exports hardware identity and capability data already intended
for device discovery. It does not process file contents, browser data, network
payloads, or user credentials.

## Known limitations

- Only the input and DRM class projections required by current drivers are
  implemented.
- Driver directories expose binding identity but do not implement bind/unbind
  control files.
- Ordinary attributes use mode `0444`; `uevent` uses `0644`. There is no
  additional capability check beyond VFS ownership and mode enforcement.

## Accepted and externally owned risks

- Authorization for replaying an allowed uevent action is intentionally
  delegated to VFS ownership and mode checks. A process granted write access
  can generate repeated valid events; this crate accepts that residual load
  rather than adding a second capability policy.
- knet may drop a publication when a listener's bounded queue is full. This is
  accepted because sysfs remains the authoritative snapshot and user space is
  responsible for rescanning after a detected event gap.
- The accuracy of device names, locations, ids, and capability bitmaps depends
  on trusted driver providers. sysfs prevents message framing injection but
  delegates semantic validation of those facts to the provider contracts.

## Audit checklist

- Keep every externally visible field derived from `DeviceNode`.
- Keep add/remove ordering aligned with object visibility.
- Reject action strings outside the fixed set before netlink publication.
- Add path-resolution tests when introducing a deeper canonical layout.
- Keep all new driver-provided uevent values behind the shared uevent
  formatter's control-character sanitization.
