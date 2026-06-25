# filemap — Security and Reliability

## Trust Boundaries

`filemap` accepts inputs from:

- `posix/mm` syscall parsing;
- `process/kexec` image mapping setup;
- VFS `File::mmap()` callbacks;
- inode-owned `pagecache::Mapping`;
- `MmSpace` fault and clone/relocation paths.

It must not trust raw user input directly. Syscall argument validation belongs
to `posix/mm`.

## Core Invariants

- File content is owned by `pagecache::Mapping`, not by filemap runtimes.
- Private file post-write pages are owned by `AnonPrivateObject`.
- Runtime backing identity must match `VmArea.backing()`.
- File-backed object ids must come from inode-owned `MappingIdentity`.
- Object invalidation requests must be derived from registered mapping views,
  not from ad-hoc runtime-local object ids.
- Writable regular-file `MAP_SHARED` must use write-fault dirty tracking:
  initial shared file PTEs are installed without write permission, and the
  first write fault marks the inode-owned folio dirty before making the PTE
  writable.
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
- COW races return retry-class outcomes instead of corrupting object or PTE
  state.
- Object invalidation apply failures are requeued by `MmSpace` rather than
  silently dropped.
- Shmem seal policy failures are returned as permission errors before the VMA
  permission or PTE state is upgraded.

## Lifetime Rules

- `FileSharedRuntime` and `FilePrivateRuntime` live as VMA runtime references.
- `SharedFileSourceAdapter` keeps a `MappingViewGuard`; dropping the runtime
  unregisters the object view.
- `FilePrivateRuntime` owns an `AnonPrivateObject` for private result pages but
  does not own the file source object.
- `MmSpace` owns VMA metadata and page-table mutation sequencing.

## Locking Rules

- Address-space shape and page-table operations are serialized by the `MmSpace`
  lock and page-table mutation guards.
- `pagecache::Mapping` protects its folio tree, length, and registered views.
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
