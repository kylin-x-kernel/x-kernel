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
  target folio lock while completing `FileMappingOperations::page_mkwrite` preparation,
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
  `VfsFile::mapping()`; `FilePrivateRuntime` additionally retains `MappedFolio` pins in `file_pages` for mapped read-only file pages.
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

- `MS_SYNC` forwards the translated half-open file range to filesystem
  datasync; filesystem metadata/journal/flush failures propagate to the caller.
  Filesystems may conservatively persist more metadata or remote data.
- Dirty-PTE harvesting and write-protection rearming after writeback remain
  Issue G: later stores through an already writable PTE may not redirty a folio.
- `MS_INVALIDATE` and locked-page `EBUSY` semantics are not implemented.
- Linux-style refusal to add `F_SEAL_WRITE` while writable shared mappings are
  already active is implemented through KFS shmem writable-shared-page
  accounting and filemap runtime registration.
- Advanced Linux behavior such as DAX, THP, readahead, reclaim, swap, memcg,
  and NUMA is out of scope.

## File Cache References and Anonymous Page Isolation

- A file pin never grants user write permission, and an mprotect upgrade must also fault again and perform COW.
- The page-cache allocation lifetime is guaranteed by the pin held by filemap; the anonymous frame releaser does not accept file pages.
- The invalidation completion callback runs only after the corresponding PTE removal and TLB completion. A failed or queued request keeps
  the pins of the not-yet-completed ranges; the completed prefix can be reclaimed immediately without waiting for a future fault.
- A partial invalidation removes only the corresponding references by runtime object offset; the child mapping of a fork owns an independent index,
  so the parent mapping's cleanup does not revoke the child mapping's lifetime guarantee.
- The uninitialized target of COW is exposed only as a MaybeUninit slice; whole-page initialization completes before publication,
  and a PTE compare/replace failure leaves the original mapping unchanged and publishes no anonymous slot.
- COW uses only the page object retained by the mapping and rechecks EOF under the invalidation read lock;
  it does not re-fetch the cached page of the same index to replace the identity of the old mapping.
- Every token in `file_pages` exists in pairs with a currently mapped read-only PTE. The paths that clear the PTE
  (invalidation, `mprotect(PROT_NONE)`, `munmap`, `MADV_DONTNEED`) all, after TLB
  completion, have the runtime itself release the token or call `on_invalidate_complete` to delete it;
  a prefix failure of a sparse unmap keeps only the PTEs that still exist. Therefore, when a read-only fault finds a token at the same offset, returning
  `AlreadyExists` is an internal ownership-contract detection, not a user-reachable recovery path.
- A protection change decides whether to clear the PTE by the PTE flags the runtime actually returns; for example, a bare WRITE
  request becomes empty after COW removes WRITE, in which case the TLB cleanup must complete first and then the token is released.
  If a later VMA's protection change fails, the already cleared prefix must also have notified the runtime to release the token.
- After an mremap preparation failure, the source PTEs and the file/anonymous page ownership stay as they were; the target PTEs are revoked first and
  TLB handling completes before the target VMA/runtime is released. The target rollback must not perform an
  ordinary unmap that would remove shared anonymous slots, so a later direct READ, WRITE, mprotect or fork does not depend on fault compensation.
- When target preparation of a shared file fails or the old source metadata is retired, the shared/writable
  registration is deregistered separately; a mapping that has already disappeared must not keep blocking shmem `F_SEAL_WRITE`.
- A file-page installation failure in fork does not commit anonymous child references; when anonymous materialization fails,
  its helper rolls back the anonymous PTEs and references, and the outer layer revokes the already installed file PTEs and then completes the TLB flush.

### Bounded private-fault prefetch

Prefetch holds the address-space invalidate shared lock across EOF selection and
submission, while the calling `MmSpace` mutex remains held. It does not overwrite
cached or dirty pages. Its exclusive `file_end` bounds selected pages, not the
last page's file contents; the existing private-frame copy still confines ELF BSS
zero-fill to the newly allocated frame. Returned prefetch errors are followed by
the authoritative demand read; allocator aborts are not recoverable.

A live cache page must not be released on the strength of a queued reverse-map
notification. The synchronous notifier reports completed PTE/TLB revocation or
contention/error. Shared read faults publish their PTE under the cache owner's
invalidate read guard. Shared mapping creation and permission upgrades check
file access and shmem seals. A write fault rechecks inode mutability, and its
VFS policy guard does not wait under the MM lock when a conflicting lifecycle
or data-policy transition owns the resource.

File runtime field order drops reverse-mapping registrations before file and
backing-page references. Thus final file release can trigger filesystem teardown
under the MM lock without retaining the very view a live cache invalidation is
waiting to revoke. MM teardown removes PTEs and completes TLB invalidation before
retiring the runtime's backing references.

WouldBlock on a shared write fault requires a backing-resource wait outside MM
serialization. The runtime uses its existing file reference and VFS admission
locks, without adding a wait registry or duplicating state. Permission changes
or unmapping during the wait are checked by the next MM lookup before any PTE
or user-buffer write can be published.

Existing shared mappings use the runtime open callback for migration, expansion
and fork. It duplicates existing writable-page accounting rather than rechecking
new-map policy, so FUTURE_WRITE permits inherited writable VMAs while blocking
new ones. Source offsets and view lengths follow the actual VMA range; expansion
extends the existing reverse-mapping registration. The source adapter no longer
keeps a duplicate immutable length. Write faults still check current inode policy.

The `msync` runtime hook runs without the MM mutex and takes an already checked
file byte range. Its owned runtime retains the source open file even if its VMA
is concurrently removed. It must not recover coordinates from live VMA state.
