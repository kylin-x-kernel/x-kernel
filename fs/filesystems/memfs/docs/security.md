# memfs — Security and Reliability Analysis

## Mount-state allocation failure

Mqueue owner allocation failure leaves the namespace cache unchanged, without
an active pin or namespace drop hook. NOWAIT avoids adding reclaim dependencies
under the cache mutex. Shared-tree identity, sealed-root behavior and per-mount
read-only handling remain covered by the mqueue regressions.

Failure to allocate the MemoryFs private owner returns NoMemory before creating
the root inode. Kernel-shmem initialization installs the shared filesystem only
after successful construction; an error leaves the slot empty for a later retry.
No anonymous file or fd is published on this path. NOWAIT avoids adding reclaim
under the singleton mutex but does not cover nested Arc/Slab allocations or
promise reserve access. Rootfs startup retains its existing fatal-boot failure
policy at the boot boundary; ordinary mounts and memfd propagate the error.

## Test Audit Scope

The audit scope covers the production sources as well as the guest test cases and fixtures in `ut-tests/lib.rs`, `ut-tests/shmem.rs`,
`ut-tests/quota.rs`, and `ut-tests/inode_quota.rs`. The tests remain private submodules of the original parent module and are
compiled only under `cfg(unittest)`; extracting the quota tests does not change capacity accounting, failure rollback,
production visibility, or existing lint exceptions. The tests add no new unsafe code or host registration.

## Trust Model

`memfs` trusts the VFS to have completed path resolution and the parent-directory permission check, and trusts the `Cred` passed in by the callback
to be a stable snapshot used for the whole operation. It is responsible for maintaining the directory structure, inode
lifetime, symbolic link contents, and the binding between a regular-file inode and the VFS-owned `AddressSpace`
(with its private `MappingPages` storage).

## External Boundaries / Attack Surface

- File creation, linking, renaming, and deletion
- regular-file read/write/truncate
- Anonymous tmpfs file object creation
- The address-space/page-cache owner exposed by `VfsInode::i_mapping`
- The MM shared object identity exposed by the inode-owned `kvfs::AddressSpace`
- The mode and `&Cred` of the create/mkdir/mknod/symlink callbacks
- Superblocks created by the `tmpfs` nodev factory.
- tmpfs mount data, including `size` / `nr_blocks` / `nr_inodes`, root metadata
  options, and remount capacity changes.

## unsafe Code Inventory

This crate contains no `unsafe`.

## Memory Safety Invariants

- A single regular-file inode owns at most one `VfsInode::i_mapping`
  `AddressSpace`.
- A single inode `AddressSpace` owns at most one private `MappingPages` implementation component.
- A given memfs backing inode number must enter the VFS-wide identity
  table keyed jointly with its owning `SuperBlock`, and obtain a unique
  `VfsInode`; `MemoryFs` must not keep a second VFS inode cache.
- All open-file / mmap paths that refer to the same inode must reuse the same owner.
- A symbolic link must not enter the regular-file page-cache path.
- An anonymous file must not be attached to a process-visible mount namespace.
- The UID/GID of a new inode must come from `inode_init_owner()`; the GID and mode propagation
  of a setgid parent directory must be completed before the inode is published.
- The filesystem type factory must not write per-mount policy into the shared superblock flags;
- The mounted tmpfs quota counts admitted physical folios, including pages that are retained by a pin, mapping token,
  or read batch after being removed from the cache; the logical file length or a sparse
  hole must not be counted as used pages; a buffered read zero-fills a hole directly, without materializing or charging;
  an mmap read fault still goes through folio materialization and charges the quota;
- The quota does not reserve physical memory; a remount must not lower the limit below current usage, and must not
  retroactively change an unlimited instance to a limited one, nor may it truncate or discard existing file data;
- `size=0` / `nr_blocks=0` is explicitly unlimited. `statfs()` reporting 0 blocks for unlimited
  is Linux-compatible behavior; it does not mean the capacity is zero or that writes must fail;
- The mount-data parser accepts only options explicitly supported by this filesystem and validates their values; `mode`, `uid`, and
  `gid` are used to construct the root metadata; `nr_inodes=0` means unlimited, a nonzero limit must not
  exceed `usize::MAX / 1024`, and numeric values and unit conversions are checked for overflow; `noswap` is accepted as a no-op because there is no swap backend;
- An inode and extra hard links are charged before publication; when references remain after the last name is removed, the inode
  quota is retained; a failed publication undoes the directory entry and returns the quota;
- The page and inode limits are both validated under the same stats write lock before being updated; on failure, neither is changed;
- Quota charge/release must be driven by the physical folio lifecycle: charge on successful publication,
  and return the quota after the last retaining reference releases the physical page; leaving the cache must not release it early,
  and memfs must not maintain a second page accounting.

## Thread Safety

- Inode metadata, directory entries, and mapping state are all protected by the lock of the respective owner.
- The root and subsequent lookup/create/link use the same VFS-wide identity table and superblock key; a new identity is
  bound to that superblock before it becomes visible.
- Regular-file content ownership lives under `VfsInode::i_mapping`; no second
  file-backed content ownership is introduced.
- `kvfs::AddressSpace` is propagated only through `Arc`, and does not expose shared raw pointers or the internal
  `MappingPages` storage.
- Mount capacity policy lives in memfs `quota.rs`. Its lifecycle provider is
  fixed during inode construction and retained once by each mapping.
- Admission takes folio -> cache index -> quota limit locks; final physical
  release returns capacity outside cache locks. Callbacks never enter mappings
  or lock folios. Remount/statfs never acquire
  cache locks. One stats snapshot supplies both page and inode capacities.
- Link removal orders locks as metadata → stats. Final inode destruction releases
  the slab lock before returning its allocation charge; admission releases stats
  before taking slab. The number allocator holds no inode reference, so final
  reclamation follows owned references without a reference-count snapshot.
- Every winning identity is charged once. The stable folio retains its charge
  and provider reference until physical deallocation; clones neither charge
  again nor allow cache removal to return capacity early.
- Sparse-hole handling belongs to memfs's read operation. Cache access retries
  retained initialization errors and excludes truncate through EOF/data copying.

## Threat Analysis

1. Hard-link paths obtain different content objects.
   - Mitigation: different dentries share the same `VfsInode::i_mapping` address-space.

2. Content remains after truncation.
   - Mitigation: `AddressSpace::truncate_setsize()` first publishes the inode size, then zeroes the surviving tail through the private cache storage
     and discards the folios beyond EOF.

3. A symbolic link wrongly enters the page-cache path.
   - Mitigation: the symlink target is stored in `FileContent.symlink` and does not go through the
     regular-file page-cache data path.

4. An anonymous file object is unexpectedly exposed to the global path namespace.
   - Mitigation: `memfs::shmem` uses a private `MemoryFs("tmpfs")` root mount to create
     regular files, and does not attach that mount to any process-visible namespace.

5. A new inode wrongly inherits the root or the caller's effective identity.
   - Mitigation: all create callbacks call `inode_init_owner()` with an explicit `&Cred`; ordinary creation takes
     `fsuid/fsgid`, and a setgid parent directory passes its GID on to the child.

6. The nodev factory bakes `nosuid/nodev/noexec` into the filesystem instance.
   - Mitigation: The `TMPFS_TYPE` factory accepts only VFS-wide
     `SuperBlockFlags`; callers apply per-mount policy when attaching the mount.

7. Unrestricted tmpfs writes turn all physical pages into non-discardable InMemory data.
   - Mitigation: a mounted tmpfs by default uses 50% of allocatable physical pages as the
     page quota; when the quota is full, materialization returns `StorageFull`.

8. Quota accounting is decoupled from the actual folio lifecycle.
   - Mitigation: quota charge/release is driven by the physical folio lifecycle; after truncate and final eviction,
     a remaining pin/token/batch still holds the charge, and the quota is returned only on the last release; regression tests cover this.

9. A memfd is rejected by exec because its mode is too narrow (runc/crun self-reexec).
   - Mitigation: `create_memfd_file()` creates a `0o777` file, so the `MAY_EXEC` check of exec passes.
     The display path `/proc/self/fd/<n>` and `/proc/self/exe` refer to the same opened backing file,
     and no writable-and-executable memory mapping is introduced.

10. memfd seal semantics deviate.
   - Mitigation: only adding seals is allowed (after `F_SEAL_SEAL`, every new seal is rejected);
     when `MFD_ALLOW_SEALING` is not set, `F_SEAL_SEAL` is applied at creation, matching Linux.

## Failure Mode and Effects Analysis (FMEA)

| Failure | Condition | Handling | Impact |
|---|---|---|---|
| tmpfs quota full | A new folio materialization request exceeds the limit | Return `StorageFull` | A userspace write receives `ENOSPC`; existing data remains available |
| tmpfs remount limit too small | The new finite limit is below current usage | Return `InvalidInput` | Userspace receives `EINVAL`; the original limit and used page count are unchanged |
| Candidate folio allocation failure | Out of memory | First re-check for a concurrently resident page; if there is no reusable page, return `NoMemory`, and nothing has been charged yet | File read/write or mmap fault fails |
| inode owner missing | attachment initialization fails | Return an error | Avoids silent corruption |
| Multiple entries for the same inode do not share an owner | VFS inode identity propagation error | Caught by audit and tests | Read/write/mmap consistency is broken |

## Fault Management

- Read, write and truncate errors are returned through `VfsResult`.
- A successful sync of an in-memory file only means no persistent I/O is needed and does not promise survival of a power loss; sync must not discard content pages.
  Half-open partial and empty ranges use the same explicit volatile fsync capability;
  VFS rejects backwards ranges before dispatch.
  The sync capability of special files is determined by the dedicated operation table installed after open.
- A missing inode/mapping binding must return an error and must not silently create an inconsistent second object.
- A capacity rejection by quota admission propagates as the filesystem `StorageFull` error. Candidate physical pages and
  heap object allocations happen before admission and still follow their own memory-allocation failure paths.

## Known Limitations

- Anonymous files are expressed as a private tmpfs mount plus a regular file; no separate VFS
  anonymous-inode type is provided.
- A mounted tmpfs quota does not provide swap-out; without swap, InMemory data that is already resident
  still cannot be dropped by generic clean-page reclaim.
- An anonymous shmem/memfd instance does not apply the default page/inode limits of a mounted tmpfs.
- The physical-folio quota does not cover inode/dentry metadata or the symlink target String (including long targets),
  and does not strictly limit unpublished candidate pages; external physical references that have been detached from the mapping are still charged to the original quota. The inode-space
  quota limits the number of nodes and extra links, and does not represent actual heap bytes; xattr is currently not supported.
- The `memfs` regular-file data path depends on the KFS `AddressSpace` and its private page-cache storage;
  `MemoryNode` itself does not directly hold ordinary file data pages.

## Audit Checklist

- Whether a regular-file inode always reuses the same `VfsInode::i_mapping`.
- Whether new paths produced by `lookup/create/link` still hit the same inode address space.
- Whether the MM paths of the same inode reuse the same `kvfs::AddressSpace`.
- Whether the symlink path completely bypasses the regular-file page cache.
- Whether the anonymous-file helper always returns an object that is not mounted in the global namespace.
- Whether all inode creation paths accept `&Cred` and call `inode_init_owner()` before publication.
- Whether the ordinary child GID under a setgid parent directory and the setgid bit of a child directory are covered by tests.
- Whether the filesystem type factory only creates the superblock and does not decide per-mount flags.
- Whether the tmpfs default is half of the allocatable physical pages, with no physical memory reserved.
- Whether `size`/`nr_blocks` parsing rejects unknown or malformed options, and whether `0` expresses unlimited.
- Whether the quota counts by physical folio, and whether truncate/final eviction wait for the last reference before returning capacity.
- Whether `statfs` reports the quota total and the remaining amount.

Finite inode limits reject excess creation and additional hard links with
`ENOSPC`; `nr_inodes=0` selects unlimited. A file-backed fault that exhausts resident capacity becomes
`BusError` and delivers `SIGBUS`; ordinary buffered writes still return `ENOSPC`.

Ordinary reads and copy_file_range use the same filesystem-owned sparse reader.
A source hole must neither increase source nrpages nor consume mount capacity,
even when native copy writes into an existing destination page at full quota.
Each bounded batch retains invalidation shared across EOF checks and copying;
only one folio is locked at a time. Initialization failures remain errors and
cannot become holes. No StorageFull error is ignored to emulate sparse reads.

Writable shmem mapping inheritance duplicates the existing writable-page count
without new-map seal admission. A positive existing count excludes WRITE sealing;
FUTURE_WRITE does not revoke the permissions of inherited mappings. The same
counter remains the sole owner of WRITE-seal exclusion, with checked addition
and no separate inherited-permission flag.

Buffered acceptance and visible size publication share the copied folio lock.
The memfs write-end hook only updates size/metadata and cannot reacquire folios
or wait for I/O. KVFS performs cancellation and cache rollback outside that
lock, preserving the existing resident-page accounting and lifecycle owner.
