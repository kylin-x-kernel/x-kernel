# fs_block — Design Document

## Positioning

`fs_block` does not own block identity, the `dev_t` registry, or the VFS block-special file
operations. Those responsibilities belong to `drivers/contracts/block` and KVFS respectively, consistent with the
layering of the Linux block core / VFS.

This crate keeps only `SeekableDisk`: it adapts the canonical `block::BlockDevice` into a medium
with a byte cursor. Root filesystem type selection does not belong to the block I/O adapter; the selected implementation directly exposes
a canonical `FileSystemType` static object to KVFS.

## Architecture

```text
driver operations -> block::Gendisk -> block::BlockDevice
                                           |
                                           +--> KVFS def_blk_fops
                                           +--> KVFS get_tree_bdev
                                           `--> fs_block::SeekableDisk
```

## SeekableDisk Algorithm

Complete blocks are submitted directly to the canonical `BlockDevice`. Unaligned accesses at the head and tail use one read buffer and
one write buffer; before changing the cursor, reading, or an explicit flush, any pending partial write is submitted first, which preserves read-after-write
ordering. The cursor state is protected by exclusive `&mut self`; concurrency on a shared medium is the responsibility of the filesystem.

The total device length is read directly from the sector-derived byte capacity returned by `BlockDevice::size()`, and is no longer computed back from the logical
block count, so capacity at the end that is smaller than one logical block is not silently truncated.

A plain drop does not implicitly swallow flush errors; the filesystem must explicitly call `flush()` on the sync/unmount path.

## Design Constraints

- This crate does not build a device table, and does not resolve names back to major/minor.
- Validation of the pathname, `nodev`, and `rdev` of a device-backed mount source belongs to KVFS
  `get_tree_bdev`.
- `Gendisk`, `BlockDevice`, capacity, and the device-removal lifecycle belong to the block core.
- This crate does not define a filesystem type, root provider, or mount callback; both root and user mounts are dispatched by the
  KVFS canonical registry.

## Fallible cursor buffers

SeekableDisk::new returns KResult and allocates two zero-filled KBox byte
slices using KERNEL in reclaim-safe task context. new_with_flags permits explicit
allocation policy. Both buffers are prepared before a cursor is returned and
before device IO; second-buffer failure drops the first allocation. FAT selects
NOWAIT while its claimed device and superblock are still being initialized.
