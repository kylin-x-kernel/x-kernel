# fat — Security and Reliability Analysis

## VFS owner allocation failure

Owner allocation failure propagates NoMemory before installing a root. Local
Arc owners release the unpublished FAT state; KVFS unregisters failed initialization,
wakes waiters and releases the device claim through its existing cleanup path.
NOWAIT is limited to owner storage and does not change the third-party FAT
allocator or guarantee rollback of earlier device effects.

## Trust Model

FAT media contents and block I/O results are untrusted. KVFS provides, as trusted inputs, the resolved canonical
`BlockDevice`, the mount flags, and the VFS inode/dentry lifecycle; `axfatfs` is responsible for format parsing, and this crate
is responsible for error conversion and lock domains.

## External Boundaries / Attack Surface

- FAT boot sector, allocation table, directory entries and file data;
- block read/write/flush failure;
- KVFS lookup/create/read/write/metadata callbacks.

This crate does not touch user pointers, MMIO, PIO, DMA, FFI or interrupt handlers.

## unsafe Code Inventory

`FsRef` in `src/lib.rs` extends the lifetime of the `axfatfs` file/dir handles, which are protected by the owner mutex, to
the lifetime of the wrapper, and provides guard-bound borrows through `UnsafeCell`. `from_file_handle()`,
`from_dir_handle()`, `borrow()`, `borrow_mut()` and the `Send/Sync` implementations all depend on the following invariants:

- `FatFilesystem` is kept alive at a fixed address by `Arc`, and `FatFilesystemInner` is marked address-sensitive by `PhantomPinned`;
- each `FsRef` stores the address of the owner that created it, which `assert_owner()` checks before borrowing;
- the matching filesystem mutex guard covers every access, and shared/exclusive borrows are distinguished by the guard type.

Each unsafe block, unsafe function and unsafe impl records its corresponding `SAFETY`/`# Safety`
contract next to the code.

## Memory Safety Invariants

- A FAT handle must not be accessed after the owner mount state has been destroyed, nor borrowed under the guard of another mount;
- a mutable handle borrow must be backed by the unique mutable guard;
- the root superblock may be published only after format parsing succeeds;
- the `register_init` callback may register only one static `FileSystemType`, and both the root mount and ordinary mounts must use that object;
- `fill_super` may only install private operations and the root
  into the newly created `(s_type, dev_t)` superblock already allocated by KVFS; an existing live instance on the same device must be reused by the VFS.

## Thread Safety

All FAT handle operations are serialized by the mount-wide mutex. `FsRef` does not expose
internal references directly to callers without a guard; KVFS itself is responsible for the concurrency of dentry/inode/mount topology.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Corrupted FAT metadata causes out-of-bounds access or panic | High | Malformed fields on external media | `axfatfs` parse errors are returned through `into_vfs_err`; `fill_super` does not `expect` format or I/O results, and its `expect` on `s_bdev` relies on `get_tree_bdev` installing the block device before calling it |
| T-02 | Handle accessed under the wrong owner/lock | High | Wrapper and mount state mixed up | `FsRef` stores the owner pointer, and every borrow checks for the matching mutex guard |
| T-03 | Behavior drift between the root mount and ordinary mounts | Medium | A separate root provider or callback is created | The crate's own `register_init` callback registers the only static `FILE_SYSTEM_TYPE`, which is then dispatched through the KVFS registry, `get_tree_bdev` and `fill_super` |
| T-04 | Two sets of mutable mount state are created for the same FAT device | High | Every mount calls `fill_super` directly | KVFS makes a `(s_type, dev_t)` reservation before calling FAT; an existing live superblock is reused directly, and an RO/RW mismatch returns `EBUSY` |
| T-05 | Resident inode times are inconsistent with the FAT directory entry | Medium | The VFS publishes the original request, bypassing date-granularity atime, the 2-second mtime/ctime coupling, or the 1980–2107 range | The superblock declares only the public range and 1ns granularity; the FAT callback normalizes each field, writes it to disk and returns the actual `MetadataUpdate`; a ctime-only request is not returned as an applied field |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | FAT initialization failure | Corrupted format or I/O error | `fill_super` returns an error | The current mount fails; boot stops if the root has no candidates | 2 | Propagate a typed VFS error and do not publish a half-initialized superblock |
| F-02 | block flush failure | Backend device error | The sync request fails | Persistence is not guaranteed | 2 | The explicit sync/flush path propagates the error |
| F-03 | owner check failure | Internal handle mixed up across mounts | A kernel assertion terminates the current execution | kernel panic | 1 | Wrapper construction binds the owner; review forbids bypassing the `FsRef` borrow API |
| F-04 | Timestamp exceeds the FAT range or precision | The user requests a time that cannot be encoded | The request is clamped or rounded in the FAT callback | The syscall succeeds and the resident value matches the on-media representation | 3 | Consistently use `FAT_TIMESTAMP_LIMITS`, the date-granularity atime helper and the 2-second mtime helper, and return the actual values to KVFS |

## Failure Management

Mount parsing errors and normal I/O errors are returned through `VfsResult`. An internal owner mismatch indicates a broken type contract, and an
assertion is used to prevent an erroneous reference from being produced.

## Privacy Analysis

This crate handles file names, metadata and file contents, but does not actively log this data.

## Known Limitations

The mount-wide mutex serializes FAT operations; FAT cannot fully express Unix owner/permission semantics. The current kernel has no
FAT `time_offset` mount option or global kernel timezone, so FAT local time is currently interpreted as UTC (offset 0).

## Audit Checklist

- Does the new mount capability still go through the single canonical `FileSystemType`?
- Is the new FAT handle borrowed only through the matching owner guard?
- Does the new unsafe lifetime extension list the owner, pinning, lock and aliasing invariants?
- Are format or I/O errors returned rather than causing a panic?
- Does the metadata callback return only the time fields and values actually written to FAT?

## Mount buffer reservation

Buffer failure returns from fill_super before FAT parsing or root publication.
The first buffer is freed if the second allocation fails. The existing KVFS
initialization-failure path unregisters the nascent superblock and wakes waiters;
this change does not add a separate cache or claim-release protocol. NOWAIT only
constrains buffer allocation, not subsequent filesystem IO or nested allocations.

## Synchronization limitation

FAT currently has no file/directory fsync capability with the published axfatfs
API. VFS fsync/fdatasync return InvalidInput, and MS_SYNC/loop flush propagate
failure. No code path may turn those errors into successful persistence. Enabling
this capability requires upstream entry detach/reattach and final-reclamation
mechanisms so an open unlinked or renamed handle cannot flush an obsolete slot.
No local dependency override is retained while that prerequisite is developed.

Upstream prerequisite: https://github.com/arceos-hypervisor/axfatfs/pull/4.
