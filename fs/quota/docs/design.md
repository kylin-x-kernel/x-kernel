# kquota design

## Purpose and scope

`kquota` owns filesystem-independent disk quota accounting and the v2 quota
file formats. `kvfs::SuperBlock` owns one
`Mutex<QuotaInfo<Arc<VfsInode>>>`, initially disabled for all quota classes. Concrete filesystems supply quota-file I/O; quota
control, credentials, namespace ID translation, journal transactions and inode
block accounting remain outside this crate.

The implementation covers user, group and project identities, record
acquisition/release, administrative field masks, hard and soft limits, grace
periods, delayed-allocation reservations, synchronization, suspension and
disable. Both format IDs 2 and 4 are supported. No concrete filesystem enables
quota yet, and this change does not expose a quota syscall.

## Ownership and semantic layers

```text
kvfs::SuperBlock
  `-- Mutex<QuotaInfo<Arc<VfsInode>>>
        |-- [Option<Dqinfo<Arc<VfsInode>>>; 3] -- file, state, grace durations, QuotaTree
        `-- BTreeMap<QuotaId, Dquot> -- native usage, disk offset, flags

filesystem's existing quota files -- QuotaIo --> v2 codec / radix tree
```

The reference semantic boundaries are `include/linux/quota.h`,
`include/linux/dqblk_qtree.h`, `fs/quota/dquot.c`, `fs/quota/quota_v2.c` and
`fs/quota/quota_tree.c` in the local kernel reference tree. The object mapping is:

| Object | Semantic state |
| --- | --- |
| `QuotaInfo<F>` | Per-filesystem `quota_info` plus that filesystem's dquot cache |
| `Dqinfo<F>` | Per-class file reference and `mem_dqinfo` with accounting/enforcement/suspension bits |
| `Dquot` | `dquot` native data, stable disk offset, dirty and active bits |
| `Dqblk` | `mem_dqblk`, including transient reserved space |
| `QuotaId` | `kqid`; the cache key is the only stored record identity |
| `QuotaTree` | Selected format, `qtree_mem_dqinfo` block count and two on-disk free-list heads |

There are no independent managers, mount owners, staging objects or cached
copies of inode ownership. Tree depth, block size and entry size are derived
from the selected format, rather than retained as duplicate fields. The
presence of loaded file information represents usage-enabled state; that bit
is derived by the public state accessor rather than stored a second time. The
selected format is stored once in `QuotaTree`. Each loaded `Dqinfo<F>` owns
one canonical file reference, folding file presence into the loaded-class
invariant rather than maintaining a separate optional file array. KVFS uses
`Arc<VfsInode>`; the core has no dependency on VFS inode implementation types.
`QuotaIo::File` ties that retained reference type to the filesystem provider.
The provider receives the retained file for every operation. Its write method
borrows mutable transaction context, without owning another filesystem.

## Execution and concurrency

I/O-facing operations require initialized allocation and scheduler services,
may sleep and are task-context only. They do not require a process identity.
No spinlock, IRQ-disable guard or preemption-disable guard may span them.

Exclusive access to `QuotaInfo` serializes cache, usage, deadlines and format
metadata together. The VFS uses a sleepable mutex. The filesystem must establish
a consistent order with its own locks; its direct quota-file I/O must not
reenter accounting or reacquire any caller-held filesystem lock. The core
does not obtain current credentials or a current clock. Callers pass real-time
seconds and the result of resource-bypass authorization.

The I/O context must identify the same filesystem until the corresponding
class is disabled. Loaded state retains the actual backing file. Backing files
must bypass quota charging.
This is an integration contract, not an extra identity registry in the core.

## Lifecycle

`enable` validates a complete header and file geometry before publishing a
loaded class and retaining a clone of the supplied file reference. It borrows
the supplied file, and failure retains no reference. It accepts accounting
alone or accounting with limits. Missing
dquots are read as zero-valued records and instantiated in the radix tree
before acquisition succeeds. Cache entries retain active state so a failed
initial record/header write cannot become a successful cached acquisition.

Acquisition can write a new zero-valued record even for a usage query or a
charge that another quota class later refuses. Admission atomicity applies to
usage and deadlines, not to acquisition's disk allocation. Such records remain
visible to ID iteration until explicit release or disable removes them. This
matches the reference acquisition boundary rather than deferring object
creation until a charge. The filesystem must account for this I/O when choosing
a writable quota context and deciding when to release cached records.

`sync` attempts every dirty record and then dirty file information, preserving
the first error and retaining failed dirty state. Filesystems with bounded
transactions use `next_dirty_id`, `sync_dquot` and `sync_info` to perform the
same writeback in separate handles. The cursor advances exclusively after the
last ID, including failed IDs; it uses the existing ordered cache without an
extra dirty list or copied record collection. Writeback accepts only active
dquots. A dirty record left inactive by failed acquisition returns `EIO` without
quota-file I/O; it retains dirty state until explicit acquisition completes.
Writeback never retries tree insertion on an inactive record. An independent record can
complete even when an earlier record repeatedly fails. It does not provide a device
flush or transaction commit. `release` writes a retained nonempty record or
deletes an unused one, then removes its cache entry. A later operation can
reacquire it; callers receive value snapshots, never references into the cache.

Suspension flushes first and skips later accounting while keeping loaded
state. Resume restores the same accounting/enforcement mode. Disable flushes,
releases unused entries, drops the cache and file information, and returns
the retained file reference for that class. The filesystem must release the
returned file after leaving its locks, so final inode eviction cannot
recursively acquire mount or quota state.
Suspend, disable and release refuse outstanding reservations with
`EBUSY`, so the filesystem must first drain delayed allocation. Destruction
does not perform implicit I/O; the filesystem owns successful final disable.

## Accounting algorithms

A charge accepts at most one ID per quota class. Disabled or suspended classes
are skipped. Each active dquot is acquired and its candidate native state is
computed in a fixed three-element stack array. Only after all checks succeed
are the candidates installed. Thus an `EDQUOT` or arithmetic failure cannot
leave a partial user/group/project usage charge or a newly started deadline.
This transient calculation adds no persistent phase or rollback fields.

Limits and usage are signed native counters measured in bytes or inodes.
Zero limits mean unlimited. Hard limits refuse growth strictly above the
limit. A first soft-limit crossing starts the class's grace duration; further
growth is refused at or after the absolute deadline. An allocation without
`WARN` cannot initiate a soft-limit grace period. Resource bypass and `NOFAIL`
override quota refusal while preserving accounting; arithmetic overflow still
fails. Inode grace uses the independent inode duration.

Grace-duration updates immediately write file information through `QuotaIo`
without flushing unrelated dquots. An information-write failure retains the
updated memory values and dirty bit for later synchronization. Existing grace
deadlines are preserved. Filesystem transaction commit and durability remain
the provider's responsibility.

Reservations count toward space admission but are not written to the quota
file. Claim transfers already admitted space from reserved to allocated
without a second limit check. Reclaim reverses that transfer and rejects an
amount exceeding any participating record's allocated bytes before changing
any class. Administrative signed reconciliation is not permission to transfer
unallocated bytes. Free operations
stop at zero and clear a deadline at or below the soft limit. An excessive
claim is rejected before mutation. Administrative `SPACE_USED` sets total
space including reservations, so the allocated counter can temporarily be
negative when an administrator reconciles usage below outstanding reservations.

## Storage algorithms

Quota blocks are 1024 bytes. A four-level radix tree consumes the ID's four
bytes, using 256 little-endian block references per level. The root remains
at block 1. Data blocks have a 16-byte header followed by 21 revision-zero
48-byte entries or 14 revision-one 72-byte entries. A leaf references a data
block; the requested ID is located within its bounded slot array.

The tree validates block ranges, ancestor cycles, data counts, duplicate IDs
and free-list links as those structures are accessed. A partial-list block is
the head exactly when its previous link is zero; unlink validates both
directions of that equivalence before any disk write. Insertions reuse the
partial-data-block list and then the free-block list before growing the file.
Deletion returns newly partial blocks to the entry list, completely empty
blocks to the free-block list, and prunes empty tree nodes while preserving
the root. Iteration is inclusive, skips empty subtrees and never wraps at the
last valid ID.

Space limits are rounded upward to 1 KiB units at encoding. The final partial
unit at the signed native maximum decodes to that maximum. The ID-zero
all-zero record uses an inode-timer value of one as its nonempty-slot escape;
decoding removes that escape. Reserved bytes have no disk field.

## Failure and persistence boundaries

Malformed geometry and traversed structure return `EUCLEAN`; incompatible
magic/version returns `EINVAL`; incomplete reads/writes return `EIO`. The
underlying I/O error is propagated. Counter overflow returns `EOVERFLOW` and
out-of-format administrative limits return `ERANGE` before acquisition.

Updating a tree or its free lists can require several writes. The core does
not copy whole files or simulate atomic rollback. A concrete filesystem must
enclose these writes in its existing transaction domain. A structural write
failure can leave partially changed disk state and requires the filesystem's
abort/recovery policy before normal operations continue. Retrying an ordinary
record overwrite or file-information write retains the pending data; this is
not a promise of recovery from arbitrary interrupted structural updates.

## Validation

Deterministic guest unit tests cover limit/grace boundaries, administrative
masks, reservations and claim/reclaim conservation, multi-class admission,
state transitions, format revisions, ID-zero escaping, radix iteration,
data/free-block reuse, corrupt input and short/failed I/O. An independent
e2fsprogs-generated quota file checks interoperability; its provenance and
regeneration commands are next to the fixture.
