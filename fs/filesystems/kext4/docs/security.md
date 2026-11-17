# KExt4 — Security and Reliability Analysis

## VFS owner allocation failure

If the Ext4Mount owner cannot be allocated, fill_super returns NoMemory and drops
the prepared state before installing operations, private data or a root. KVFS
performs its existing failed-initialization transition and releases the reserved
identity/claim. Orphan cleanup starts only after the owner is installed, so
this allocation failure precedes orphan mutations. Prior journal replay is not
undone, and nested mount allocations keep their existing contracts. NOFS still permits reclaim/I/O and is not used as a proof
that canonical-device initialization can safely be re-entered.

## Writeback callback allocation boundary

Callback owner OOM returns NoMemory before queue sequence/slot publication and
before any data BIO submission. The finish callback is not moved into a fallible
constructor: failed reservation first records the error, then releases captures
in the submitting task. Worker initialization remains retryable; an already
prepared worker is reused. Prior unwritten extent allocation is not rolled back
by this owner change and retains the existing retry contract.

Tests inject failure into private worker preparation and finish reservation, not
the global allocator. They check unpublished state, retry, NOWAIT reservation,
error-before-capture-drop ordering, no submitted data writes and exactly-once
completion/capture destruction on retry. WorkHandle storage, Arc/Vec allocations,
BIO callbacks and BIO buffers keep their existing allocation contracts; this is
not end-to-end OOM safety. NOWAIT does not promise NMI safety or bounded latency.

## Shared snapshot allocation boundary

Direct vector-to-`Arc` conversion does not make metadata snapshots fallible:
vector and shared-slice allocations retain the existing fatal OOM behavior.
It introduces no `KERNEL` reclaim permission and does not relax the existing
lock or execution-context requirements. BIO buffers and writeback callback
owners are outside this conversion cleanup.

## Trust Model

Allocation requests borrow their actual ext4 inode. A physical goal selects the
starting group; without a goal, data allocation uses the inode's group. Metadata
allocation and block release also receive their owner, while group bitmap
primitives do not own inode accounting. A preallocation group twin must retain a
strong reference to the object borrowed by the request; an identity mismatch
fails before the first bitmap mutation.

Recovery requires exclusive access before root publication and retains the same
`Ext4SbInfo` and journal owner. Replay refreshes the disk mirror and derived policy
before mount capabilities are fixed and the private owner is installed. Orphan
cleanup must succeed before the root inode is loaded. Failure follows nascent
superblock teardown, releasing private state and the device claim without
publishing an accessible root directory.

Metadata reload validates the decoded superblock's features and frozen
geometry before changing allocator counters. Recovery refresh additionally
checks journal backing identity and hash policy before installing that same
image as the new mirror. It does not decode the superblock a second time or
consult the previous mirror to initialize the new policy. Ordinary checkpoint
reload preserves the existing mount mirror and static policy.

Protected ranges are reconstructed from filesystem geometry and the mounted
journal mapping, using the same checked insertion path at mount and recovery.
The index must remain ordered and non-overlapping for binary-search queries;
journal ownership permits only its inode to reference its blocks. A journal range
that overlaps filesystem metadata is rejected before the new index is installed.
Recovery reconstruction does not trust or copy the previous derived index. Changes to the
loaded journal's backing identity are rejected before its mapping can be assigned
to a different inode.

KExt4 treats the block device only as an I/O transport layer and does not assume that the metadata on it is valid. The
superblock, group descriptors, directory blocks, extent trees, xattr blocks, bitmaps and
journal records read from disk are all external input, and must pass range, format and checksum validation before they are interpreted or modified.

When the superblock feature bitmap enters the filesystem logic, it is wrapped into strongly typed
flags distinguished by compat category. Unknown bits are not discarded during decoding, and unsupported incompat bits still cause the mount to fail, so that type
encapsulation does not relax the validation of features from an untrusted disk.

KExt4 currently provides a complete mutation contract only for extent-backed block maps, so images lacking
`EXT4_FEATURE_INCOMPAT_EXTENTS` fail before the mount state is published. This check prevents a valid but unsupported
legacy indirect format from first entering a writable runtime state and then having deferred writeback turn writes that already succeeded in user space
into asynchronous `EOPNOTSUPP`, a journal shutdown error or partial persistence. Even if the superblock enables the
extents feature, `write_begin`/`page_mkwrite` on an individual legacy inode is rejected synchronously, before the delayed-allocation
reservation is made and before user data is marked dirty.

KExt4's KVFS operation implementations trust that KVFS has already provided kernel-owned path names, dentries, inode `AddressSpace`
(including directly held `MappingPages` storage) and file lifecycle callbacks. The KExt4 core does not directly dereference user-space pointers.
It also trusts that `fill_super` receives only a nascent superblock that KVFS has already reserved by canonical `(s_type, BlockDevice)` and holds an
exclusive claim on; KExt4 does not build a parallel device-to-mount cache.

## External Boundaries / Attack Surface

The relevant boundaries include:

- ext4 block-device images, including malicious or corrupted metadata;
- JBD2 journal replay records and checksum fields;
- directory names and dirent file types read from disk;
- extent tree entries and block mappings;
- the KVFS FIEMAP request range, and the extents that the core reports uniformly from the on-disk mapping and the inode extent-status ranges;
- KVFS fallocate's mode, offset and len: mode bits are admitted against the
  backend's `supported_fallocate_modes()` declaration. The kvfs layer filters
  requests routed through `VfsFile::fallocate` (mode 0, UNSHARE_RANGE,
  COLLAPSE_RANGE, INSERT_RANGE; kext4 declares KEEP_SIZE, stray combinations are
  rejected, and UNSHARE_RANGE — needing reflink — is rejected like Linux ext4
  without shared blocks), and the posix entry consults the same mask before
  dispatching PUNCH_HOLE/ZERO_RANGE: a backend that declares the mode owns it, and
  kext4 does not, so those are emulated by zeroing the range without freeing
  blocks;
  append-only inodes allow only plain preallocation and immutable inodes reject
  everything (Linux `vfs_fallocate`'s EPERM semantics, judged in the posix entry
  before the node-type dispatch, the size-bound check and the backend mask); the
  posix layer rejects `offset < 0` and `len <= 0` (EINVAL), and the kext4 entry
  additionally validates `offset + len` overflow and `max_file_size()`;
  on ENOSPC the committed batches remain as unwritten preallocation beyond EOF
  (block occupancy observable via df/fiemap/st_blocks, surviving across close and
  resumable via retry); before returning the original error, `i_disksize` advances
  to the successfully allocated prefix (capped at the request end, never shrinking)
  and ctime/mtime are refreshed, matching Linux `ext4_alloc_file_blocks`; this
  failure state is defined, not undefined;
- xattr names, values, external xattr block headers, refcounts and checksums;
- KVFS runtime operations such as create, unlink, rename, truncate, writeback, fsync and syncfs;
- the atime, mtime and ctime passed in by KVFS, and the representable range determined by the on-disk inode size/extra timestamp fields;
- device write/flush failures during transaction commit, journal replay or checkpoint.

As checked, this crate uses `#![forbid(unsafe_code)]`, does not access MMIO/PIO directly, does not manage DMA
ownership, and does not use FFI, inline assembly or architecture-specific raw interfaces.

## Read Buffering and Failure Publication Boundary

Whole-block initialized file reads reuse the caller's `&mut [u8]`, without introducing unsafe, user-pointer dereferences or
new DMA ownership. The block trait accepts an arbitrary byte start for both direct reads and partial-block staging buffers; the backend and its DMA capability layer
are responsible for device address alignment, contiguity, mapping and cache maintenance. The filesystem does not substitute constants or the incidental allocation alignment of a `Vec<u8>`
for the device contract, nor does it push buffer requirements onto the `read_at` caller.
This convention does not remove block length/range validation, and it does not promise failure atomicity for device I/O.
The hole/unwritten branches must not read old physical data, and partial blocks and EOF must not copy
bytes outside the request to the caller.

The block backend may modify the target prefix before returning an error, so `read_at` does not guarantee that a failed buffer keeps its old value, nor does it return
a publishable successful prefix. The caller must propagate the error and must not set uptodate or copy the failed content into the read iterator.
Cached pages move into bios through `FolioRead` and complete only after device
access ends. Contiguous pages belong to one bio. Fragmented subpage reads use
disjoint part owners: zeroed holes, unwritten extents and device-read runs
converge before whole-page publication. A failed or abandoned part keeps the
page stale; pending parts retain the lock even after another part fails. Each
part spans only adjacent bytes with the same zero-fill policy or contiguous
physical mapping; grouping cannot bridge a hole or a physical discontinuity.
Each part zeroes its own EOF tail before completion. Larger-than-page filesystem
blocks retain the synchronous partial-block reader.
Failed pages remain non-uptodate until a complete retry succeeds. PageCache
removes unconsumed reservations, while independently successful pages may become
readable. Failed buffer contents are not valid file data. Submission holds the
core read guard to stabilize block mappings; truncate waits for transferred page
locks before freeing blocks. Completion uses only the bio status, original pages,
and sampled EOF, without taking inode data, invalidate, or core locks. After
reading the disk block containing EOF, it zeroes every remaining byte in that page.
Writable ext4 integration tests retain both tail-page bios after demand returns,
start a VFS shrink, and verify that mappings and free-block accounting survive
until the last owner retires, including when another part has already failed.
The same tests verify that shrink/regrow cannot expose the discarded suffix.
`vfs/read_tests.rs` verifies the above isolation over the real ext4 + KVFS + PageCache path, and checks that the retried page is not
marked dirty and that the 17-byte EOF tail is zero-filled. The test of a partial write after a failed read also verifies that the page is fully re-read before the write copy,
and only the complete data after a successful write is marked dirty, without publishing a prefix polluted by the earlier failed read. This verification does not claim that device I/O is failure-atomic.

## Unsafe Code Inventory

None. `fs/filesystems/kext4/src/lib.rs` forbids unsafe code for the entire crate. Unsafe or device-specific
operations are located outside the boundary of this crate.

## Memory Safety Invariants

- The on-disk offset and size must be validated before slicing metadata bytes.
- Metadata checksum validation must complete before a checksum-protected block is trusted.
- A metadata checksum failure of the primary superblock must be returned before mount feature negotiation;
  capability rejection may happen only after the checksum passes, and must run before the first open and before a recovery reload publishes the runtime state.
- Block numbers and inode numbers must be validated against the layout, group, bitmap and system-zone rules.
- A JBD2 handle must hold enough credits before it dirties or revokes a metadata buffer.
- A speculative prealloc space exists only in memory: it is not marked in the block bitmap, the group
  descriptor or the inode, so discarding it at any moment leaves no disk residue; it reaches the bitmap only
  when consumed, through the normal allocation path, and inside that same critical section both indexes are
  detached before the consumed span is marked, so no window separates exclusion from the disk mark.
  Conversely, an entry kept on one side alone protects nothing and must not be treated as available space -
  the inode-side logical range and the group-side twin entry must be added and removed together inside one
  allocator critical section, and `preallocated_blocks` must always equal the sum of the group-side span
  lengths (verified after every change in debug builds).
- The group-side twin entry holds a strong reference to the owner inode, so eviction must unconditionally discard that file's space; missing the discard
  would let the entry and its strong reference outlive the inode object.
- The `MountedJournal` for the mount lifetime is the sole journal identity on the production path, and owns the on-disk mapping,
  the active superblock state, the transaction engine and the FIFO checkpoint queue. The same runtime
  transaction must migrate Running → Committing → Checkpoint → Finished; the queue holds only that object,
  the persistence evidence belongs to its Checkpoint phase, and it does not copy the payload or carry a replaceable coordinator reference.
- A commit request first closes admission of new handles to the target running transaction under the transaction lock, then releases the state lock and waits for
  active handles to exit; stop and abort must wake waiters. It must not wait for commit while holding its own handle or a lock the exiting side needs,
  nor nest a begin that waits for its own transaction to be unsealed. A duplicate persistence request must recognize a pending/finished
  target and must not append or retire twice; an invalid future target must fail and must not wait on an unrelated transaction.
- The ordered-data flush of a mutable transaction may only acknowledge the dependency generation snapshotted before the barrier began; an older barrier
  completing must not move the acknowledged watermark backwards, and a failure must not clear dependencies. After freezing no new dependencies are accepted, and commit satisfies
  its complete ordered-data dependencies before the journal write.
- The journal persistence execution mutex serializes append, checkpoint, ring/tail and recovery flag I/O;
  short transaction/state/buffer guards must not span device I/O. No retirement before both the home and the tail persistence succeed,
  and a failure must abort before handing off execution. The first feature update and PreserveDuringRecovery take part in the same protocol;
  replay/reset remains limited to exclusive recovery before the mount identity is published.
- After replay completes, the transaction engine's next sequence, the runtime ring and the checkpoint
  watermark must first be reset to the replay report before orphan cleanup begins; otherwise an old sequence must not be used to create a new transaction.
- The internal-journal extent must be contiguous and cover at least the number of logical blocks declared by JBD2 `s_maxlen`;
  the preallocated capacity of an inode-backed journal may exceed `s_maxlen`. Every physical extent must lie within the
  filesystem block count; no `MountedJournal` may be created before the on-disk mapping validation completes.
- Multiple handles may exit independently and share the running transaction; handle stop must return unused credits.
  Expected errors must be validated and returned as-is before that handle's first metadata access, and must not change the bytes,
  dirty buffer ownership or transaction membership already published by other handles. A single handle must not, on
  a failure path, delete metadata/revoke membership already published, because the same block may already be
  shared by other handles. Any ordinary error occurring after metadata has been published is handled as a failure of the internal invariant of a missed preflight/unwind,
  and the journal is aborted permanently; it must not roll back other operations in the same running transaction that have already returned successfully,
  nor report the failure as a partial rollback from which operation can continue. Private state not yet published is explicitly
  unwound by the ext4 algorithm.
  The upper bound of an ordinary transaction is limited conservatively by journal descriptor/commit/revoke overhead, and a fixed
  operation count must not replace the capacity constraint. An explicit filesystem sync must first commit the running transaction, then advance the
  committed checkpoint queue.
- Resident `Ext4InodeState` owns each regular inode's metadata target and records
  the handle's transaction identity at publication. Retain the transaction Arc
  and inspect its phase; long-lived tids must not alias across sequence wrap.
  Unattributed publication and extent mutation invalidate the target until later
  inode-table staging restores it. Unknown targets and non-regular inodes use
  conservative running-transaction commit. Committed targets still flush later
  data and check abort. Targets replace neither cache drain, full-sync timestamp
  staging, mount exclusion, nor independent KVFS error observation.
  Legacy orphan link staging advances the operation's resident inode target
  only after successful metadata publication and leaves dirty timestamps pending.
  A predecessor-only chain splice is recovery bookkeeping owned by the removal
  transaction, not a change to that predecessor's file attributes or mappings.
  It may retain the predecessor's old target: an uncommitted removal leaves the
  old chain and its allocated inode recoverable together. No temporary inode
  object or second resident-inode registry substitutes for the canonical owner.
- Before freeing bitmap bits, the allocator must register the pending-reuse ranges per block group and freeing transaction ID;
  the ranges, statistics and bitmap/cache publication belong to the same allocator owner and are protected by the same lock. JBD2 does not store physical
  space records; the filesystem glue notifies the allocator to release that transaction's protection only after commit persistence succeeds and after the journal lock is released.
  Handle stop, freeze, cache rebuild or failure must not release it; there is no need to wait for the home checkpoint.
  Contiguous ranges are processed per group in bounded fashion, and a cross-segment failure must take the mutation-error abort. Allocation excludes both prealloc and
  pending-reuse ranges; surplus registration must not promise non-reusable blocks via a bitmap-free sample. A commit failure keeps the records and
  aborts, and must not open these blocks to later allocations.
- The reclaim phases may share a transaction, but each prefix committed under sync/quota pressure must keep the orphan list and the remaining mapping consistent;
  each reclaim phase independently forbids automatic extra-isize expansion of the dying inode after its handle is created successfully, and must not
  depend on the previous phase's flag, nor re-establish the EA block after xattrs are cleaned; an admission failure does not change that flag.
  Runtime reclaim only trims the rightmost leaf and reclaims empty nodes along the path, without allocating or rebuilding subtrees; the mapping cache is invalidated before the operation.
  When a batch exceeds the capacity of a single transaction it must first shrink in a bounded way, with the capacity decision sharing the journal contract; any other NoSpace or
  commit error must not be swallowed and treated as a batch-shrink retry.
- Bounded physical-space admission may only occur under mount mutation exclusion and before opening a handle, and does not treat metadata
  credits as physical block demand. When space is insufficient, optional preallocation may be dropped first, and then a freeing transaction that can be released is committed;
  it must not hold an active handle to wait for its own commit, nor retry a namespace operation that has already partially published metadata.
  The writeback conversion counts tree-block demand from the actual leaf-entry changes and the split layout; a zero-growth operation must not drive a commit for a fixed budget.
  When there are pending-reuse blocks, the handle is closed per mapping before admission; stopping a handle does not by itself establish a persistence boundary.
  After admission or handle completion commits implicitly and successfully, it must, like an explicit Restart, acknowledge the completed prefix and
  release the reservation of that range. A later error/abort must not recount the committed prefix as incomplete; a failed commit must not
  advance the prefix. The basis for completion comes from the journal's existing transaction lifecycle and follows the transaction ID wraparound rules.
  The existing boundaries are unchanged: an admission failure does not abort, and a post-mutation failure aborts and publishes the read-only state.
- truncate must force-commit the orphan + `i_disksize` transaction before releasing the old block mapping; during recovery
  orphan cleanup must not be mixed with ordinary transactions, and must complete commit/checkpoint synchronously under a policy that keeps the
  recovery evidence.
- When multiple committed transactions reside in the on-disk journal, sequence/start must keep pointing at the oldest live
  transaction; appending may only start from the mount runtime append head and must keep at least one empty journal block;
  the first commit of a clean journal must first persist a non-zero start before issuing descriptor/data/commit, so that a power loss between activation and the
  commit block is recognized by the recovery path as an unfinished active transaction;
  while active, the runtime head must not be persisted as `s_head`, and that field is written only for a clean journal; only after the home-block flush completes
  may the tail advance to the next transaction, and while the queue is non-empty the ext4 recovery evidence must not be cleared. A frozen checkpoint image containing the
  primary superblock must retain the recovery feature, and an older
  checkpoint must not overwrite the recovery flag of a later live journal.
- The cached journal-superblock image must cover one complete journal block and may only come from input that was successfully decoded and
  checksum-verified; every in-memory update must be re-decoded to produce the next trusted state. A journal write batch
  must not exceed 128 KiB and must not cross a ring wrap or a discontiguous internal-journal physical extent. Batched writes
  change only the request granularity; they must not merge the clean-journal activation flush with the final transaction durability
  flush, nor omit a sync flush that is otherwise provided in place of a metadata commit.
- A partial extent update must refresh the modified leaf, the ancestor indexes, the inode root and the metadata checksum together;
  ordered writeback must not switch from a path update to a whole-tree rewrite within the same handle, and the credits upper bound covers level-by-level splits according to the size of this write
  and the maximum path depth, and the computed result must not be truncated.
- Ordered writeback must not treat a full-range static mapping/space plan as execution truth. The blocks required by the dirty range
  are already included in delayed-allocation reservation accounting; the execution cursor re-queries the extent before every actual mapping/allocation
  run, and the journal handle itself constrains the mounted credit limit. A handle extension failure
  may only flush at a complete run boundary and commit the durable prefix. The corresponding delalloc range may be released only after that transaction succeeds;
  a later ENOSPC caused by metadata/fragmentation must also be returned as partial progress. The unfinished
  suffix and its reservation must be retained, and the optional preallocation budget must not be
  granted again because of a transaction restart, nor may it consume ext4 reserved blocks or the delalloc reservations of other inodes.
  If a later transaction fails, the overall writeback must return the prefix of bytes already completed; PageCache ends writeback only for the complete
  folios among them, while the boundary folio and the suffix stay dirty. An ordinary retryable error may resubmit that suffix;
  a device/JBD2 error aborts the current mount journal, and recovery/remount must happen first.
- A range mutation may begin partial metadata writes only if the complete operation can be expressed by a single `ExtentPath`;
  a cross-leaf fallback must happen before the first metadata write/create/revoke.
- A journal without the revoke feature may release a metadata block without revoke only when "all old transactions have been checkpointed";
  forget must also remove that block from the current handle's metadata set. A clean v2
  journal must first flush the revoke feature before transaction/checkpoint overlap is allowed; a v1 journal keeps the
  synchronous checkpoint fallback path.
- In-memory counters and descriptors should be updated only after the corresponding metadata bytes have been staged within the same transaction.
- Block allocation/release must synchronize the free-block counters of the primary superblock and the group
  descriptor within the same metadata mutation; delayed-allocation admission depends on the primary counter and additionally
  subtracts the ext4 reserved blocks and the delayed-allocation mount aggregate held by the core.
- `Ext4StatFsMode` may only change the reporting basis of the externally visible total block count; the free, available, reserved-block and
  delayed-allocation aggregate must keep the same single source of truth, and must not be recomputed per `minixdf` or loosen space admission.
- An external xattr name decoded from disk must reject an embedded NUL, consistent with Linux/e2fsck corruption
  handling.
- Directory record lengths must be interpreted by the sole encode/decode entry of `RawDirectoryEntry` according to Linux on-disk semantics. The fixed fields, on-disk hash version and tree depth of the HTree root
  must be validated by the shared root decoder; on-disk versions `3/4/5` are
  illegal metadata, and version `6` is a legal SIPHASH format, but a mutation lacking the fscrypt name key must return `Unsupported` before selecting or writing an HTree leaf based on
  that hash.
- HTree signedness must be owned uniformly by the mount state: the default hash is validated at mount time, and signedness must not be interpreted when `DIR_INDEX` is not enabled;
  explicit unsigned takes precedence over signed, and when neither is specified the current Linux
  unsigned-char choice is followed; policy resolution must not write to disk, a writable mount may persist that default only after the root inode is initialized successfully and before the superblock is
  published, and a read-only mount must not write to disk. Orlov placement must always use signed HALF_MD4
  and the superblock seed, and must not reuse the HTree default/signedness policy.
- A newly written external xattr block must generate each entry's `e_hash` and the aggregate `h_hash` according to the Linux ext4 algorithm, and then
  compute the metadata checksum; it must not use a zero-hash encoding that is only readable by KExt4 itself but would be judged corrupt by e2fsck.
- Xattr name-only traversal must still validate the entry end, the value range, the external block header and the checksum;
  the sink may only borrow the name bytes during a single callback and must not keep that reference.
- The `h_reserved` of an external xattr block is treated as an opaque field following Linux semantics, and must not reject a legal block repaired by
  e2fsck merely because it is non-zero; when metadata checksum is enabled, the full-block checksum still detects unexpected modification.
- A permitted replacement with the same xattr value must short-circuit before journal credit preparation and metadata access, must not update the
  inode ctime, and must not produce a meaningless journal write.
- An ordinary xattr set/remove must always use the current `i_extra_isize` and change only the storage region of the target entry; the optional
  extra-isize expansion is triggered only by the unified ordinary inode-dirty entry, and the xattr apply and zero-link cleanup must
  bypass that entry to prevent recursion.
- Before enlarging `i_extra_isize`, it must first be proven that all xattrs can be partitioned within the shrunk inode body and one external block
  in full; the migration, the `i_file_acl`/`i_blocks` update and the inode checksum update must belong to the same transaction.
  The runtime effective `want_extra_isize` must include the default 32 bytes per Linux rules, and merge the on-disk min/want only when
  `RO_COMPAT_EXTRA_ISIZE` is present; a new inode must have that value written directly in the allocation transaction.
  When the effective want cannot be satisfied, it may only fall back to the on-disk `min_extra_isize` or keep the current value, and must not drop attributes or block
  an otherwise legal ordinary inode metadata update. Migration selection must be a bounded entry-by-entry scan, and moving
  `system.data` to an external block is forbidden. An existing external block with unchanged content must be reused directly; a candidate that needs a new
  external block must check allocator availability in the planning phase and complete the allocation before apply. When a free-space race between planning and allocation
  returns `NoSpace`, record a resident `no_expand` and continue the ordinary update, and it must not be escalated to a false
  `ENOSPC`; deleting an xattr clears that state, and a transient journal credit/busy failure should not permanently forbid retry. Device I/O,
  checksum and corruption errors must still propagate and trigger the existing journal failure policy, and must not be swallowed on the grounds of best-effort.
- Runtime inode allocation must use the explicit UID/GID that the kext4 callback has already derived via `inode_init_owner()`;
  the KExt4 core must not default a new inode's owner to root.
- The filesystem-level timestamp range must judge the full `i_[acm]time_extra` capacity by inode size; a 128-byte
  inode may only declare second-level precision and a signed 32-bit seconds range. Core encode/decode must still truncate nanoseconds and bound the epoch by whether the inode's actual extra
  field exists, and publish the decoded value as the sole resident metadata result.
- The logical/physical/length multiplications and additions in FIEMAP must be checked; traversal must be bounded both by the request end and
  by the format upper bound cached at inode creation, and a corrupt mapping must not produce out-of-bounds output.
- Resident identity belongs only to the KVFS `VfsInode` and the VFS-wide `(SuperBlock, ino)` table; KExt4
  must not establish an inode-number cache,
  a `Live/Evicting/Evicted` state, or a last-reference rule based on core handle reference counts. `RawInode` may only serve
  as an on-disk decoded value and must not carry runtime identity or ext4-private transient state.
- `VfsInode` must directly embed one `Ext4Inode` private component, and must not store only the inode number and re-decode it per operation.
  unlink/rmdir/rename must pass the private component of the already locked VFS victim/moved/replaced inode into the ext4 algorithm,
  preventing an open handle and a namespace mutation from each updating a different object.
- The `i_dtime` of the legacy orphan chain must be read from the journaled inode-table bytes and updated in place; a second copy of private state must not be constructed by number for
  a resident predecessor, nor may next be recovered from a possibly stale private snapshot.
- The delayed-allocation ranges, the per-inode reserved count and the mount aggregate are all owned by KExt4; one
  reserve/release/truncate/writeback/eviction API must update them jointly within the core mutation guard,
  and the kext4 KVFS layer must not maintain a set, per-block nodes or another aggregate.
- KVFS publishes `Freeing` before the final hook, so ordinary VFS capabilities cannot coexist with eviction Phase A/B/C.
  All three core phases borrow the same private component held by the VFS inode embedding and return no escapable eviction
  handle; after the inode slot is released, KVFS deletes the old cache entry, and number reuse constructs entirely fresh private state.

## Thread Safety

The runtime KVFS `SuperBlock` stores the stateless `Ext4SuperOperations` and the sole `Ext4Mount` (internally `RwLock<Ext4SbInfo>`)
as `s_op`/`s_fs_info` respectively; `s_op` is a truly static table shared by all mounts, and the private object is
exclusively owned by the superblock and can only be recovered by borrowing; there is no operation wrapper carrying mount identity and no escapable
typed owner. All inode mappings also share the static `Ext4AddressSpaceOperations`, and callbacks only
borrow the sole `Ext4Inode` from `mapping->host`; the preparation of a shared write fault is done by the
`FileMappingOperations` of `Ext4Inode`. Mutations that still need to jointly modify
superblock/group metadata are serialized by the mount-level write guard; read-only core operations enter shared,
and the delalloc ranges and the mount aggregate are updated jointly by a separate reservation mutex under the shared guard. The KVFS
cache coalesces concurrent `iget` and seals `New/Freeing`; ext4 private metadata and transient state are updated under the inode
state mutex, and KExt4 no longer takes another inode-cache lock. Internal JBD2 and metadata-buffer state still record
transaction ownership.
Shared-buffer atomic mutations hold the slot mutex and preserve old bytes on
failure. Writers in the same transaction must not independently read old snapshots
and then replace whole blocks.
Inode-table publication merges only the target entry, preserving neighboring
inodes' current data; callers still serialize logical changes to the same inode.
Writeback, both VFS truncate phases, last-writer preallocation cleanup, and final
eviction share the inode submission mutex. Mapping mutation/release drains
in-flight requests after acquiring it; completion workers do not acquire it.
Acquire it before mount protection, never while holding mount or inode-state
guards. PageCache enumeration holds no core mutex, and batch writers enter the
core only after mapping/folio locks are released. Journal execution protection
follows mount protection; stopping a handle does not acquire it.
The capacity preflight and begin are still guaranteed by the same mount-mutation exclusion, and are not yet an independent credit reservation,
so this must not be used to release all metadata mutations. When finer-grained locks are introduced later, this cross-layer boundary must be kept, along with the ordering constraints among buffer
ownership, journal handles, allocator bitmaps and inode metadata.
FIEMAP relies on the VFS inode shared lock to stabilize the single-inode mapping; the core read lock and the delayed-block
mutex are each taken only briefly, and released before the safe writer accesses user pages. User memory access must not be placed under the mount-level
core lock or the delayed-block mutex. Both buffered write and shared write fault must publish the hole reservation into the same delayed set under the inode
exclusive data lock first, and a dirty-folio
scan must not replace the filesystem mapping prepare.
The per-inode map lock of speculative prealloc space is a module-private leaf lock below the allocator lock, and deliberately does not use the
inode state lock: both consumption and discard take and put it inside the critical section holding the allocator lock, no further lock is taken inside the `update` closure,
and the group-side discard also takes the map of a foreign owner in the same order, so prealloc operations cannot form an ABBA with
inode-state users. The ENOSPC retry valve carries its own allocator lock and may only be called outside the locked section of the scan.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Countermeasure |
|------|----------|----------|----------|----------|
| T-01 | An xattr entry name or value range in a corrupted image is treated as a valid attribute by the name-only list path | Medium | A malicious or corrupted image encodes NUL, an out-of-bounds/overlapping value, or an invalid external block | `decode_xattr_entries()` rejects NUL and malformed entries; name-only traversal still validates the value range, block header and checksum, and is covered by regression tests |
| T-02 | The external xattr block leaks after unlink or rename overwrite, or the inode keeps referencing an already-freed block | Medium | `i_file_acl != 0` on a zero-link victim | Zero-link eviction first frees or decrements the EA block refcount, clears `i_file_acl`, updates `i_blocks`, and only then frees the inode |
| T-03 | A runtime unlink releases too early an inode that is still referenced by an open fd | Medium | Unlinking a still-open inode on the KExt4 runtime backend | The namespace transaction persists only nlink/orphan; the open file holds the single VFS inode and its ext4 private state, and only the destruction of the last VFS reference enters superblock final eviction |
| T-04 | Journal credits are underestimated, or overestimated based on data blocks, so that a metadata update fails midway through a transaction or is wrongly rejected | Medium | Namespace remove, HTree conversion/split, writeback, truncate, preallocation discard or final eviction modifies orphan, xattr, extent and inode metadata | The namespace insertion precheck explicitly records the HTree path; the split immediately after conversion is estimated as two independent single-block extents, and HTree credits are added before the inode flag is published; namespace removal covers only dirent, nlink and orphan metadata and excludes the subsequent final eviction; ordered writeback is fixed to a path-local algorithm in which a live cursor renews the handle per actual unwritten conversion and hole allocation run, and when the transaction limit is reached it commits the complete prefix and restarts; legacy final eviction and extent truncate are computed from the actual tree blocks, the old tree blocks that need revoke, and the affected groups, and the release of directory/block-mapped symlink data blocks counts revoke credits one by one per freed block; when the victim carries an external xattr, the final eviction budget includes EA cleanup |
| T-05 | A malicious extent or bitmap metadata causes a block that does not belong to the inode to be freed | High | A corrupted image points an extent/xattr block at the system zone or an illegal group | Block ownership, system-zone, bitmap and checksum validation is performed before release or mutation |
| T-06 | A device write/flush failure makes partial metadata visible | Medium | Device failure during commit, replay, checkpoint, explicit sync or xattr update | Journal abort preserves the recovery state or pending checkpoint; subsequent sync/mutation returns aborted, and in-memory modifications are not rolled back across syscalls; tests cover explicit-sync commit, checkpoint, xattr and replay failure |
| T-07 | An orphan remains on a clean journal, so mount permanently returns `NeedsRecovery` | Medium | The namespace transaction has been checkpointed, but final inode eviction has not yet happened | Explicit recovery traverses the legacy list and the orphan file regardless of whether the journal needs replay; the clean branch establishes recovery evidence with `PreserveDuringRecovery`, synchronously commits/checkpoints one by one, and clears the recovery features only after confirming that the journal start is zeroed and the orphan file is empty; a zero-link entry reuses the journaled final-eviction path |
| T-08 | The free-block aggregate, delalloc ranges and reservation aggregate drift apart, causing over-reservation | Medium | The VFS and ext4 sides are updated separately, or the per-block set and the mount counter are updated on only one side | A block mutation publishes the superblock and the group descriptor together; the ext4 range API jointly updates the inode interval, the equivalent of the `i_reserved_data_blocks` counter and the mount aggregate under a separate reservation mutex, and admission/statfs use this aggregate directly |
| T-09 | A newly created inode is fixed to root, bypassing the caller's owner semantics | High | A callback drops the credential, or the ext4 constructor implicitly fills in UID/GID 0 | The create/mkdir/mknod/symlink callbacks use `inode_init_owner()`, and the explicit `uid` and `gid` are persisted together with the namei transaction |
| T-10 | The journal head catches up with the tail and overwrites a commit that has not yet been checkpointed | High | Multiple committed transactions fill the circular log, and appending still continues to write | Live space is computed from the oldest tail/current head, and one empty block is always kept; when space is insufficient, a contiguous prefix large enough to free space is checkpointed first and then the write is retried, and no overwriting write is issued; a real ext4 image test covers tail advancement across two transactions |
| T-11 | A corrupted extent or an oversized FIEMAP range causes arithmetic overflow, a wrong physical address or unbounded traversal | High | The disk mapping length exceeds the format capacity, or the byte/block conversion is unchecked | The ext4 algorithm exposes the Linux-equivalent inode-format-dependent maximum byte count; the KVFS operation performs checked validation on every addition, multiplication and output field, and truncates the mapping to the request and format boundaries |
| T-12 | The immutable/append-only state of an on-disk inode is lost in the VFS integration, so xattrs get modified | Medium | iget always uses empty `NodeFlags` when constructing the KVFS inode | ext4 exposes `EXT4_IMMUTABLE_FL/EXT4_APPEND_FL` through semantic methods; the kext4 KVFS layer maps them to KVFS flags when publishing the VFS inode identity, and the generic xattr permission layer returns `EPERM` before the mutation |
| T-13 | A snapshot fork appears for the same inode, or an inode number reuse inherits the old transient state | Medium | Concurrent `iget` during KVFS initialization/release, namei reloading a live child by number, or orphan removal decoding a temporary object for a resident predecessor | The `New/Live/Freeing` states of the KVFS-wide `(SuperBlock, ino)` table are the only identity state; `New/Freeing` wait and retry; kext4 passes the existing private component to unlink/rmdir/rename; the legacy orphan next pointer reads and writes only journaled inode-table bytes; KExt4 has no resident cache; reuse can happen only after `Freeing` completes and the old slot is deleted |
| T-14 | Re-growing after a shrink exposes stale PageCache data or reuses an invalidated extent proof | High | ext4 backing prepare publishes `i_size` early so that `truncate_setsize()` skips cleanup, or a single atomic publication is mistaken for a size transaction and the mapping-invalidation mutex is removed | Regular-file metadata publish updates only `i_disksize`; the VFS and ext4 share the single `i_size`, which the VFS publishes with a release store at the PageCache ordering point, and EOF reads use an acquire load; size writes hold the inode-state write lock, a shrink clears the proof before publishing the new EOF, and the snapshot and the extent proof are sampled under the same lock; the inode data/invalidation lock continues to constrain the PageCache/PTE ordering, and the atomic provides no invalidation mutual exclusion; the generic attribute sharing, concurrent shrink/proof, core prepare, and KVFS shrink/regrow and partial-grow regression tests together enforce this responsibility boundary |
| T-15 | Enlarging `i_extra_isize` overwrites or loses xattrs, generates an external EA block that e2fsck does not accept, or rejects an update that could have completed when the disk is full | High | An xattr syscall wrongly changes extra-isize at the same time, the ordinary inode dirty path misses the expansion, new extra fields overlap the old inline range, the external entry/block hash is left zero, a shared block whose content is unchanged is wrongly COWed, or free blocks from the layout phase are taken before apply | Ordinary set/remove always uses the current extra-isize, and the unified ordinary inode-dirty entry then expands according to want/min/current; migration uses Linux-style entry-by-entry selection with `system.data` fixed; an external block whose content is unchanged is reused directly, a layout that needs a new block is preallocated before apply and downgrades `NoSpace` to `no_expand`; the same journal transaction rewrites the external block, generates a Linux-compatible `e_hash`/`h_hash`, clears the old inline area and updates the inode checksum; unit tests cover deferred expansion, directory inode dirty, shared external reuse when the disk is full, migration selection, and independent fixed hash vectors |
| T-16 | A corrupted orphan entry makes mount recovery fail, loop, or access an unallocated/special inode | Medium | `s_last_orphan` or an orphan-file slot is out of bounds, points to an inode whose allocation bit is zero or to an internal backing inode, the loaded inode has a link type that cannot be truncated, or the legacy `i_dtime` points to an illegal next | The unified `orphan_iget` validates the number, the special-inode boundary and the checksummed bitmap before inode-table decode, and validates the format and cleanability after loading; a legacy bad chain is zeroed persistently in a `PreserveDuringRecovery` transaction, an illegal orphan-file entry is skipped and processing continues with the subsequent valid slots, and in the end the recovery evidence is preserved and the operation fails; bitmap I/O/checksum errors, inode decode errors, and legal but unsupported cleanup are not downgraded |
| T-17 | `minixdf`/`bsddf` changes the source of truth for free space or wrongly deducts metadata overhead | Medium | The two modes each implement the whole statfs formula, or the VFS integration modifies the free counters directly | The typed mode only selects `blocks_count - overhead` or the raw `blocks_count`; the free/available/inode statistics go through shared code, and a Linux-image regression test verifies that the two agree except for the total block count |
| T-18 | A 128-byte inode exposes nonexistent nanosecond precision, or a newly created inode loses its nanosecond/extended epoch and wraps around after 2038 | Medium | Truncation only at inode-table encode time, the VFS comparing raw high-precision times, or the initialization structure storing only the base `u32` seconds | KExt4 returns the shared `TimestampLimits` according to the inode size, held by the single KVFS `SuperBlock`; `VfsInode::current_time()` truncates only once, before comparison, callback and publication; a new inode stores the full `Ext4Timestamp` and writes it to disk through the base/extra encoder, the 128-byte format clamps at the boundary, and the 256-byte format preserves nanoseconds and the epoch; regression tests cover capability selection, initialization encoding and resident publication for both inode sizes |
| T-19 | A valid SIPHASH HTree is misreported as corrupted | Medium | A root `hash_version=6` enters the directory write path | The shared HTree root decoder accepts only the on-disk versions `0/1/2/6`, and HTree signedness comes from the mount state; SIPHASH returns `Unsupported` at the hash boundary that needs a key, and the related transaction cannot commit a metadata mutation |
| T-20 | An old HTree image with no signedness mark has invisible names after being mounted across Linux, or Orlov directory placement drifts | High | The computation treats the image as signed when both on-disk signedness flags are empty, or Orlov reuses the HTree default/unsigned policy | The mount state generates `hash_unsigned` according to Linux unsigned-char semantics, RW persists the unsigned flag, and RO leaves the disk unchanged; HTree hashes only through the mount policy, and Orlov independently uses a fixed signed HALF_MD4 + seed; a high-byte name regression test distinguishes signed/unsigned |
| T-21 | The two indexes of a speculative prealloc space drift apart: a leftover entry on one side protects no block yet is treated as an available span, or a group twin outlives its owner inode | Medium | The inode side and the group side change in different critical sections, or close / writeback settlement / eviction drops only one side | Both sides change only together inside one allocator critical section; `preallocated_blocks` is conserved against the sum of the group-side span lengths and checked by a debug assertion; each twin holds a strong owner reference; close (when no delalloc reservation remains), writeback settlement (when reservations reach zero and no writable open remains, with an empty reclaim skipped via the inode-side emptiness hint) and eviction (unconditional, never short-circuited on the inode-side hint so a drifted pair cannot leave a twin behind) all discard; regression tests cover the close, evict and shrink reclaim paths and dual-index consistency |
| T-22 | The ENOSPC retry valve rescans without bound in the state "aggregate is nonzero but no block can be selected", or uses retries to mask bitmap corruption | Medium | The free aggregate counts unconsumed space as free; a corrupted group is hit again by every rescan round | The search count is bounded by the fixed `PREALLOC_DISCARD_SEARCH_LIMIT` (3 attempts, since a discard can only occur after the first); a discard that returns 0 ends the search immediately; a retry is triggered only by the no-space conclusion of a healthy group, and geometry/descriptor precheck skips do not set it; the first `InvalidBlockBitmap` encountered during the scan is remembered and returned in preference to `NoSpace`, but does not close off the reclaim retry for healthy groups |

## Failure Mode and Effects Analysis (FMEA)

The shared eviction preflight for namespace removal validates the inode data
format before the first metadata mutation. The orphan file is validated and initialized after mount/replay; at runtime, `OrphanInfo`, under mount-level exclusive access,
maintains the slots and the free-entry count; the count is updated only after metadata publication succeeds, and on failure the journal abort path
preserves the on-disk recovery evidence. An ordinary inode with multiple hard links whose link count does not drop to zero is not given an orphan entry. Kernel component tests use synthetic
metadata to cover unlink, rmdir, overwriting rename, and out-of-order final eviction; image interoperability and e2fsck belong to the external harness.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Unsupported is returned for a feature that is valid in Linux ext4 | Features such as EA inode, bigalloc, and inline-data write are not yet implemented | The current operation fails | The filesystem remains in an auditable state, but the capability is unavailable | 3 | Feature negotiation and an explicit `UnsupportedKind` |
| F-02 | Journal commit or checkpoint fails | Device write/flush error | The committing or pending checkpoint state is preserved and the journal is aborted | Subsequent sync/mutation on the current mount is refused, and recovery is relied on at remount | 2 | Abort the journal permanently before the failure is returned; preserve recovery evidence and pending state; forbid subsequent syncs from falsely succeeding |
| F-03 | Recovery-time orphan cleanup fails | The crash happened after the namespace commit but before final cleanup, a bad legacy chain must be cut, or orphan-file/bitmap/inode metadata is bad, the device fails, or an inode uses a format that is not yet supported | An explicit legacy bad orphan terminates after a durable clear; errors that cannot be safely downgraded preserve the orphan entry or recovery evidence and return | The filesystem is not exposed as a writable root without durable cleanup, and checksum/I/O errors are not discarded as stale orphans | 2 | Orphan cleanup uses an independent journal transaction; both bad legacy clear and ordinary inode cleanup use `PreserveDuringRecovery`; after a checkpoint, only the mutable superblock/group state is reloaded and the orphan-file free count is maintained incrementally, and only after success does processing continue; N3 completes the fault/powercut matrix on the final execution graph |
| F-04 | The coarse-grained write lock serializes slow mutation I/O | The live kext4 KVFS layer holds the mount-level core write guard around blocking filesystem mutations | Throughput drops | Other KExt4 mutations wait; read-only paths can still share the read guard | 4 | Inode private state already has an independent state lock; N2 continues to build per-group/journal/metadata-buffer lock domains based on the actual workers and shared state; mutation concurrency performance is not claimed before the locks are split |
| F-05 | Journal reservation has insufficient space | An operation's actual metadata targets exceed the capacity of an empty journal, or a reservation mixes in work from another lifecycle phase | A single mutation that cannot fit fails before modifying metadata; when a writeback transaction fails to extend, only the complete prefix is committed | Admission failure leaves the disk unchanged; if a later transaction of a segmented writeback fails again, the caller may receive an error while the durable prefix is preserved | 3 | Namespace and final eviction are budgeted separately; directory insertion checks disk capacity before the transaction, the credit is reserved at the structural upper bound for the current extent depth, and a single-block directory that may be converted is counted for a second append; before each live mapping run, ordered writeback guarantees headroom for that operation and the inode publish, first tries `reserve_more()`, and if capacity is insufficient it commits the already flushed prefix, settles its delalloc exactly, and reopens the transaction, rejecting at the admission stage only when even a single mutation cannot fit; a cross-leaf truncate chooses, before the first metadata write, the whole-tree path estimated from tree blocks/groups; constrained by regression tests for small-journal actual-run restart, fragmented unwritten, failure/retry, rename-overwrite, balanced-split, and preallocation-tail credit |
| F-06 | Lock order inversion between PageCache and the filesystem core | Writeback holds the core mutex while waiting for the mapping mutex, while a cache miss holds the mapping mutex and enters a backing read | Concurrent buffered I/O and `sync()` stop making progress | The watchdog reports a mutex deadlock, and the filesystem workload cannot continue | 2 | The kext4 KVFS layer does not hold the core mutex outside the PageCache traversal; writeback of the same inode is serialized independently, and the batch callback enters the core only after the mapping/folio mutex is released; VFS/MM must still eliminate backing I/O under the tree lock |
| F-07 | Committed journal fills the appendable space | Checkpoint lags behind commit, and the head approaches the oldest tail | New transactions temporarily cannot be made durable | Mutations wait for checkpoint progress | 3 | Before appending, validate the space against the circular live range and keep one empty block in reserve; the commit path catches `JournalBusy`, synchronously processes a prefix of pending checkpoints sufficient to free space, and retries |
| F-08 | A FIEMAP query encounters a corrupted mapping | The extent tree/legacy pointer returns an invalid length or physical range | The current ioctl returns a data error | File contents are not modified, and the caller cannot obtain the layout | 3 | Reuse ext4 mapping validation; the KVFS operation rejects zero progress and arithmetic overflow and does not output unchecked extents |
| F-09 | Eviction interleaves with a concurrent `iget` | Another path loads the same inode number while final teardown has already begun | A caller may observe metadata that is being released, or establish a parallel identity | Stale I/O, double free, or crosstalk of transient state | 2 | `Live -> Freeing` is completed under the KVFS cache mutex; lookup/insert wait internally, and finish removes exactly the old Weak entry, wakes waiters, and they retry, without returning `EINVAL`/`ESTALE` to path operations |
| F-10 | The extra-isize expansion of an existing inode fails repeatedly on every dirty write | The xattr layout or external block resources continue to be unsatisfiable, but the failure conclusion has no resident state | Every ordinary inode dirtying repeats metadata I/O, checksum, decoding, sorting, and layout planning | Metadata update throughput drops, and the disk-full scenario may be amplified | 4 | When the layout is determined to be infeasible, set `no_expand` in the unique resident `Ext4Inode` state; clear it after an xattr is successfully removed; journal busy/insufficient credits remain retryable |
| F-11 | Wrong selection of the statfs total-capacity mode | The context loses the explicit option, a reconfigure without options resets the current value, or the default mode becomes minixdf | `f_blocks` is inconsistent with the Linux ext4 ABI semantics | Capacity monitoring and application space decisions are wrong | 3 | A new mount defaults to `Bsd`, and the explicit mode is stored in the unique `Ext4SbInfo`; a reconfigure without options keeps the current value; the exact total block count of minixdf and the overhead result of bsddf are verified separately |
| F-12 | A speculative prealloc space is not reclaimed with its owner's lifecycle | Close was gated by a delalloc reservation and eviction did not cover the gap, or only one of the two indexes was changed | Blocks are counted in `preallocated_blocks` without an owner, and the group scan keeps excluding those bits | Shows up as "the free aggregate is nonzero but allocation fails"; the ENOSPC retry valve's discard bounds it, and a group twin holds a strong reference so a missed drop would keep a dead inode alive | 2 | Both sides change inside one allocator critical section; the conservation debug assertion; eviction discards unconditionally; close drops only when no reservation remains, and otherwise writeback settlement (reservations zero and no writable open) or eviction reclaims; a settlement racing the writer-count decrement keeps the spaces for the next settlement or eviction; retries are bounded and stop when discard returns 0; close/evict/shrink/settlement regression tests assert both indexes and the counter return to zero |

## Fault Management

KExt4 uses `Ext4Result` and typed `Ext4Error` to express corruption, unsupported format,
out-of-bounds metadata, checksum mismatch, journal credit exhaustion, and device error.
The journaled modification path moves ordinary failures ahead of the first metadata access; an empty failure of multiple handles does not affect other modifications in the same
running transaction. After metadata/revoke has been published, an ordinary error, a device/checksum/state-machine error, or a
handle accounting error permanently aborts the journal, and no cross-operation in-memory rollback is performed.
A successful data I/O only records a persistence dependency and does not mark the handle as having published metadata; an ordinary failure before metadata access
keeps the dependency and allows a retry, for example an unwritten extent conversion that returns `NoSpace` because too few tree blocks could be allocated.
Ordered writeback that publishes metadata must set the transaction dependency after the device I/O completes; the dependency is kept after freeze, and commit performs one device flush before
writing any journal/recovery evidence. A flush failure must not clear the dependency and must not enter the
checkpoint phase; it must abort the journal and report an error to the synchronous caller. This path is still serialized by the mount write lock;
only after the explicit flush of a capacity restart succeeds can all current data dependencies be considered satisfied. Both the running and committing
`flush_ordered_data` entry points internally issue a barrier first and clear afterwards, expose no standalone clear method, and do not hold the transaction's internal lock while performing I/O.
A data-only overwrite of already-mapped blocks does not depend on new metadata; its persistence is provided by the same-device journal flush or direct
flush of inode sync, or by the final flush of filesystem sync, and it is not promised to be persisted before the commit record of an unrelated transaction.

Checkpoint batching must preserve the FIFO write-back order of frozen images: only after all home-block
writes of the selected contiguous prefix have succeeded and been flushed may a single persistent update of the journal tail be allowed; before the tail flush and recovery-state handling succeed, no
transaction in the batch is removed and no journal space is reclaimed. A failure midway through write-back or in either flush keeps the whole batch of pending work
and aborts; retrying an already consumed cache snapshot in the current mount is forbidden, and a remount relies on the retained journal for recovery.
When space is insufficient, the prefix is selected according to the worst-case encoding overhead of the actual journal block count of the transactions awaiting commit plus the upper bound of the next transaction credit,
and the target is capped at the ring capacity minus one, so that one empty block is always kept. `usable_log_capacity()` is the single source of
the reservation rule, shared by append admission, rejection of oversized transactions, and target capping, which avoids semantic drift where the target still cannot be
met after a full drain. The optional budget must not block a current
append that fits; the computation uses the same descriptor/revoke format and checked arithmetic as journal writes. The complete next-transaction
budget covers any metadata/revoke mix sharing the credit pool; the proof is in the design document. Capping at the ring capacity may
reduce the optional budget, so it cannot be taken as a promise that the next transaction will be exempt from another reclaim. The remaining transactions and their policy are not removed. The tail points to `next_oldest`, the journal must remain active, the ext4 recovery feature
must not be cleared, and caches that still have frozen checkpoint images must not be invalidated. Prefix retirement first reclaims, by targeted block key,
entries that are clean, unpinned, and without subsequent ownership; caches of the suffix or of the committing transaction must not be released.
General cache cleanup is not triggered by the checkpoint batch count. `MetadataBlockCache` computes demand from whether the resident block payload
exceeds the 4 MiB soft retention quota; the read/write guard of `Ext4Mount`, at the end of an access and while still holding the mount
protection, advances an incremental scan of at most 64 entries. Read-only accesses can advance reclaim without journal pressure.
Each round fixes its end key, so newly added high addresses cannot postpone wrap-around; dirty, pinned, owned, and frozen-image entries cannot be evicted,
and the budget limits the number of checks rather than the number of successful evictions. A hit sets a one-bit reuse mark under the existing index lock, and a first insertion does not set it;
the scan clears the bit and gives recently reused entries one chance to be retained, which still consumes check budget, and no extra scanning is done because an entry was not evicted.
This bit does not grant eviction permission and does not replace the pin/ownership/frozen-image checks; targeted checkpoint reclaim
and explicit full reclaim ignore the access bit, so that hotness does not block an explicit cleanup. Sustained reuse may leave the soft quota exceeded for a long time.
When there is no demand, only a Relaxed read of the cache's own `AtomicBool` hint is made,
without acquiring the cache mutex; the hint is updated from the actual length inside the same cache critical section as each index insertion/removal/invalidation,
so that a stale scan does not overwrite the demand created by a new insertion. It neither publishes buffer data nor grants eviction permission, and a concurrent stale value is only allowed to
postpone maintenance or trigger one redundant recheck. The actual demand recheck, cursor, and eligibility checks are performed under the same cache mutex;
device I/O is not performed while holding that lock. Ordinary cache read/insert does not evict; an immutable buffer pins both its bytes and its
slot, and the allocator holds prefetch pins until the end of the critical section, so even if the bytes are replaced by a new publication, the slot cannot be reclaimed and
rebuilt. Removal after forget/revoke likewise checks under the index mutex whether another slot reference exists.
The maintenance entry of the mount guard is retained for now, but the correctness of the ordinary reclaim above does not depend on mount exclusivity.
Whole-cache invalidate still requires an exclusive recovery/lifecycle boundary and must not run in parallel with active pins.
Explicit full reclaim and invalidation of the whole cache reset the cursor, and syncfs still drains and reclaims globally.
There is no periodic full scan and no checkpoint-count state. The soft retention quota does not include index/frozen-image overhead and cannot guarantee a
hard memory upper bound: pin/transaction holds, a large number of insertions in a single access, or additions faster than the scan speed can all cause overshoot;
without subsequent accesses there is no automatic progress. There is as yet no general shrinker, capacity admission, or memory-pressure feedback.
The space target limits the reclaim completion condition, but I/O inside the mount write lock can still take long,
and this policy does not guarantee a latency upper bound under device failure.
A production checkpoint entry must first obtain persisted evidence and then be enqueued under the transaction lock; missing evidence is
a broken internal invariant and must not be skipped as a still-committing `JournalBusy` state. Before write-back, it must be confirmed that each transaction's
policy record exists. After `PreserveDuringRecovery` is appended, it is drained synchronously under the mount write lock; if the batch contains
that policy, it must be the last transaction and the whole batch retains recovery evidence; a later ordinary transaction must not be allowed to
overwrite the end-of-batch policy before the drain. The final recovery feature clearing may be performed only after all recovery work has completed.

A commit/checkpoint failure keeps the committing or pending work and permanently aborts the journal, to avoid silently
losing metadata that has not yet reached disk; subsequent sync and mutation both observe `JournalAborted`, and an in-memory
transaction must not be treated as durable. The journal tail may only advance past a contiguous prefix of transactions whose home-block persistence has completed;
the journal start and the ext4 recovery
evidence are cleared only after a batch covering all pending commits has completed. Currently inode sync
uses the conservative semantics of committing the whole running transaction. Explicit recovery likewise may return success only after the legacy list and
orphan-file cleanup have all completed commit/checkpoint, the journal start is confirmed to be zero and the orphan file empty,
and the final superblock state has been flushed; a failure before the cleanup is durable keeps the orphan entry or the recovery feature
as the entry point for the next recovery, and a failure of the final state flush must also return an error to the caller.

The completed N1 `MountedJournal` boundary must continue to preserve the recovery-evidence, revoke, and
checkpoint-failure-retain invariants. The later N2 backgrounding must not swallow errors or clear pending state on the grounds that the complete N3 fault matrix is temporarily missing.
The full linkage of the user-space sticky error/errseq and forced-readonly is to be completed in N3,
after the background execution graph is stable.

## Privacy Analysis

KExt4 stores and returns filesystem data and metadata, where an xattr value may contain a security label
or application data. The core does not log xattr values or file contents.

## Known Limitations

- The whole-disk legacy direct/indirect block-map format currently cannot be mounted; a read-only legacy mount first needs the
  core mount state split into a capability type that cannot obtain the mutation API. In images with the extents feature,
  individual legacy inodes still support only checked reads; buffered write/page fault is rejected synchronously before reservation,
  and other mutations are rejected no later than the first metadata access.
- Runtime already exposes `user.*`, `trusted.*` and `security.*` xattrs through KVFS; namespace/DAC is
  checked in KVFS, and without a complete LSM/capability, `security.*` set/remove requires a privileged
  credential, and the four flag combinations of create/replace are checked by the KExt4 core in the mutation plan
  under the same write lock. The on-disk immutable/append-only flags are mapped to KVFS at iget and block xattr mutation;
  `FS_IOC_SETFLAGS` is not yet implemented, so there is no modification entry that needs to refresh these flags at runtime.
- POSIX ACL is currently only opaque xattr bytes inside the core; ACL permission enforcement,
  mode synchronization or inheritance is not implemented, so the kext4 KVFS layer does not expose `system.posix_acl_*` as an ordinary xattr;
  the mount parser accepts `acl` only for the positive spelling compatible with Linux's default-on option, and it must not change the permission boundary above.
- EA inodes, oversized xattrs, bigalloc, inline-data write, huge-file write
  block-unit accounting, encryption/casefold, direct I/O and mmap coherence remain future work.
- An HTree SIPHASH root can complete format classification, but encryption/casefold name-key semantics are not yet implemented; operations that need a name
  hash, such as create/unlink/rename, return `EOPNOTSUPP`.
- KVFS unmount already synchronizes VFS and journal state before and after topology detach, but blocking new references/writes,
  lazy unmount, freeze and background worker drain still need a final lifecycle gate.
- Regular inodes have metadata transaction targets, but fsync and fdatasync do
  not yet distinguish timestamp-only and data-required metadata. Directories,
  missing targets, and unattributed mutations may still commit unrelated metadata.
- When adding or extending a mutation, one must continue to prove that all expected failures can be detected before the first metadata access; if that cannot be
  done, path-local explicit unwind must be implemented, and an operation savepoint must not be restored. A missed path conservatively
  aborts the entire running transaction, preserving correctness but reducing the availability of that mount.
- Owned data BIOs and task-context completion cover allocation, unwritten extents,
  and append. Partial initialized-block reads and metadata I/O remain synchronous;
  background triggering, wider inode-target coverage, and mount-lock separation
  remain future work.
- Birth and consume of speculative prealloc spaces are wired into the writeback path: the Hole branch consumes
  a registered space first and, only on a miss, allocates and registers the surplus inside the same allocator
  lock section; the reclaim side (close, eviction, shrink, writeback settlement), group-scan exclusion and the
  ENOSPC retry valve are all live. The `preallocated_blocks` conservation assertion is checked after every
  change in debug builds, so the two sides desynchronizing is exposed immediately.

## Audit Checklist

- Does the metadata parser reject truncated, out-of-bounds, unsorted, or checksum-invalid input?
- Is the directory parser's Linux `rec_len` decoding performed uniformly by `RawDirectoryEntry`? Do HTree reads and writes
  reuse the same root decoder, and are an invalid on-disk hash version, a mount-time unsigned algorithm, and a valid but unimplemented
  SIPHASH classified separately? Do RW/RO mounts with unspecified signedness persist/preserve the on-disk flags respectively? Does Orlov
  always bypass the HTree default/signedness policy and use signed HALF_MD4?
- Does the xattr list only borrow names, while still validating value ranges and the external block checksum? Does a set with an identical
  value return before the journal/ctime update? Are the on-disk immutable/append-only flags
  mapped to KVFS at iget time, and does every xattr mutation return `EPERM` before doing anything? Does a mixed inline/external layout preserve
  all attributes, do ordinary set/remove keep the current extra-isize, does the unified ordinary inode-dirty entry point cover
  directories and non-write metadata updates, does a failed extra-isize expansion fall back safely and cache only
  failures whose layout/resource determination has completed, without recording a transient journal busy condition as a permanent state?
- Does every mutation reserve enough journal credits for all metadata blocks that may be dirtied or revoked?
- When a linear directory is converted to an HTree and then immediately split, does the extent precheck treat the two independent block allocations as two
  worst-case non-merging mappings, and reserve the HTree credits before `EXT4_INDEX_FL` is published?
- Does zero-link cleanup release data, the external xattr block, the orphan entry, and the
  inode bitmap state in a single auditable transaction?
- Can a zero-link inode be accessed only through an existing VFS identity, with its inode number,
  extents, and xattrs remaining valid until the last reference goes away?
- Is FIEMAP bounded both by the end of the query and by the maximum file size of the inode format, and does it correctly distinguish mapped,
  unwritten, delayed, and hole results, as well as `LAST` for truncated results?
- Does a MAP_SHARED write fault first establish a delayed reservation through `page_mkwrite()` before publishing a writable
  PTE; does that path hold the address-space invalidate shared lock and the folio lock without taking the inode
  exclusive data lock, so that non-`SYNC` FIEMAP does not miss dirty holes?
- Is the runtime `iget` private state decoded only by the KVFS cache initializer, with `New/Freeing` waiting and retrying?
- Do namespace mutations pass in the existing VFS victim/moved/replaced private state, rather than having the core reload it by inode number?
- Are new ext4-private transient fields placed in the `Ext4Inode` held by the VFS inode composition, rather than in a separate wrapper
  snapshot, a mount-wide inode-number map, or a generic state container?
- When a directory insertion needs to allocate a new block, does link/rename reserve the bitmap/group/superblock
  growth quota before publishing metadata, rather than relying on the child inode allocation budget that exists only on the create path? Does ordinary file
  creation avoid requesting the parent directory growth quota twice, while preserving the independent child block reservation for mkdir/long symlink?
- Does the documentation distinguish core support, live KVFS syscall exposure, and roadmap capabilities?
- Does the statfs mode change only `blocks`, while keeping free/available/reserved/delalloc accounting shared?


### Directory growth extent-credit boundary

Namespace insertion reserves an operation bound independent of directory contents or
predicted insertion paths. The extent layer bounds new blocks and old external rewrites
using the validated current depth: existing external blocks are reused, each level can
add one sibling, and inline spill/root growth are included. Only a single-block linear
directory eligible for index conversion reserves a second append, allowing one level
of depth growth between appends. HTree dispatch and credits share the feature-and-inode
index predicate. Allocation/create and release/revoke keep distinct credit definitions.
Capacity validation remains ahead of metadata publication because namespace mutations
have no operation-local rollback. No plan object or cached growth-credit field is retained.

An oversized admission returns `NoSpace` without aborting or publishing metadata. A full
running transaction waits for active handles, then commits before the caller retries its
reservation; exact-capacity admission is allowed. Waiters observe the existing transaction
state and share its notification queue. Handle stop and abort notify outside journal locks.
The mount mutation lock serializes complete filesystem mutations. Admission checks the
existing active-handle count and rejects nested handles before any setup or wait, so a
caller cannot wait for its own handle while holding that lock. Independent journal-core
users retain the wait/stop protocol; handle stop requires no mount lock.

Credit exhaustion before metadata publication returns `InsufficientJournalCredits`, maps
to `ENOSPC`, and leaves the journal writable. Exhaustion after publication violates mutation
prevalidation, becomes `InvalidJournalTransaction`, aborts the journal, and maps to `EIO`. The existing
abort state rejects subsequent mutation with `EROFS` before feature or superblock setup;
the mount write guard publishes `RDONLY` to the existing VFS superblock before releasing
mutation protection. Writable remount validates the same abort state before applying
options; buffered-write admission and delayed reservation also reject abort independently
of VFS flag publication, before dirtying the page cache. The guard borrows the caller's
superblock; the mount retains no additional reference or
writable-state field. Regression fixtures cover HTree conversion,
conversion followed by split, indexed leaf splits with external extents, fragmented linear
link/rename, admission one credit below the bound, 64-credit batching across conversion,
feature/index-flag mismatch, nested admission, remount rejection, stop/abort wakeups and
durable reopen.

### In-flight BIOs for allocation, conversion, and overwrite

Hole preparation publishes only bitmap ownership and unwritten extents, never
initialized mappings or a larger `i_disksize`. Committing that metadata before
data cannot expose old disk contents. Once allocated, blocks no longer consume
delalloc reservations; retries reuse their mappings. A later preparation failure
retains completed unwritten allocations for retry, truncate, or final cleanup,
and submits no BIO from the failed batch. Partial unwritten buffers must be zero
filled; partial initialized blocks use read/modify/write outside mount protection.
Blocks larger than cache pages retain synchronous draining to prevent overlapping
partial writes.

Synchronous aligned overwrites use task-owned `BioWait`; deferred overwrites and
allocation/conversion use the inode queue. Both use the same four-batch limit and
drain the other path before switching. Producer exclusion and these drains keep
the combined bound at four; each cache batch separately holds at most 31 pages.
The synchronous pipeline retains submission protection and waits for every
submitted BIO before any failure exit releases/redirties cache completion.
Waiting holds no mount guard and does not depend on the completion worker.
Initialized overwrites update inode metadata and ordered-data dependencies
without reinterpreting extent mappings.

All BIOs are prepared before queue installation and device submission. Truncate,
FIEMAP, last-writer close, and eviction acquire submission protection and drain
without mount protection. Producer preparation and task/worker metadata completion
serialize through mount write protection; no journal handle crosses data I/O.
Workers take neither submission nor invalidate locks. `Completed` mode neither
rewrites data nor allocates holes; it converts only successfully written unwritten
ranges and registers the ordered-data barrier before commit.

Device callbacks only publish BIO results and wake `BioWait` or queue normal work.
Metadata, page state, buffers, and references whose destruction can evict inodes
are handled in task context. Private work exposes no cancellation/closure. Every
accepted BIO must complete exactly once; a missing completion blocks drain and
cannot justify early block release. Workers process submission order, at most
four batches per occurrence, removing slots and waking waiters before releasing
captures. Synchronous callers retain the inode, release submission protection,
wait for their target prefix, and flush completion work before shutdown proceeds.

VFS writeback always uses DataOnly and acknowledges only a contiguous prefix whose metadata settled;
successful BIOs alone cannot clean pages after conversion/accounting failure.
Full fsync no longer changes data-error handling through writeback control.
Real metadata/journal failures retain the abort policy. Original device errors precede
prefix-settlement errors; unsuccessful ranges stay dirty. Preparation and
submission failures are recorded before releasing completion ownership, without
running post-I/O metadata settlement. Queue throttling is not an error. Both
mapping and superblock errors are recorded before ending page writeback. Retry
and non-consuming checks preserve unseen failures, including for newly opened
files; fsync observation does not consume the independent syncfs channel.
Temporary passes read the current error generation before waiting for submission
or pending I/O and check for newly recorded failures after draining. They do not
inherit old mapping errors or mark them observed. Thus repeated syncfs calls on
one file stop reporting a recovered failure after its superblock cursor advances,
without consuming mapping history belonging to existing or later file observers.
Regression tests cover syncfs-only retries, clean repeats, subsequent new errors,
independent file cursors, and a pending I/O failure during submission-lock wait.
Superblock sync attempts remaining inodes, metadata, and filesystem flush after
failure while retaining the first error and honoring filesystem abort checks.
Transaction restart, pending-free admission, and ordered/commit barriers remain
unchanged. A finite error generation cannot distinguish an entire 2^64 wrap cycle.

## Deferred automatic timestamps

Regular-file writes and shared writable faults retain immediate resident mtime/ctime
updates. Only their journal staging is deferred. Per-field dirty state is protected
by the existing inode-state lock; no state guard survives device I/O. Metadata
publication acknowledges only captured values, preserving racing automatic updates
and explicit setattr values. Failed staging leaves timestamps dirty.
Automatic write updates check journal writability before publishing, then compare
and update both timestamps and dirty bits under one inode-state write lock. They
preserve timestamp precision, pending atime, and already-dirty unchanged fields.

Writability queries use the transaction engine's atomic `has_abort_error`
projection rather than acquiring its shared transaction mutex. Abort retains the
first error under that mutex and publishes the projection before unlocking and
waking waiters. Only a successful empty-state replay reset clears it. The atomic
query does not expose transaction data or authorize later metadata mutation:
those operations retain their locked abort checks. A buffered write that races
abort can pass a check before publication, as with the previous locked query;
subsequent checks reject the published abort. This adds no timestamp batching,
precision loss, or change to the superblock read-only publication boundary.
Mounted abort callers, including asynchronous writeback metadata completion,
retain the mount write guard through atomic abort publication. The guard's Drop
marks the superblock read-only before releasing mount exclusion; another write
guard for that mount cannot overlap this publication. Any abort source moved
outside mount protection must publish superblock read-only state at its own
failure boundary rather than depending on a later guard drop.

File-range fsync waits for the requested data before filesystem persistence; an
empty range still runs journal/device sync. Generic data wait does not flush the
device and does not advance an open file's error cursor.

Full fsync, syncfs/unmount inode metadata writeback and linked-inode eviction stage
pending timestamps. Data-only mapped overwrite may omit a timestamp-only transaction;
a later full fsync must still persist timestamps with no dirty folios remaining.
This does not weaken the data or journal flush barriers or add a background worker.

Buffered write admission checks live immutable policy even for empty requests,
before generic size limits, and publishes automatic timestamps only after the
size and journal checks plus the Path read-only check. It uses the VFS inode's
representable current time and the same resident dirty-field update as `update_time`.
This VFS policy also applies with `KFEAT_FS_TIMES=n`: disabling the adapter feature
must not replace an existing write timestamp with a synthetic epoch value. Shared
writable faults and explicit timestamp updates retain the same VFS policy.
It does not suppress timestamps or coarsen their precision. Journal abort may race
a completed admission check, as before; write-begin and later metadata mutation
retain their own authoritative rejection points. Shared write faults and explicit
setattr keep their existing paths.

Buffered preparation selects the extent/bitmap size limit and publishes mtime/ctime
under one inode-state write lock. The mounted capability snapshot contains only
immutable geometry/format limits and a strong reference to the same journal;
abort is checked live, never cached. Empty or rejected
requests do not modify timestamps; clipping uses the remaining representable file
range without offset addition. These guards are released before data copy or I/O.

An initialized-range hit may skip mount protection at write-begin, but still checks
journal abort and supported inode format. KVFS data/invalidate protection excludes
truncate and fallocate, and the inode-state lock observes size, reservations and
mapping-proof invalidation together. Holes, unwritten extents, extensions, partial
EOF blocks, pending reservations and cache misses retain the existing slow path.
The hit does not allocate or remove blocks, change reservation accounting, submit
data or claim durability. Each fsync retains its own data wait, timestamp staging,
transaction target, device barrier and error cursor checks. Tests require a cached
overwrite to complete while mount mutation protection is held, then verify its
bytes on the backing device after fsync; format and aborted-journal rejection are
tested separately.

Stable mapped overwrite completion skips inode-table rewriting when no timestamp
is dirty: mapping and disk size are already protected by submission exclusion.
It still registers ordered-data flush for every successful range, even if an earlier
transaction committed the inode metadata. A later timestamp update remains dirty
for the next range or final full fsync. Data/barrier failures retain the existing
error recording and journal abort behavior; clean metadata never implies durable data.

## Fsync transaction and data coverage

Every file synchronization waits for its data and applies its own metadata intent
before synchronizing the tracked transaction. Transaction progress can avoid a
duplicate journal commit; it cannot cover data written after that commit. The
already-committed target and no-new-transaction paths therefore retain device
flushes. Full fsync must stage dirty timestamps even after fdatasync cleaned the
PageCache. Journal abort is checked before accepting a committed target.

KVFS retains per-open error cursors independently of transaction completion, so
a successful retry does not erase another file's unobserved failure. There is no
inode request batch, saved batch result or coordinator lock. Existing submission
guards and drain-before-truncate/release rules still protect concurrent mapping
changes; missing device completion can stall data waits and is never treated as
successful persistence. Tests exercise overlapping full/data-only requests,
per-open failure observation and later data barriers after an earlier commit.

## Quota metadata I/O boundary

Quota files use the canonical VFS inode retained by superblock quota state.
The trusted caller must validate same-filesystem membership and protect the
file against ordinary writes before activation. The storage adapter accepts
an existing mount-state borrow and, for writes, a journal handle in task context;
it introduces no unsafe code, persistent owner, atomics or recursive lock.
Mount write exclusion precedes the quota mutex. It must not execute under
spinlocks, IRQ/preemption-disable guards, in IRQ or during early boot.

Quota payload reads use the metadata cache to avoid stale home-block reads
before checkpoint. Writes reject filesystem-block crossings before mutation,
check existing extent/inode credit bounds, initialize newly exposed blocks and
journal payload with mapping/size changes. A mutating format operation must remain
in one admitted transaction; its caller must complete or abort the handle before
releasing the filesystem state. Physical admission includes optional
extra-isize external-xattr allocation through the existing inode bound; journal
credit extension alone is insufficient. Native read and write failures are classified
before errno conversion. A pre-publication nonfatal error leaves the journal
usable; a failed operation with published metadata still requires abort.
Inactive dirty dquots report failure once without quota-file I/O and clear
their dirty indication; initialization remains owned by acquisition. Structural failures cannot be recovered by
retrying a partially modified tree. Filesystem sync uses the existing abort
policy on write failure and still attempts journal drain; dirty record state
for active records is only cleared after its quota-file write succeeds. A later commit failure
requires filesystem recovery even if in-memory dirty bits were cleared.

Internal user/group/project quota activates from validated allocated regular-file inode
numbers before recovery can change allocation. Activation validates the reserved
user/group numbers and rejects malformed quota identities with EUCLEAN before
quota-file I/O; invalid identities never exempt ordinary inodes from charging.
Quota identities are excluded
from namespace lookup even when already cached, and only those identities bypass
charging. A weak back-reference reaches the sole VFS quota owner without an
ownership cycle. Project identity is read from the inode only when the filesystem
advertises the project feature.

Acquisition precedes inode reservation locks; each inode binds the same record
as the cache and imports existing reservations under its state lock. Cached updates cannot
perform quota-file I/O. Extent and inode-counter validation precede all
reservation admission; successful admission has no fallible publication step.
Rollback failures are logged without replacing the original admission or
allocation error, and fatal journal errors abort mutation. Lock order is mount
exclusion, ordinary inode state, quota state, then quota-file inode/allocator
state. Internal quota files cannot
enter ordinary buffered-write reservation paths. Record updates are atomic
across active classes; inode/space limits fail before bitmap publication.
Preallocation hints pass through the same quota admission and settlement as
fresh physical runs. Rejection leaves the hint and bitmap intact; successful
allocation consumes reservations once. Failed published operations
abort rather than reusing partial state. Ownership transfer includes both
allocated and reserved bytes and persists quota records with the inode owner.
Allocated-sector changes and quota record changes share the inode state lock;
overflow and quota admission failures leave both unchanged. Quota backing inodes
update their own sector count without recursively charging quota records.
Shared external-xattr reference release frees one owner's usage even if the
physical block is retained; copy-on-write cannot accumulate a duplicate charge.
Fallocate batches include quota-record credits alongside allocator metadata.
Fallible extra-isize expansion precedes transfer and resident owner publication.
Writable feature and journal-abort checks are repeated under the mutation guard
used to activate, resume or initialize quota state.

Read-only remount first excludes VFS mutations and writable opens and rejects
remaining writers or unlinked live inodes. It drains writeback and cached
records before setting the existing suspended state, retaining file ownership.
A failed writable remount suspends loaded classes without further I/O; it does
not leave accounting active on a read-only superblock. Lookup and read-only
open require no quota acquisition or writable journal, including after abort.
Writable open and metadata/namespace mutations acquire their affected owners
before changing allocation state; journal-abort checks still reject mutation.
External-xattr release validates the inode block count and bound quota records
before changing a block bitmap or shared reference count; unsupported inode
accounting and invalid counts cannot partially publish that release.

Final disable drains inode I/O and completion captures before taking mount
mutation protection, then drains cached records with bounded transactions.
Suspension waits for in-flight inode eviction outside mount mutation protection
before detaching inode records. Suspended
classes unload without quota-file writes. If the journal has aborted, final
release invalidates the quiesced class cache and file information without I/O.
Writeback failure is returned separately from file ownership, all classes are
processed, and recovery evidence is preserved on disk. Retained quota
inode references are released by the mounted owner after its filesystem/quota
guards end. The core release helper is private to quota integration, preventing
callers from receiving a file under their own mount guard. Final eviction can
therefore enter filesystem code safely. Quota-control authorization is
performed by the syscall adapter.

Filesystem range/length checks precede sector-addressed Bio I/O. Live media
capacity, queue alignment and read-only checks remain in block core; a later
media shrink returns a device error. Aligned byte reads borrow the destination
and may partially modify it on failure; partial blocks use staging.

## Quota-control boundary

Quota operations run under superblock shutdown admission. On/off hold its
exclusive side; queries and record updates share admission. They acquire native
mount protection before the canonical quota mutex. Backing-file references are
copied while holding that mutex, then inspected or released after unlocking it;
status queries therefore cannot invert the native mount/quota lock order.

External activation rejects a foreign superblock, non-regular backing inode,
duplicate class, or already protected quota file. `NO_QUOTA` excludes payload
allocation from recursive charging. Persistent immutable/no-atime flags and
live VFS flag queries prevent ordinary writes, metadata changes, namespace
removal and automatic atime updates from modifying active quota files.
Quota-file data exclusion precedes native mount protection. Sync invalidates
old file-cache pages only after journal and device persistence completes.

Privileged record changes require the current subjective CAP_SYS_ADMIN.
Own-user and effective-group queries remain available without that capability.
The integration checks CAP_SYS_RESOURCE for a current user caller before quota
admission; no opener credential or per-inode permission cache supplies bypass.
Project-ID changes transfer allocated and reserved usage before the new ID is
published. Project-inherit directories reject links and renames whose project
IDs differ, preventing an operation from bypassing the directory's quota domain.
Attribute changes require inode ownership or CAP_FOWNER; changing immutable or
append-only flags remains unsupported until mapped-write exclusion is complete.
For an inode that remains immutable, project identity is checked only when the
filesystem supports the project feature.

An external quota header is file input until activation succeeds. Header-format
rejection is propagated without aborting the filesystem's journal. The read-only
load has no journal handle, and its I/O adapter retains ordinary filesystem
I/O/corruption classification. Activated quota updates remain subject to the
existing metadata transaction and abort rules.

Read contexts cannot write without the optional borrowed journal handle. This
is write authority, not another journal or quota owner. Record acquisition reads
existing state before reserving space for a missing record; queries retain the
same acquisition requirements as accounting. Existing-record updates and grace
headers need bounded journal credits without physical-growth admission.

Quota file ownership handoff requires both mapping revocation and persistence.
Activation revokes writable PTEs before flushing ordinary data. Existing writable
descriptors cannot bypass immutable policy through mmap, mprotect or write faults.
Each ordinary write reenters shared superblock admission; quota enable/disable
retain exclusive admission through initialization and teardown. Disable stops
class accounting, detaches inode references and waits for in-progress evictions
before removing records. Reservations remain with their inodes for a later
activation. Disable drains the final quota metadata
transaction through checkpoint before restoring ordinary file access. No parallel
mapping registry, quota file table or compensating policy flags are added.

## Selection/admission boundary

The existing producer mutex excludes another scan until every declined unsent
batch is redirtied and its WRITEBACK ended. NONE admission refusal is capacity
feedback with skipped-page accounting, not an I/O failure. ALL waits capacity;
workers require no producer mutex. No new rejection field or coordination lock
is added. Buffered write_end cannot reacquire the copied folio or wait for
writeback while it publishes accepted size. A batched run settles through the
separate `BatchedWriteOperations` vtable after its folio locks are released: the
settlement hook cannot change the published byte count, publishes no size,
releases only the reservation ranges that run created, and may report only
settlement bookkeeping failures — never a data veto; published bytes can already
be writeback-visible when settlement fails. Completion remains independent of
the inode data lock retained by writer rollback.

Reservation settlement resolves filesystem context only when the accepted
prefix leaves an unaccepted tail. A complete overwrite keeps all reservations
without accessing that context. Single-folio size publication delegates the
growth check to the address space, which reads the inode's current size under
the still-held inode data lock. The completion request carries the prepared
range and copied count; the range's exclusive end is validated before admission,
and copying stays within that range. Short writes and cancellation release only
their token-owned unaccepted tail. Writeback retains its live EOF sampling
protocol.
