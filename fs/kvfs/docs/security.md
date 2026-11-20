# kvfs - Security and Reliability Analysis

## Dynamic directory disappearance

An exited procfs child can vanish after `child_names` returns its snapshot.
SimpleDir skips that child's `NotFound` while enumerating subsequent live
entries. It does not suppress permission, allocation or I/O errors, and direct
lookup of the missing child retains its normal failure behavior.

## Writeback test fixture allocation

Writeback test operations are allocated fallibly before inode publication or
worker startup. Allocation failure is a fixture setup failure, not an injected
writeback failure. The deferred setup helper propagates the error to its test
caller. Successful operations retain their existing guest-lifetime leak because
the inode stores a static operations reference. Nested Arc/Vec allocations and
production writeback behavior are unchanged; this is not an end-to-end OOM test.

## Parallel lookup result publication

The parent children mutex protects both candidate membership and the decision
to expose a completed cache hit. Checking `PAR_LOOKUP` only after releasing
that mutex could expose an obsolete negative candidate after successful
replacement or non-cacheable lookup, causing spurious `NotFound` errors for
existing names. Pending candidates stay discoverable for lookup serialization,
but neither fast nor slow lookup consumes them as results. Tests cover candidate
retraction, replacement, and retention of genuine completed negative entries.

## Prepared mount-state ownership

Private-owner allocation errors occur before the SimpleFs root factory runs and
before a superblock is published. Once transferred, a prepared owner's address
is stable; the superblock drops it exactly once, including after root initialization
failure. Block-backed initialization still uses the registry's existing failure
transition to wake waiters and release the device claim. Tests cover pointee
address preservation, teardown, failed root initialization, and rejection of
invalid owner flags before root construction; they do not force global OOM.
Nested Arc/Vec/String allocations and root construction remain infallible where
they were infallible before. This is not whole-mount OOM safety.

## Fallible directory iterators

Iterator allocation errors propagate before directory entries are emitted, so
this failure leaves the directory position unchanged. Child-iterator errors and
outer-owner allocation errors release already-created iterators. Vec/String
snapshots and lazy name formatting remain infallible allocations: migrating the
iterator owner is not a guarantee of end-to-end OOM-safe directory enumeration.

## Mount-transaction private-state allocation failure

Both FsContext setters allocate fallibly before changing their slot. An OOM
leaves the old state intact; initialization errors destroy already installed
state without publishing the failed transaction. tmpfs/ext4 propagate errors
instead of converting them to fatal OOM. Tests cover erased-state replacement
and cleanup after initializer failure; they do not force global allocator OOM.
Default KBox allocation requires sleepable task context and reclaim-compatible
locking, even though the backend does not currently reclaim. This does not
make parsers, nested Vec/String/Arc allocations or superblock creation fallible.

## File-lock context allocation failure

The inode's first advisory-lock context allocation can return `NoMemory`.
`Once::try_call_once` publishes only a fully initialized `KBox`; on error the
cell stays uninitialized and no lock request reaches `apply`. Later attempts
may retry. Both record-lock and BSD-lock entry points propagate the error.
`KBox::new_nowait` prevents this spinning initialization path from opting into future
sleeping reclaim. This change does not make subsequent lock-record, waiter,
or other VFS allocations fallible.

## Test Fixture Scope

Demand-read cancellation releases only the wait registration, leaving submitted
initialization owned by `FolioRead`. Writes reject invalid sleeping context
before `write_begin`, and truncation does so before publishing size. Filesystem
callbacks must restore the admitted context before returning, allowing mandatory
rollback and writeback cleanup to complete without a new admission failure.

The audit scope includes guest fixtures under `ut-tests/` and the host-only
path fixture under `tests/host/`. Guest modules remain private children of
their original production parents and require `cfg(unittest)`; the host
module retains `cfg(test)`. Address-space fixture nesting, readahead
observation, and test-only production hooks remain unchanged.
All 17 serial flags are preserved. Moving the fixtures adds no production
entry point and changes no unsafe, registration, locking, or cleanup contract.

## Named cdev identities and projection

Named registration validates class/name path components and rejects overlapping
ranges or duplicate class/name identities before publishing any identity. Handles expose no unregister authority;
only the range owner may call cdev_del. Retired snapshots cannot supply backend
operations or claim availability, even if the number is later reused. Callbacks
run in sleepable task context outside registry locks, and consumers must
serialize snapshot installation with their callback handling. No registry lock
is held across filesystem invalidation or device callbacks.


## Captured Poll Source Boundary

Backends choose the captured wait source; KVFS does not infer identity from
readiness or retain a file/process owner through `FilePollSource`. Disjoint
events and expired sources register nothing and allocate nothing. Capture
errors propagate without publishing a registration. Consumers must retain
successful registrations, recheck readiness, and keep an expired binding
distinct from an absent binding. Registration may run waker callbacks when
its temporary source owner is released, so conflicting outer locks are forbidden.

## Trust Model

A plain open of a magic link follows the Path it holds, rather than re-resolving the
display name used for readlink. The target is still subject to the magic-link follow policy, the link count limit, and the may_open permission check;
this guarantees that a retained old path is not replaced because an object with the same name was re-created.

Character-device ranges are accepted only from kernel registration; empty ranges, minor overflow, and any overlap within the same major are
rejected at the registration boundary. Unregistration accepts only a complete range and does not allow splitting other registrations. The retirement
marker of a registered object only transitions from valid to invalid; the inode weak cache cannot keep an old registration or backend alive indefinitely.
When a cache hit races with unregistration, the already selected backend may continue to execute open; unregistration is not a synchronous drain,
so the backend must check instance admission on its own. Callbacks, failed registrations, and the final backend destruction after unregistration all happen outside the table lock.

Access by device number (`DeviceId`) is constrained by the global
device-access policy (see design.md, "Device-access policy (reverse
injection)"): when a registered policy denies the access, the
`open_character_device`, block-device open and `mknod` enforcement points
reject with `VfsError::OperationNotPermitted` (`EPERM`). The protected assets
are device-number access itself and the policy slot. With no policy
registered, access is unrestricted.

User-provided paths, open flags, rename flags, mount flags, and mount data are untrusted. The POSIX syscall layer
is responsible for copying user memory and completing the initial ABI validation; `kvfs` receives only kernel-owned path strings, opaque mount-data
byte slices, and typed flags.
Directory entries, extents, and metadata returned by a concrete filesystem must also be treated as external input that may fail. `kvfs` validates name, mount relationship, type, topology, and operation flags before committing
namespace state.

`Cred` comes from trusted task state, but its UID/GID is not a "privilege guarantee" and can only serve as DAC input.
The caller is responsible for obtaining a stable `Arc<Cred>` at the operation entry point; `kvfs` is responsible for making the entire path walk, the final check,
and the namespace mutation use the same passed-in object.

`CommandFile` does not trust the read callback to always return content; `Ok(None)` returns
`VfsError::InvalidData` instead of triggering a kernel panic.

The complete-command transport is crate-private; procfs supplies only business
callbacks and never calls the copy helper directly. `SeqFileInode` opts into the
writable form only when a command callback is configured; read state remains
owned by `SeqFile`.

Sequential observation errors are not EOF: `try_start`/`try_next` preserve
`ENOENT`/`ENODATA`/other errors and discard the partial record. A private
`ReadRecovery<T>` state distinguishes format retry from observation recovery:
format failure always retains the failed item, including at a nonzero offset
with no prefix; observation failure reports first when a prefix was copied, then
rewinds and rebuilds the committed byte position rather than trusting the failed
iterator cursor. `SeqIterator::Item` is `Send` because an item may be retained
across reads. `SeqTextSink` keeps the real `NoMemory`/`FileTooLarge` in a private
buffer side channel; a formatter cannot clear it, and a plain `fmt::Error`
without a sink error is mapped to `Io`. Lazy `SeqFileInode` forms do not observe
at open and apply `validate_read` to each new read/read_iter.

## Directory position serialization

Directory iteration samples f_pos, runs the filesystem callback, and commits
`ctx.pos()` as one `VfsFile::dir_position` operation. Directory `llseek` takes
the same lock, so concurrent dup descriptors cannot interleave a read-commit
with a seek or with another iterate. `position` remains the sole f_pos storage
and is only briefly locked; the operation lock, not the position lock, defines
the callback's lock lifetime. Directory callbacks must not re-enter iterate or
seek on the same open description. Production position changes outside
`iterate_dir()` go through `llseek()`; direct `set_position()` is limited to
iteration commit and tests.

## Sparse directory sources

A sparse directory source is a kvfs opt-in and must return canonical, unique
names and a current object matching the key/name pair. `name_for_key()` or
`lookup_key()` `NotFound` is a vanished candidate: it is skipped without
consuming an output cookie. A returned dentry with a mismatching name, or an
empty/dot/slash/NUL name, is rejected with `InvalidData`. kvfs does not build a
full name table to detect duplicates; the source owns that invariant.

Cookies are VFS positions, not keys or inode numbers; they are bounded by
`i64::MAX`, sparse directories reject `SEEK_END`, and positive seek overflow
returns `EOVERFLOW` without changing f_pos. The sparse adapter disables child
dentry caching because the source has no invalidation contract in this version;
fresh pathname lookups must re-consult the source. A sink rejection keeps the
pending key and does not advance the cursor. A seek past EOF preserves the
requested position rather than silently clamping it. Source locks must not span
the sink callback, and sparse does not support `ChainedDirOps` composition in
this version.

## External Boundaries / Attack Surface

- `Filename::open_with_flags_at` and `dentry_open` are compatibility entry points that preserve raw `O_*` bits, and they normalize the
  raw bits into `OpenParams` and `OpenFlags`.
- `Filename::open_with_openat2_at` receives the raw `struct open_how` triple
  (flags/mode/resolve) copied from user space by the POSIX layer, which checks
  the size and reserved bytes; kvfs validates the triple through
  `OpenHow::from_openat2` before it reaches namei.
- A device open callback can inspect the transient `O_NOCTTY` only through the narrow interface of `VfsFileBuilder`, which avoids
  exposing or misusing the full set of creation-time flags.
- `sys_renameat2` converts the raw rename bits into `RenameFlags` before entering the VFS.
- Filesystem operation traits may return errors and metadata produced by disk, network, or device backends.
- The optional `InodeOperations::fiemap_operations()` can output the backend block mapping, but it may pass checked fields only through the borrowed
  `FiemapExtentInfo` and a safe writer; it cannot retain the writer or touch user pointers;
  the callback runs inside the inode shared lock.
- The `&Cred` of every pathname and namespace mutation API is a permission boundary; omitting or replacing it changes
  the authorization subject of the current operation.
- `VfsFile::cred()` exposes the immutable credential captured at open time, for file operations that need Linux `f_cred`
  semantics to read.
- The superblock default dentry operations accept only kernel-constructed static tables; once a dentry is bound, the operation identity cannot be
  installed or replaced from user input.
- `AnonInodeFs::global()` is the runtime entry point for creating anonymous files, but it accepts only a singleton already
  initialized during the boot phase; it does not trigger global VFS initialization directly from user input.
- `Path::render_from()` explicitly distinguishes whether the requested root is reachable or unreachable. When crossing mounts, the
  mountpoint itself must be included; the empty dentry name of a mounted root must not be treated as a path component, and the `..` semantics of an ordinary
  `Path::parent()` must not be used to skip the mountpoint.
- The filesystem type registry accepts only kernel-constructed static descriptors; a user-provided type name can only perform an exact lookup
  and cannot install or replace registry entries.
- `FsContext::data()` exposes only the opaque mount-data slice that the syscall layer has already copied with bounds; the filesystem
  type must interpret its own text or binary representation within the synchronous `get_tree()`, and copy any
  typed state that needs to be kept long-term, and must not retain user pointers or the borrowed slice.
- Filesystem mutation results enter the VFS cached inode attributes; an incorrect identity or immutable
  geometry is untrusted.
- Xattr names and values come from the syscall boundary; KVFS accepts only kernel-owned `XattrName` and value slices, and
  namespace permissions must be checked before the filesystem callback is invoked.

`kvfs` does not directly dereference user pointers and does not directly access MMIO, PIO, DMA, or architecture FFI.

## unsafe code inventory

| Location | Call path | Purpose | Invariant / safety argument |
|---|---|---|---|

The `unsafe` encapsulation of the file-lock shared node lives in the private `locks/node.rs`: range access relies on unique ownership,
and the wait relationships and cancellation unlinking are protected by a global wait lock. The lease payload `LeaseState` is stored in
`LockNode.lease` as an `UnsafeCell<Option<LeaseState>>` with the same ownership discipline: mutations go only through `&mut OwnedLock`,
and every read or write of the lease list additionally runs under the owning inode's lock-state mutex, which serializes all lease-list
accessors — this is the prerequisite the lease SAFETY comments record and it is what makes the raw dereferences sound. The intrusive list uses the `intrusive-collections`
typed adapter and the safe `clone_pointer()`, and does not adjust Arc reference counts by hand. The rest of the namespace
model maintains memory safety through safe Rust locks, `Arc` ownership, and operation traits.

## Memory-safety invariants

- Writeback error returns preserve the budget updates of already completed pages and finish every folio writeback that has started;
- The location guard of `LockedDentry` bounds the lifetime of the borrowed dentry name.
- `parent` and `name` are replaced together under the same location write lock.
- During rename, the source dentry and the corresponding `VfsInode` stay alive.
- A directory inode is associated with at most one live dentry; a repeated lookup must reuse that alias and must not create a second
  child cache and topology location.
- Before a create-like callback succeeds, it must instantiate the negative dentry supplied by the VFS and must not return another
  object that has not passed transaction validation.
- On a lookup miss, a hashed negative dentry carrying the parallel-lookup state is published before the filesystem callback;
  a lookup of the same name must wait for the owner to finish and must not build a second candidate in parallel.
- A filesystem callback must not modify VFS cache internals directly.
- Every raw flags family must be converted to its bitflags type at the boundary.
- Unknown open/rename bits must not enter the internal namespace or open algorithms.
- `f_flags` in `AtomicU32` is read and written only through the `OpenFlags` API.
- Different flags types must not be converted into each other inside the VFS through `.bits()`.
- `FiemapExtentInfo::fill_next_extent()` must reject zero length and logical/physical end overflow;
  delayed, encrypted and inline/tail states must be completed with the implied flags required by Linux FIEMAP.
- FIEMAP `SYNC` writeback and the extent walk must be covered by the same inode data read lock, so that no buffered write can establish a new delayed allocation between the two
  phases; a shared write fault publishes its reservation through the filesystem
  mapping lock and the delayed-state lock, not through the inode data lock.
- A live child dentry strongly holds its parent, while the parent keeps only a weak index of its children; the superblock dcache
  strongly holds hashed dentries, and eviction must remove both the weak index and the dcache ownership.
- The operation table inherited by a dentry must match the counterpart of the default `s_d_op` of its unique superblock;
  after binding, the table must not be replaced and the superblock must not be rebound.
- The magic, block size, maximum file size and root inode of a pseudo filesystem are installed once by a generic
  helper; a concrete implementation must not duplicate another set of geometry/identity in its private state.
- `Nameidata` does not hold a credential; the caller of an operation must pass the same `&Cred` through the whole method chain.
- The `openat2(2)` `RESOLVE_*` constraints must be enforced uniformly by namei,
  not by replacing the root in the syscall layer: every `BENEATH`/`IN_ROOT` path
  transition must verify the result stays inside the anchored subtree
  (`Path::render_from`); `NO_XDEV` must pin the starting mount and cover descent
  into a mount, an upward `..`, an absolute symlink target and a magic-link jump;
  `NO_SYMLINKS` must cover the final component and magic links.
- An anchored lookup must refuse to deliver an outside object (`EXDEV`) when a
  concurrent rename changes an intermediate directory's topology; it must not
  trust later parent traversal just because `path == root` held earlier.
- `RESOLVE_CACHED` must return `EAGAIN` before a slow lookup or a side effect such
  as create or truncate.
- Intermediate directories of a path must pass `MAY_EXEC` before the next component is looked up.
- A trailing `/` must force `FOLLOW_FINAL | DIRECTORY` before the final component lookup; the final
  `LOOKUP_DIRECTORY` must be consumed by the generic namei with `Dentry::can_lookup()`, and must not be replaced in the open layer by
  `d_is_dir()`, which accepts autodirs.
- The final open must check the target inode according to the access mode, not merely whether the path exists.
- A namespace mutation must check `MAY_WRITE | MAY_EXEC` on the relevant parent directory; the sticky and mountpoint policies of unlink, rmdir and rename
  must be enforced against the victim obtained by the final lookup under the parent directory lock, and must not
  reuse an object pre-looked-up outside the lock.
- The initial UID/GID of a created inode must be derived by `inode_init_owner()` from `fsuid/fsgid` and the parent directory
  setgid state, and must not use a fixed root owner.
- `mkdir` must first clear the set-user-ID/set-group-ID bits in the user mode; only a setgid parent directory may let
  `inode_init_owner()` add set-group-ID back to the new child directory.
- `Path::chown` authorization includes Linux `chown_common()` set-ID cleanup:
  non-directory set-UID is always removed and set-GID is removed when group
  execute is set, or when the caller is not in the pre-chown GID and lacks
  effective `CAP_FSETID`. When cleanup clears a bit and therefore generates a
  mode update, set-GID is also removed unless the caller belongs to the new GID
  or has effective `CAP_FSETID`; a GID-only change that clears no set-ID bit does
  not trigger that extra check. A generated mode change requires inode
  ownership or effective `CAP_FOWNER`; `CAP_CHOWN`/`CAP_FSETID` alone do not
  authorize it. Directories preserve their set-ID bits, and a `chown(-1, -1)`
  that changes no mode bit remains an authorized no-op.
- A metadata update must first be authorized through `Path::chown/chmod/set_times`; the backend `setattr` callback
  only performs the already authorized mutation and must not serve as a public bypass entry for the syscall layer. The default implementation is a common fallback layer that
  accepts only owner/mode/times and publishes them as is, and always rejects size: an inode without a dedicated table installed must not use the fallback
  path to forge `i_size` or gain any mutation capability beyond what `Path` authorized. The fallback layer applies only to inodes whose identity and
  metadata are both owned by themselves; an inode whose identity is shared across files (the anon_inodefs singleton)
  must explicitly override `setattr` to keep the rejection semantics, so that one authorized update is not observed by all holders.
- Timestamp authorization must distinguish touch from an explicit times array; a combination of a single `UTIME_NOW` and
  `UTIME_OMIT` is still an explicit request.
- `TimestampLimits` construction must reject a zero value, a granularity above one second and a reversed range; the generic superblock
  construction path uses a conservative whole-second default, and a filesystem must raise its capability explicitly. An authorized explicit time is prepared by
  `VfsInode::truncate_timestamp()`, while automatic I/O and namespace times are prepared by `current_time()` before the
  comparison; a filesystem callback must report the values actually applied through the returned `MetadataUpdate` and omit fields it did not apply,
  and the resident inode publishes only that returned value; the raw setter no longer re-applies policy or restores the original precision.
- Descriptor truncate must verify `FMode::WRITE`; pathname truncate must check inode write permission
  with the credential at call time.
- A dentry backing metadata refresh must first confirm the positive state, then match inode number, node
  type, block size and `rdev`; an external filesystem bridge does not obtain the internal `Arc<VfsInode>` directly.
- Split truncate must call `truncate_setsize()` exactly once under the inode data lock and the address-space invalidate exclusive lock;
  that entry first publishes `i_size` and performs one mmap invalidation before and one after the cache truncate. A filesystem with separate
  backing must override the default `set_len()` and is responsible for maintaining
  the disk recovery protocol for failures after backing prepare.
- The VFS-wide identity table is indexed jointly by `(SuperBlock, inode number)` and is the only hashed resident inode
  identity table; `SuperBlock`, a filesystem and a bridge must not keep a second hashed cache field. Linux-style
  pseudo/unhashed inodes may stay out of the table, but the same filesystem identity must not mix direct construction with the hashed initializer.
  A missing entry must first publish
  `New` and then run the fallible initializer; a local reservation guard must, on an error or a panic unwind, remove
  the unpublished `New` and wake the waiters; a new inode must be bound to its owning superblock before `Live` is published; `New` and
  `Freeing` can only be waited on and must not return an ordinary inode. The key itself holds a `Weak<SuperBlock>` allocation identity and compares by pointer;
  this does not keep the superblock value alive, but guarantees that the address cannot be reused before the hashed slot is removed. Establishing a parallel identity with a reusable raw integer address
  or an extra superblock serial is forbidden.
- A filesystem using `InodeAttributeOperations` must make the operations describe the inode of the same number and type
  being constructed; KVFS keeps only that shared backend and must not allocate another copy of generic attribute storage at the same time.
- The final drop of `VfsInode` must, before the filesystem eviction hook, turn the exactly matching cache entry from `Live`
  to `Freeing`, and after the hook returns delete only that object's entry and wake the waiters; a backend must not build its own resident cache.
- Once the superblock identity of a dentry and an inode is first bound, it cannot change; even if the old `Weak<SuperBlock>` can no longer
  be upgraded, the same object must not be bound to another superblock. A legitimate pathname callback keeps the superblock alive through `Path/Mount`;
  a call that bypasses that owner returns `ESTALE`.
- `VfsInode` owns exactly one `AddressSpace`, and MM/filemap obtains it only through `VfsFile::mapping()`;
  the address-space operation table must be a static table shared by all inodes, and a callback uses only the unique host inode/superblock of the mapping
  passed in; it must not use the filesystem inode as an aops object at the same time, nor keep a parallel
  owner back-pointer. The generic block size and max file size are kept only by `SuperBlock`.
  `MappingPages` is held directly by the AddressSpace and must not be exposed to
  filemap or a filesystem as an independently clonable cache owner. The entries and the derived dirty marks are maintained under the same mutex, while Folio references
  continue to keep page data alive independently; this adjustment does not change the invalidate lock or the mapping identity check.
  Filesystem-owned `CacheResidency` is installed once during inode construction.
  Admission and release run as cache identities enter/leave the mapping index.
  Callbacks must not sleep, reenter a mapping, lock folios, perform I/O, or
  invert the folio-to-index-to-provider lock order. Detached physical references
  do not retain cache-residency capacity.
- The operation table of a `SuperBlock` is separate from the filesystem-private state, corresponding to Linux `s_op` and
  `s_fs_info`. The construction API accepts only a static shared operation table; a filesystem callback must recover the private state through a typed borrow bound to
  `&SuperBlock`, and must not obtain an escapable private owner, store the mount owner in the
  operation object, or create a second filesystem identity.
- The active `PipeObject` of a pathname FIFO must be held by a typed slot of the `VfsInode`; the shared
  `FifoFileOperations` must not hold session state.
- The pipe access direction can come only from `VfsFile::mode`, the HUP generation snapshot can come only from
  `VfsFile::pipe_generation`, and the private data holds only the `PipeObject`.
- A non-blocking reader with no writer must not report HUP until the writer generation changes; a poll waiter
  must register on the queue according to the file mode, and the user event mask must not remove the wakeup source of HUP/ERR.
- `PIPE_BUF` atomicity must be decided on the complete user write request, and must not turn
  a large write into an atomic write because of the iterator's internal staging chunks; after a partial write, the uncommitted source iterator progress must be rolled back.
- A writable control file consumes only the current write buffer; a write command must not infer the persistent content length from `i_size`,
  and the size of a regular file may be published only after the backing store has accepted the write.
- The automatic atime/mtime/ctime updates of a pathname FIFO must go through the VFS mount/time policy and the
  filesystem `update_time` callback; an anonymous pipe must not update pathname inode metadata.
- The target of an immutable simple symlink is held only by the `VfsInode` cached-link state;
  `SimpleFsNode::i_size` must equal the byte length of the target, and a copy must not also be kept by a closure or an extra field.

## Dynamic directory metadata callback

`SimpleDirOps::link_count()` is a trusted filesystem callback, executed in a sleepable task context,
and invoked without holding the `SimpleFsNode` metadata lock. The implementation is responsible for its backend synchronization and for lifetime errors; errors
are propagated to `getattr()`. The interface's return value covers only the nlink of this metadata copy, and does not modify the inode identity,
permissions, owner, or the node's stored metadata. The return value is a single observation and must not be used as a basis for member synchronization or authorization.

## Thread Safety

The character-device adapter holds an `Arc` to the selected backend and takes no lock outside the open/release or poll callbacks.
Replacing the backend in open must complete before the file is published; on failure the backend rolls back, and the release of a successful file is not called.
Only the final operation table determines the capabilities and release callback of an opened file; the multiplexer does not additionally release the replaced table.

The interrupt-source marker of a file wait does not imply zero completion, and does not authorize replaying the entire transfer. The caller must retain
the completed amount of the object and the iterator; an ordinary operation error or a registration failure must not be disguised as a signal interruption. The wait
future owns the subscription and cleans it up on cancellation; the operation closure must not carry the object lock into the wait phase.

Shared dentry, inode, mount and file state is protected by mutexes, atomics, `Arc` and `Weak`.
The VFS inode-cache mutex protects only the `New/Live/Freeing` entries; the initializer, the eviction hook and waiting never hold
that mutex. Each slot has its own wait queue; both the drop of an unpublished reservation and normal state transitions wake only that slot after releasing the mutex,
and waiters also validate the slot generation, which avoids missed wakeups, cross-inode thundering herds, or treating a `Live` entry that has entered
final drop, where the Weak upgrade fails, as a cache miss and rebuilding it in parallel.
Mutable per-mount flags are stored in `VfsMount::mnt_flags` and are wrapped by `flags()` and `set_flags()` as the
strongly typed `MountFlags`, using relaxed ordering because the flags neither publish nor protect other mount state.
remount/reconfigure and make-private validate, before the operation, that the target is the root path of a mount registered in the current mount namespace.
make-private relies on the global design invariant that no shared propagation group currently exists,
so it never silently treats a truly shared mount as private. An ordinary remount receives the superblock
flags and the per-mount flags independently, and synchronously updates the read-only policy of the shared
superblock; `MS_REMOUNT|MS_BIND` updates only the per-mount flags of the target mount.
The superblock flags are wrapped by `AtomicSuperBlockFlags` as the strongly typed `SuperBlockFlags`; an ordinary remount
constructs an `FsContext` whose purpose is reconfigure, calls the static
`FsContextOperations::reconfigure` before publishing the proposed flags, and serializes the hook, the publication and the final shutdown with each superblock's
umount lock. The last active reference is first
decided under the lifecycle lock, then re-validated after the umount lock is taken and switched to dying; the shutdown callback does not hold the lifecycle
lock, and the dying/dead state rejects new mount activations. The context is bound to a unique target superblock, and the callback
recovers the owner only from the context; the VFS verifies the identity before the call. The context separately owns the parsed `fs_private`
and the proposed `s_fs_info`, and carries `sb_flags_mask`, so a failed transaction does not pollute the existing `s_fs_info`.
The default hook accepts pure VFS-flags changes; a block-backed superblock
is validated by the VFS in two phases — pre-hook against the caller's
proposed flags (gating hook execution) and post-hook against the
data-adjusted final flags (gating publication, covering data-level
`ro`/`rw` adjustments).
Explicit filesystem
sync and the global Weak registry sync snapshot
are also serialized with the final shutdown through the same umount lock, and skip dying/dead superblocks.
`get_tree_bdev` rejects a
writable mount of a canonical read-only block device before calling the filesystem fill-super, so that a read-only constraint of the medium is not deferred into a later filesystem I/O error. Before fill it establishes a nascent reservation keyed by the canonical
filesystem-type pointer and the canonical block-device object and holds an exclusive
claim; concurrent requests with the same identity wait for the same fill, and on failure or final shutdown the registry identity is removed and the claim
released before waking them to retry. Different filesystem types cannot own the same device at the same time.
mount attach/detach modifies the topology under the mountpoint inode namespace lock. Each child mount
protects its complete parent `Path` with a single mutex, the parent's child map stores `Weak<Mount>`, and the namespace
registry stores the `Arc<Mount>` of visible mounts; detach clears the parent path before removing the registry reference.
attach, bind, remount and detach all first use the existing registry to validate that the mounts involved belong to the current
`MntNamespace`, and make the registry mutex cover validation, topology modification and registry commit, which avoids a
membership TOCTOU. Recursive detach also collects the overmount stack of each child mountpoint,
and validates the complete set before any topology modification; the commit phase performs no more fallible validation. The registry mutex
is explicitly released before anything that may trigger the release of the last mount reference. The dentry children map and the superblock dcache
are not locked in a nested fashion; namespace operations first
update the parent's weak index and then update the dcache's strong ownership. Typed flags are immutable value snapshots and provide no shared
mutable state.

A regular inode's data lock covers generic buffered writes and the entire
set-length callback. Invalidate shared protection covers `page_mkwrite` and the
writepages call; exclusive protection covers truncate, mapped-view invalidation,
and cache truncation. The target folio lock serializes fault preparation, dirtying,
and PTE publication. Filesystem mapping/reservation locks are acquired within these
boundaries. Writeback callbacks must not upgrade to invalidate exclusive protection.

All namespace locks are sleepable locks. Mount tree operations first take the mount-namespace registry
mutex; the global order after that is superblock topology, parent directory, child directory, non-directory inode, and only last
dentry cache/location. Cross-directory ordering is decided by parent dentry identity: parent locks are taken ancestor-first, and unrelated parents are ordered by `Dentry::namespace_lock_key()` (ancestor inode pointer chain, then pointer value).
Directory
inodes rely on the single-alias invariant; the inode locks corresponding to non-directory aliases are deduplicated by identity and sorted by pointer value.
The mount topology lock is always acquired after the related inode namespace locks.

The default dentry operation table is published when the nascent superblock is initialized. Root/child dentries inherit only
a static reference; concurrent paths neither allocate nor replace the operation object.

The FIFO session lifecycle uses the inode pipe-slot lock to serialize the get/create/`files` transition with
release/clear, and the pipe state lock may be acquired only after the slot lock. reader/writer close first releases the
pipe state lock and then enters the slot lifecycle path, which avoids reverse nesting. The same inode therefore cannot publish
two active pipe sessions at the same time.

Pipe duplication must serialize destination reception with source consumption; it must not consume first and then discard a failed suffix. A two-pipe
operation takes the locks in object-address order, and both waiting and notification happen after unlocking. The single-pipe duplication callback runs under a sleepable
mutex and permits only non-pipe backends; it must not re-enter any pipe. The pipe lock precedes the non-pipe file-position lock, and
entering pipe duplication while holding a file-position lock is forbidden, so that waiting for space does not block other users of that file position.
The number of bytes reported as successful by the callback must not exceed the length of the passed slice; if it does, EIO is returned, the source pipe is not consumed and no destination
pipe data is published, and the count is not truncated to pretend success. Side effects that the backend itself has already performed cannot be rolled back.
The transfer position locks for file-to-file transfers are ordered by open description address, and the same object is acquired only once;
the staged read position must not be committed by the readahead length. The transfer entry point updates only the kernel-side offset, and the write-back to the user pointer is completed by the
syscall after the position lock is released; a write-back failure cannot revoke data that has already been accepted.
Copying between files validates the returned read and write counts before slicing, accumulation and position commit: a read must not exceed this round's request,
and a write must not exceed the amount read in this round. An excess is handled as EIO and this round's staged position is not committed; the legitimately completed amount from before
is still returned first, and data side effects that the backend has already performed are not promised to be rolled back.

The filesystem-type registry is protected by its own mutex. Boot registers serially, lookup returns a static descriptor copied by value,
and enumeration returns a snapshot; the mount factory is not called under the registry lock, so that filesystem initialization is not brought into the global critical section.

A Credential itself is an immutable `Arc` snapshot. The task credential lock is not held during a permission check, and the current task is not
re-read, so a concurrent `commit_creds()` can affect only the next operation.

A backend may sleep while holding a VFS lock. Re-entering a VFS namespace
operation on the same namespace object is not supported and may cause a deadlock. A layered filesystem may acquire a
lower filesystem lock only after an upper VFS lock; before such nesting is generalized, an explicit cross-filesystem lock rank is also needed.

## Threat Analysis

Aligned direct passthrough for block device files uses only the capacity-trimmed buffer sub-range, and the general block layer still performs the
range, partition-offset and read-only checks. The buffer is only borrowed for the synchronous call and must not be returned until the driver completes; no raw pointers,
driver-entry bypasses or copies are added. A failed multi-block write does not guarantee that the medium was left unmodified; when the backend provides no partial-completion length,
KVFS returns the mapped error, does not fabricate a successful byte count, and does not retry regions that may already have been written.

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Unknown flags change control flow | Medium | The user passes an unsupported bit | The boundary uses `from_bits` and returns `EINVAL` |
| T-02 | rename mode conflict | Medium | `EXCHANGE` combined with `NOREPLACE/WHITEOUT` | The syscall and the VFS entry validate twice |
| T-03 | flags family passed by mistake | Medium | Internal APIs use bare integers | Separate bitflags types give compile-time isolation |
| T-04 | A missed dentry eviction causes wrong directory state or resource lifetime | Medium | A namespace update modifies only the weak index or only the dcache, or a dynamic filesystem updates out of band without invalidating the cache | The insert/remove/forget paths update both cache layers in pairs; `invalidate_cached_child` evicts positive/negative children for dynamic filesystems under the namespace exclusive lock, and behavior tests cover it |
| T-05 | A dentry operation is inconsistent with its owning superblock | High | A concrete filesystem adds or replaces the operation object one by one after the dentry is created | The nascent superblock installs the static counterpart of the default `s_d_op`, and the root/child inherit it at binding time and cannot replace it |
| T-06 | A path checks only the final inode, bypassing a non-searchable directory or wrongly accepting a final object that cannot be looked up | High | namei does not check execute/search on intermediate directories, or leaves the trailing `/` and `LOOKUP_DIRECTORY` to the open layer | Before each next-component lookup, `Path::permission(MAY_EXEC, cred)` is called; the generic `path_lookupat()` converts the trailing `/` before the final walk and consumes the directory constraint with `Dentry::can_lookup()` |
| T-07 | After a missing owner class, access wrongly falls back to group/other | High | DAC treats the three classes of permissions as an arbitrarily selectable set | `generic_permission` selects only one class, in the mutually exclusive order owner, group, other |
| T-08 | A namespace modification bypasses parent directory permissions | High | A backend callback is called directly or a VFS wrapper misses the check | The `Path` mutation API performs the parent directory `MAY_WRITE | MAY_EXEC` check while the final object is still protected by the namespace lock |
| T-09 | A sticky directory deletes another user's file | High | A name checked outside the lock is replaced before the callback | The validator of unlink/rmdir/rename enforces the sticky owner and mountpoint policy on the final victim under the lock |
| T-10 | A created object is fixed to root or the wrong group | High | The backend fills in the UID/GID itself | The create callback receives `&Cred` and uses `inode_init_owner()` |
| T-11 | One namei mixes credentials | High | Every component calls the current helper in reverse | The syscall captures an `Arc<Cred>` once, and the VFS passes the reference explicitly |
| T-12 | `ftruncate` is wrongly denied by the current pathname DAC or bypasses the write mode | Medium | An fd operation reuses pathname truncate | `VfsFile::truncate` first verifies `FMode::WRITE`, then goes through the opened truncate |
| T-13 | A non-owner directly modifies the inode owner, mode or explicit times | High | A syscall or caller enters the backend `setattr` directly | The `Path` metadata API uniformly enforces the Linux owner/group/write policy after the mount write check |
| T-14 | A pathname socket or other special inode is fixed to root | High | Dynamic mknod of a simple filesystem bypasses the owner helper | `SimpleDir` mknod uses `inode_init_owner()`, and only directory implementations that support persistent insertion allow open creation |
| T-25 | Unmounting one mount breaks other mounts of the same superblock | High | A bind root shares a dentry with its source path, yet the dcache is cleaned selectively by the mount's creation origin | A mount does not store a bind-origin marker; every `VfsMount` holds an active reference, and only when the count reaches zero is the superblock shutdown performed once |
| T-15 | A core mutation result pollutes the wrong live inode identity | High | A bridge writes the metadata of one inode into another `VfsInode` | The cached metadata refresh validates identity, node type, block geometry and `rdev` |
| T-16 | During the truncate window a private mmap faults again beyond the new EOF, or shrink-regrow exposes old cached data | High | i_size is published later than the cache truncate, backing prepare overwrites the old i_size early, or only one unmap is performed | The inode data lock serializes write/truncate; backing prepare does not modify i_size; `truncate_setsize()` performs i_size publish -> unmap -> cache truncate/EOF zero -> unmap |
| T-17 | Disk inode reclamation is triggered too early at unlink | High | Dentry removal is conflated with the last open-file reference | The `Arc` inode identity defers final teardown, and disk reclamation runs only in the superblock `evict_inode()` hook |
| T-18 | Concurrent rename creates a directory loop | High | A cross-directory rename does not serialize topology or does not check the ancestor relationship under the lock | The topology mutex, stable parent lock and ancestry check reject the operation |
| T-19 | create, link or symlink races with lookup, deletion or replacement | High | The final lookup, object validation and mutation hold locks separately | The final lookup, validator, participant lock and callback are in the same parent directory exclusive transaction; a create-like callback reuses the final negative dentry, and a create-only error is returned only if the object is still negative |
| T-26 | Device node authorization checked outside the lock causes a wrong error priority or a bypass | High | The syscall checks privilege first, or other `Path::mknod` callers do not check | Both `Filename::mknod_at` and `Path::mknod` enter the unique `Path::vfs_mknod` inside the locked negative-dentry callback; an existing target returns `AlreadyExists` first |
| T-27 | open, mkdir and mknodat produce different callback modes | High | Each entry prepares the mode/type itself, or calls the inode callback directly | open, mkdir, regular mknod and special mknod all run the same DAC, setgid, umask, allowed-permission and node-type policy through the corresponding `Path::vfs_*` capability |
| T-28 | unlink/rmdir deletes a same-name replacement object after the pathname's initial resolution | High | The syscall resolves the full target first, then performs a second lookup by parent/name | `Filename::unlink_at/rmdir_at` resolves only the parent; the Dentry resolves and operates on the final victim exactly once under the parent exclusive lock |
| T-29 | A pathname FIFO re-looks-up after authorization and opens a replacement inode | High | The syscall performs a second path resolution based on the first open error | The FIFO file operations complete the open on the same `Path` and `VfsFileBuilder` that namei already authorized, with no fallback lookup |
| T-30 | A pathname FIFO fd loses the identity of the original inode | Medium | A special open creates an anonymous-inode-backed file | The shared FIFO operations modify only the stream/private state of the original builder, keeping the original path, inode and filesystem |
| T-31 | The same pipe or FIFO inode publishes two pipe sessions at once | High | The last release and a new open modify the slot and the file count separately | get/create/increment and decrement/clear are all completed within the inode pipe-slot lock domain |
| T-32 | A non-blocking reader that never connected a writer immediately gets HUP | Medium | poll checks only `writers == 0` | The file stores a `w_counter` snapshot from open time, and HUP also checks that the generation has changed |
| T-33 | A poll waiting only for HUP/ERR is never woken by peer close | Medium | The wait queue is chosen by the user event mask | The reader/writer wait queue is registered by `f_mode`, and the event mask only filters readiness |
| T-20 | Reverse cross-directory rename deadlock | High | Two threads lock the parent directories in opposite orders | One topology mutex serializes topology mutation, and parent directories are locked in topology order |
| T-21 | dentry name and parent are inconsistent | Medium | parent/name are updated separately, or a reader observes an intermediate state | Both fields are replaced under the same location write lock |
| T-22 | A directory alias forms two child caches or bypasses topology serialization | High | The same directory inode establishes multiple live dentries | The inode alias table enforces a single alias per directory; lookup reuses the existing alias, and rename topology decides by parent dentry identity |
| T-23 | A filesystem callback replaces the creation object already checked by the VFS | High | The callback returns a dentry under another parent/name | A create-like callback may only instantiate the transaction's negative dentry; the lookup alternate result validates positive state, parent and name |
| T-24 | Concurrent slow lookups create duplicate dentries | High | The cache-miss check is separated from candidate publication, or a waiter observes an unfinished object early | The candidate is atomically added to the dcache before the callback; the owner keeps the parallel-lookup state and the lookup mutex, and waiters read the result only after lookup is done |
| T-29 | A detached mount can still return to the original namespace along the old parent | High | Unmount deletes only the parent child map but keeps the parent/mountpoint state in the mount | parent and mountpoint are merged into one mutex-protected `Option<Path>`; detach removes the topology under the mountpoint namespace lock and clears that location |
| T-30 | The mount support set drifts from `/proc/filesystems` | Medium | The syscall and procfs each maintain filesystem names or construction branches | Both read the same `FileSystemType` registry; registering a duplicate name fails |
| T-31 | An immutable symlink target has two separable copies of state | Medium | A simple-file closure and the inode cache each store the target | A cached-link inode is constructed directly at creation; the target lives only in the inode, and the node keeps only metadata such as the length |
| T-34 | The last mount release loses dirty pages, inode metadata or a journal checkpoint | High | After topology detach the superblock has no active lifetime, and the Weak registry cannot keep it alive | `VfsMount` acquires/releases a superblock active reference; the last reference performs writeback, dcache eviction and the final filesystem/device sync under the umount lock; teardown still completes after errors are recorded |
| T-35 | Recursively unmounting an overmount subtree leaves mounts unreachable from the registry | High | Only visible children are traversed, or the topology is modified while traversing | Collect the full covers chain of each child mountpoint, first validate all registry membership, then perform a topology commit with no failing branch |
| T-36 | A filesystem emits a malformed extent, causing range overflow or misinterpretation by the ABI layer | Medium | The backend returns zero length or a logical/physical end out of bounds | `FiemapExtentInfo::fill_next_extent()` centrally checks non-zero length and `checked_add` of both ranges; the ABI writer receives only validated fields |
| T-37 | Unprivileged caller forges a `security.*` xattr | High | VFS delegates authorization to an unimplemented LSM hook, and the backend accepts that namespace directly | KVFS requires effective `CAP_SYS_ADMIN` to set/remove `security.*` until concrete LSM hooks are connected; reads retain the delegation semantics, and the residual risk is the missing concrete LSM read policy |
| T-38 | `listxattr` builds intermediate copies for invisible names or attribute values, causing unnecessary memory amplification | Medium | The backend first returns an attribute vector that owns the values, and KVFS then filters `trusted.*` | `InodeOperations::list_xattrs` outputs through a borrowed name sink; `Path` filters `trusted.*` in the stream, and the caller receives only visible names |
| T-39 | The xattrs of an immutable or append-only inode can still be modified | Medium | A namespace special case authorizes directly before the generic inode state check, or the filesystem does not map the disk inode flags to KVFS | `check_xattr_permission()` checks `NodeFlags::IMMUTABLE/APPEND_ONLY` before all namespace branches, and set/remove uniformly return `EPERM`; a concrete bridge must map the backend flags when establishing the VFS inode identity |
| T-40 | Inode initialization or eviction races produce a second resident identity, or waking the whole cache causes a thundering herd | High | The slot is taken only after construction on a cache miss, a Weak upgrade failure rebuilds immediately, or all inodes share one wait queue | The cache publishes `New` first; concurrent initializers wait on that slot; the last reference drop publishes `Freeing` before calling the hook; a same-number lookup waits for the entry to be deleted and retries; each slot's queue wakes only waiters of the same number, and the backend receives no `EINVAL`-style race error |
| T-41 | Two mutable superblocks appear on the same block device, or a mount lands on an instance being torn down | High | The filesystem allocates its own state on every mount, or there is no state check between `get_tree` and active acquisition | KVFS builds a nascent reservation and an exclusive claim keyed by the canonical `(s_type, BlockDevice)`; the same identity waits/reuses, a RO/RW mismatch or a cross-type conflict returns `EBUSY`; `mount_new` retries after a dying/dead activation race |
| T-42 | A filesystem keeps using a stale mount data borrow after the transaction completes | High | The filesystem stores the `FsContext::data()` byte-slice reference directly into superblock state | The opaque data serves only as synchronous context input; the filesystem must parse and store owned/typed state, and the generic fill closure is `FnOnce` |
| T-43 | A resident inode exposes timestamp precision the disk format cannot store, or field coupling inconsistent with the disk | Medium | After the filesystem callback the VFS republishes the original request, masking the backend's range clamping, field-specific granularity or ignore behavior | `TimestampLimits` handles only superblock-level common capability; the callback reuses `MetadataUpdate` to return the fields actually applied, and KVFS publishes only the returned value; FAT date-granularity atime, the 2-second mtime/ctime coupling, and KVFS callback echo tests constrain this contract |
| T-44 | A command-style pseudo file merges multiple writes by offset, or splits one write_iter into multiple commands | High | The default scalar write algorithm handles a command file | The crate-private complete-command transport fully copies one request into a fallible buffer, ignores the offset and hands `VfsFile::f_cred` plus the complete buffer to the callback at once; `CommandFile` uses a 4096-byte bound, and opt-in `SeqFileInode` writable files compose it with sequential reads |
| T-45 | A dynamic directory backend re-resolves by name a victim the VFS has already locked | High | The `rmdir` callback receives only a string, and a replacement race makes the backend delete a different identity | `SimpleDirOps::rmdir` receives `LockedDentry`; the backend can validate `inode_ref()`, and `mkdir` also receives the parent inode, prepared mode and credential |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Invalid open arguments | Unknown bits or illegal combination | open fails | No state change | 3 | Return `InvalidInput` |
| F-02 | Invalid rename arguments | Mode conflict or unsupported by the helper | rename fails | The namespace remains unchanged | 2 | Validate before the operation |
| F-03 | Filesystem callback failure | Backend I/O or metadata error | The current operation fails | May degrade to an I/O error | 2 | Propagate through `VfsResult` |
| F-04 | Hashed dentry not reclaimed in time | There is no Linux shrinker/LRU at the current stage | dcache usage grows | Long-running systems may see increased memory pressure | 3 | Explicit eviction on unlink, rename, and forget; a global reclaim policy will be integrated later |
| F-05 | dentry does not inherit the default operations | The superblock construction or binding path fails to pass them on | Dynamic name/lookup policy is missing | Incorrect behavior of the specific pseudo filesystem | 3 | Superblock bind inherits them uniformly; specific filesystem tests cover root/per-file dentry behavior |
| F-06 | Permission check failure | The mode, owner, or group does not allow the request | The current VFS operation returns `PermissionDenied` | The namespace and inode state remain unchanged | 3 | Complete the generic checks before backend mutation and propagate the error |
| F-07 | Backend cannot store the Unix owner | The on-disk format has no UID/GID | getattr cannot fully reflect the creating identity | DAC semantics are limited by filesystem capabilities | 3 | Document the backend limitation explicitly; backends that support owners must persist the helper result |
| F-08 | Cache invalidation fails after backing prepare in split truncate | Allocation or mapping invalidation error | The current truncate returns failure | The backing inode may remain in an orphan/recovery state | 2 | Converged by the specific filesystem's persistent recovery protocol; silently executing finish is forbidden |
| F-09 | Duplicate filesystem type registration | Duplicate startup wiring or name conflict | Registration returns `ResourceBusy` | boot stops before userspace starts | 2 | boot registers each built-in type exactly once and treats failure as an initialization error |
| F-10 | FIEMAP writer capacity exhausted | The user output capacity is smaller than the number of mappings | Returns the extents already filled in, and traversal stops | The caller can retry with a larger capacity | 4 | `fill_next_extent()` expresses capacity with a boolean return value and does not treat normal truncation as a backend error |
| F-11 | The filesystem rejects mount data or an option | Binary/text representation mismatch, syntax error, or the type has not yet implemented the corresponding semantics | mount fails before superblock/topology publication | The user gets a clear error and does not mistakenly believe the option took effect | 3 | The generic VFS is only responsible for safe handoff; the filesystem returns an error for unknown and unimplemented options, and only explicitly modeled compatibility no-ops are acceptable |

Validation failures return a typed VFS error before the filesystem callback. When the backend fails, the dentry cache
location remains unchanged, because the cache commit is performed only after the callback succeeds; the key and cache
slot required by rename already exist before the callback, and the commit only swaps the location and replaces the slot in place, without performing
allocation that could fail. Rollback and logging of persistent operations remain the backend's responsibility.

File locks use the lock order inode context → blocked owner index; sleeping and waking both take place after
releasing these two lock levels. UnsafeCell and intrusive nodes exist only in the private `locks/node.rs`;
modification of wait relations and the global guard are not exposed to the algorithm layer. Installed ranges are owned exclusively by `LockList`,
and a borrowed list can only obtain value copies; once detached, it is modified exclusively by a non-cloneable `OwnedLock` and then handed back to the list.
The shared `Blocker` can neither read the range nor obtain the intrusive node; a `Waiter` cannot be converted into an install owner.
This makes range safety independent of arbitrary same-type inode guard arguments. The public `FileLock` contains no shared mutable state.
A detached installed lock checks for an empty list through get_mut only when the Arc exclusivity check succeeds, avoiding taking the global lock when there is no waiting;
the parent relation uses Weak; the inode algorithm still holds its mutex to complete the update, and deadlock tracking does not read installed ranges.
Signal cancellation unlinks directly and releases the successor; pending notification requests and registrations are handed off from the original node, and a retry is allowed only after dequeue,
waker invocation and destruction both take place outside the lock, and close cleanup needs no notification allocation. Close cleanup precedes owner identity invalidation.
Locks constrain only cooperating callers; file reads and writes without an advisory lock are not blocked by them.

## Known Limitations

- The pipe, FIFO, socket, and generic character device operation tables explicitly reject sync and return `EINVAL`.
  `O_PATH` sync returns `EBADF` before calling the backend, and backend sync errors are propagated unchanged.
  The generic default sync is still a compatibility successful no-op; a successful return from backends such as FAT and 9p does not constitute a persistence guarantee,
  which must be filled in by subsequent backend sync implementations and persistence verification.

- Effective process capabilities now cover file permissions, sticky directories,
  xattr, chown/chmod, and related checks; LSM, POSIX ACL, user-namespace ID
  mapping and idmapped mounts are not supported. KVFS stores and validates the
  `security.capability` attribute but does not interpret it: `execve(2)` decodes
  it through `kcred`. Ambient
  capabilities are not a separate filesystem input; they affect a check only
  after becoming effective. `trusted.*` access and `security.*` mutation require
  effective `CAP_SYS_ADMIN`, except `security.capability` mutation, which
  requires effective `CAP_SETFCAP` and a value that decodes as a supported
  revision; user xattr modification in a sticky directory
  requires directory ownership or effective `CAP_FOWNER`. Content writes strip
  set-UID always and set-GID when group-execute marks it as a privilege or the
  writer is outside the file's group, unless the writer holds effective
  `CAP_FSETID`, and drop `security.capability`; chown drops it before
  publishing the new owner or mode. The capability decision uses the writing
  task's credential supplied by the syscall entry, matching Linux's
  `capable()`, not the opener's credential. An immutable target is rejected
  before any revocation, and a write starting at or beyond the size limit
  leaves the decision and the revocation to the backend, so an `EFBIG` does not
  strip privileges. A per-inode privilege lock covers the revocation and the
  protected backend change; `setxattr`, `chown`, `chmod`, file-attribute
  changes, `truncate`, `fallocate`, native `copy_file_range` and exec metadata
  sampling share it. Revocation precedes the first content or size commit:
  a failed backend therefore leaves privileges removed, and a failed
  revocation leaves content and length unchanged. `chown` samples and
  authorizes inside the lock. Content-changing paths bump an inode content
  generation that keys the exec ELF-header cache.
  A write with nothing to revoke and no attribute store to consult returns
  before any filesystem metadata work, so a buffered write never waits for
  mount persistence. The `security.capability` presence is cached per inode
  (`FileCapabilityState`) together with a generation counter: one lookup answers
  it, absence is a constant-time check afterwards, and a cached absence is
  published only when no concurrent capability operation overtook the lookup
  and the removal's own snapshot still matches; a losing update degrades to
  `Unknown`, so a stale absence can never hide a newly installed attribute. Empty writes take the fast
  path without revoking, and native `copy_file_range` revokes on its output
  before the backend copies. The transfer boundary carries the writing task's
  credential, so `copy_file_range`, `splice` and `sendfile` use its
  `CAP_FSETID` and group membership.
- FAT and other backends cannot fully express Unix UID/GID owners.
- The current POSIX rename path does not support `RENAME_WHITEOUT`. The types of inode lookup and getattr are
  established, but no extra semantic bits are defined yet; current calls use empty flags.
- The superblock dentry cache does not yet implement a Linux-style LRU/shrinker, and currently relies on namespace deletion paths such as unlink, rename
  and on the last active mount shutdown to evict actively.
- KVFS provides AddressSpace view invalidation notifications, but each filesystem still needs to verify its own wiring with live mmap/truncate
  cases.
- Fast lookup is still mutex-based, with no RCU or rename sequence validation; slow lookups of the same name
  are merged through the owner/waiter protocol of the hashed candidate.
- Lock ordering of layered filesystems is not yet modeled.
- Mount topology synchronization and the superblock rename mutex are different mechanisms, but both follow the documented
  inode-then-topology nesting order.
- Filesystem types do not currently support runtime unloading; a registration entry and its function entry points must live for the whole kernel lifetime.
- `FsContext` can carry binary mountdata bytes, but is not responsible for parsing concrete representations or
  filesystem-specific remount parsing.

### Mount propagation limits

- There are no shared propagation groups, so every propagation request is behaviourally private. `MS_PRIVATE`/`MS_SLAVE`
  are accepted after validating the target is a registered mount in this namespace, and `MS_SHARED`/`MS_UNBINDABLE` are accepted
  as validated no-ops for the same reason: a shared mount here would propagate no events and an unbindable one has no peer to
  refuse, and systemd remounts the namespace root `MS_SHARED` after `pivot_root`, so rejecting it would abort the switch-root.
  An invalid target is still `EINVAL` in all four cases, and the mount tree is never silently flattened.
- Recursive bind (`MS_BIND|MS_REC`) is supported: `attach_bind_recursive` clones the source mount subtree and re-grafts each cloned
  child under its cloned parent. Only submounts at or below the bind source are cloned (sibling submounts elsewhere in the host mount
  are excluded), so every cloned child stays reachable under the new root and can be unmounted; the tree is not silently flattened.
- `MS_MOVE` (`move_mount_to`) is supported with Linux `do_move_mount` safeguards: a target at or below the moved subtree is rejected
  with `ELOOP` (no self-referential mount), the directory/non-directory match is validated before any detach so a failing move leaves
  the source attached, and the whole detach + re-graft runs under the namespace registry lock plus the touched inodes'
  namespace-exclusive locks (no split critical section).

### Validity of buffered read window hints

A batch reference only keeps the allocation alive and does not prove residency or initialization; under the page lock, invalidated and non-uptodate candidates are rejected,
falling back to ordinary lookup and initialization. The copy is still limited by the current EOF under the page lock, and a short copy stops immediately.

The synchronous readahead control object is borrowed only within the callback covered by the invalidate shared lock; complete data is published
sequentially through a cursor, insufficient length does not produce a valid cache page, and existing residents are not overwritten by backing data.
A page-fault read likewise checks EOF and completes publication under this lock, and truncate exclusive excludes late stale data.
The generic buffered user copy does not hold the invalidate lock and rechecks EOF after locking each page; `prepared_end` only suppresses repeated
readahead and does not authorize skipping the cache lookup. A readahead failure can be retried by an on-demand read; when an on-demand read or copy fails, the
completed prefix is returned preferentially, and an error is reported only when there is no prefix.

## Audit Checklist

- Whether new syscall flags are converted to their types at the ABI boundary.
- Whether internal code uses `contains`/`intersects` instead of re-parsing integers bitwise.
- Whether `.bits()` is used only for ABI output, atomic storage, or explicit low-level interfaces.
- Whether new flag combinations come with conflict validation and behavior tests.
- Whether dentry cache insert, rename, unlink, and forget keep the parent weak index and the superblock
  strong ownership in sync.
- Whether mount attach/detach validates current namespace membership, holds the mountpoint inode
  namespace lock, and clears the parent location after detach.
- Whether a hashed child can still take part in the directory non-empty check when there is no external `Dentry` reference.
- Whether the default dentry operations of a new filesystem are installed on the superblock, rather than allocated and patched in per dentry?
- Whether new pathname entry points explicitly accept and pass down the same `&Cred` at every level.
- Whether search permission on intermediate directories, permission on the final inode, and parent directory mutation permission are each checked at the correct stage.
- Whether a trailing `/` forces following of the final symlink, and whether every `LOOKUP_DIRECTORY` uniformly requires
  `Dentry::can_lookup()` rather than merely checking for a directory-like type.
- Whether newly created inodes use `inode_init_owner()`, and whether the backend persists UID/GID.
- Whether fd-based operations use the open file mode/`f_cred`, rather than re-running pathname authorization.
- Whether a pipe or FIFO session is owned only by the inode pipe slot, and whether fops remain shared and stateless.
- Whether pipe private data stores only `PipeObject`, and whether direction and the HUP snapshot come only from
  `f_mode` and `pipe_generation` respectively.
- Whether FIFO poll registration selects the wait queue by file mode, rather than by the user event mask.
- Whether inodes of anonymous objects (pipes, etc.) use `metadata_only_inode_operations`, so that
  `fchown`/`fchmod` owner/mode updates are published on the KVFS-resident inode instead of being rejected with `EOPNOTSUPP`.
- Whether pivot_root completes all fallible validation before any mutation (new_root is a registered mount root,
  put_old is reachable from new_root and not inside the current root filesystem, `put_old` is a directory),
  and, once all inode namespace locks (totally ordered by `namespace_lock_key`, deduplicated by Arc) are held,
  rechecks that the old root is still attached to its own parent mountpoint before changing the topology.
- Whether pathname FIFO time updates are performed through VFS policy only after a successful external I/O.
- Whether xattr raw flags are first converted to `XattrSetFlags`, whether set/remove check for a read-only mount and hold the
  inode data lock, whether `trusted.*` hides names and existence from unprivileged list/get, whether `security.*`
  mutation rejects unprivileged credentials, whether immutable/append-only rejects
  all mutations before the namespace branch, and whether the list backend borrows names only for the duration of the sink callback?
- Whether, after a backing mutation, the shared attribute backend has been updated in place; whether a
  filesystem that uses KVFS-owned attributes refreshes all affected live inodes, rather than only returning a new dentry.
- Whether truncate keeps the order of backing prepare, i_size publish,
  two rounds of AddressSpace invalidation, cache truncate, and backing finish through `set_len()` and `truncate_setsize()`.
- Whether the file-backed MM runtime stores only `VfsFile`, and accesses the unique
  inode `AddressSpace` through `VfsFile::mapping()`.
- Whether `release()` and the final `evict_inode()` remain two distinct lifecycle stages.
- Whether the filesystem constructs private state through the fallible inode initializer of the owning `SuperBlock`, and
  has no backend inode-number resident cache?
- Whether `New/Freeing` waits and retries inside KVFS, and whether eviction finish deletes only the exactly matching old entry?
- Whether each `VfsMount` acquires and releases the superblock active reference exactly once, whether a non-final mount avoids
  teardown, and whether the last reference performs shutdown only once.
- Whether block-backed fill initializes only a nascent superblock already reserved by KVFS; whether both failure and final
  shutdown release the claim, delete the identity, and wake waiters; whether an RO/RW mismatch returns `EBUSY`.
- Whether recursive detach includes the child overmount stack, and completes full registry validation before modifying the topology.
- Whether every namespace mutation acquires the parent directory exclusive lock.
- Whether a new filesystem registers only one canonical static descriptor, and whether boot, mount lookup, and
  `/proc/filesystems` add no parallel provider, object, or name branch.
- Whether filesystem mount data is parsed during context initialization according to that type's representation, whether unknown or unimplemented
  options fail explicitly, and whether mount state retains no borrow of `FsContext::data()`?
- Whether an immutable symlink target is stored only in the inode cached-link state, and whether the metadata size is kept in sync.
- Whether unlink/rmdir and rename replacement lock the victim inode.
- Whether cross-directory rename acquires the topology mutex first, then the inode lock.
- Whether directory locks are acquired before non-directory locks.
- Whether callbacks avoid re-entering the same set of VFS namespace objects.
- Whether cache commit is performed only after backend success.
- Whether the FIEMAP backend calls only `fill_next_extent()`, and stops immediately after it returns `false`.
- Whether the FIEMAP backend completes the requested `SYNC` writeback inside the inode data read lock held by the VFS, before starting the traversal.
- Whether a shared-file write fault completes `FileMappingOperations::page_mkwrite` prepare, folio dirty, and writable PTE publish inside the address-space invalidate shared lock and the folio lock, without taking the inode
  exclusive data lock.
- Whether the dynamic directory nlink callback runs after the node metadata lock is released, and overrides only the nlink
  of the returned copy; whether the default `None` and the callback error propagation semantics are preserved.

## Snapshot resource bounds

Snapshot generation may allocate and fail during open. The caller must bound
generated data; each read-capable open retains its own buffer until close.
Sequential iterators can preserve a typed filesystem error through
`SeqIterator::try_show`; the default implementation still maps `fmt::Error` to
`Io`. A rendering failure clears the partially generated record and resets the
iterator before returning. It must not expose a failed record on a later read.
Per-open sequential state remains serialized by its existing mutex.

Private snapshot operations own initialization and access to the private data;
no snapshot representation or construction API escapes the crate. Missing
state makes reads and `SEEK_END` return `InvalidData` rather than regenerating
content. `SEEK_SET` and `SEEK_CUR` need no snapshot and do not allocate.
Read offsets are checked against the buffer length before conversion and
slicing. Seek bases use checked signed conversion and addition; invalid seeks
do not change the file position. `SEEK_END` reads the descriptor's own buffer
length, so another open cannot change its EOF by updating inode metadata.

The helper imposes no aggregate memory or generation-work quota. Independent
readable opens each retain their own allocation, so a per-buffer bound does not
bound total memory across processes. Filesystem integrations must account for
this cost in access and resource policy.

### Per-open file readahead policy

`FileRaState` is a policy hint; it grants neither cache residency nor EOF authority. The policy lock is not held across I/O or user copies;
when a marker is hit, the lock order is folio → policy → index (bounded history lookups only); the policy holder never waits on a folio.
The budget check, marker consumption, and window decision are completed within the same policy critical section; only after the page lock and the policy lock are released
is invalidate shared acquired and EOF resampled, and then the subsequent window is published.
Concurrent pread can change the history of the same open file description, but cannot bypass initialization, identity revalidation, or truncate protection.
Independent opens do not share policy state; dup/fork references to the same open file description share state.

## Pinned file-page mappings

- `with_pinned_file_page` holds the invalidate read lock through EOF validation
  and publication, and rejects page starts at/beyond EOF or overflowing indices
  before materialization or admission. A callback error keeps an initialized
  in-range cache page resident and charged for reuse. Its callback must not wait for this mapping's exclusive
  invalidation lock.
- Read-only publication acquires an independent managed PTE hold and returns
  an optional content token. No access permissions install no PTE/token. Content
  tokens preserve exact source identity for COW; releasing them does not wait
  for TLB retirement, which retains the PTE's own physical reference.
- The stable `Folio` owns its page identity, allocation and content lock.
  Ordinary access uses `FolioGuard`; initialization consumes `FolioRead`, which
  retains the allocation and lock until completion; replacing
  an index cannot replace a published owner or its lock.
- Copies deliberately remain available after detachment and are serialized
  with writers of the original object. They initialize all destination bytes,
  but are neither current-EOF validation nor an immutable snapshot.
- Pins and mapping tokens retain stable content independently of cache quota.
  Each managed PTE owns a separate physical reference through retirement.

## Cache residency lifecycle

The mapping owns one filesystem-supplied `CacheResidency` provider. Only a
winning cache identity is admitted and charged; cache removal and final index
destruction return each charge once. Surviving Folio, pin, read or PTE references
retain physical memory without retaining tmpfs `size=` quota. Physical lifetime
is enforced by PageRef and PTE retirement, not the cache capacity callback.
Admission/removal run in task context under the cache index lock; providers must
not sleep or reenter the mapping. Memfs admission takes quota-limit locks after
folio/index locks, while removal is an atomic decrement. Remount/statfs never
acquire cache locks.

Sparse-hole zero filling applies only to absent cache identities. A retained
non-uptodate reservation retries initialization under the invalidate read lock;
failed reads do not expose partial contents or silently replace them with zeros.
An error after a copied prefix returns that prefix, while a first-page error is
propagated.

Filesystem sparse reads must not consume remaining quota by prefetching absent
pages. Truncate invalidates retained read-batch entries before removing cache
identity; references cannot serve stale cache bytes. They keep physical pages
alive independently of the quota returned at identity removal.

Filesystem-driven resident traversal uses one invalidate read guard per bounded
31-page snapshot. Page callbacks may only consume data, cannot reenter the
mapping, and must check EOF while guarded. Missing snapshot slots never allocate
storage. Partial data before a later callback/initialization error remains visible
to the filesystem caller, which decides its normal short-read completion.

Read submission may return before initialization completes. The filesystem must
retain its `FolioRead` and every backing resource until all I/O releases the
page, then complete or drop the token. A waiting caller can hold inode data or
invalidate locks; completion must not acquire those locks or the mapping index.
Only completed whole-page initialization becomes uptodate. Truncate waits on
the transferred folio lock before modifying/removing the page; unconsumed
readahead reservations are still removed before unlocking.

## Unrecoverable filesystem write failure

`SuperBlock::mark_read_only()` only performs an atomic set on the existing flags; it does not create a separate writable state,
nor does it overwrite other flags that are set at the same time. This interface does not guarantee persistence of dirty data or transaction recovery; the caller must first handle
the unrecoverable internal error, and publish the read-only flag before releasing the mutation protection. The VFS write entry points reject subsequent writes accordingly.
remount may modify ordinary mount flags; the filesystem must use its existing failure state to refuse re-enabling writes, and keep checking that state at the internal
write boundaries, so that protection during error recovery does not rely solely on the `RDONLY` bit.

The submit-style `submit_cached_ranges` neither waits for I/O automatically nor stores deferred errors. The filesystem must drain the submitted batches before
synchronous writepages returns, including on the scan/admission failure paths; it must not allow physical mappings that are not protected by some other mechanism to remain in use after the invalidate
protection ends. Consuming and dropping a PageCache completion
require task context, and may only be done after the corresponding I/O has finished.

### Asynchronous writeback errors and lifetime

A submission-based filesystem may retain completion after `None` returns, but
cannot rely on the caller's invalidate guard to protect physical mappings. Before
truncate/release, it must exclude new same-inode submissions and drain accepted
requests. In task context, real preparation/submission/completion errors enter
both mapping and superblock channels before completion ownership is released or
folio writeback ends. Ordinary queue throttling must not create an error.

Per-file fsync/syncfs cursors observe the channels independently. Sampling and
non-consuming checks preserve unseen failures for later opens; advancing one
cursor leaves older observers pending. Observing the mapping error does not
consume the filesystem error. A successful retry cannot erase either channel.
Temporary writeback checks use the current generation rather than the open-time
sample that inherits unseen errors. The boundary is taken before waiting for
submission or existing I/O, so failures recorded during those waits still count.
This neither marks errors seen nor adds a persistent observer. Repeated syncfs
calls report history through their own superblock cursor; they must not depend
on a file fsync consuming mapping history to stop reporting an old failure.
These cursors do not replace waiting or prove device durability. Every channel
operation may sleep and requires task context without spinlocks or interrupt/
preemption exclusion. Superblock sync attempts remaining inode data, inode
metadata, and filesystem flush after failure, preserving the first error and the
backend's abort checks. Shutdown must drain every inode's accepted I/O.

## Automatic write timestamps

Removing timestamp publication from buffered write or shared writable faults violates
the observable inode metadata contract. Deferred filesystem persistence must preserve
resident visibility and retain pending metadata until the corresponding sync or
eviction boundary; it must not acquire filesystem locks while holding a folio lock.
The automatic `update_time` callback owns resident publication and must finish
fallible checks before publishing values. Its default publishes only timestamps
confirmed by `setattr`; explicit setattr retains the separate returned-field contract.
Mount read-only and atime/relatime checks remain at the Path boundary.

The filesystem-owned buffered-write preparation callback replaces both extra
admission checks and automatic timestamp dispatch. Its caller must preserve
mount/journal write rejection and inode timestamp limits, and publish mtime/ctime
before exposing changed data. Preparation also checks policy for empty requests
and applies generic size limits in the filesystem's required order. Empty or
rejected requests must not publish timestamps. The callback may only reduce the
accepted byte count;
failure prevents page-cache copying and I/O-position advancement. Shared writable
faults retain the ordinary `file_update_time` path.

## Quota state boundary

The superblock's `quota_info()` guard serializes its single generic quota
owner and its canonical backing-file inode references. File selection comes
from loaded quota state rather than from an independent provider registry.
It may span blocking quota-file I/O only in task context without
spin/IRQ-disable/preemption-disable guards. Providers must use direct quota
I/O without recursive charging or caller-held lock reentry. Administrative
authorization, ID mapping, backing-file protection and transaction recovery
remain integration obligations. Ext4 enables internal user/group/project usage accounting
from its superblock quota inode fields.
Inode snapshots pin live identities and must be released outside locks needed
by eviction. Waiting for evictions must likewise happen outside those locks.

## Notification-interest lifetime

Static `DirMapping` entries retain their inode independently of dentry-cache
residency. Fresh lookups share its subscriptions and metadata; dynamic child
providers must supply their own stable object identities. Mapping teardown
releases these references without an inode-to-mapping ownership cycle.

The notification registry pins watched inode objects until after routing removal.
It publishes their aggregate event masks with release/acquire ordering and
serializes recomputation with subscription changes. Hints never grant access or
replace per-watch validation. Final inode references are released outside the
registry lock because eviction can sleep and enter filesystem code.

Dentry linkage, parent and name share the location lock. Successful unlink and
rename replacement mark only the removed dentry; other hard links and renamed
sources stay linked. A snapshot owns its parent reference and name, and releases
the location guard before any notification callback. Cache membership alone is
not evidence that an entry has been unlinked.

Block files retain their opened device claim, preventing `rdev` reuse from
redirecting I/O or release. Superblock open/close runs outside registry locks;
failed open skips fill, and failed fill/final shutdown closes once before
unregistering. Dead superblock references retain identity without a live holder.

## Bounded private-fault readahead

Private-fault readahead holds the invalidate lock for reading across EOF
selection and submission, so truncate cannot change the bound before stale pages
are inserted. The page-cache index lock is not held across backend I/O. The
16-page bound limits one call, not global cache residency; sparse access may
read adjacent pages and later concurrent inserts still win over prefetched data.
Only returned I/O/allocation errors permit demand fallback; allocator aborts are
not made recoverable.

## Final filesystem resource release

Final `put_super` executes after inode/dcache writeback under umount exclusion.
Callbacks may sleep and must release retained inode references outside their
filesystem locks. Live-inode visitation pins a snapshot and drops the registry
lock before callbacks. Reconfigure callbacks use their context's sync entry to
avoid recursively acquiring the superblock umount read/write lock.

Read-only remount holds exclusive umount protection against ordinary VFS
mutations and writable-open admission. Existing writable files and zero-link
live inodes cause `EBUSY` before filesystem state changes. The live snapshot
pins identities through policy publication, while existing cache transition
queues drain evictions that had already started. No new eviction takes umount
protection: final shutdown may itself drop the last inode reference while
holding that lock. The VFS cache binds and registers initialized identities
before publication; inode constructors must not rely on write-dependent
lifecycle initialization outside mutation or writable-open admission.

FIFO timestamp mutations share remount write admission even though FIFO
file descriptions carry no inode writer count. The guard spans the read-only
check and timestamp callback; read-only policy prevents metadata mutation while
allowing the special file's already-completed data operation. Regular writable
files reuse their retained writer count instead of taking a nested read guard.

## Quota administration admission

Quota callbacks execute only while the superblock remains available and the
umount lock excludes shutdown/reconfiguration. Unsupported filesystems return
ENOSYS; unsupported quota classes return EINVAL before command authorization.
Lifecycle commands take exclusive admission, without introducing an independent
quota-control lock. Callbacks must retain mount-before-quota lock order and
release retained inode references outside filesystem mutation guards.

Mutable attribute-operation flags are read from resident filesystem state.
Immutable files reject ordinary content and metadata mutation, and namespace
removal/replacement checks victim policy. A buffered write rechecks protection
under file-data exclusion, covering descriptors opened before protection was
published. Quota metadata I/O is an explicit filesystem path and bypasses these
ordinary-file gates.

Live cache invalidation must not use final eviction: success requires completed
PTE removal and TLB invalidation before releasing any folio. Deferred-only or
missing notifiers cannot authorize cache destruction. A failed revocation or
writeback leaves cache ownership intact; earlier PTE revocation may remain.
The invalidation guard is released before retrying a busy MM. Callers must not
hold an MM lock or writeback-completion locks across this operation.

`AddressSpace::invalidate_range()` applies the same rule to a byte range: a
busy target MM reports `Ok(false)` and no folio is released, a covering view
without a notifier is rejected with `OperationNotSupported`, and only fully
revoked views allow clean-folio discard. A dirty folio reported as
`ResourceBusy` may follow clean folios that were already discarded, so a caller
must treat the range as not fully invalidated and must not assume the cache
still holds every folio.

Buffered writes dirty cache folios only while holding the folio lock, and the
generic write path holds the inode data lock that every invalidation and
set-length entry also takes, so an invalidation cannot report completion while
a concurrent writer can still make the range dirty again. A registered
view never spans a partial tail page: that page is anonymous and must survive a
file invalidation, so rounding the view to the page boundary keeps file
invalidation from zapping user data in an executable's BSS tail. Executable text
mapped directly from the cache is registered like any other private file view, so
a file overwrite or truncate cannot leave those PTEs pointing at stale pages.

A writable descriptor alone does not authorize later shared mapping writes.
Write faults recheck the current inode policy under data exclusion. A fault's
superblock admission and data guard are acquired without waiting under the MM
lock; transient contention becomes a retry, while immutable policy rejects the
write. Existing writable PTEs are revoked before quota activation flushes and
repurposes ordinary file contents.

A nonblocking shared write-fault denial due to contention is not an invalid
user address. Its sleepable wait phase runs outside the MM mutex on existing
lifecycle/data lock queues. Acquiring and releasing those resources does not
replace current VMA, mount or inode policy checks on the subsequent fault.


### Destination faults during buffered reads

A destination shared mapping can refer to the source inode itself. Its write
fault can wait for inode data or superblock admission locks. Buffered reads
therefore use nofault copies under the source folio lock and run destination
fault preparation only after releasing it. This prevents a folio-to-inode lock
inversion with buffered writers. A retry revalidates residency and EOF; neither
the earlier page reference nor a successful prefault authorizes stale data or
unchecked access after a concurrent truncate, unmap or permission change.

An address-space view guard can extend its existing registration under MM
serialization. The registry mutex protects the VMA/object spans, preserving the
registration identity and invalidation notifier. Coordinates are validated before
publication; invalidation snapshots do not hold this mutex while entering MM.

## Data completion and persistence boundaries

Raw writepages return is not a persistence acknowledgement. Initial zero budget
and empty range are rejected as data work before filesystem admission; explicit
file synchronization still checks capability and runs filesystem barriers.
`write_and_wait_range` validates range and sleepability before observing errors
or cache indexes, captures a finite resident bound including old clean WRITEBACK,
and waits that same bound even after submission failure. Index/invalidate locks
are absent from the completion wait. New completion failures are recorded before
WRITEBACK ends and checked without consuming another open file's observer.

Missing filesystem fsync support is EINVAL, rather than unearned success. Volatile
backends explicitly succeed; block-device sync always issues its actual flush.
The file wrapper validates backwards ranges before backend dispatch or advancing
its cursor; an empty data range still dispatches and observes errors. Generic data
completion does not replace filesystem drain-before-block-release protection,
metadata settlement or device barriers. A captured target bounds work; it does
not certify writable-PTE dirty detection or eliminate the separate G requirement.

Live cache invalidation holds exclusive invalidate admission while submitting
and waiting, without reacquiring the shared entry guard. Submission success alone
cannot authorize cache destruction: completion errors are observed before any
dirty retry data is discarded. Submission failure keeps precedence while earlier
owned I/O is still drained and cache ownership remains intact.

## Buffered acceptance/writeback handoff

A single-folio write keeps the copied folio locked through write_end acceptance
and i_size publication. A batched run accepts each slot while its own folio is
locked and releases the run's unaccepted tail afterwards, unlocked, in one
tail-release hook whose return type cannot change the published count; it
publishes no size and releases
only the reservation units that run newly created, never reservations owned by
earlier writes. Write-end implementations cannot reacquire a folio or wait for
its I/O. Cancellation and rollback release those guards before lifecycle waits,
while retaining writer data serialization. Writeback samples live EOF only
after taking the same folio lock. Attempted work and filesystem declines share
one pagecache-owned control; completion never retains it. This does not replace
shared-PTE dirty rearming/invalidation.

Buffered completion requests describe only the prepared range and copied count;
the range's exclusive end is validated before admission, and the copy count is
bounded by its length. The size-publication helper compares the accepted end
against the owning inode's current size while the inode data lock excludes
competing writes and truncate. Hooks must accept at most the copied count and
publish a nonempty accepted prefix before returning success. Cancellation
preserves the file length. Writeback continues to sample live EOF under its own
folio-lock protocol.

## Open validation bounds

The optional open hooks are checked once at the common `SimpleFile`/`SimpleDir`
open boundary, before private state is installed. They do not replace VFS
permission checks and are not evaluated through `link_count` or other metadata
observations. A failed validation leaves no open file or directory state.

The writable `SnapshotCommand` backend and its per-open generation state were
removed after procfs IRQ moved to `SeqFileInode` command transport. Its
`validate_write` hook is gone with it. Command-size limits, complete copying,
callback-once behavior and iterator rollback now belong to the retained
`SeqCommandOptions` path; ordinary `CommandFile` and read-only snapshots keep
their existing behavior.
