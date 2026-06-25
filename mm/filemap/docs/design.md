# filemap — Design

## Position

`filemap` is the MM-side bridge for file-backed mappings.

It is the X-Kernel counterpart of the Linux file mmap bridge spread across
`file_operations::mmap()`, `generic_file_mmap()`, `filemap_fault()`, and the
VMA-side `vm_operations_struct` setup.

It is not a file content owner. File-backed content ownership belongs to the
inode address-space object: `VfsInode::i_mapping -> kvfs::AddressSpace ->
pagecache::Mapping`. `filemap` receives an opened `kfs::File` and reaches the
Linux-style `file->f_mapping` equivalent through `File::page_cache_mapping()`.

## Responsibilities

- Convert file-backed mmap requests into `(VmArea, VmRuntimeRef)`.
- Keep Linux `MAP_SHARED` and `MAP_PRIVATE` file mapping modes explicit.
- Call the VFS `File::mmap()` callback through an internal `MmapMapper`.
- Build file-backed VMA metadata, including file offset, inode id, and path.
- Implement shared file runtime fault handling.
- Implement shared file runtime `msync` dispatch into the inode-owned
  `pagecache::Mapping`.
- Enforce shmem/memfd shared writable mapping seal policy through KFS
  inode-scoped checks.
- Implement private file runtime first materialization, EOF checks, file prefix
  handling, and ELF `memsz > filesz` zero-fill semantics.
- Register file object views so `pagecache::Mapping` truncate/invalidate work
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
- `pagecache`: file-backed content object and `Mapping` identity.
- `kfs`: VFS/open-file facade used by current file descriptor paths.
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
  -> Arc<kfs::File>
  -> File::page_cache_mapping()
  -> VfsInode::i_mapping / AddressSpace
  -> pagecache::Mapping
  -> map shared folio into PTE

msync(MS_SYNC)
  -> MmSpace::msync_range()
  -> FileSharedRuntime::msync()
  -> VfsInode::i_mapping / AddressSpace::writepages_range()
  -> AddressSpaceOperations::writepages()
  -> pagecache::Mapping::writeback_range()

FilePrivateRuntime
  -> file source pagecache::Mapping from inode address-space for first materialization
  -> AnonPrivateObject for post-write private state and fork COW

pagecache::Mapping::resize()
  -> MappingViewNotifier
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
2. `FileSharedRuntime` checks file permissions.
3. The runtime resolves the faulting page's file offset.
4. If the faulting page starts at or beyond current file length, the runtime
   returns object-level bad-address so `MmSpace` can report a bus-error class
   fault.
5. Otherwise the runtime materializes a folio through inode-owned
   `pagecache::Mapping` and maps the folio into the page table.
6. Shared writable mappings initially install the PTE without write permission.
   The first write fault marks the folio dirty and remaps the PTE writable.
7. `mprotect(PROT_WRITE)` updates VMA permissions but keeps existing shared
   file PTEs write-protected, so write-fault dirty tracking is preserved.
8. For shmem/memfd files, `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` reject new
   writable shared mappings and `mprotect(PROT_WRITE)` upgrades. `F_SEAL_WRITE`
   also rejects later shared write faults.

### Private file fault

1. `FilePrivateRuntime` computes the file source offset and VMA-local page
   prefix.
2. If this is a valid file-backed portion, it copies initial bytes from the
   file source `Mapping`.
3. If this is an executable/image zero-fill tail (`memsz > filesz`), it keeps
   the page faultable and zero-filled.
4. Private page state is prepared and committed through `AnonPrivateObject`.
5. Later write faults use the shared private-anon COW helpers, not filemap-owned
   frame tables.

### Truncate / invalidate

1. `pagecache::Mapping::resize()` emits object invalidation work.
2. The registered view notifier calls `MmSpaceInvalidate`.
3. `MmSpaceInvalidate` submits an `ObjectInvalidateRequest` through
   `InvalidateHandle`.
4. `MmSpace` drains the request and asks the matching runtime to unmap present
   PTEs while preserving VMA metadata.

### `msync`

1. `posix/mm` validates raw `MS_*` flags and calls `MmSpace::msync_range()`.
2. `MmSpace` walks VMAs and dispatches shared file overlaps to the runtime.
3. `FileSharedRuntime` converts the VMA overlap to a file object byte range.
4. `AddressSpaceOperations::writepages()` writes back intersecting dirty folios
   through the inode-owned page cache.
5. Private file mappings are not synced because post-write pages are owned by
   `AnonPrivateObject`.

## Current Compatibility Boundary

Supported now:

- read-only file `MAP_SHARED`;
- writable regular-file `MAP_SHARED` with write-fault dirty tracking;
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

- Linux `page_mkwrite` filesystem callback semantics;
- readahead/fault-around;
- hole-punch/collapse-range;
- DAX, THP, reclaim, swap, memcg, NUMA.
