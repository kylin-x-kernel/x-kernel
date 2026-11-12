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

## File-cache references and anonymous-page isolation

- Folios and installed file PTEs own independent `PageRef` holds. Content tokens
  retain the original object and its lock; Folio destruction does not wait for
  the TLB. Page-table retirement prevents reuse after leaf removal/replacement.
- Shared faults publish managed holds under invalidation, EOF and content
  protection. Permission upgrades verify the original PA and retain its mapping
  hold. Private file pages gain write access through COW rather than in place.
- Private-file read-only publication on the first fault and during fork holds
  the `AddressSpace` invalidate read lock across current EOF validation and
  `pin.map_readonly` PTE publication. Each path records the returned content token
  before releasing that guard.
- COW copies the retained original object after rechecking EOF and fully
  initializes the target before conditional publication. The dedicated managed
  source to external-raw target operation commits only on a matching snapshot.
  The old hold retires; the existing anonymous object holds the target. Failure
  before commit drops only the candidate; a committed target cannot be freed as
  an unpublished candidate.
- No access permissions means no leaf, token or populated count. Partial
  invalidation removes only corresponding tokens. Physical references survive
  independently in page tables/retirement; cache detachment returns quota.
  Content tokens do not perform TLB retirement.
- Fork gives child file leaves independent holds. Failure removes only this
  attempt's child PTEs without committing anonymous child references. Managed
  mremap destinations own independent holds; failed preparation preserves source
  PTEs and objects. Rollback retires only destination metadata and must not call
  ordinary runtime unmap that could detach source anonymous slots.
- Metadata-only retirement removes shared/writable registrations for failed
  destinations and successfully replaced sources, preventing leaked counts from
  blocking `F_SEAL_WRITE`.
- Unconfirmed retirement retains its `PageRef` and physical backing without
  retaining tmpfs cache quota. This change adds no TLB retry, recovery or
  fail-stop policy, changes no global return contract, and does not claim to
  repair inherited synchronous permission-revocation semantics.

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
waiting to revoke. MM teardown removes file PTEs; their physical holds retire
independently of runtime content references. Unconfirmed retirement retains them.

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
