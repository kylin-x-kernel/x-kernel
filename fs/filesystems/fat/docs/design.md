# fat — Design Document

## Prepared VFS mount owner

FAT fill_super allocates `KBox<Arc<FatFilesystem>>` with NOWAIT and transfers the
prepared owner to KVFS without a second allocation. The canonical device claim
and nascent identity already exist; avoiding new synchronous reclaim prevents
introducing dependencies on that unfinished mount. The FAT mutex is not held
at the owner allocation. Filling the filesystem remains a sleepable operation.

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.
Guest validation must enable `KFEAT_FS_FAT`; a crate filter alone does not
enable the filesystem or register its tests.

## Positioning

`fat` plugs the FAT media objects of `axfatfs` into KVFS. It owns the FAT mount state, the directory/file inode
adapters, and the `fs_block::SeekableDisk` byte cursor; it does not own the block-device registry, mount
namespace policy, or any root-specific provider.

## Scope

- `src/lib.rs`: the canonical `FileSystemType`, `get_tree`, and the lock-protected FAT handle wrapper;
- `src/fs.rs`: `fill_super`, the mount state, and the superblock operations;
- `src/dir.rs`, `src/file.rs`: KVFS inode/file operations;
- `src/util.rs`: metadata, time, and error conversion.

## Architecture

```text
KVFS FileSystemType registry
        -> FsContext -> get_tree_bdev
        -> FatFilesystem::fill_super
        -> fs_block::SeekableDisk -> block::BlockDevice
        -> SuperBlock / VfsInode / FAT handles
```

This crate owns only a single static `FILE_SYSTEM_TYPE`, which is registered into KVFS by its own `register_init` callback. Both root boot
and user `mount(2)` enter through this object via `get_tree_bdev -> fill_super`; there is no root-specific name,
capability field, or mount callback.

## Calling Constraints / Execution Context

Mounting and file operations may allocate memory, acquire a sleepable mutex, and perform block I/O. They may only run in a blockable task context in which the scheduler and the
block subsystem are ready, and must not be called from IRQ context. Before entering
`fill_super`, KVFS has already completed validation of the source pathname, the block-special inode, `nodev`, `rdev`, and the read-only
policy of the medium.

## Algorithm Flow

1. `FileSystemType::get_tree` calls KVFS `get_tree_bdev` to obtain the canonical `BlockDevice`, and the
   VFS registry looks up or reserves an entry by `(s_type, dev_t)`; an existing instance does not call FAT again.
2. The VFS allocates a nascent `SuperBlock` carrying `s_type/s_bdev/s_flags` and only then calls `fill_super`; FAT
   obtains the device from that object and parses the format through `SeekableDisk`, installs the static `s_op` shared by all FAT mounts,
   the unique mount-private state, and the root, and does not receive or copy identity parameters; format or I/O errors are returned as
   `VfsResult` and do not panic.
3. The mount state has a fixed address, and a mutex serializes FAT handle operations.
4. Only after the KVFS `SuperBlock`, root inode, and root dentry have been established is the mountable object returned to the caller.
5. The superblock timestamp capability declares only the 1980-2107 range common to all FAT fields, and adopts
   the 1ns common granularity of Linux FAT so that the VFS does not perform incorrect uniform rounding in advance. Before writing to disk, the file metadata callback
   truncates atime to the local date boundary (the current kernel has no FAT `time_offset` or global timezone, so the local offset is
   0, i.e. UTC), truncates mtime to 2 seconds, and makes ctime equal to it; the callback returns these values actually in effect through
   `MetadataUpdate`, and the KVFS resident inode no longer publishes the original request.

## Concurrency Model

A single `Mutex<FatFilesystemInner>` serializes access to `axfatfs` and the self-referential handles.
`FsRef` records the owner address and validates a matching guard on every borrow; it does not establish a second inode cache.

## Design Decisions

- Type identity is expressed only by the static `FILE_SYSTEM_TYPE` in the KVFS registry.
- Mount instance identity and RO/RW consistency are owned by the KVFS superblock registry; FAT does not maintain a
  second device instance cache.
- Block source policy belongs to KVFS, the byte cursor belongs to `fs_block`, and FAT format state belongs to this crate.
- Initialization failures return errors, so that a root candidate and an ordinary mount use the same failure semantics.
- `TimestampLimits` does not try to express FAT's per-field time rules; the date-granularity atime and the shared mtime/ctime
  belong to the FAT inode callback, which avoids introducing FAT-specific state into the generic VFS.

## Drop / Resource Release

`Arc<SuperBlock>`/`Arc<FatFilesystem>` manage the lifetime of the mount state. File and directory wrappers access the internal FAT handles only
under the owner mutex; persistence errors from `SeekableDisk` must be propagated
through an explicit sync/flush path and must not rely on drop to swallow them.

## Fallible disk buffers

FatDisk::new propagates SeekableDisk allocation errors before entering the FAT
library constructor. It selects NOWAIT for both zero-filled buffers: fill_super
runs without the superblock registry mutex, but the canonical device is claimed
and its superblock is nascent. Reclaim must not acquire a dependency on completion
of that mount. This matches the existing private-owner policy. NoMemory remains
a VFS allocation error rather than passing through the FAT IO error adapter.

## Explicit synchronization capability

The published axfatfs dependency does not provide the open-inode directory-entry
lifecycle required for Linux-compatible unlink/rename with file fsync. F1 does
not enable FAT file/directory fsync while that prerequisite is missing. These
operations use the VFS default InvalidInput result; fdatasync, shared MS_SYNC and
loop flush propagate the corresponding failure instead of claiming persistence.
The previous generic page-cache writeback success did not persist FAT's private
metadata or device buffers. This is a known capability limitation pending the
independent upstream API work, not an alternate persistence implementation.

The kernel keeps the published dependency and carries no vendored source, local
patch or fork revision. Upstream supplies format/entry mechanisms; a later FAT
integration change will own inode references, association indexing, namespace
serialization and final cluster reclamation in the kernel.

Upstream prerequisite: https://github.com/arceos-hypervisor/axfatfs/pull/4.
