# filemap — Security and Reliability

## Trust Boundaries

`filemap` accepts inputs from:

- `posix/mm` syscall parsing;
- `process/kexec` image mapping setup;
- VFS `File::mmap()` callbacks;
- inode-owned `kvfs::AddressSpace`;
- `MmSpace` fault and clone/relocation paths.

It must not trust raw user input directly. Syscall argument validation belongs
to `posix/mm`.

## Core Invariants

- File content is owned by `kvfs::AddressSpace`, not by filemap runtimes.
- Private file post-write pages are owned by `AnonPrivateObject`.
- Runtime backing identity must match `VmArea.backing()`.
- File-backed object ids must come from the inode-owned
  `kvfs::AddressSpace::object_id()`.
- Object invalidation requests must be derived from registered mapping views,
  not from ad-hoc runtime-local object ids.
- Writable regular-file `MAP_SHARED` must use write-fault dirty tracking:
  initial shared file PTEs are installed without write permission, and the
  first write fault must hold the address-space invalidate lock shared and the
  target folio lock while completing filesystem `page_mkwrite` preparation,
  marking the inode-owned folio dirty, and making the PTE writable. Truncate
  and cache invalidation take the invalidate lock exclusively.
- `mprotect()` must not upgrade existing shared file PTEs directly to writable;
  VMA permissions may become writable, but the PTE remains write-protected
  until a write fault marks the folio dirty.
- For shmem/memfd files, shared writable mapping policy must be checked through
  KFS inode-scoped seal state: `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` reject
  new writable shared mappings and `mprotect(PROT_WRITE)` upgrades, while
  `F_SEAL_WRITE` also rejects shared write faults.
- `FileSharedRuntime::msync` must sync only the inode-owned source mapping; it
  must not touch private anonymous COW state.

## Failure Handling

- Permission elevation above file flags returns permission errors.
- File-backed faults past EOF return bad-address to the fault layer, which maps
  the condition to the bus-error class.
- Private unmapped faults check the current inode size before reusing retained
  COW state, so truncate's second PTE invalidation cannot be refaulted past EOF.
- COW races return retry-class outcomes instead of corrupting object or PTE
  state.
- Object invalidation apply failures are requeued by `MmSpace` rather than
  silently dropped.
- Shmem seal policy failures are returned as permission errors before the VMA
  permission or PTE state is upgraded.

## Lifetime Rules

- `FileSharedRuntime` and `FilePrivateRuntime` live as VMA runtime references.
- Both file runtimes keep an `Arc<VfsFile>` and reach the source only through
  `VfsFile::mapping()`; neither runtime keeps a direct page-cache reference.
- `SharedFileSourceAdapter` keeps an `AddressSpaceViewGuard`; dropping the
  runtime unregisters the object view. The guard weakly references the owning
  `AddressSpace` and is not a second content owner.
- `FilePrivateRuntime` owns an `AnonPrivateObject` for private result pages and
  keeps the Linux-like `vm_file` lifetime through `Arc<VfsFile>`; file content
  remains owned by the inode address space.
- `MmSpace` owns VMA metadata and page-table mutation sequencing.

## Locking Rules

- Address-space shape and page-table operations are serialized by the `MmSpace`
  lock and page-table mutation guards.
- `VfsInode` owns the only visible file size and serializes write/truncate with
  its data lock. Shared write faults hold that same lock across EOF recheck,
  filesystem mapping preparation, dirtying, and PTE publication.
  `kvfs::AddressSpace` protects registered views and delegates only folio
  storage to its private cache component.
- Individual folios protect their bytes with per-folio locking.
- Private anon publication and fork/COW state use `AnonPrivateObject`
  prepare/commit contracts.

## Known Limitations

- `MS_INVALIDATE`, exact range fsync, and locked-page `EBUSY`
  semantics are not implemented.
- Linux-style refusal to add `F_SEAL_WRITE` while writable shared mappings are
  already active is implemented through KFS shmem writable-shared-page
  accounting and filemap runtime registration.
- Advanced Linux behavior such as DAX, THP, readahead, reclaim, swap, memcg,
  and NUMA is out of scope.
