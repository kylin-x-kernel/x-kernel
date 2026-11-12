# filemap — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Position

`filemap` is the MM-side bridge for file-backed mappings.

It is the X-Kernel counterpart of the Linux file mmap bridge spread across
`file_operations::mmap()`, `generic_file_mmap()`, `filemap_fault()`, and the
VMA-side `vm_operations_struct` setup.

It is not a file content owner. File-backed content ownership belongs to the
inode address-space object: `VfsInode::i_mapping -> kvfs::AddressSpace`.
`filemap` receives an opened `kvfs::VfsFile` and borrows the Linux-style
`file->f_mapping` equivalent through `VfsFile::mapping()`.
It has no direct dependency on the `pagecache` crate and cannot retain the
underlying cache container as a second owner.

## Responsibilities

- Convert file-backed mmap requests into `(VmArea, VmRuntimeRef)`.
- Keep Linux `MAP_SHARED` and `MAP_PRIVATE` file mapping modes explicit.
- Call the VFS `File::mmap()` callback through an internal `MmapMapper`.
- Build file-backed VMA metadata, including file offset, inode id, and path.
- Implement shared file runtime fault handling.
- Implement shared file runtime `msync` dispatch into the inode-owned
  `kvfs::AddressSpace`.
- Enforce shmem/memfd shared writable mapping seal policy through KFS
  inode-scoped checks.
- Implement private file runtime first materialization, EOF checks, file prefix
  handling, and ELF `memsz > filesz` zero-fill semantics.
- Register file object views so `kvfs::AddressSpace` truncate/invalidate work
  can reach `MmSpace`.

## Non-Responsibilities

- It does not own page-cache folios.
- It does not own inode lifecycle.
- It does not define the object/view/invalidate language.
- It does not own private anonymous page state.
- It does not own the VMA tree or page tables.
- It does not parse syscall ABI flags.

Those responsibilities belong to:

- `kvfs`: inode `i_mapping`, `AddressSpace`, and filesystem address-space ops.
- `pagecache`: cache storage algorithms privately owned by `kvfs::AddressSpace`.
- `kvfs`: VFS/open-file facade used by current file descriptor paths.
- `vmobj`: object/view/invalidate language.
- `anon`: private/shared anonymous object ownership.
- `memspace`: VMA tree, page-table mutation, fault dispatch, and invalidate
  consumption.
- `posix/mm`: Linux syscall ABI parsing and errno policy.

## Architecture

```text
posix/mm or process/kexec
  -> filemap public constructor
      -> VFS File::mmap() callback bridge
      -> build FileSharedRuntime or FilePrivateRuntime
      -> build VmArea file metadata
  -> MmSpace::map_runtime_vma()

FileSharedRuntime
  -> SharedFileSourceAdapter
  -> Arc<kvfs::VfsFile>
  -> VfsFile::mapping()
  -> VfsInode::i_mapping / AddressSpace
  -> map shared folio into PTE

msync(MS_SYNC)
  -> MmUserHandle::msync_range() -> MmSpace::msync_range()
  -> FileSharedRuntime::msync()
  -> VfsFile::fsync_range(file_range, true)
  -> filesystem FileOperations::fsync()
  -> AddressSpace::write_and_wait_range()
  -> filesystem metadata/journal/device persistence

FilePrivateRuntime
  -> Arc<kvfs::VfsFile>
  -> VfsFile::mapping() for first materialization
  -> AnonPrivateObject for post-write private state and fork COW

kvfs::AddressSpace::truncate_setsize()
  -> publish inode::i_size
  -> MappingViewNotifier (first unmap)
  -> internal cached-folio truncate
  -> MappingViewNotifier (second unmap)
  -> MmSpaceInvalidate
  -> InvalidateHandle
  -> MmSpace drain and PTE zap
```

## Modules

- `mmap.rs`
  - Public file mmap bridge functions.
  - Internal VFS `MmapMapper` bridge.
  - Shared/private file mapping mode selection.
- `runtime.rs`
  - Common file-backed VMA metadata construction.
  - Source/view registration helpers.
  - `new_file_private_vma()` for executable/image private mappings.
- `shared.rs`
  - `FileSharedRuntime`, the `MAP_SHARED` file-backed `VmRuntimeOps`.
- `private.rs`
  - `FilePrivateRuntime`, the `MAP_PRIVATE` file-backed `VmRuntimeOps`.
  - First materialization from file source and private anonymous COW handoff.
- `invalidate.rs`
  - `MappingViewNotifier` bridge from object-side invalidation into
    `MmSpace`.

## Public API

The crate root intentionally exposes only:

- `FileMmapRequest`
- `mmap_shared_file()`
- `mmap_private_file()`
- `new_file_private_vma()`

Callers must not depend on runtime internals.

## Key Flows

### File-backed `mmap`

```text
sys_mmap
  -> posix/mm parses Linux flags
  -> filemap::mmap_shared_file() or mmap_private_file()
  -> File::mmap(MmapMapper)
  -> FileSharedRuntime or FilePrivateRuntime
  -> VmArea with FileMappingInfo
  -> MmSpace::map_runtime_vma()
```

### Shared file fault

1. `MmSpace` finds the `VmArea` and dispatches to its runtime.
2. File permissions were already checked by `FileSharedRuntime` at map/protect time; write faults recheck the shmem write seal.
3. The runtime resolves the faulting page's file offset.
4. If the faulting page starts at or beyond current file length, the runtime
   returns object-level bad-address so `MmSpace` can report a bus-error class
   fault.
5. Otherwise the runtime materializes a folio through inode-owned
   `kvfs::AddressSpace` and maps the folio into the page table.
6. Shared writable mappings initially install the PTE without write permission.
   The first write fault enters `kvfs::VfsFile::page_mkwrite()`, which
   takes the address-space invalidate lock shared, locks the target folio,
   rechecks EOF, invokes the filesystem preparation callback, marks the folio
   dirty, and only then remaps the PTE writable.
7. `mprotect(PROT_WRITE)` updates VMA permissions but keeps existing shared
   file PTEs write-protected, so write-fault dirty tracking is preserved.
8. For shmem/memfd files, `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` reject new
   writable shared mappings and `mprotect(PROT_WRITE)` upgrades. `F_SEAL_WRITE`
   also rejects later shared write faults.

### Private file fault

1. `FilePrivateRuntime` computes the file source offset and VMA-local page
   prefix.
2. If this is a valid file-backed portion, it copies initial bytes through
   `VfsFile::mapping()` from the inode address space.
3. If this is an executable/image zero-fill tail (`memsz > filesz`), it keeps
   the page faultable and zero-filled.
4. Private page state is prepared and committed through `AnonPrivateObject`.
5. Later write faults use the shared private-anon COW helpers, not filemap-owned
   frame tables.
6. Every unmapped refault rechecks current inode `i_size` before consulting a
   retained private-anon page. This prevents the second truncate unmap from
   being followed by reuse of stale private object state beyond the new EOF.

### Truncate / invalidate

1. The set-length path takes the address-space invalidate lock exclusively,
   then `kvfs::AddressSpace::truncate_setsize()` publishes inode `i_size`.
2. `AddressSpace` emits mapped-view invalidation, truncates its private cache
   storage, then emits a second invalidation for a private COW race.
3. The registered view notifier calls `MmSpaceInvalidate`.
4. `MmSpaceInvalidate` submits an `ObjectInvalidateRequest` through
   `InvalidateHandle`.
5. `MmSpace` drains the request and asks the matching runtime to unmap present
   PTEs while preserving VMA metadata.

### `msync`

1. `posix/mm` validates raw `MS_*` flags and enters the captured live MM capability.
2. `MmSpace` finds one VMA and checks its file byte range under the MM mutex.
3. MM retains the shared file runtime and releases its mutex before calling the
   hook. The runtime receives only the checked file range, never a borrowed VMA.
   MM reacquires its mutex and looks up the next VMA afresh after each sync.
4. `MS_SYNC` calls the existing open file's `fsync_range(range, true)`. The
   filesystem writes and waits for intersecting dirty folios, then supplies the
   metadata, journal and device persistence required for datasync. Backend or
   recorded writeback failures propagate through the file's existing error cursor.
5. `MS_ASYNC` and unspecified writeback remain no-ops; MM carries no separate
   datasync flag because Linux MS_SYNC always selects datasync.
6. Private file mappings are not synced because post-write pages are owned by
   `AnonPrivateObject`.

## Current Compatibility Boundary

Supported now:

- read-only file `MAP_SHARED`;
- writable regular-file `MAP_SHARED` with write-fault dirty tracking;
- filesystem `page_mkwrite` preparation for shared write faults;
- memfd/shmem `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` enforcement for new
  shared writable mappings and `mprotect(PROT_WRITE)`;
- memfd/shmem `F_SEAL_WRITE` refusal while writable shared mappings are active,
  using filemap runtime registration against KFS inode-scoped shmem state;
- file `MAP_PRIVATE` first fault;
- file-private COW and fork isolation through `AnonPrivateObject`;
- EOF page-start bus-error behavior;
- final file page zero-tail behavior;
- object-driven truncate invalidation for registered views.

Not supported by the current filemap runtime:

- shared-file readahead, adaptive/asynchronous readahead and fault-around;
- hole-punch/collapse-range;
- DAX, THP, reclaim, swap, memcg, NUMA;
- dirty-PTE harvesting and write-protection rearming after writeback (Issue G).
  Range-fsync dispatch alone does not make repeated stores through an already
  writable PTE visible to the folio dirty index.

## File-page object and mapping references

Each cached Folio owns one `PageRef`; every installed file PTE owns an
independent reference. Shared faults acquire a reference under the invalidation
read lock and Folio lock, then publish a managed mapping. A first write fault
without a leaf, including a write-only mapping, calls `page_mkwrite` before
publishing a writable managed leaf. Permission upgrades verify that the Folio PA
matches the old PTE and update permissions without another persistent reference.
`PageRef` does not replace file-extent, mkwrite, content-lock or dirty protocols.

Aligned full private file pages use read-only managed PTEs. The runtime indexes
`MappedFolio` tokens by object offset; each token retains the original content
object and its lock for COW. Folios and tokens release their references without
checking the TLB. After a PTE is removed, page-table retirement retains its
reference until invalidation completes. Removing the cache entry returns tmpfs
cache quota independently of outstanding physical references.

COW rechecks EOF under the invalidation read lock, copies the complete retained
page into a `MaybeUninit` destination, and uses `prepare` / `commit_after` to
publish the existing anonymous object slot. The dedicated managed-to-external-raw
conditional replacement validates the old snapshot, publishes the target held by
the anonymous object, and retires the old file PTE reference. A failed compare
drops only the unpublished candidate. Success removes the old content token;
the old file page never enters the anonymous raw-release path. Migrating the
anonymous page's own allocation to `PageRef` remains separate work.

Fork gives each copied private file PTE an independent reference and content
token. Shared mappings retain runtime cloning and fault reconstruction. Failed
preparation removes only the new destination PTEs, preserving the parent object
and source PTEs. `mremap` uses `map_existing` to preserve the source leaf's actual
ownership protocol, prepares all destinations before removing any source, and
rolls back only its prepared destination prefix. Metadata-only retirement drops
shared/writable registrations without detaching source anonymous slots or
leaving registrations that block `F_SEAL_WRITE`.

After truncate/invalidate, munmap, `MADV_DONTNEED` or `PROT_NONE` clears a file
PTE, its content token can be released; the page table retires the physical
reference independently. VMA splits and moves preserve object offsets, and
partial cleanup affects only the selected pages. No access permissions means
no leaf, token or populated count; restoring access acquires a valid mapping
reference again. The global TLB return contract and inherited synchronous
permission/EOF completion semantics remain separate work; this change adds no
retry, recovery state or fail-stop policy.

### Bounded private-fault prefetch

Before the private demand copy, the file-backed runtime may synchronously
prefetch at most 16 missing file-cache pages through
`AddressSpace::readahead_for_fault`. The window is bounded by inode EOF and the
optional ELF file prefix; in-memory objects are skipped. A returned error does
not replace the demand read, and the helper installs no PTE and does not alter
COW ownership.

Live inode cache invalidation uses the registered file-view notifier's
`try_invalidate` callback, which returns success only after the MM has removed
matching PTEs and completed TLB invalidation. A busy MM is reported to the
address-space owner for retry, without blocking under its invalidate write lock.
Shared read faults hold the address-space invalidate read lock through cache
population and PTE publication. Shared mappings check file access and shmem
seals at creation and permission changes. Write faults check current inode
policy; transient admission contention is retried after releasing the MM.

File runtime field order drops reverse-mapping registrations before file and
backing-page references. Thus final file release can trigger filesystem teardown
under the MM lock without retaining the very view a live cache invalidation is
waiting to revoke. MM teardown removes file PTEs; their physical holds retire
independently of runtime content references. Unconfirmed retirement retains them.

The shared file runtime implements the task-context fault wait hook by waiting
for its retained file's canonical lifecycle/data admission outside the MM lock.
The runtime reference survives only to keep resources alive during that wait;
it never installs a PTE or assumes that its original VMA still exists. The MM
owner repeats lookup, access checks and write preparation afterwards.

Existing shared mappings use the runtime open callback for migration, expansion
and fork. It duplicates existing writable-page accounting rather than rechecking
new-map policy, so FUTURE_WRITE permits inherited writable VMAs while blocking
new ones. Source offsets and view lengths follow the actual VMA range; expansion
extends the existing reverse-mapping registration. The source adapter no longer
keeps a duplicate immutable length. Write faults still check current inode policy.
