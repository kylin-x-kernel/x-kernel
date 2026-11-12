# fs_boot — Design Document

## 9P transport preparation

host_9p_get_tree allocates the Virtio9pTransport owner with KERNEL before
get_tree_nodev and before any 9P request. Device discovery has returned an owned
class handle; no registry or transport lock is retained at allocation.
Like the mount path, this requires reclaim-safe sleepable task context with
IRQs/preemption enabled, no non-sleepable RCU section and no reclaim/IO locks.
NoMemory propagates through VfsResult, without creating a client or root.


## Fallible filesystem-owner preparation

Bootstrap rootfs and the initial cgroup filesystem now return VfsResult for
private-owner allocation. Namespace preparation runs in the late-init kernel
task, with no namespace lock held over these constructors, so their default
KERNEL allocation policy is permitted. These are not early-boot allocations.
The existing boot boundary still stops startup if a required filesystem cannot
be constructed; ordinary filesystem constructors do not impose that policy.

## Positioning

`fs_boot` owns the layout of the initial mount namespace and the root device selection policy; it does not own the mount algorithm of any specific block
filesystem. Built-in filesystem types are registered before userspace starts, and ordinary mounts, the boot
root, and reads of `/proc/filesystems` all use the same set of descriptors.

## Architecture

```text
block::block_devices -> select root policy -> /dev/<disk_name>
                                              |
bootstrap rootfs -> bootstrap devtmpfs         v
                               FsContext -> FileSystemType::get_tree
                                              |
                                              v
                                      KVFS get_tree_bdev
                                              |
                                              v
                                      real root SuperBlock
                                              |
                                              v
                                   overmount bootstrap rootfs
```

The Kconfig ext4/FAT switches only decide which block filesystems are linked into the runtime; they do not preselect the root format.
Each linked backend registers its own canonical `FileSystemType` static object through the unified `register_init` section;
`kruntime` only executes that section and does not reference or call any specific backend. `fs_boot` does not accept a root descriptor, and it defines neither a root
provider trait nor a second source of names/capabilities. Root candidates are
taken from the `REQUIRES_DEV` types in the same registry,
and `get_tree` is exactly the same as for a user `mount(2)`. This corresponds to the Linux layering in which each filesystem's init function calls
`register_filesystem()`, and `mount_root_generic()` then reads `list_bdev_fs_names()`.

The initial namespace first mounts a ramfs named `rootfs` on the KVFS structural nullfs, and temporarily points the init
`FsStruct` at it. A shared devtmpfs is then mounted at `/dev`, so that the root source can be resolved by pathname.
After the root disk is selected, an ordinary `FsContext` is constructed, the real superblock is looked up or created through `get_tree_bdev`,
and it is then overmounted onto the bootstrap root. Finally, the root/pwd of the init `FsStruct` are updated as a pair to the new
visible root. The bootstrap rootfs remains under the overlay, which corresponds to the lifecycle of the Linux initial rootfs.

After the real root is published, boot explicitly initializes the hidden mounts of `anon_inodefs` and `pipefs`. Both are
kernel pseudo filesystems that users cannot mount, and they do not enter the user-visible filesystem registry; initialization happens before
the ordinary task/eventfd/socket/pipe paths run, and runtime callers only read the already published objects.

Before userspace starts, the same shared devtmpfs, together with tmpfs, procfs, sysfs, and the optional
bpffs, is mounted on the real root. When `KFEAT_FS_CGROUP2=y`, `/sys/fs/cgroup` is also created and mounted. Usually, each fixed path directly
uses the canonical descriptor already registered by the init section to construct an `FsContext`, and then calls
`MntNamespace::mount_new()`. cgroup2 is a boot-time exception: PID 1 has not been created yet, so the current cgroup namespace used by the ordinary mount
path cannot be used. Therefore `fs_boot` asks `cgroup2fs` to construct a superblock bound to
`CgroupNamespace::initial()`, and then mounts it through namespace attach; that superblock
still references the canonical `FILE_SYSTEM_TYPE`. Boot does not look up the table a second time by name. The internal
mounts of devtmpfs/sysfs guarantee that the kernel-maintained tree stays alive after the visible mount is unmounted.
The generic mount helper requires the target
directory to already exist; paths that boot needs to create are first established level by level through `ensure_directory_path()`. The dynamic 9P
`/mnt/hostshare` and the fixed virtual filesystem paths use the same explicit policy, and recursive creation is not hidden inside the mount mechanism.
When the root is writable and lacks these directories, boot creates them according to policy; a read-only root, like a real Linux root, must
already contain the necessary mountpoints such as `/dev`, `/tmp`, `/proc`, and `/sys`, otherwise the image does not satisfy the boot contract.

## Algorithm Flow

1. The unified init section executes the registration callbacks owned by each filesystem and publishes the canonical types.
2. Establish the bootstrap rootfs, the initial namespace, and the init `FsStruct`.
3. Mount devtmpfs on the bootstrap `/dev`.
4. Select the root from the non-zero-capacity canonical `BlockDevice`s according to `KFEAT_ROOT_BLOCK`; when it is not configured, the compatibility
   path selects in `dev_t` order.
5. Iterate over the `REQUIRES_DEV` candidates from the registry; first try one complete round with RW flags, then add
   `SB_RDONLY`/`MNT_READONLY` and retry one complete round. Each time, construct an
   `FsContext` with `/dev/<disk_name>` as the source and call `MntNamespace::mount_new()`; as with Linux `mount_root_generic()`,
   only `EACCES`/`EINVAL` continue on to try the next format, and any other error stops startup immediately.
6. Update the init root/pwd as a pair to the real root after the overmount.
7. Initialize the hidden mounts of `anon_inodefs` and `pipefs`.
8. Install the boot-time virtual filesystems on the real root; ordinary types use `mount_new()`, and the enabled cgroup2
   uses an attach of the initial hierarchy superblock.

## Ownership and Concurrency

The initial namespace is established serially by the boot CPU. The publication of `block::BlockDevice` and the `dev_t` lookup are owned by
the block core; boot only holds an `Arc` during selection and mount. Kconfig only controls whether a block filesystem
implementation is linked; each linked implementation registers exactly one static descriptor into the KVFS
registry, and the root format is determined by the registry probing result.

`nosuid`, `nodev`, `noexec`, and `relatime` are written to the respective `Mount`; only the filesystem-wide read-only
state goes into `SuperBlockFlags`. Neither the bootstrap devfs nor the devfs on the real root may set `nodev`.

## Design Decisions

- Root device selection is boot policy; source resolution and fill-super are generic VFS mechanisms.
- A non-empty `KFEAT_ROOT_BLOCK` is authoritative configuration; if no disk with that name is found, startup stops, and no other media are guessed.
- `fs_boot` does not reference the ext4/FAT crates and does not maintain a filesystem name switch; `kruntime` only initializes the
  unified init section, and descriptor registration is owned by the implementations themselves.
- An empty-capacity loop disk does not take part in the root fallback, but it is still a normal block-core object.
- Fixed virtual filesystem paths belong to the initial namespace policy and are kept explicit.
- The boot-time mount of the cgroup2 hierarchy is a feature-controlled boot policy; userspace
  consumers such as `mini-oci` are not responsible for initializing the host-global hierarchy.
- Boot is responsible for recursively creating the mountpoints it declares ownership of; the KVFS mount helper only mounts already resolved directories.

Device tree snapshot capture and static node publication are done uniformly by the first root directory construction of devtmpfs.
`mount_virtual_filesystems()` is only responsible for mounting and no longer performs late device tree capture.
