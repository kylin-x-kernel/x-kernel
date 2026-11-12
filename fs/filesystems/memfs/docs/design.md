# memfs — Design Document

## Fallible superblock state

The per-IPC-namespace mqueue cache uses NOWAIT for private-owner allocation
while holding `MQUEUE_TREES`; the uncached fallback uses KERNEL without that
lock. Construction errors propagate through `get_tree_nodev` before active
pinning, drop-hook registration or cache insertion. Cache hits allocate no
owner. Nested Arc/Slab/BTreeMap allocations remain infallible.

MemoryFs, ramfs/rootfs and tmpfs builders return `VfsResult` after preparing a
`KBox<Arc<MemoryFs>>`. Ordinary mounts and private memfd creation use KERNEL in
sleepable, reclaim-safe task context before root publication and before any
fd-table installation. The kernel-shmem singleton instead passes NOWAIT while
holding `KERNEL_SHM_FS`: its initialization lock must not be re-entered by future
synchronous reclaim. Memfd does not take that lock and retains KERNEL. The
caller chooses the policy explicitly; it is not inferred from current allocator
behavior. Both paths still require task context for the surrounding VFS work.

## Test Layout

The guest unit tests live in the crate-root directory `ut-tests/` and are
mounted under their original parent module via `unittest::path_mod_test!`,
preserving the private `tests` module name, parent-module access, and `cfg(unittest)`.
The test files are `lib.rs`, `shmem.rs`, `quota.rs` and `inode_quota.rs`.
There is no additional architecture or feature gating, and no serial/user markers or host tests.
All four kernel targets select these test cases once memfs is linked; `fs/boot` depends on memfs unconditionally,
so no new filesystem feature switch is needed. Verification uses the configured kernel `UNITTEST=y` flow.

## Positioning

`memfs` provides the inode and directory-tree implementation of a pure in-memory filesystem. Regular file content is provided through
`VfsInode::i_mapping -> kvfs::AddressSpace`, which supplies
cached content and MM shared object identity.

Linux counterparts:

- A tmpfs/shmem inode holds an `address_space`
- `mm/shmem.c` manages file content through the inode-backed page cache
- `shmem_file_setup()` creates an anonymous tmpfs file object that is not attached to the global path namespace

## Background

`memfs` owns only inodes, the directory tree, symbolic links and metadata. The page cache, evict
listener and MM object identity of regular files are managed by the VFS inode address-space.
Therefore the open-file, hard-link, mmap and truncate paths of the same inode share the same content
owner, rather than each dentry or open file owning its own content object.

## Scope

- `src/lib.rs`
- `src/shmem.rs`
- `src/quota.rs`, `src/ramfs.rs`
- `ut-tests/lib.rs`, `ut-tests/shmem.rs`, `ut-tests/quota.rs`
- Connection between regular-file inodes and the VFS address-space/page-cache
- Maintenance of directories, links, symbolic links and metadata
- Inode owner initialization based on the VFS `Cred` callback
- fd-only tmpfs/shmem anonymous regular-file factory and opened-file conversion
- mounted tmpfs page quota, `size`/`nr_blocks` options, and `statfs` reporting
- Static KVFS `FileSystemType` descriptor for `tmpfs`.

## Architecture

```text
MemoryFs
  -> inode_numbers: Slab<()>
  -> Option<Arc<TmpfsAccounting>> (tmpfs; shared by all inodes)

InodeRef / MemoryNode -> Arc<Inode> -> Arc<MemoryFs>

VFS-wide identity table: (SuperBlock, ino)
  -> VfsInode
       -> i_mapping: kvfs::AddressSpace
            -> i_pages: pagecache::MappingPages (directly owned)

VfsFile::mapping()
  -> VfsInode::i_mapping / kvfs::AddressSpace
```

`memfs` itself does not define a second file-backed content owner. After the upper KFS layers enter the file cache path through
`VfsFile::mapping()`, they obtain only the inode address-space; the unified host of private
page-cache storage, mapped views and MM shared object identity is the `AddressSpace` under
`VfsInode::i_mapping`.

`MemoryFs` also does not keep a second table mapping inode numbers to `VfsInode`. During construction it first allocates the
`SuperBlock`, and then the root initializer enters the VFS-wide table through `SuperBlock::get_or_init_inode()`
to establish the root; ordinary lookup/create/link enters the same table through the superblock of the current directory inode. The slab of `MemoryFs`
itself only allocates inode numbers; directory entries and VFS operation objects directly hold the backing inode.

`TMPFS_TYPE` describes only the canonical name and nodev superblock factory.
Boot and KVFS own filesystem-type registration and per-mount policy, so that
policy is not stored in `MemoryFs` inode state. Each tmpfs mount creates an
independent instance. Device discovery uses the separate `sysfs` crate rather
than a memfs-backed pseudo sysfs.

## Calling Constraints / Execution Context

- Regular-file read/write/truncate may sleep, because they may allocate folios.
- Directory and metadata operations are protected by `memfs`'s own locks.
- Not applicable to interrupt context.

## Algorithm Flow

### Creating a Regular File

1. After the VFS completes path resolution and the parent-directory DAC check, it passes the same `&Cred` to the create/mkdir/mknod/symlink callback.
2. `memfs` calls `inode_init_owner()`, initializes the owner with `fsuid/fsgid`, and handles group inheritance from a setgid
   parent directory and setgid propagation to child directories.
3. `memfs` creates the corresponding inode; the `VfsInode` holds a stable `kvfs::AddressSpace` when constructed.
4. The file cache path is entered for the first time through the KFS page-cache file path.
5. `AddressSpace` establishes the unique private `MappingPages` implementation component.
6. The open-file, mmap, truncate and evict paths reuse the same address-space mapping.

### Creating an Anonymous File

1. The `memfs::shmem` factory creates a private `MemoryFs` named `tmpfs`.
2. The factory creates a regular file under the root mount of that private filesystem.
3. The file inode still obtains its page cache and MM object
   identity through the same `VfsInode::i_mapping`.
4. The returned `Location` is opened into an fd by the caller through KFS `OpenOptions`, and is not attached to the process-visible
   path namespace.

### tmpfs mount entry

The public VFS mount path enters `tmpfs_get_tree` in `src/lib.rs`. That entry
reads the tmpfs context options parsed by `init_tmpfs_context`, resolves the page limit and root metadata,
and passes the selected `FileSystemType`, VFS-wide `SuperBlockFlags`, and
mount options to `shmem::new_tmpfs_with_type_and_mount_options`, which
constructs the private `MemoryFs` superblock. After the factory returns, VFS
attaches the superblock to the caller's mount namespace and applies per-mount
policy there.

Without an explicit size, the mount limit is one half of `kalloc`'s allocatable
physical pages, matching Linux's default tmpfs policy. `size=<bytes>` accepts
`k`/`m`/`g`/`t`/`p`/`e` suffixes and `%` of total allocatable memory. Explicit byte and percentage sizes round up to a page, so `size=50%` of
257 allocatable pages allows 129 pages; the unspecified default still uses
integer half (128 pages), with zero selecting unlimited. `nr_blocks` uses
page units and accepts the same size suffixes. `size=0` and `nr_blocks=0`
explicitly select unlimited. `mode`, `uid`, and `gid` select the root inode's
metadata at initial mount; remount leaves those fields unchanged, as Linux does.
`size`, `nr_blocks`, and `nr_inodes` share decimal, octal and hexadecimal
number parsing with binary size suffixes. `nr_inodes` limits inode space;
zero selects unlimited, and finite limits are bounded by
`usize::MAX / 1024`. The default is `min(total_pages / 2, usize::MAX / 1024)`.
The root and each additional hard link consume one unit; directory `.` and `..`
links do not. Removing the last name preserves the inode charge until its final
reference is released. `statfs` reports total and free inode units. `noswap` is accepted as a no-op because this tmpfs implementation
has no swap backend. The page limit charges physical folios, including detached
pages retained by pins, mapping tokens or read batches, not a
reservation and not a file logical-size sum.

For a finite limit, `statfs()` reports the current maximum and available quota
pages. For an unlimited limit, it reports zero block fields like Linux
`shmem_statfs()`; zero means "no fixed capacity", not "the quota is exhausted".
Reconfigure validates explicitly requested page and inode limits together
before changing either one, without evicting existing data. A finite limit may be lowered only to the current
usage, and an unlimited instance cannot be retroactively limited; it may still
become unlimited again. A requested finite limit below current usage returns
`EINVAL` and preserves the old limit, matching Linux `shmem_reconfigure()`;
`ENOSPC` is reserved for materialization beyond an installed limit.
Anonymous shmem/memfd instances remain unlimited in both dimensions.
Both limits use zero for unlimited throughout parsing and accounting;
`Option` in a remount request only distinguishes an omitted limit from an explicit value.

### Reading a regular file

1. The KFS file path locates the inode's address-space mapping.
2. memfs `MemoryNode::read_iter` owns sparse-hole handling and never invokes
   generic readahead. It uses `AddressSpace::visit_resident_folios` to access the
   common cache with invalidation excluded and EOF sampled under that guard.
3. Existing uptodate pages supply bytes; retained failed reservations retry
   initialization and preserve errors. Only an absent identity supplies zeroes
   from memfs's static zero page, without allocation or admission.
4. File-mapping faults materialize cache identities normally. Capacity failures
   become BusError/SIGBUS; buffered write capacity failures remain ENOSPC.

### Writing a regular file

1. The KFS file path locates the inode's address-space mapping.
2. `kvfs::AddressSpace` materializes a missing folio through the tmpfs page
   quota and writes into it.
3. Candidate physical storage is prepared outside the cache index lock. The
   winning resident identity is charged before publication; a rejected charge
   returns `StorageFull` and the unpublished candidate is released. Candidate
   allocation can fail before quota admission, so the quota is not a physical
   memory reservation.
4. The inode-owned `AddressSpace` continues to provide the shared object identity and the mmap contract.

### File synchronization

The single `FileOperations::fsync` callback accepts a half-open byte range and
`data_only`. MemoryNode explicitly accepts full, partial and empty ranges for
both intents; no persistent device or required data I/O exists.

`MemoryNode` explicitly provides a successful no-op `fsync/fdatasync` for in-memory files and directories,
including tmpfs, ramfs and shmem-based memfd; these objects have no persistent device,
and synchronization must not discard the only copy of the data by clearing or evicting dirty folios.
FIFO and device inodes install a dedicated operation table at VFS open time and do not inherit this no-op.

### memfd and exec

`create_memfd_file()` creates the anonymous memfd file with the fixed mode `0o777`, which is not subject to umask (matching Linux memfd creation through `shmem_file_setup`).
The executable bit is required for `fexecve(memfd)` /
`execveat(dirfd, "", AT_EMPTY_PATH)`: runc/crun copy their own image into a memfd and
re-exec it (CVE-2019-5736 self-clone hardening), and the exec `MAY_EXEC` check must pass. The display path
of a memfd exec is the caller-supplied `/proc/self/fd/<n>`, and `/proc/self/exe` follows the same
opened backing file.

The memfd seal set is managed atomically by `ShmemSealSet` and only allows additions. `create_memfd_file`
takes the initial seals: without `MFD_ALLOW_SEALING`, `F_SEAL_SEAL` is set immediately, after which no
further seals can be added (aligned with Linux).

## Concurrency model

- Inode metadata is protected by `Mutex<Metadata>`.
- The directory entry table is protected by `Mutex<HashMap<...>>`.
- VFS inode identity within the same filesystem instance is initialized serially by the VFS-wide table, keyed by `(SuperBlock, ino)`;
  `MemoryFs` adds no cache lock or cache field.
- The inode address-space's private page-cache storage, mapped views and MM object identity
  are protected by synchronization internal to `kvfs::AddressSpace`;
- `MemoryNode::cache_residency()` gives each newly constructed mapping one
  reference to the filesystem quota; lookup never traverses weak inode or
  superblock references on a resident access.
- The generic cache serializes admission with winning identity publication,
  taking folio -> index -> quota limit locks. Final physical deallocation returns
  the charge outside folio/index locks through its non-sleeping atomic release. Remount/statfs never acquire cache locks.
- `TmpfsAccounting` protects both limits and remaining inode space with
  `RwLock<TmpfsStats>`. Inode admission/release and remount take the write lock;
  statfs samples both capacities under the read lock. No per-inode charge object
  or additional usage counter is stored.
- Namespace link removal takes metadata → stats locks. Final inode destruction
  releases its number under the slab lock, then returns its allocation charge.
  Admission releases stats before acquiring slab; stats never acquires inode locks.

## Design Decisions

1. `memfs` does not implement its own file-backed content owner, but reuses the VFS inode
   address-space mapping.
   Reason: file I/O, mmap, truncate and hard-link aliases of the same inode must share the same
   content object.

2. Symbolic links are still stored in `FileContent.symlink`.
   Reason: the current symlink data path is independent of the regular-file page cache. String allocations, including those for long targets,
   are not counted against the resident-folio quota; this quota is not a total budget for all filesystem memory.

3. Anonymous files are represented as regular files in a private tmpfs mount.
   Reason: this preserves the Linux `shmem_file_setup()` semantics of a private file object + inode-owned page
   cache, without attaching the object to the process-visible path space. The caller converts the shmem object into
   an opened `VfsFile`, and does not redo the open details of an anonymous path.

4. `memfs` does not store the "current credentials".
   Reason: the current task belongs to the upper-layer `kprocess`; the creator identity is provided by the `&Cred` of each VFS callback
   explicitly, and the inode persists only the derived UID/GID.

5. The filesystem type descriptor does not store mount flags.
   Reason: `nosuid`, `nodev`, `noexec` and the atime policy belong to each KVFS `Mount`.
   The nodev factory only selects or creates the filesystem and superblock;
   every tmpfs mount creates a new instance.

6. tmpfs size is enforced at folio materialization rather than file length.
   Reason: sparse holes do not consume pages, while a small file can dirty one
   full page. Charging physical folios makes truncate/unlink return quota after
   the last retained reference releases that allocation, without a second
   accounting pass. A buffered-read hole is served from the
   zero page, so it cannot consume the last quota page or turn `read(2)` into
   `ENOSPC`. Mmap read faults deliberately materialize and charge a folio, as
   Linux does for `shmem_fault()`; only the buffered-read path follows
   `SGP_READ` and zero-fills holes.

## Drop / Resource Release

- When an inode is released, the `AddressSpace` is destroyed when its last reference is released, and the `MappingPages` it directly holds are released along with the field.
- Physical folio destruction returns its page-quota charge. `truncate(0)` and
  final inode eviction detach entries; retained pins, tokens and batches keep
  capacity charged until they release the physical pages.
- Directory removal logic is maintained by `InodeRef` and nlink.
- `InodeRef` holds the backing inode; `Inode::drop` returns the inode number and
  allocation quota when the last reference is released, and temporary references and VFS operation objects follow the same lifecycle.

`quota.rs` owns the mount limit, usage and remount/statfs policy. Each mapping
stores one lifecycle provider reference. Admission runs only for winning
identities, whose stable folios retain one charge and provider reference each.
Clones never multiply charges. Removal and mapping destruction cannot return
capacity before the physical page is freed by its final retained reference.

Inode-space exhaustion rejects creation or an additional hard link with
`ENOSPC` before publication; publication failures undo the directory entry and
return its charge through the existing release path. A file-backed fault that exhausts resident capacity becomes
`BusError` and delivers `SIGBUS`; ordinary buffered writes still return `ENOSPC`.

### Shared sparse reader and native copy

`read_memory_file` is the common implementation for ordinary regular-file reads
and `move_copy_chunk`. Both access the same inode mapping, zero-fill source
holes without materialization, and preserve resident initialization failures.
Native copies write that prefix through the ordinary destination write checks;
a full quota does not prevent overwriting already resident destination pages.
Neither source readahead nor source quota admission occurs on holes.

Reads progress in batches of at most FolioBatch::CAPACITY (31) pages. Each batch
holds one invalidate read guard and takes one index snapshot, instead of repeating
both locks per page. Folio locks still serialize data copying. An unaligned EOF,
a short sink, or a later initialization error returns the copied prefix and
advances Kiocb by exactly that prefix. Sparse policy remains in memfs.

Writable shmem mapping inheritance duplicates the existing writable-page count
without new-map seal admission. A positive existing count excludes WRITE sealing;
FUTURE_WRITE does not revoke the permissions of inherited mappings. The same
counter remains the sole owner of WRITE-seal exclusion, with checked addition
and no separate inherited-permission flag.

Buffered write_end now runs under the copied folio lock and invalidate shared
guard. simple_write_end publishes accepted inode size and MemoryNode updates
its metadata before this handoff ends. Neither operation reenters the cache or
waits for writeback; short-accept rollback waits are owned by KVFS after unlock.
