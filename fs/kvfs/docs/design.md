# kvfs - Design Document

## Unit-test ownership boundaries

Local polling futures use stack pinning with lexical destruction boundaries;
cancellation assertions run after the future and its registrations are dropped.
Block-device fixtures retain narrowly exempted standard Box owners because
`Gendisk::alloc` still requires them. The UT lint does not change that driver
ownership contract.

## Writeback test fixture ownership

The guest writeback fixtures in `ut-tests/address_space/writeback.rs` prepare
their operations with `KBox::new` before publishing an inode or starting worker
tasks. These setup sites run in sleepable task context without holding mapping,
inode or fixture locks, so they use the default KERNEL allocation policy.
`deferred_mapping` returns allocation errors to its test caller before inode
construction. Tests require successful fixture preparation before proceeding.

`KBox::leak` preserves the existing static operations-reference contract. These
per-test operations intentionally survive for the guest lifetime, as their Box
predecessors did; this change does not introduce fixture reclamation or alter
the production writeback protocol.

## Superblock private owners

`SuperBlock::{new_with_flags_and_private,try_new_with_flags_and_private,
initialize_with_private}` take a prepared `KBox<T>` and erase its type without
allocating or moving its pointee. Policy selection and allocation failure now
belong to the filesystem caller; root-initializer errors retain their existing
error type. `SimpleFs` constructors return `VfsResult`, allocate the owner before
calling the root factory, and use KERNEL by default. The explicit allocation-flags
variant supports singleton builders that must not enter synchronous reclaim.

KERNEL callers require sleepable task context, IRQs/preemption enabled, no
non-sleepable RCU section, and no locks needed by reclaim, I/O or filesystem
callbacks. NOWAIT changes only owner storage allocation, not the root callback
or nested container allocations. In particular it does not make mounting IRQ-safe.

| Private-owner preparation path | Policy | Context decision |
| --- | --- | --- |
| SimpleFs default, procfs, cgroup2fs, bpffs | KERNEL | Before the root factory; mount namespace and hierarchy locks are not held |
| devfs and sysfs singleton | NOWAIT | Inside Once initialization; avoid reclaim re-entering the initializing singleton |
| MemoryFs, ramfs, tmpfs mounts, private memfd | KERNEL | Reclaim-safe task context, before root/namespace/fd publication |
| Shared kernel shmem | NOWAIT | KERNEL_SHM_FS mutex remains held to preserve singleton identity |
| ext4 and FAT fill_super | NOWAIT | Canonical device identity is reserved but nascent; avoid new reclaim dependencies on mount completion |
| 9P mount session | KERNEL | Negotiation and metadata transport calls have returned; no transport guard is retained |

These choices cover the new private owner only. NOWAIT does not make input
construction or destruction non-sleeping, and does not imply reserve access or
guaranteed success. NOFS/NOIO still allow direct reclaim; neither is a general
solution for initialization re-entry. Preparing outside an existing singleton
gate would require a separate identity/publication redesign, not a mechanical
owner conversion.

## Parallel lookup completion

Parallel lookup candidates are not completed cache hits. `lookup_cache_entry`
checks `PAR_LOOKUP` while holding the parent's children mutex, so candidate
replacement or removal cannot race with that classification. In-progress
candidates remain hashed for `cache_lookup_candidate` to join; waiters release
the children mutex before waiting on the candidate's lookup mutex and then
retry. A detached negative candidate must never escape as a completed miss
after its owner clears `PAR_LOOKUP`. Completed negative entries in cacheable
directories remain valid cache hits.

## Static simple-directory entries

`DirMapping` retains one inode for each statically declared directory, simple
file, sequence file or supplied `Arc<VfsInode>`. Lookup materializes a dentry
around that inode; disabling or invalidating the dentry cache does not change
the object's identity. Inode-owned subscriptions and state therefore survive
repeated lookups. The mapping releases its inode references on teardown, and
inode aliases and superblock references remain weak to avoid ownership cycles.
Dynamic `add_child` callbacks retain responsibility for their object lifetimes.
File operations still generate or snapshot contents independently on each open.
Symlink entries are cached by the same mapping. kvfs provides
`SimpleFsNode::new_symlink_inode`; devfs consumes it for fixed links such as
`/dev/ptmx -> pts/ptmx`, so the call direction is devfs -> kvfs. The target is
stored in the inode and returned by `read_link` without being re-resolved.

File creation and hard links in simple directories are opt-in. The default
`SimpleDirOps::create_file` and `SimpleDirOps::install_link` reject both
operations, matching pseudo filesystems such as procfs and sysfs whose
directories must stay immutable from userspace. Filesystems that mirror a
writable backing store — ramfs-backed devtmpfs directories — override the two
hooks; `SimpleDirInodeOperations` then routes `vfs_create` and `vfs_link`
through them and instantiates the negative dentry like every other
create-like callback. Link accounting is symmetric: `install_link` increments
the target inode's link count and `DirMapping::unlink` decrements it, so
`stat` nlink tracks the alias count for hook-backed filesystems.

## Same-superblock mount reuse

Mounting a filesystem over the root of its own superblock with no requested
superblock flags (or an empty set) returns the existing `Mount` when its
per-mount flags already contain the request. The mount is not stacked a second
time, so child mounts such as `/dev/shm` and `/dev/pts` remain visible. The
containment test is intentionally one-way: a request that would loosen a flag
is not treated as a remount; callers must use `MS_REMOUNT` to change flags.

## Fallible directory iterators

SimpleDirOps::child_names returns a fallibly allocated KBox iterator. DirMapping
releases its entries lock after collecting the name snapshot; ChainedDirOps owns
both child iterators and releases the first if the second fails. The outer owner
also propagates NoMemory. All use the default KERNEL policy. The getdents path
obtains an owned file handle; VfsFile::iterate_dir samples/updates position with
separate lock acquisitions and holds no position lock over iterate_shared.
SimpleDir::read_dir_at constructs the child iterator before publishing even dot
entries. These paths require sleepable task context, enabled IRQs/preemption,
and no non-sleepable RCU section or lock required by reclaim/IO/FS callbacks.
There is no current held-lock reason to restrict these allocations to NOWAIT or
NOFS; future callers must preserve this contract.

A dynamic child may disappear between its name snapshot and lookup. Directory
reads skip only `NotFound` for that child and retain the later entries' cookies.
Snapshot-construction errors and other lookup failures still propagate.

## Fallible mount-transaction private state

`FsContext` owns both private-state slots through `KBox<dyn Any + Send + Sync>`.
The setters return `VfsResult<()>`; allocation succeeds before replacement,
so failure drops the input but preserves the installed state. tmpfs/ext4
initializers propagate failures through the existing mount/remount Result.
These allocations use KERNEL policy in sleepable task context, before mount
publication or superblock reconfiguration locking. The process filesystem
snapshot lock is released before context initialization. Other callers must
also avoid locks required by reclaim IO/FS callbacks. State destruction has
its own context requirements and does not inherit allocation flags.
Boot mounts run from kruntime's preemptible late-init kernel thread after
scheduler initialization, not from the IRQ/preemption-disabled early boot path.
Persistent superblock-private state remains a separate, unmigrated owner.

## Fallible file-lock context

Inodes own advisory-lock contexts through `Once<KBox<FileLockContext>>`.
Initialization uses `KBox::new_nowait` because Once waiters spin, even
though the lock operation itself runs in task context. Failed initialization
returns `NoMemory` through both `FileOperations::lock` and `flock`, publishes
no context, and permits a later retry. The constructor returns `KResult`, so
VFS forwards its error directly. A ready context is reused without
allocation. Queries and unlocking an inode without a context remain allocation
free. Lock-record allocations are unchanged by this migration.

The inode guest regression injects a local initialization failure and verifies
absence, successful retry, and reuse without invoking another allocator.

## Test Layout

Guest tests live in crate-root `ut-tests/`; `unittest::path_mod_test!`
mounts them at their original private parents under `cfg(unittest)`.
There are 25 mounts and 28 modules containing 250 guest cases. The
`address_space::tests` mount retains its `read`, `truncate` and `writeback`
children in `ut-tests/address_space/`, including shared fixtures and the
`observe_readahead_run` re-export used by the production-side test hook.
The other files mirror their source names, with nested names flattened:
`node_dentry.rs`, `node_inode.rs`, `node_device.rs`, and
`node_device_registration.rs`. The dentry module remains `tests_dentry`;
other mounted guest modules remain `tests`.

All guest cases retain their original assertions and parent access, without
additional architecture/feature gates. Seventeen cases retain `serial`
(seven superblock, nine device, one lock); none carries `user`.
Production-owned `cfg(unittest)` hooks remain in place.
The separate `path::test` host module is mounted from `tests/host/path.rs`
under its original `cfg(test)`, with its one `#[test]` case unchanged.
It is not a guest case or a standalone Cargo integration-test target.
The guest mount in `src/path.rs` also has an outer `cfg(unittest)` so a
standalone `rustc --test` wrapper can import this alloc/core-only source
without resolving the kernel `unittest` crate.
Guest validation uses the configured kernel unittest workflow.

## Named character-device registration

`node/device/registration.rs` adds discoverable identities to the existing
cdev registry. `cdev_add_named` installs dispatch and name/class metadata in
one transaction; rejection publishes nothing. Duplicate live class/name identities are also rejected. The registry owns names and
numbers. Snapshot handles retain the exact entry and its retirement bit, so
reusing a number cannot revive an old identity. Subscribers and snapshots
are captured under the registry mutex; add/delete invoke callbacks only after
unlocking. devfs constructs an inode from the returned registration, procfs
reads the current list, and sysfs subscribes and gates attributes on availability.
Existing unnamed cdev_add callers and hardware DeviceNode consumers are unchanged.

The global device-access policy slot is protected by
`RwLock<Option<Arc<dyn DeviceAccessPolicy>>>`. The write path is
`register_device_access_policy` (a second registration while one exists is
rejected, and passing `None` clears the slot and drops the registered policy,
returning device access to the unrestricted state); the read path is
`check_device_access` at every open/mknod enforcement point — the `Arc` is
cloned and the read lock released before the policy callback runs, so
callbacks always execute outside the lock.


## Role

`Dentry::notification_context` captures linkage and an interested parent inode
under one location read lock. It copies the name only when the parent subscribes
to the requested event bits. Bind roots retain their actual dentry parent; no
synthetic path across a mount root is constructed. Callbacks run after the lock
is released. Unlink and replacement mark the removed dentry's location as
unlinked without discarding its former parent/name; cache eviction does not.

Each inode caches an atomic notification event mask. The notification registry
serializes all aggregate updates, including subscription replacement and removal.
The cache filters work; the notifier still validates each watch at delivery.
Namespace event records retain inode references captured under the operation's
namespace locks, keeping routing independent of mount IDs and name reuse.

`kvfs` provides a Linux-style VFS object model, including superblock, mount, dentry, inode,
open file, path resolution, and generic filesystem helpers. Concrete filesystems plug in through inode/file operation
traits, and the POSIX layer converts syscall ABI arguments into VFS semantic objects. `kvfs` also owns
namespace validation, lock ordering, dcache identity, typed operation flags, and
the page-cache attachment on `VfsInode`.

The permission model stays at the same layer as Linux: `kcred` defines `Cred`, the syscall layer takes a single
credential snapshot from the current task, and `kvfs` receives an explicit `&Cred` and performs path traversal and generic DAC. `kvfs` does not
depend on `kprocess`, so it can be reused with an explicit identity by boot code, in-kernel pseudo filesystems, and tests.

## Character-device dispatch

The character-device registry and multiplexers that select a backend at open time, such as PTMX, share
`open_character_device`. The private `ChrdevFileOperations` uniformly forwards capabilities, I/O,
poll, and open/release; filesystem nodes no longer duplicate this adaptation layer.
Installing a replacement operation table does not automatically call open again; the unified entry point is responsible for calling the selected backend once.
After open succeeds, the VFS computes capabilities from the final operation table, and calls the final backend's release when the last reference is dropped.
The registry lookup lock is not held across backend callbacks; registration and unregistration of device numbers remain owned by the caller.

### Device-access policy (reverse injection)

kvfs defines the `DeviceAccessPolicy` trait and the registration entry
`register_device_access_policy`, and invokes the policy at three enforcement
points: character-device `open` (`open_character_device`) and block-device
open (both map the open mode onto READ/WRITE bits and OR in the inode's
TYPE_CHAR/TYPE_BLOCK), and device-node `mknod` (`MKNOD | TYPE_*`). The
implementor is `bpffs`, which owns the controller state (the cgroup-v2 device
controller) and injects it during `init_bpf_fs`. The dependency direction is
inverted: kvfs does not depend on bpf/cgroup, and policy callbacks run after
the registry read lock is released. With no policy registered, device access
is unrestricted; a denial answers `VfsError::OperationNotPermitted` (`EPERM`).

## Block-device file size and range operations

Block devices have two deliberately separate size views, consistent with Linux: the device node's
`getattr`/`fstat` metadata stays at zero size (as with Linux device nodes), while
`VfsFile::size()` - the basis for `lseek(SEEK_END)` - reads
`BlockDevice::size()` live, corresponding to Linux's `bd_inode`. Capacity changes immediately with operations such as loop
attach/detach and is not cached as a snapshot; the `SEEK_END` basis for record locks is the node
size, and an append write stays at the caller's position (`blkdev_write_iter` does no repositioning).

The validity of `fallocate` modes (unknown flags, mutual exclusion of `PUNCH_HOLE` and `ZERO_RANGE`,
`PUNCH_HOLE` requiring `KEEP_SIZE`, exclusivity of COLLAPSE/INSERT/UNSHARE) is validated uniformly by
the public entry point following `vfs_fallocate`; the block device is then handled by
`DefaultBlkdevFileOperations` with device semantics, not the byte-by-byte emulation used for regular files:
capacity (out of range without KEEP_SIZE gives `EINVAL`) and logical-block alignment (`EINVAL`) are validated before any
media write, and once they pass, only `ZERO_RANGE` (with or without `KEEP_SIZE`) writes zeros over the whole aligned range.
`PUNCH_HOLE` carries deallocation semantics: Linux calls it with `BLKDEV_ZERO_NOFALLBACK`, which succeeds when the device
has write-zeroes/discard offload and otherwise returns `EOPNOTSUPP`. The block-device contract
`BlockDeviceOperations` does not yet define these two capability bits, so this kernel deliberately narrows the behavior to returning `EOPNOTSUPP` for all
backends (reporting the missing capability instead of passing off software zero-writes as deallocation). Without this step, upper-layer semantics that depend on the file size (such as
the zeroing request in `ext2fs_zero_blocks2`) would treat the whole device as empty and silently skip the actual work.

Character devices are uniformly registered as a non-empty minor range within a single major, and a single device uses length 1.
The registry looks up the covering range in a BTreeMap ordered by `(major, first_minor)`; duplicate, containing,
or partially overlapping ranges all return an error, and an existing driver is never implicitly replaced. Unregistration must match both the full start and the length.

An inode holds only a weak reference to one registration object; that object owns the operation table and a one-way retirement flag. Unregistration marks it retired before releasing the
registry lock, and a new registration produces a new object, so even if the same operation table is reused, the old
cache is not revived. The next open ignores the retired cache and looks up the table again, with no need to keep or walk a reverse inode list.
An already selected operation table and already open files still hold backend references; admission after the device goes offline is the backend's responsibility.
Registration, unregistration, and open use a sleepable context; backend callbacks and final destruction all run outside the registry lock.
The VFS does not determine whether a path comes from devpts, and is not responsible for PTY instance lifetime.

The final magic link of an ordinary open and path lookup use the same object jump, and the readlink
display string is not re-parsed as a path. Therefore procfd can retain the identity of an unlinked inode, and
will not open a different object because a path of the same name was recreated. The jump is still subject to the follow policy, the link count, and
the target's open-permission checks; ordinary textual symbolic links continue to be resolved by their target string.

## File synchronization contract

File operations explicitly wired into `poll_file_io` try the I/O first; a non-blocking call with nothing ready returns immediately.
A blocking wait interrupted by a task signal tries only once more, and returns the file-wait interruption marker only if it still cannot complete.
Object locks must not be held across a wait; the future owns and cleans up its subscriptions. The object and its iterator remain responsible for saving the amount already completed;
the marker alone cannot prove that the whole request can be replayed. Ordinary operation errors and registration failures are not escalated to this marker;
whether it is converted into a syscall restart code is decided by the audited syscall exit.

Pipe reads and writes are wired into this wait flow, while the peer wait at FIFO open still uses the original interface. The completed amount of a single pipe write
and the cumulative amount of a vectored write across blocks take priority over interruption; requests of at most PIPE_BUF still keep the
all-or-nothing atomic-write constraint. The recheck observes data, space, and endpoint closure together, and must not turn EOF/EPIPE into a restart.

`transfer_files` takes files and an optional kernel-side offset, and manages the completed amount and position commit for the whole request;
it takes no user pointers and does not generate syscall restart codes. The transfer stack has two layers: the boundary layer resolves the
`TransferPosition` protocol into an explicit start (the address-ordered position lock, the shared lock for the same open description, and the lazy slot for non-seekable files
are all settled once in `resolve_pair_positions`), and the engine layer
sees only explicit offsets and no longer touches `f_pos`. All shapes share the same settlement pump: each step moves at most one block, through
pipe semantics of staged, then confirm, then consume only the confirmed prefix, or through positioned reads and writes; after an error with progress made, the
completed byte count is returned, and only a zero-progress failure propagates outward. File-to-file transfer uses a 4 KiB intermediate buffer;
a pipe leg re-acquires the position lock at each step, and the lock is not held across pipe readiness waits; two pipes are transferred directly to each other in object-address order,
without buffering.

The `copy_file_range` engine completes range validation and the copy within the same critical section, in an order matching Linux
`generic_copy_file_checks`: a directory reports `EISDIR`, a non-regular file reports `EINVAL`, and a destination opened with
`O_APPEND` reports `EBADF`; pairing admission follows immediately, before the wraparound check and EOF truncation; a range that
wraps under unsigned addition reports `EOVERFLOW`, the request is first truncated at the source EOF and then limited by the destination's maximum file size
(a start at or beyond the limit reports `EFBIG`, even if the request has already been truncated to zero), and overlapping ranges in the same inode report
`EINVAL`. The copy may partially succeed: after an error with progress made, the completed byte count is returned and the corresponding position is committed,
so the published position always describes the bytes the destination actually accepted.
Pairing admission follows the branch structure of Linux `f_op->copy_file_range` and rests on the destination operation table's
native copy primitive `copy_chunk_from`: when the destination table carries the primitive, it decides input compatibility itself
(for incompatible storage the implementation returns `EXDEV`, and the VFS does no identity comparison), block-level primitives are driven by the pump,
and settlement is still defined only once, in the pump; when the destination table has no primitive, it falls back to the generic same-superblock pump, and a cross-superblock request returns
`EXDEV`. Admission is done with a single primitive query with `len == 0`, and, as in Linux, its position is before the EOF
truncation, so an unserviceable cross-superblock empty request likewise reports `EXDEV`, instead of being truncated to zero and
silently succeeding (the cross-device capability probe in `copy_file_range01` has exactly this shape). memfs provides a real
primitive (a page-cache move from memfs storage to memfs storage), so cross-superblock copying between memfs instances
is a real capability.

Pipe-to-pipe copying has its consumption boundary managed by `PipeObject`: the input pipe removes only the prefix the destination accepted, and the output pipe
first confirms space inside the mutex before calling the source read. Two pipes hold their mutexes in object-address order and directly transfer
the accepted bytes; readers and writers are notified after the locks are released. Non-pipe callbacks may sleep, but must not re-enter a pipe.
The callback interface is for VFS-internal use only and may acquire the position lock of a non-pipe file while holding a pipe lock;
callers must not enter pipe operations while holding a position lock,
and while waiting for the pipe's own readiness, neither the pipe lock nor the file position lock is held.

`VfsFile::fsync(data_only)` delegates to `fsync_range(0..u64::MAX, data_only)`.
Both use the one `FileOperations::fsync(file, range, data_only)` capability.
Ranges are half-open bytes; backwards ranges return `EINVAL`, empty ranges still
reach the filesystem, and the full sentinel never requires adding one to MAX.
Path-only files fail with `EBADF` before dispatch; readonly opens may sync.
Invalid range/context does not consume the file's error cursor. After dispatch,
the backend error takes precedence while recorded mapping history is observed.

Missing fsync capability returns `EINVAL`. Pipe, FIFO, socket and generic character
devices explicitly reject it; replacement character operation tables decide their
own support. Memfs, simple directories and nullfs explicitly succeed for volatile
storage. Block files perform the real device flush, including on empty ranges.
9p forwards the intent to server fid fsync and may synchronize more than the
requested range. Ext4 composes data completion with its metadata/journal/device
duties. All synchronous entry points require sleepable task context, no held
spinlock or IRQ/preemption guard, and no current userspace process.

## Captured Poll Sources

Open-file operations may supply one `FilePollSource` containing a weak source
and its supported events. `VfsFile` forwards capture to its file operations.
Ordinary backends return no captured source
and retain their existing registration path. Capture fixes wait-source identity,
not readiness: consumers still query the file in the current calling context.
Event filtering and weak registration introduce no additional owner or source
list. An expired captured source must not fall back to the default path.

## Cross-crate interface

- `Path::dentry_is_mount_root(&self) -> bool` (`src/mount.rs`): reports whether
  the path's dentry is its mount's root dentry (Linux `path_mounted`,
  `dentry == mnt_root`), independent of overmount visibility. Provider: kvfs;
  consumer: `posix/fs` for the statx `STATX_ATTR_MOUNT_ROOT` attribute; call
  direction `statx(2)` -> `sys_statx` -> `Path::dentry_is_mount_root`. The
  related `Path::is_mount_root` is `pub(crate)` and additionally requires the
  mount to be the parent's topmost child mount; it serves mount/umount and
  mount-stacking validation, not the statx attribute.

## Scope

- `src/open_flags.rs`: normalization of raw `O_*` arguments and the open intent.
- `src/namei.rs`: path traversal and the open flow.
- `src/permission.rs`: owner/group/other DAC and the open access mapping.
- `src/node/`: dentry, inode, inode-identity state machine and filesystem
  operation traits; also the character/block device file operations and the
  device-access policy registration (see "Device-access policy (reverse
  injection)").
- `src/address_space/`: the inode-owned `AddressSpace`, the `MappingPages` container it holds directly, and
  the writeback and truncate/invalidation boundaries. `mod.rs` defines the object and its construction; `operations.rs`
  defines the callbacks and default implementations; `filemap.rs`, `readahead.rs`, `writeback.rs`, and `truncate.rs`
  each carry the orchestration of one algorithm; `views.rs` manages registration and invalidation notifications; the crate-root
  `ut-tests/address_space/` organizes regression tests by algorithm.
- `src/fiemap.rs`: the inode FIEMAP request state, flags, and safe output interface, decoupled from the user ABI.
- `src/file.rs`: an open file and its mutable state.
- `src/file_ra.rs`: the per-open-file readahead history, window selection, and error backoff.
- `src/xattr.rs`: xattr names/flags, namespace permissions, and the `Path` semantic entry points.
- `src/pipe.rs`: the data path, session state, and stateless
  file-operation objects shared by anonymous pipes and pathname FIFOs.
- `src/file_system_type.rs`: registered filesystem types and their creation entry points.
- `src/mount.rs`, `src/super_block.rs`: the mount tree and filesystem instances.

The concrete `anon_inodefs` and `pipefs` live in `fs/filesystems/`; KVFS does not store their filesystem
type, hidden mount, or singleton inode.

## Architecture

```text
current task                          kcred::Cred
    |                                     |
    | one Arc snapshot                    | explicit &Cred
    v                                     v
POSIX syscall ABI --> Filename --> Nameidata methods --> Path/VfsInode::permission
        |                                      |
        +-- open --> OpenHow --> OpenParams ---+--> VfsFile { f_cred: Arc<Cred> }
        |                         |
        |                         +-- flags: OpenFlags
        |
        +-- rename --> RenameFlags --> Path / Dentry --> InodeDirOperations
        |
        +-- statfs <----- StatFs (filesystem statistics)
                         SuperBlockFlags + MountFlags
                                      |
                                      v
                                StatFsFlags (ABI output)

Mount / Path
    |
    v
Dentry ---- namespace location (parent, name)
    |                         |
    v                         v
VfsInode                 child cache
    |
    v
filesystem operation traits
```

`Path::render_from(root)` is the shared mount-aware pathname renderer for
process-root-relative views. Crossing a mounted filesystem root contributes the
covered mountpoint name before continuing in the parent mount. The result is
typed as `RenderedPath::Reachable` or `RenderedPath::Unreachable`; `getcwd(2)`
uses the latter to emit Linux's `(unreachable)` prefix instead of converting the
condition to `ENOENT`. `absolute_path()` uses the same component traversal with
the mount-tree root as its boundary.

pipe/FIFO follows the three-layer ownership of Linux `inode`, `pipe_inode_info`, and `file`:

```text
VfsInode
├── shared stateless FifoFileOperations
└── pipe: Option<Arc<PipeObject>>

PipeObject
└── buffer, files, readers/writers, r_counter/w_counter, rd_wait/wr_wait

VfsFile
├── mode
├── pipe_generation
└── private_data -> PipeObject
```

Anonymous pipes and FIFOs share the inode pipe slot. The last close clears the
slot; a later open creates a new session. Reopening anonymous pipes does not
wait for a peer, whereas FIFO opens retain their partner-waiting semantics.
Both use the same data operations. Like Linux `fs/pipe.c`, only the FIFO
wrappers update access and modification timestamps after successful I/O.

`PipeObject` corresponds to `pipe_inode_info`, not to a file-operation table. The pipe module must live in the
same fs-core crate as `VfsInode`: the inode needs to hold the concrete pipe session, and the pipe file
operations in turn need the VFS file/inode interfaces; splitting them into mutually dependent crates would only force the implementation to add a factory
or type erasure.

Raw integers exist only at the ABI or compatibility entry points. Once inside the VFS, the different flags families are expressed by distinct bitflags
types, and callers read them through `contains`, `intersects`, or combined-semantics methods, which avoids reinterpreting
bare integers and passing the wrong flags family by mistake.

The raw C layout of FIEMAP and the user pointers stay in the POSIX layer. KVFS expresses the inode capability through the optional
`InodeOperations::fiemap_operations()`, and gathers the typed flags, capacity,
counters, and the borrowed safe writer into `FiemapExtentInfo`. Filesystem backends only call
`fill_next_extent()`; they never touch or store user addresses.

`OpenParams` corresponds to the complete normalized open arguments in Linux namei. Its fields are kept private, and the create
intent, exclusive-create, lookup behavior, and mount write requirement can only be read through narrow interfaces.
`OpenFlags` is the `O_*` bit set among them and also represents `VfsFile::f_flags`; atomic storage still uses
`AtomicU32`, and the value is restored to the type immediately after loading.

`SuperBlock` first allocates the object, then the root initializer constructs the root dentry and hands it back, and only at the end is the root bound.
A nodev instance enters the global registry after initialization completes; a block-backed instance, like Linux `sget_dev()`,
enters the registry before fill in the nascent state with an exclusive block-device claim, so that concurrent callers wait on the same
identity. The initializer can enter the VFS inode
identity table and construct the root inode through the inode methods of the nascent `SuperBlock`; on failure the instance is marked dead, the claim is released, it is removed from the registry, and
waiters are woken, so a half-initialized superblock is never published. This corresponds to the Linux layering of first allocating
`struct super_block`, then running `fill_super()`, and finally making `s_root` visible. The filesystem operation
table is a static object shared by all mounts of the same filesystem type; mount-private state can only live in the superblock
private object, and the root is not stored again. This corresponds to the ownership boundary of `s_op`, `s_fs_info`, and `s_root` in
Linux, and also avoids a reference cycle between the private state and the root inode. `SuperBlockFlags` corresponds to
`super_block.s_flags` and currently carries `SB_RDONLY`, `SB_NOATIME`, and `SB_NODIRATIME`;
`SuperBlock` also owns the generic values corresponding to Linux `s_magic`, `s_blocksize`, and `s_maxbytes`, which the filesystem
provides at construction time, and inode-private state no longer duplicates the magic, block size, or generic file size limit.
`MountFlags` corresponds to `vfsmount.mnt_flags` and carries per-mount policies such as `MNT_RELATIME`.
`SuperBlockOperations::statfs()` returns only filesystem statistics, and only at POSIX export time does the VFS convert the
superblock/mount state into `StatFsFlags`. The conversion follows Linux `fs/statfs.c`: among the currently modeled
superblock bits, only `RDONLY` is exported as `ST_RDONLY`; `ST_NOATIME`/`ST_NODIRATIME` are exported only from the
per-mount bits, even though the superblock bits of the same names still take part in inode atime decisions. Before publishing the new
`SuperBlockFlags`, remount calls the static `FsContextOperations::reconfigure`, and does not put a transaction callback
into `SuperBlockOperations`; this hook receives the proposed flags, changed mask, purpose,
credential, parsed `fs_private`, and proposed superblock-private `s_fs_info` from `FsContext`. The reconfigure context
is bound to the target `SuperBlock` at creation, corresponding to Linux `fs_context::root->d_sb`; the callback receives only the context
and is not passed a separate superblock that might mismatch. The VFS accepts only a context whose purpose is reconfigure and whose target identity
matches; the hook and the publication of
`sb_flags`/`sb_flags_mask`, as well as the final shutdown, are serialized by the umount lock inside the superblock,
corresponding to the role of `s_umount`. As with Linux behavior when no callback is provided, the default hook accepts pure VFS
flags changes; before the callback, the VFS uniformly rejects, for a block-backed superblock, a read-write target that conflicts with a canonical read-only
device.

`ktime_types::TimestampLimits` corresponds to Linux `super_block`'s `s_time_gran`, `s_time_min` and
`s_time_max`. `SuperBlockOperations::timestamp_limits()` adopts Linux `alloc_super()`'s whole-second,
full `i64` range as a conservative default; after the filesystem fill operation installs the operation table and private state
it queries this capability once, and it is then persisted by the sole `SuperBlock`. A filesystem may explicitly override it based on the on-disk format or protocol state, just as Linux
`fill_super()` sets `s_time_*`; KVFS does not create a parallel
format object and does not duplicate this state in the bridge.
`VfsInode::current_time()` rounds and clamps the current time;
`truncate_timestamp()` applies the same conversion to explicit values. Explicit
setattr, automatic I/O updates, and namespace helpers apply superblock policy
before comparison and filesystem callbacks. Backends then apply format-specific
rules such as FAT field precision. Explicit setattr returns effective values in
`MetadataUpdate`, and KVFS publishes only those fields. Automatic `update_time`
callbacks publish resident timestamps themselves; the default likewise publishes
only fields confirmed by setattr. Ignored fields are never restored from the
original request. Raw setters only store values without repeating policy lookup.
This follows Linux's separation between `current_time()`/`timestamp_truncate()`
and `inode_set_*_to_ts()`, while allowing FAT-style distinct atime and mtime/ctime
invariants.

`FileSystemType` corresponds to Linux `struct file_system_type` and describes the name of a filesystem implementation,
whether it needs a backing device, `init_fs_context`, and type flags. `init_fs_context` installs the statically shared `FsContextOperations` for each
transaction and initializes the context-private state, corresponding to
the layering in which Linux `file_system_type::init_fs_context` sets `fs_context::ops`/`fs_private`; the operation
table itself holds no transaction state. The global registry corresponds to the Linux
`file_systems` list and is the common source of truth for mounting by type name and for `/proc/filesystems`.
Each type constructs a superblock through the `get_tree` callback of the context operation table, corresponding to Linux's `->get_tree`: nodev
types call `get_tree_nodev`; device-backed types call KVFS `get_tree_bdev`, and the VFS super
layer completes validation of the source pathname, block-special inode, `nodev` and `rdev`, and then obtains the canonical `BlockDevice` from the block core.
`get_tree_bdev` then performs, by
`(FileSystemType, BlockDevice object)` in the same superblock registry, the lookup equivalent to Linux
`sget_dev()`: an existing live instance is reused directly; an instance being initialized
or shutting down waits for its lifecycle transition; only the caller that obtains the reservation executes
filesystem fill-super. A new block-backed superblock obtains an exclusive claim in the block core,
so the same canonical device cannot be owned in parallel by different filesystem types or instances; a `BlockDevice` object republished for the same `dev_t`
after revocation is a new medium generation and will not hit the old root and cache. This is consistent with
Linux `setup_bdev_super()` using the new superblock as the exclusive `bdev_open()` holder: different
filesystem types create different holders and therefore cannot obtain the same device simultaneously. The VFS
first allocates and registers the nascent `SuperBlock` that already carries `s_type/s_bdev/s_flags`; the fill callback receives only
that object and installs the filesystem operations and root, and no longer receives or copies type, device or flags; the one-shot
fill closure may capture the mount state already parsed in the same `FsContext`, without putting the option parser into
generic VFS. Driver open runs after reservation, outside the registry mutex. The once-installed
claim corresponds to Linux `s_bdev_file`, separate from reserved `s_bdev` identity.
Failure/final shutdown closes it before unregistering and waking mount waiters.
VFS publishes the available lifecycle only after fill succeeds.
When an existing block-backed superblock disagrees with the new request's `SB_RDONLY`, `EBUSY` is returned, corresponding to Linux's
existing-`s_root` branch of `get_tree_bdev_flags()`, rather than downgrading the block mount to a per-mount policy.
As with Linux `bdev_file_open_by_path`, a writable mount rejects a
canonical read-only device; a read-only mount may still proceed to filesystem fill-super. The `fs_flags` field is the typed
`FileSystemTypeFlags` (bitflags), corresponding to Linux `struct file_system_type::fs_flags`,
in which `REQUIRES_DEV` declares whether a backing device is needed, and the bit numbering is aligned with Linux `include/linux/fs.h`,
for the nodev column of `/proc/filesystems` and the mount error path; when flags are added in the future
following Linux semantics (such as binary mountdata, subtype), keeping the same numbering scheme is enough to extend it.

block-special inodes uniformly install KVFS's `DefaultBlkdevFileOperations`, corresponding to Linux
`def_blk_fops`. Only open looks up `rdev`; existing file-private storage retains
`BlockDeviceClaim`. Capacity, I/O, ioctl and close use that original device even
after withdrawal or device-number reuse. The claim preserves `O_EXCL`; ioctl uses
the current nonblocking flag. Last-file release closes once.
A block-device special inode has no canonical block address space yet, so mmap returns ENODEV; the medium-change invalidation protocol must be implemented before enabling it.
KVFS supplies checked argument access and `has_sys_admin` from the current caller;
the scalar argument is read through `posix_types::UserConstPtr`, whose `UserRead`
bound constrains the ABI type at the call site.
The ioctl path receives the syscall caller's `&Cred`, without retaining extra credentials.
Generic block commands and the driver fallback for unknown commands are dispatched uniformly by `BlockDevice::ioctl`. An ordinary close and write do not implicitly flush; only an explicit fsync passes the
durability request to the backend. KVFS maintains no second `dev_t -> DeviceFileOps` table;
devfs projects only names and `rdev`, and loop devices are also published as ordinary `Gendisk`s.

Byte I/O on a block-device file is first clamped to the current capacity. If both the offset and the clamped length are aligned to the backend
block size, the entire borrowed buffer is submitted through a single general block call, using the existing contiguous multi-block
contract and avoiding resubmitting and waiting for every sector; calls are not merged across files, and no extra data copy is added.
Unaligned requests keep the block-by-block read or read-modify-write path. A single multi-block call has only an overall success or error result,
so KVFS does not guess how many sectors a failed request completed internally, and does not automatically split and retry the write after an error.

`FsContext` corresponds to Linux `struct fs_context` and holds `fs_type`, the mount/reconfigure purpose, the source,
one page of bounded kernel-owned opaque mount data, `sb_flags`/`sb_flags_mask`, the mounter credential,
static operations, an optional parsed `fs_private`, and the proposed `s_fs_info`. The opaque data is only borrowed for the synchronous
transaction; KVFS imposes no text encoding or option grammar, and a concrete filesystem parses its own
representation in `init_fs_context` and copies the options that must survive after mounting into typed private state. Both kinds of private state
are exclusively owned by the context; callbacks validate the proposed `s_fs_info` through a typed borrow, and a failed reconfigure does not modify the existing
superblock private state.
A reconfigure context also borrows its sole target superblock, so that a callback needs only the context to recover the owner.
It does not hold the root/pwd of the process `FsStruct`. In Linux, the path environment is obtained from the ambient `current->fs` in
`get_tree_bdev -> lookup_bdev -> kern_path`;
to avoid a reverse dependency on `kprocess`, KVFS has the mount execution entry point take a snapshot of the root/pwd of `FsStruct`
and pass it explicitly when calling `FsContext::get_tree`. The snapshot exists only along the call stack and does not become a field of the mount transaction.
The `FileSystemType` registry and the `SuperBlockRegistry` correspond respectively to the Linux filesystem type list
and the superblock instance set, and do not duplicate each other's state. For block-backed types, the latter uses the
canonical `FileSystemType` pointer and the canonical `BlockDevice` object address as the unique identity; an existing available
instance is reused directly, nascent/dying instances wait for the state transition, and dead instances are removed and the lookup retried. When different types contend for the same
device, the exclusive claim of the block core rejects the later one, so a second copy of mutable mount state cannot be established.

`SuperBlock` enforces a separation between the static filesystem operation table and the mount-private object, corresponding to Linux
`s_op`/`s_fs_info`. The construction API accepts only `&'static dyn SuperBlockOperations`; a stateful
object allocated for each mount cannot be used as an operation table. A filesystem that needs mount state
hands the one private object to the superblock for exclusive ownership through
`try_new_with_flags_and_private()`. Callbacks recover it by borrowing through `private::<T>()`, which is
bound to the lifetime of `&SuperBlock`; this does not clone the `Arc`, and it does not allow a typed owner
to escape the superblock lifetime. The block size and max file size are generic superblock fields set once by the fill
operation, and the private inode or operation table is not required to duplicate them.

`Dentry` is a movable namespace object. rename preserves the source dentry and the inode identity, and only
changes the dentry's position and cache membership. The inode holds the file state and the address space, so
rename neither flushes file data nor replaces the inode `AddressSpace` identity. Consistent with Linux `d_sb`/`i_sb`,
once a dentry and an inode are first attached to a superblock, neither can be rebound; both hold a `Weak<SuperBlock>` to avoid
a strong reference cycle through root/dcache, but destroying the old superblock does not hand the same VFS identity over to a new superblock either.

Every hashed
`(SuperBlock, backing inode number)` published by a filesystem through the VFS-wide identity table reuses the same `VfsInode`;
hard links, rename and repeated lookups
therefore share the same `AddressSpace`. Linux-style pseudo/unhashed inodes do not enter this table, and direct construction and `get_or_try_init_inode()` must not be mixed for the same
identity. A non-directory inode can have multiple
dentry aliases; a directory inode has at most one live alias, and repeated lookups reuse that dentry, corresponding to the single-alias directory invariant that Linux
`d_splice_alias()` relies on. A filesystem can have its operation object implement
`InodeAttributeOperations`, so that the KVFS generic attribute API and the filesystem component share one copy of the state; this corresponds to the
Linux filesystem-private inode structure embedding `struct inode`. For a filesystem that does not use a shared backend, after a mutation completes,
if the operation callback already holds a `VfsInode`, it can still use `update_metadata_after_backing_change()` or
`update_attributes_after_backing_change()`, which does not change the size, to refresh the VFS cache; if it holds only the target
`Dentry`, it uses the dentry metadata refresh of the same name. `Dentry` does not expose its internal
`Arc<VfsInode>` to external crates. The update entry points validate the positive state, inode number, node type, block size and
`rdev`, preventing the result of one core inode from being written into another identity.

The identity table corresponds to the global `inode_hashtable` in Linux `fs/inode.c`, and is indexed jointly by superblock identity and inode
number; it does not become a field of `SuperBlock` or of the filesystem/bridge.
`SuperBlock::lookup_inode()` is the object-oriented entry point to this global algorithm, corresponding to Linux `ilookup()`;
`get_or_try_init_inode()` corresponds to
`iget_locked()` plus the filesystem fill, followed by publication by the VFS. The cache also takes on the gate that Linux's inode cache provides for
initialization and release: a missing slot is first turned into `New`, and only the
owner runs the fallible filesystem initializer while other lookups sleep; on success it publishes `Live`, and on failure or
initializer panic unwind, the local reservation guard removes the `New` slot and wakes the waiters to retry. This guard only expresses
the paired constraint of Linux `unlock_new_inode()`/`iget_failed()`; it does not enter the resident inode, and it does not add a cache
state. The wait queue of each slot corresponds to Linux's wait on the `I_NEW/I_FREEING` state bits of that inode; a waiter
saves the slot generation and sleeps outside the cache mutex, and only the publish/remove of that inode wakes it, avoiding
a whole-cache broadcast and a cross-inode thundering herd. If, after a cache-only lookup waits for a transitional object, the slot has been unhashed, it returns
`None`; a hashed path that must load from disk uses `get_or_try_init_inode()`. A new hashed identity is
bound to its owning superblock before `Live` is published, so a filesystem callback never observes a resident inode lacking `i_sb`. When the last
`VfsInode` reference enters drop, before the filesystem eviction hook the cache
turns the exactly matching `Live` entry into `Freeing`; a lookup of the same number waits for the hook to complete and the old entry to be removed, and then looks up
again. `New`/`Freeing` are never returned as ordinary inodes, and are not converted into the pathname `EINVAL`/`ESTALE`.
Filesystem private state is held by composition in `VfsInode`, and a backend must not build another resident inode-number cache.
The `Path -> Mount` strong reference of an ordinary pathname callback keeps the superblock alive for the duration of the call; if an unbound or
post-teardown raw inode is used directly in a filesystem callback, bypassing Path, `VfsInode::super_block()` returns
Linux `ESTALE`, and does not misreport an object lifetime error as a media `EIO`.
The constructor entry of the shared attribute operations validates the inode number and node type before publication; after construction KVFS
does not allocate another copy of the owner/link/time/size/block state. High-frequency generic field reads call the
single-field getters of the shared component directly; only a full `stat/getattr` snapshot calls `fill_metadata()`. The inode
number and the superblock block size of a full snapshot are supplied by the unique `InodeIdentity`/`SuperBlock`, and the attribute component
no longer duplicates `i_ino` or the generic block-size field.

`Nameidata` stores only the path, root, components and lookup state that Linux namei needs, and does not store
credentials. Every entry point that resolves or modifies the namespace passes `&Cred` down level by level as a method parameter.
The lifetime of the credential is guaranteed by the `Arc` held by the syscall; object fields need not store the calling context again.

The `openat2(2)` constraints `BENEATH`/`IN_ROOT`/`NO_XDEV`/`NO_SYMLINKS`/`CACHED`
are invariants of the namei state machine, not a syscall-layer wrapper: every
`enter` of a new path checks anchored-subtree membership and the starting mount,
symlink hops inherit the same root and mount, and membership is re-checked before
delivery. `RESOLVE_CACHED` returns `EAGAIN` before a slow lookup, a create or a
truncate. `sys_openat2` therefore only validates the arguments, selects the
dirfd/cwd base and decides whether the lookup is anchored; it does not duplicate
the lookup policy.

## Calling Constraints / Execution Context

Path and namespace operations acquire sleepable locks, allocate objects and call the concrete filesystem, so they may block;
they are not usable in interrupt context, and must not be called while holding a spinlock. These APIs depend on the scheduler, the allocator and the
normal kernel runtime environment. POSIX paths usually need the current process's mount, root and cwd; methods on pure VFS objects
depend only on the objects passed in explicitly.

Releasing the last `VfsInode` reference may synchronously perform filesystem eviction; releasing the last
`VfsMount` may synchronously perform superblock shutdown. Both can likewise happen only in a sleepable task
context, and the caller must not hold a non-sleepable lock or a lock needed by teardown. This corresponds to the calling constraint that Linux
`iput()` itself may sleep, and does not introduce an extra inode-specific deferred worker.

One complete pathname operation must reuse the same credential snapshot. The caller must not re-query the current task for each path
component, otherwise a concurrent credential commit could make one resolution mix identities.

A filesystem callback may block on I/O, but while holding the same set of VFS inode namespace locks it must not
re-enter these VFS namespace operations, otherwise it would deadlock on itself.

Built-in filesystem types must also finish registering before user processes start. Runtime lookup only reads the registry, and is not responsible for
loading implementations or making up for startup initialization.

## Algorithm Flow

At its entry, open clears legacy flags, validates the known bits, and generates the access mode, open intent and lookup
flags. namei uses these semantics to perform lookup, creation and the final open, and no longer combines `O_CREAT` and
`O_EXCL` directly. The `O_PATH` path likewise applies the final object type constraint; for example, `O_PATH | O_DIRECTORY`
accepts only a directory in which child lookup is permitted by execute permission, and regular files and autodirs return `ENOTDIR` according to Linux semantics. A `/` after the final component
is converted in the generic `path_lookupat()` into `FOLLOW_FINAL | DIRECTORY`, so even if `O_NOFOLLOW` is also specified,
the trailing slash still follows the final symbolic link, and the resolved result is validated with `Dentry::can_lookup()`.

A pathname FIFO is not opened through a syscall fallback or a second lookup. All FIFO inodes share one
file-operation table with no runtime fields; each inode holds the currently active
`PipeObject` in its special state, corresponding to Linux `inode->i_pipe`. namei completes
`may_open` on the same resolved `Path` and then enters `open()` of that table. The operation table may modify the stream mode, private data and final file operations of the existing
`VfsFileBuilder`, but must not replace the path
already bound by the builder. As a result, the pathname DAC, the inode actually used and the opened-file identity
belong to the same open transaction.

A device open callback can check the transient
`O_NOCTTY` request through `VfsFileBuilder::requests_no_controlling_tty()`. This check happens inside the callback; after open completes, `O_NOCTTY` and the other creation-time
flags are removed from the final `f_flags`.

FIFO open creates or reuses a session and increments `files` under the inode pipe-slot lock, and then
`PipeObject::open_fifo()` completes the reader/writer rendezvous according to the builder's `f_mode`.
The last file release decrements `files` and clears the inode slot within the same slot-lock domain, preventing an old
release from interleaving with a new open and clearing a session that is still in use.

### File extent query

The caller first obtains an optional inode capability from `VfsInode::fiemap_capability()`, and then issues the query with
`FiemapExtentInfo`. For a regular file the query runs under the inode data shared lock; for a directory it runs under the namespace
shared lock. The same mechanism therefore covers both regular inodes and directory inodes, without
wrongly attaching FIEMAP to an open-file operation. The filesystem passes the format limit determined at inode creation to
`FiemapExtentInfo::prepare()`; this helper first validates the length, the maximum file size and the flags supported by the filesystem,
and only then performs data-only writeback if needed.

The backend emits only the mapped, unwritten or delayed extents that intersect the queried range, and skips holes.
When `extent_count == 0`, `FiemapExtentInfo` only counts; when the array is full it stops normally, and only the
last extent whose traversal is confirmed to have finished gets `LAST`.

The VFS follows the Linux directory-locking ownership model:

- slow lookup takes the shared namespace lock on the parent directory inode;
- create takes the exclusive namespace lock on the parent directory inode;
- unlink and rmdir lock the parent directory first, then the victim inode;
- link locks the new parent directory first, then the non-directory source inode;
- same-directory rename decides by parent dentry identity and locks the parent directory only once;
- cross-directory rename first acquires the superblock topology mutex, then locks the parent directories in topological order. When there is no ancestor relationship,
  ordering uses `Dentry::namespace_lock_key()` (the chain of ancestor inode pointers, then the pointer itself); `pivot_root`
  takes the locks of its multiple inodes with the same rule. The participant inodes of rename are a separate later stage and do not belong to this total order.

Slow lookup corresponds to Linux `d_alloc_parallel()`: the task that suffers a cache miss creates a unique hashed negative
dentry, sets the parallel-lookup state, and calls the filesystem
`lookup` while holding that dentry's lookup mutex; concurrent lookups of the same name reuse this object and wait for the owner to finish. The filesystem expresses a
negative miss with `Ok(None)`; when it finds an inode it usually instantiates the candidate in place; it returns another dentry only when `d_splice_alias()` semantics require
reusing a directory alias. Ordinary lookup and namespace mutation use the same
locked-lookup object method, differing only in whether the parent directory holds the shared or the exclusive namespace lock;
there is no negative-cache path specific to mutation.

After the two parent directories are stable, rename resolves source and target, then locks the participating child directories first, and then the non-directory
inodes. The single-alias invariant for directories guarantees that different parent dentries never reference the same directory inode. The participants
are at most two inodes, source and target, acquired directly by
directory/non-directory combination without building a temporary `Vec`; two non-directory inodes are ordered by pointer value. The parent-directory topology walk
yields both the lock order and the `trap` of Linux `lock_two_directories()` semantics; the final source/target are compared directly against
`trap`, without walking the parent chain again. The type, flag and ancestor-relationship checks, as well as the Path
policy for the final source/target, are all completed within the same namespace transaction; the old/new cache
keys are saved before the filesystem callback, and after success the VFS only swaps the dentry locations and replaces the existing cache slot in place, without allocating a name or inserting a
new hash slot. Whether a directory is empty is decided by the filesystem rename/rmdir callback;
the generic layer does not treat the dentry child cache as the backend's directory contents.

Open-create and exclusive namespace create reuse the same final-lookup skeleton under the parent directory lock, which avoids races of
`O_EXCL`, `mknodat`, `linkat` and `symlinkat` against lookup/create.
`O_CREAT | O_EXCL` skips the speculative lookup and performs the final lookup under the lock directly; exclusive creation
returns `AlreadyExists` on a positive dentry first, and runs the create callback only for a negative dentry.
`Filename` only does parent resolution and pathname-specific errno handling; Dentry only hands the final negative
dentry under the lock to the callback, and does not additionally expose the parent inode. `Path::vfs_create()`,
`Path::vfs_mkdir()`, `Path::vfs_mknod()`, `Path::vfs_symlink()` and
`Path::vfs_link()` obtain the inode from the parent Path itself, and are responsible for mount write, parent-directory DAC,
device-node authorization, mode preparation and the inode callback.
The same negative dentry obtained by lookup is passed to the callback, and no second object is constructed by name;
regular `mknodat` passes
`exclusive=true`. The create, mkdir, mknod, symlink and link callbacks must all instantiate this negative
dentry; lookup returns another dentry only when it reuses an existing directory alias. Create-only errors are checked only when the
final lookup under the lock is still negative.

unlink and rmdir likewise first resolve the parent via `Filename::unlink_at()` / `Filename::rmdir_at()`,
and then Dentry resolves the victim exactly once under the parent directory's exclusive namespace lock. The syscall does not first obtain a
complete target Path and then look it up by name, so it cannot delete another inode with the same name in a replacement race.

### Path traversal and DAC

Before entering each directory and looking up the next component, namei checks the directory search permission (`MAY_EXEC`); on the final
open, the target inode is then checked by access mode. The default `InodeOperations::permission` enters
`generic_permission()`, which selects the owner, group (including supplementary groups) or other permission bits in turn.
effective `CAP_DAC_OVERRIDE` overrides ordinary mode bits; effective
`CAP_DAC_READ_SEARCH` additionally grants exact read (and search for
directories). Neither grants execute permission on a regular file with no
execute bit set, matching Linux `generic_permission()`.

Namespace modifications are checked uniformly by `Path` or `Filename` before the filesystem callback is invoked:
create, mkdir, mknod, symlink, link, unlink, rmdir and rename require the parent directory
`MAY_WRITE | MAY_EXEC`. All create-like operations call the common `Path::vfs_*`
capabilities from the in-lock callback; unlink, rmdir and rename pass the Path policy to the Dentry operation as a validator.
These policies execute while the parent directory lock, the required participant locks and the final lookup result are still valid.
Deletion and replacement in a sticky directory require effective
`CAP_FOWNER`, directory ownership, or ownership of the final victim; the
mountpoint check applies to that same dentry.

Inode metadata modifications are likewise authorized uniformly by `Path` and then enter the same backend `setattr` callback:

- `chown` requires effective `CAP_CHOWN` to change the UID; the owner may
  preserve the current UID and change the GID to a current or supplementary
  group;
- `chown` also applies Linux `chown_common()` set-ID cleanup. Directories keep
  their mode. Other nodes always lose set-UID, and lose set-GID when group
  execute is set or the caller is not in the pre-chown GID and lacks effective
  `CAP_FSETID`. If cleanup changes the mode, Linux additionally drops set-GID
  unless the caller is in the new GID or holds effective `CAP_FSETID`; that
  new-GID condition does not apply when a GID change leaves both set-ID bits
  untouched. The generated mode update requires inode ownership or effective
  `CAP_FOWNER`, so `CAP_CHOWN`/`CAP_FSETID` alone do not authorize it and
  `chown(-1, -1)` stays a no-op when no set-ID bit changes;
- `chmod` requires inode ownership or effective `CAP_FOWNER`; retaining a
  requested setgid bit additionally requires the target GID to belong to the
  caller or effective `CAP_FSETID`;
- `set_times` distinguishes touch from an explicit times array. Empty times or
  two current-time values permit the owner, effective `CAP_FOWNER`, or a caller
  with write permission on the inode; explicit values and mixed
  `UTIME_NOW`/`UTIME_OMIT` permit only the owner or effective `CAP_FOWNER`.

`SetattrTime` carries only the authorization information of "current value/explicit value" within a single call, and the persisted `MetadataUpdate`
contains only the resolved atime/mtime/ctime values; the call context is not saved in the inode or namei state.
Automatic I/O updates use `InodeUpdateTime`, corresponding to Linux
`FS_UPD_ATIME` / `FS_UPD_CMTIME`, and invoke filesystem `update_time`. Authorized
explicit values pass through `VfsInode::truncate_timestamp()`; automatic values
come from `current_time()` before comparison. Path retains mount/relatime atime
checks; the callback decides whether mtime/ctime changed. It receives the resident
inode, returns `VfsResult<()>`, and owns publication; KVFS does not invoke setters
again. The default and filesystem fallbacks share `generic_update_time`: compare
`times()`, skip unchanged values, otherwise call setattr and publish only confirmed
timestamps. Backends owning resident attributes may compare, update, and mark dirty
in one critical section, deferring persistence under filesystem sync rules. They
may reduce precision, couple fields, or ignore unsupported fields; raw setters do
not repeat capability policy.

The default implementation of `InodeOperations::setattr` is a common fallback layer, corresponding to the Linux form in which `notify_change()` routes an
inode without `->setattr` to `simple_setattr()`: owner/mode/time updates are accepted and
published to the resident attributes, so backend-less inodes that use `empty_inode_operations()` (anonymous pipes,
roots of pseudo filesystems, etc.) can support `fchownat`/`fchmod`/`utimensat` and automatic
I/O time updates without a dedicated table. The precondition of the fallback layer is that the inode identity is independent and the metadata resides only in the inode itself; an inode that deliberately shares
identity does not satisfy this precondition and must explicitly override `setattr` to keep the rejection semantics. The anon_inodefs
singleton follows exactly this policy: it is referenced jointly by the eventfd/epoll/timerfd/signalfd/pidfd/socket/BPF
anonymous fds of all processes, and accepting a single fallback update would rewrite the
shared attributes that every process sees when it calls `fstat` on its own anonymous fd. The fallback layer rejects size updates and reports
`EINVAL`: such inodes have no truncatable storage, and
publishing a fabricated `i_size` would break the semantics; this matches the reachable behavior in which Linux returns
`EINVAL` at the syscall boundary for truncate on non-regular files, while filesystems that bring their own storage (memfs, kext4, fat, 9p, etc.) each implement
their own `setattr` and are unaffected. Authorization still happens only at the `Path` layer, and the fallback table does not re-execute the policy.

Xattr policy also flows through `Path`, which applies mount, namespace, and DAC
policy before entering `InodeOperations::{get,list,set,remove}_xattr`.
get/set/remove inputs keep names in a checked kernel `Vec<u8>`; list passes
complete names incrementally through `XattrNameRef` and `XattrNameSink`,
allowing backends to add namespace prefixes without copying and letting `Path`
filter `trusted.*` before returning names to the caller. Both representations
preserve non-UTF-8 suffixes intact. `XattrSetFlags` is converted from ABI raw
bits before reaching a backend, and the `CREATE|REPLACE` combination remains a
constraint requiring both flags to be set. `user.*` is allowed only on regular
files, directories, and sockets, and writes to a sticky directory require
directory ownership or effective `CAP_FOWNER`. `trusted.*` is visible only to
callers holding effective `CAP_SYS_ADMIN`. Reads of `security.*` retain the
Linux VFS LSM delegation model, but until concrete LSM hooks are connected,
set/remove require effective `CAP_SYS_ADMIN`, which approximately prevents
unprivileged callers from forging security attributes; `system.*` remains
delegated to the filesystem or ACL layer. Before any namespace branch, the
common write check rejects set/remove on inodes carrying
`NodeFlags::IMMUTABLE` or `NodeFlags::APPEND_ONLY` with `EPERM`; reads are
unaffected. Set/remove call the backend under the inode data lock, and each
filesystem must atomically resolve all four `CREATE`/`REPLACE` combinations
inside its own transaction or lock.

### New inode owner

The create callback receives the same `&Cred`. The backend obtains the initial mode, UID, and GID through `inode_init_owner()`:

- open-create, `mkdirat`, regular `mknodat`, and special-node `mknodat` all enter the corresponding
  `Path::vfs_*` capability; the mode-preparation method of the parent inode handles, in order, setgid, the caller's
  umask, the VFS allowed-permission mask, and the callback node type; currently there is no POSIX ACL branch that defers
  the umask;
- `vfs_mkdir` uses the allowed-permission mask to exclude the caller-supplied set-user-ID/set-group-ID
  bits, rather than modifying the result by hand again in the Path layer;
- the UID uses `fsuid`;
- under an ordinary parent directory the GID uses `fsgid`;
- under a setgid parent directory the parent's GID is inherited;
- when a subdirectory is created under a setgid parent directory, `inode_init_owner()` propagates the setgid bit again.

Backends that support Unix owner metadata persist these values when allocating the inode. File systems
that cannot express a Unix owner remain limited by their on-disk format.

The `DirMapping` of `SimpleDir` can persistently insert special inodes allocated by `mknod`, and can also insert
symbolic links with immutable targets, so that simple file systems such as devfs can create FIFOs, pathname Unix sockets,
and boot-time links such as `/dev/fd`. A fast-link target is stored only in the `VfsInode` cached-link state,
corresponding to Linux `inode::i_link`; `SimpleFsNode` stores only metadata such as the inode number, mode, owner, and
`i_size`, and no longer keeps a second copy of the target in a `SimpleFile` closure. A simple directory
that does not explicitly support dynamic insertion keeps returning `EPERM`.

A non-recursive bind mount creates a new `Mount` that shares the source path's superblock and root dentry, but has its own
mount ID, parent mount location, and overmount relationship, and inherits the per-mount flags of the source mount. This corresponds to Linux
`clone_mnt(old, root, clone_flags)`; namespace copying and bind mounts share
`Mount::clone_mnt`: it only constructs a detached `Mount` and does not write the parent topology at the same time;
`Path::graft_tree()` then performs the root/target type checks and publishes the object at the mountpoint. A new file system
mount likewise first constructs a detached `Mount` and then goes through the same graft stage, which avoids a half-mounted state in which "a parent path exists but the mount is not in the
parent child map". As in current Linux `graft_tree()`, when the directory-ness of root and target
does not match, both directions return `ENOTDIR`, and the bind path does not introduce a separate errno branch. A non-recursive bind does not copy
the child mounts under the source path, and the initial `MS_BIND` does not apply the ordinary mount flags of the same call to the copy.

`Mount` is a stable identity referenced jointly by `Path`, open files, and the namespace topology, and it is not replaced on remount.
The parent mount and the mountpoint are represented by a single `Mutex<Option<Path>>`, set once on attach and
cleared on detach; they are no longer stored in separate parent/mountpoint fields that could become mismatched. Namespace operations verify through the existing
mount ID registry that the source, target, and detach objects belong to the current `MntNamespace`; the same registry
mutex covers verification, topology modification, and registry commit, serving as the namespace-level transaction boundary.
A recursive detach first collects, starting from the target mount, the complete overmount stack of every child mountpoint, and then verifies that all
objects are still in the registry; only after verification succeeds does it perform a topology commit that can no longer return errors, and finally it removes the registry
references. The old layers covered by the target mount itself are not part of the subtree to be removed, and are restored to the parent mountpoint at commit time.
`VfsMount::flags()` and `set_flags()` wrap atomic reads and writes on the `mnt_flags` field, and keep the strongly typed `MountFlags` interface
for the rest of the mount code.

An ordinary remount is only allowed on the root path of a registered mount; `MntNamespace::remount()` receives a
reconfigure-purpose `FsContext` and per-mount flags, updates the read-only policy on the `SuperBlock` that is observable by all shared mounts,
and atomically replaces the target mount flags; `MS_REMOUNT|MS_BIND` goes through
`MntNamespace::reconfigure_mount()` and replaces only the target's per-mount flags, without requiring the target itself to have been
created by bind. The superblock flags are stored in `AtomicSuperBlockFlags`; before publishing the new flags, an ordinary remount
calls `FsContextOperations::reconfigure`, and a per-superblock umount lock serializes the hook
and the publication. When a file system backend always reports read-only, switching to read-write returns `ReadOnlyFilesystem`.
`MntNamespace::make_mount_private()` reuses the same registered-mount-root validation. KVFS currently has no
shared propagation group, and after construction and namespace clone no mount tree propagates to any other
namespace, so make-private is idempotent; a recursive request does not need to traverse or modify descendants.
A mount does not store origin state recording "whether it was created by bind"; unmounting any mount only removes the topology node,
and `VfsMount` takes one superblock active reference on creation and copy and returns it on final release, corresponding to `dput(mnt_root)` and `deactivate_super(mnt_sb)` in Linux
`cleanup_mnt()`. The release of a mount that is not the last one does not repeat teardown; when the active count drops to zero, under the umount lock it first writes back the inode/page cache and
file system state, then evicts dcache ownership, and finally synchronizes again the metadata, journal
checkpoint, and device state produced by eviction, corresponding to the responsibilities of `generic_shutdown_super()` and the block-device final flush.
`MntNamespace::mount_new()` combines `get_tree` and active-reference acquisition into a retry loop:
if the last old mount puts the reused instance into dying/dead between the two, the VFS does not graft that object into the namespace,
but runs `get_tree` again. A new mount of the same shared superblock that requests a different read-only state returns
`EBUSY`, so that per-mount creation does not silently change the filesystem-wide policy of an existing instance.
`Path::unmount()` does not call `sync_fs()` at the topology layer, so when an open file or another `Path` holds the detached
mount, final shutdown is deferred just as in Linux. A sync error during shutdown is logged but does not roll back the already committed
topology; cleanup and the final flush still continue.
Unmounting removes the parent child index and restores the covered
mount under the inode namespace lock of the parent mountpoint, and clears the parent location of the detached mount, so that an open path still holding that mount cannot walk back along the old
parent into a namespace it has already left.

`MntNamespace::pivot_root()` (`pivot_root(2)` semantics) replaces the visible root of the namespace with the registered mount that
`new_root` points to, and mounts the original visible root at `put_old` (the OCI runtime then detaches it with
`umount2(MNT_DETACH)`). Requirements: `new_root` is a registered mount root, and `put_old` is a directory and
lies within the `new_root` subtree; `put_old == new_root` is allowed (the runc `pivot_root(".", ".")`
path), and when `put_old` is covered by other mounts the covered layers are preserved. The surgery runs under the registry mutex plus the namespace write locks of the affected inodes
(in the total order of `Dentry::namespace_lock_key()`, deduplicated by Arc), and after taking the locks it rechecks that the old visible
root is still mounted on its own parent mountpoint (the stack read may predate lock acquisition), returning an error rather than asserting if not. It first detaches `new_root` from its original location and restores its covered
layers, so that it is stacked on top of the namespace's structural root, then takes the original visible root out of the
position where it was covered by `new_root` (its own overmount chain is still preserved with the subsequent root, corresponding to Linux's stack-preserving move), and finally mounts the
original root at `put_old`. All fallible checks are completed before the first modification, which guarantees that a failed pivot does not leave a half-edited
tree behind. `mount_edge_detach` is the shared single-edge detach primitive (restore covers / delete the child index + clear the
location), reused by pivot and by the later MS_MOVE.

`MntNamespace::make_mount_private()`, `make_mount_slave()`, `make_mount_shared()` and `make_mount_unbindable()` only validate
that the target is a registered mount root and record no propagation state. KVFS does not yet have shared propagation groups, so all
four behave equivalently: without a shared master a slave receives no events, a shared mount propagates nothing, and an unbindable
mount has no peer to refuse. `MS_SHARED` is accepted rather than rejected because systemd remounts the namespace root `MS_SHARED`
after `pivot_root`, and an `EINVAL` there aborts the switch-root. Each entry validates its call separately (`MS_PRIVATE`,
`MS_SLAVE`, `MS_SHARED` and `MS_UNBINDABLE` may carry `MS_REC`/`MS_SILENT`, and any other combination returns `EINVAL`), to prevent
illegal flags from being silently swallowed. A recursive bind (`MS_BIND | MS_REC`) clones the source subtree:
`attach_bind_recursive()` re-grafts each cloned child under its cloned parent, so only submounts at or below the bind source are
included and the tree is never silently flattened. (`clone_mount_tree()` is the separate entry point behind `open_tree(OPEN_TREE_CLONE)`.)

### Detached mounts and the new mount API

`MntNamespace::clone_mount_tree()` produces the detached tree behind `open_tree(OPEN_TREE_CLONE)`: it clones the mount at the
resolved path, then walks that mount's subtree and installs each clone under the clone of its parent, returning the root together with
every clone so the caller (the mount fd) keeps the whole tree alive. `create_detached_mount()` and `attach_detached_mount()` are the
pair behind `fsmount` and `move_mount`; the attach consumes the detached mount and restores it if the graft fails, so a failed move
does not spend the descriptor. `set_mount_attributes()` and `set_mount_attributes_on()` apply a translated `MOUNT_ATTR_*` set/clear
pair to the mount at a path or to a mount object directly (`AT_EMPTY_PATH`), optionally recursing into the subtree. The
`CAP_SYS_ADMIN` check lives in the syscall layer, which is the boundary that owns it; the namespace methods validate only mount
identity and flag legality.

### Pathname and Open Files

`VfsFile` captures the `Arc<Cred>` used at open time, corresponding to Linux `file::f_cred`. The path-based
`Path::truncate()` checks write permission using the `&Cred` passed at call time; the descriptor-based
`VfsFile::truncate()` verifies that the file was opened in write mode and then reuses the authority already established by open, without repeating
pathname DAC. `O_TRUNC` runs the same opened truncate path after the open permission check.

The access direction of a pipe/FIFO is read only from `VfsFile::mode`, and private data holds only the `PipeObject`.
`VfsFile::pipe_generation` corresponds to Linux `file->f_pipe`: opening a non-blocking read-only FIFO with no writer
records the current `w_counter`, and poll reports HUP only if a new writer generation was seen while this file was alive and
the current writer count is zero. The poll waiter registers `rd_wait`/`wr_wait` according to `f_mode`,
and the event mask requested by the user filters only the final readiness; it cannot cancel the wakeup sources required for HUP/ERR.
Both `r_counter` and `w_counter` are non-zero from the start, preserving the HUP semantics of anonymous pipes whose default `f_pipe` snapshot is zero.

Anonymous pipes and pathname FIFOs share the read/write core of `PipeObject`. Only the file-operation wrapper of a pathname FIFO
applies the unified atime policy after a successful external `read`/`read_iter`, and updates mtime/ctime after a successful `write`/`write_iter`; the internal
4 KiB chunks of iterated I/O do not each update the timestamps. `PIPE_BUF` atomicity is decided on the complete write request at the start of the call, and internal
staging chunks do not wrongly promote a large write to an atomic write; when a partial write occurs, the progress of the uncommitted source iterator
is rolled back. The atime hot path reads only `MountFlags` and `SuperBlockFlags`, does not call `statfs`,
and does not construct a full `Metadata` snapshot; the RELATIME and "equal to current time" early-exit predicates read the resident atime/mtime/ctime through
`InodeAttributeOperations::times()` under a single storage lock, corresponding to Linux
reading `inode->{i_atime,i_mtime,i_ctime}` directly rather than filling a `struct kstat`;
RELATIME uses Linux's `mtime/ctime >= atime` and "at least 24 hours" boundaries.

At the syscall boundary, rename rejects unknown bits with `RenameFlags::from_bits` and rejects mutually exclusive modes.
The VFS rename entry point checks the combination invariants again, and the filesystem helper then checks the subset it supports.

The default `FileOperations::read_iter` uses a page-sized kernel buffer to adapt the scalar read callback.
Once an iteration has already read some data, a later callback error is converted into the number of bytes completed, so that stream I/O
keeps POSIX partial-read semantics; if the first callback fails, the original error is preserved.
The default `write_iter` advances only the iterator and `ki_pos` by what the scalar write accepted, and a short write rolls back the remaining input;
`i_size` is published by `write_end` or by the filesystem callback that actually writes to the backing store.

`Kiocb::flags()` records the open file description's `O_DIRECT`, `O_DSYNC`, and
`O_SYNC` state in `KiocbFlags` when the control block is created, mirroring Linux
`iocb_flags()`. `O_SYNC` contains `O_DSYNC` in its encoding, so a synchronized request
sets both `SYNC` and `DSYNC`. A control block is built per operation, so an `F_SETFL`
change applies from the next operation. The flags are recorded only: no read or write
path dispatches on them or changes completion behavior yet.

`libfs::new_pseudo_super_block()` corresponds to Linux `init_pseudo()` and
`pseudo_fs_fill_super()`: it uniformly installs the shared simple `s_op`, `s_magic`, page-size geometry,
the standard private root inode, and the concrete filesystem's static `s_d_op`. The root dentry inherits this static table
when the superblock is published; a new dentry with an existing parent/superblock inherits the same reference when it is constructed or bound.
Dentry operation identity therefore needs no per-dentry `Arc`, no mutex, and no patching by the concrete
filesystem after `alloc_file_pseudo()`.

The `VfsInode` data lock serializes buffered writes and truncate. Shared-file
write faults do not take this exclusive lock: `VfsFile::page_mkwrite()` holds
address-space invalidate shared protection, then under the folio lock rechecks
EOF, calls the filesystem prepare hook, dirties the folio, and publishes the PTE.
`AddressSpace::writepages_control()` holds invalidate shared protection through
enumeration and submission; synchronous implementations also cover backing I/O.
Asynchronous filesystems must retain inode/mapping ownership separately and drain
requests before set-length preparation or physical release. Filesystem
`InodeOperations::set_len()` runs under invalidate exclusive protection and calls
`VfsInode::truncate_setsize()` once after backing preparation and before block
release. That entry publishes the sole `i_size`, unmaps views, truncates cached
folios, then unmaps again to remove private COW PTEs created during cache truncate.
Backing preparation must not publish `i_size` early, so the common boundary retains
the old/new EOF transition.
`VfsFile::fallocate()` dispatches the backend under the same lock shape (the data
lock and the invalidate exclusive lock) across both the backing change and the
visible-size publish: writeback samples the visible size per batch under the
invalidate shared lock, while fallocate-style backends commit the backing disk size
first and publish the visible size afterwards; without excluding concurrent
writeback, a batch carrying a stale visible-size snapshot would enter the backing
store after the disk-size advance and spuriously fail against the "writeback range
not yet visible" guard. A backend publishing growth inside the dispatch's
held-lock context must call `VfsInode::truncate_setsize()`; the self-locking
`update_size_after_backing_change()` / `grow_size_after_backing_write()` helpers
deadlock in that context. Before taking those locks, `VfsFile::fallocate()`
queries `FileOperations::supported_fallocate_modes()` and answers `EOPNOTSUPP`
for any requested mode bit outside the backend's declaration; the default empty
mask admits plain allocation (mode 0) and rejects every other mode, so backends
supporting only plain allocation need no override. The syscall layer consults the
same mask before dispatching: a backend that declares PUNCH_HOLE/ZERO_RANGE owns
those modes, and a mode it does not declare (kext4 today) is emulated locally by
zeroing the range, which frees no blocks.
A file without independent backing uses the inode operation's default truncate implementation; a filesystem with backing state
overrides it and completes its own preparation before calling `truncate_setsize()`.

`AddressSpaceOperations` provides folio read, writeback, cache-state and
per-folio buffered-write callbacks (`write_begin`/`write_end`); it no longer
goes through the byte-level `read_at`/`write_at` default forwarding. A backend
that wants batched buffered writes additionally exposes a
`BatchedWriteOperations` vtable through `batched_write_ops()`; the mapping
samples it once at construction, and `None` keeps every write on the per-folio
path. Byte reads that a filesystem needs can stay in its private backing helper,
and are reused explicitly by `read_folio` and `readahead`.

`simple_write_end` stays in `kvfs::libfs` and serves only ramfs/memfs-style aops.
A per-folio write-end of a block-device filesystem publishes the accepted `i_size` via `AddressSpace::write_end_set_size()`
within the same generic-write data-lock and copied-folio-lock critical section, instead of disguising ext4 semantics as a
libfs helper. Batched tail release publishes no size because a batched run never extends the visible size.

`WriteEndRequest` contains the matching `WriteBeginRequest` range and copied
byte count. The generic writer validates the range's exclusive end before
admission, and copying is bounded by the range length. `write_end_set_size`
compares the accepted end against the inode's current size under the caller's
inode data lock and publishes growth.
The hook returns at most the copied count and publishes a nonempty accepted
prefix before returning success. The generic writer can then advance its local
EOF bound for cache preparation and batch admission. Zero-byte cancellation
preserves the file length.

`AddressSpace` itself holds the object-id, the mapped views, the statically shared `AddressSpaceOperations` and
`i_pages: MappingPages`. This field embeds the cache container directly without a separate `Arc` allocation, corresponding to Linux
`address_space::i_pages`; Folio uses `Arc<Folio>` to keep the physical page, the stable identity and the internal page-lock lifetime together.
`read_folio` obtains the stable index from the Folio itself; the AddressSpace constructor no longer takes a mapping kind that only controls diagnostics.
`MappingPages` does not store length, object-id or views. `AddressSpace` holds the cache-invalidate
lock, used to serialize readahead insertion, writeback, page_mkwrite and truncate onto the same
mapping. Writeback reads the live EOF from the inode via an owner closure after each folio is locked.
The construction API accepts only `&'static dyn AddressSpaceOperations`, so aops are not an inode private object.
Backing-store callbacks borrow the sole filesystem-private inode only from the current `AddressSpace`'s `mapping->host`,
and then recover the superblock through the host's `i_sb`; the filesystem-private inode therefore does not need to store a parallel
inode identity or superblock back-pointer.
`InodeOperations::cache_residency()` supplies optional filesystem-owned admission
and removal callbacks once, before inode publication. Resident hits perform no
quota lookup or superblock recovery. The common cache contains no capacity policy.

Memory-file sparse reads and native copies share a memfs-owned reader. The generic
`AddressSpace::visit_resident_folios()` capability holds one invalidation read
guard for at most 31 pages and performs one bounded index snapshot. Initialized
resident hits then need only their folio lock. Retained initialization failures
are retried, not reported as holes. The callback samples EOF and selects data or
zeroes; no sparse policy or readahead is added to generic cache traversal.

`VfsFile::mapping()` borrows the counterpart of Linux `f_mapping`; MM does not touch the underlying storage, and explicitly clones the `Arc` only when it really needs a lifetime
that outlives the file borrow.

The generic buffered-write limit check reads only the maximum file size cached at `SuperBlock` creation.
A filesystem that needs to tighten the range further by inode format has `FileOperations::write_iter()` call
`generic_file_write_iter_with_prepare()`; this callback composes the filesystem policy, the generic size limit and automatic timestamp publication
within the same inode data critical section, executing before entering the page-cache write,
and does not push the open-file write policy down into the address-space operation.
An empty request also undergoes the policy check; a request that is rejected or trimmed to empty does not update timestamps.

Before a writable open of a non-special file enters the filesystem `open` callback, KVFS increments the
inode `write_count`; on success it records with `FMode::WRITER` that the current file holds that count, and
it decrements the count when the open fails or after `FileOperations::release()` returns. Therefore reading 1 in the release callback
means the current file is the last writer, matching the call order of `get_write_access()`/`FMODE_WRITER` in Linux `do_dentry_open()` and of `ext4_release_file()`. Currently KVFS
does not yet implement the negative count state caused by executable deny-write.

`WritebackControl` is defined by pagecache, and KVFS re-exports the same type. Scanning and submission borrow
the same control object synchronously; the budget is deducted by the number of pages handed to the backend, an error or rejection does not refund it, and a completed prefix does not change the budget. The `WritebackRangeResult` of the byte-range callback is an alias of
`Result<usize, (usize, VfsError)>`,
expressing a completed prefix or a completed prefix with an error, and no separate writeback outcome type is exported any more.

## Concurrency Model

A dentry's inode and children are protected by their own mutexes; the static operation table is published when it is first bound to a
superblock and never changes afterwards. A live dentry
holds a strong reference to its parent, while the parent's children map holds only weak indexes; the superblock dentry cache
strongly holds children that are still hashed, until unlink, rename, forget, and similar paths explicitly evict them.
This corresponds to the Linux dcache lifecycle, in which a child references its parent and a hashed dentry with zero external references can still stay resident in the cache,
while avoiding strong parent-child reference cycles. `DentryKey` stabilizes object identity by holding a weak reference to the parent,
so no extra ID needs to be allocated for each dentry. The position and private data of `VfsFile` use mutexes;
the plain read/write entry points of a stream file do not acquire the position mutex and do not advance the file offset, which prevents a blocking read
from serializing write operations on the same open file; non-stream files still lock and update the shared offset.
`f_flags` and `f_mode` are stored as atomic integers, and the inode `write_count` is updated atomically by the open and release
paths. bitflags types are semantic snapshots copied by value and introduce no extra locks or allocations. `VfsFile::f_cred` is an `Arc<Cred>` that does not change after creation, so it needs no extra lock.
The FIEMAP writer is borrowed only for the duration of a single synchronous call and is not stored by the VFS or the filesystem. The inode capability holds
the shared lock of the corresponding inode across the whole callback, which prevents mapping modifications from being inserted after the `SYNC` writeback and before the extent walk;
buffered write, shared write fault and truncate all update mapping state from the exclusive side,
without widening into a filesystem-mount-level write block.

A dynamic filesystem whose namespace is driven by an external registry reports out-of-band additions and removals through
`Dentry::invalidate_cached_child()`. This interface evicts both positive and negative children under the parent directory's namespace
exclusive lock; a directory child additionally has its cached descendants forgotten recursively.
A filesystem should invalidate only the highest-level path that actually changed, so as to preserve the dentry
identity of unchanged directories and their mountpoints.

The superblock default dentry operations use a static reference, corresponding to Linux `s_d_op`; a dentry stores only the inherited
table identity and does not turn the operation object into dentry-private runtime state.

The filesystem type registry is protected by a dedicated mutex. Built-in types are written during the serial boot-CPU phase; lookup by name
copies a static descriptor while holding the lock, and enumeration takes a snapshot of the list and then formats the output, without creating a
superblock or executing filesystem code while the lock is held.

Namespace callbacks run under the inode namespace lock. A filesystem should synchronize the core
mutation result into the affected live inodes before the callback returns, and should complete the inode attachment of the created
object through `LockedDentry::instantiate()`; removal/rebind in the dentry cache is still done by KVFS after the callback returns successfully.

The global namespace lock order is:

```text
mount-namespace registry mutex (mount tree operations only)
    -> superblock rename mutex (rename only)
    -> parent-directory namespace locks
    -> child-directory namespace locks
    -> non-directory namespace locks (pointer order)
    -> per-dentry parallel-lookup mutex
    -> mount topology lock
    -> dentry cache and location locks
```

A detach transaction releases the namespace registry mutex before dropping the `Arc<Mount>` values it collected.
The superblock lifecycle corresponds to Linux `SB_BORN`, `s_active`, `SB_DYING` and `SB_DEAD`: a nascent
block-backed instance can be found by `sget_dev` but cannot be activated; concurrent callers wait for the fill to succeed or fail. A non-last reference is simply
decremented; a last-reference candidate keeps its count, releases the lifecycle lock together with it, revalidates after acquiring the umount lock, and then
switches to dying. The shutdown callback runs without holding the lifecycle lock, which prevents inode eviction
from re-entering mount release in the reverse direction; a dying/dead superblock cannot obtain a new active reference. Final shutdown publishes dead, releases the block claim and removes the identity
within the same registry critical section, and only then wakes the waiters. Explicit filesystem
sync and the global sync snapshot of the Weak registry also acquire the umount lock, so they cannot interleave with final dcache
eviction, and after acquiring the lock they skip superblocks that are already dying/dead. Shutdown does not acquire the
mount-namespace registry mutex in the reverse direction.

Both mount operations and the namespace validator run in the order with the inode namespace lock outside and the mount topology lock inside,
so mountpoint checks do not introduce reverse nesting. The parallel-lookup mutex serializes only the filesystem lookup callback of the same hashed
candidate, and does not serialize slow lookups of different names.
`SuperBlock::rename_mutex` corresponds to Linux `s_vfs_rename_mutex` and is not the dcache rename
seqlock; it is acquired only in cross-directory rename. `VfsInode::namespace_lock` expresses the
namespace-related subset of Linux `inode->i_rwsem`. `DentryLocation` uses a single `RwLock`
to protect both `parent` and `name`, so readers never observe a mixed location.

The lock order of a pathname FIFO is the inode `fifo_pipe` slot lock outside and the `PipeObject` state lock
inside. release first decrements the reader/writer count under the pipe state lock, and after releasing that lock enters the slot lock to complete the
`files`/slot lifecycle update; forming reverse nesting is forbidden. The buffer, the reader/writer counts,
`r_counter/w_counter` and the poll state are protected only by the `PipeObject` state lock; waiter wake-ups
are performed after the state lock is released.

The child cache still uses a mutex. RCU, seqcount lookup and lock-free dcache traversal remain out of scope until the current
lock model and regression coverage are stable.

## Design Decisions

- The ABI carrier is separate from the kernel semantic types, and conversion stays as close to the boundary as possible.
- FIEMAP connects the ABI and the inode operation through `FiemapExtentInfo` and a safe writer, avoiding passing the user
  pointer down, and avoiding allocating a temporary extent array in the VFS according to the user-declared capacity.
- No generic raw-flags getter is provided; `bits()` is called only when writing to the ABI or the underlying storage.
- Different flag families do not share integer aliases, so wrong combinations fail at compile time.
- Current task lookup exists only in `kprocess`; `kvfs` explicitly takes `&Cred` for all security-related operations.
- `Nameidata` does not store credentials, to avoid turning one call's context into redundant object state.
- Generic DAC, parent-directory modification checks, the sticky policy and the initial owner are implemented uniformly by the VFS, and the backend is responsible only for its
  own metadata and operation callbacks.
- An open file stores `f_cred`; descriptor permission and pathname permission are expressed through different entry points.
- The `FileOperations::open()` of a pathname special file must preserve the `Path` already resolved by namei;
  FIFO uses a shared stateless operation table, and the runtime session is held by the inode's typed pipe slot;
  per-inode stateful fops, global factories, raw-pointer keys, or anonymous inodes replacing the
  original opened-file identity are forbidden.
- Explicit pre-initialization of concrete pseudo filesystems is owned by their respective `fs/filesystems` crates; KVFS provides only
  the generic object mechanisms needed by hidden mounts and provides no concrete filesystem facade.
- `FileSystemType` keeps Linux's "type descriptor + global registry" layering; POSIX mount does not depend on
  concrete filesystem crates, and `/proc/filesystems` maintains no parallel name table either.
- No filesystem-private target field is added for an immutable simple symlink; the cached-link is the sole target
  owner, and the operation table only identifies and reads that inode state.
- `InodeOperations::set_len()` must go through `VfsInode::truncate_setsize()`; a filesystem must not compose
  i_size, cache resize and mmap invalidation by itself, nor establish a second EOF/cache owner.
- `FileOperations::fallocate()` must publish visible-size growth through
  `VfsInode::truncate_setsize()`; dispatch already holds the data and invalidate
  locks, so the self-locking publish helpers deadlock in that context.
- `VfsInode` directly stores the sole `Arc<AddressSpace>`, with no single-field address-space wrapper added;
  `VfsFile` stores a Linux-style `f_mapping` reference, and the MM runtime stores only `VfsFile`.
- Locks are held by semantic VFS objects, not by the filesystem bridge.
- `RenameData` corresponds to Linux `struct renamedata`; its one-shot `execute` consumes the operation object and drives
  the VFS orchestration; helper methods only borrow that object and provide no `Clone`/`Copy` for it.
- The callback closure is a narrow interface between the Path policy and the in-lock Dentry final lookup; it adds no persistent
  state and keeps no lookup result outside the lock; the parent inode is resolved by Path itself, and the caller cannot pass an
  inconsistent Path/inode/dentry combination.
- `mknodat` drives the above transaction directly from `Filename`, adding no syscall-specific type or
  transaction state structure; `Umode::mknod_node_type()` only decodes the type bits,
  and the namei-layer `may_mknod()` assigns the Linux errors uniformly and returns the validated `NodeType`.
  The syscall boundary writes that type back into the existing `Umode` before starting `dirfd`/pathname resolution.
- The namespace lock uses a blocking lock, because filesystem callbacks may perform I/O.

### SimpleFile Write Modes

`SimpleFileOps` rejects writes by default; when a writable file is needed, the concrete operation object implements the write semantics.
The read callback of `CommandFile` must return `Some(content)`; `Ok(None)` is converted to
`VfsError::InvalidData`, errors from the callback itself are propagated unchanged, and empty content uses `Some(Vec::new())`.
`CommandFile` expresses command-style pseudo files such as those in procfs/cgroupfs. A single write/writev
request is first copied in full into a kernel buffer of at most 4096 bytes, and then handed to the callback as one command; the file offset
and earlier writes take no part in assembling the command. The callback also receives the `VfsFile` that stores the opener credential, so
descriptor-based authorization does not need to query the current task in the reverse direction. `SimpleFile` dispatches the write algorithm according to the concrete operation object's
`write_iter`, and does not select it through an extra mode field.

`SimpleDirOps::mkdir()` receives the parent inode already resolved and locked by the VFS, the prepared mode, and the credential of the same
pathname operation; `rmdir()` receives the parent inode and the locked victim dentry. A dynamic
filesystem must commit the backend mutation based on these objects, and must not look up again by bare name or query the ambient
credential. `child_names()` may propagate lifecycle or backend errors, so that readdir on an old directory fd is not
silently converted into an empty directory. A pseudo filesystem that needs persistent identity can reuse inodes with `SimpleFile::new_inode()`,
`SimpleDir::new_inode_with_owner()` and `SimpleDirLookup::{file,dir}_from_inode()`.

### SimpleDir Dynamic Directory Link Count

`SimpleDirOps::link_count()` provides an optional dynamic nlink for the whole directory. `getattr()` first obtains a copy of the
`SimpleFsNode` metadata, and calls the callback only after releasing its metadata lock; `Some(nlink)`
overrides the link count only for this return, `None` keeps the original value in the copy, and callback errors are propagated upward.
`ChainedDirOps` does not merge the link counts of its component directories and keeps the `None` default. The procfs task directory
uses this interface to read the thread members of the target process on every query; an already-open directory fd also gets the newly queried value.
Other metadata fields and the directory lookup/iteration paths keep their existing behavior.
The dynamic value belongs only to the copy returned by getattr and is not written back to the `SimpleFsNode` or `VfsInode` cache.
Callers that need the current nlink should go through getattr; `metadata()` keeps its cached semantics. Directory enumeration takes only
inode/type from the cache and does not use nlink. The newly added guest definitions cover Some dynamic changes, the default None,
error propagation, other fields, and cache preservation; these definitions have not yet been compiled or executed.

### Buffered read and synchronous readahead

Cached reads reuse `FolioBatch` to collect consecutive resident references in batches, with at most 31 pages per batch;
the cached reference batch is independent of the per-open readahead window, and on a missing page the synchronous readahead protocol is used,
after which collection is redone. Batch references only keep the allocation alive; when copying page by page, invalidated or non-uptodate pages are rejected,
falling back to resident lookup and on-demand initialization. Subsequent batches collect only the remaining range of the current read request.

`VfsFile` holds a `Mutex<FileRaState>`; independent open / alloc_clone initialize independent history,
and dup/fork references sharing the same open file description share the history. The state stores only the window start page, the page count,
the length of the early-trigger zone, the shrinkable page budget and the byte position of completed reads, and stores no EOF or folio reference.
When a file is opened, the readahead budget is obtained from `Gendisk::bdi` of the canonical block device of the owning superblock;
a block-device special file resolves that device directly, and a file without a device uses the 128 KiB default. The budget is snapshotted only at open time,
and existing fds are not affected by a later BLKRASET. Sequential reads build the window by request size, and grow it up to the limit after consuming the folio readahead mark.
A small random fault prepares only the requested range and does not overwrite the sequential window; when necessary the window is rebuilt from the cached bounded contiguous history.
An open instance that only allows readahead may consume the shared mark; an instance with a zero budget leaves the mark for other readers.
Other open instances that allow readahead may also consume the shared mark and restore their own window from the current cache. When an on-demand read finally
returns EIO, that open instance's budget shrinks to a quarter, and once it drops to zero only one demanded page is prepared.
When only the mark hits, the policy lock is taken inside the folio lock, making the budget check, mark consumption and window computation a single decision;
an ordinary hit does not take the policy lock per page. The policy lock does not cover folio waits, user copies or filesystem I/O.
Early triggering describes when the window advances. Current backends still read
synchronously, but the interface allows initialization ownership to be transferred
without introducing a background task.

The window budget is independent of the batch capacity; a single-page missing segment and the single-page tail segment after batching are also initialized in this round.
When dispatching, if there is only one reserved page, `read_folio` is called directly, avoiding allocating a readahead bounce buffer for single-page I/O.
The window is submitted as contiguous missing ranges of at most 31 pages per
batch. Private folios are allocated and locked outside the index lock, then a
contiguous prefix is rechecked and published under that lock. I/O runs after the
index lock is released. `ReadaheadControl` borrows the AddressSpace and reservation
iterator; `next_folio()` transfers a `FolioRead` owning the page reference and
responsibility for its held lock, without a separate request index.
Default readahead consumes existing reservations through per-page `read_folio`
calls and continues after submission failures. The completer publishes uptodate
and unlocks the folio.
When the first candidate is inserted concurrently, the window cursor only skips past that position and continues querying the following missing pages.
The cursor consumes the current page when it attempts initialization; the filesystem provides a complete page with holes and zero-filled EOF tail, and then it is marked uptodate.
Consumed failed pages may be kept for on-demand retry; the suffix not consumed by the callback is cleaned up on exit, so that empty readahead or an early return
does not leave useless resident pages. Cleanup keeps the lock of the unconsumed page, takes the index lock to recheck identity and unlink it, and then unlocks;
after waiting readers wake, they look up the resident page again and do not continue initializing on an already unlinked reserved page. A read failure exposes no partial data, and the already copied prefix is returned first.

Demand reads also reserve an uninitialized folio before reading under its lock.
Competing readers wait for the folio lock and repeat the resident lookup;
successful pages need no duplicate I/O, while failed pages can be initialized
again. A successful `read_folio` return indicates submission only.
`FolioRead::complete` completes that read, publishing uptodate only on success;
failure or abandonment leaves the page unreadable.
Demand waits reject invalid sleeping context and may return `Interrupted` for
an uncatchable kill without taking ownership back from submitted I/O. A read
token describes one initialization attempt; a detached replacement may require
another call for the same index. Buffered writes validate context before
`write_begin`, and size changes do so before publishing EOF, keeping mandatory
rollback/truncation waits inside an admitted operation.
The filesystem is responsible for the read boundary and tail zeroing of this read, and the generic layer does not revalidate the returned byte count against the post-completion EOF.
The buffered fault path holds the invalidate shared lock from the EOF check to initialization completion, and the cache index lock does not cover I/O. The generic buffered copy holds no invalidate lock, and re-reads after locking each page
the inode EOF. The prepared end of a single read is used only to avoid issuing readahead repeatedly for a failed reservation, and does not guarantee cache residency.
Only newly published reserved pages carry the window mark; mark consumption and window decision complete inside the folio lock, and the subsequent window runs after the folio and the policy lock are released.

### Advisory file locks

`locks.rs` creates a `FileLockContext` on demand on `VfsInode`, corresponding to Linux `i_flctx`.
One mutex protects the POSIX/OFD range set and the separate flock set; the public `FileLock`
expresses only an immutable request and query results, while the internal `LockNode` holds the shared node. `LockList` and the non-cloneable
`OwnedLock` hand over the sole ownership of an installed range, and only the exclusive borrow of the latter can change the range; the `Waiter`
range is immutable for its whole lifetime, and `Blocker` only provides the identity of a wait relation. The notification list is held privately by `WakeQueue`; the lists uniformly use
`intrusive-collections::LinkedList` and the same node-embedded link field.
These thin types reuse the existing Arc/list, add no node fields, locks or heap allocations, and prevent shared handles from bypassing ownership
and linking into a list directly. Ranges of the same owner are merged, split and replaced; conflicting requests
form a wait tree by conflict along the specific blocker; on lock grant the successors are transferred, and on cancellation the node is unlinked directly and the next level is woken.
Installed ranges are protected by the inode lock, and the first merge object of the same type is extended in place and keeps its identity.
Notification reuses the request list node and the already registered waker, and close cleanup does not allocate; a request must not
reuse its node before the notification is dequeued. The global owner index is
registered by the POSIX type of the actual blocker (including OFD requests), and only POSIX requests perform a
bounded owner deadlock detection along the tree root, without copying the inode lock set. The traditional owner is the fd table identity, and the OFD/flock owner
is the open file identity; lock records do not strongly hold these two kinds of owners. `FileOperations` dispatches queries and
locking; closing a descriptor cleans up POSIX locks even if flush fails, and the final file release cleans up OFD/flock.

## Drop / Resource release

VFS objects manage their lifetimes through `Arc`/`Weak`. unlink, rename replacement and forget remove the dentry from the
parent children weak index and the superblock dentry cache; if Path/File references still exist,
the object stays alive, otherwise on release the references are returned level by level along the strong parent chain. Each `VfsMount` holds one
superblock active reference; when the last active mount is released, writeback, dcache eviction and
the final filesystem/device sync are performed. The final release of the superblock object's `Arc` is only responsible for dropping the already shut down
root and private state. The dentry cache and open files both hold `VfsInode` references; after the last reference disappears,
the hashed VFS inode identity table first publishes `Freeing`, and then gives `SuperBlockOperations::evict_inode()` the final teardown
opportunity. The default implementation only drops the cached folios in the inode `AddressSpace`, and a disk filesystem may append journaled inode reclamation
when nlink=0. After the hook returns, the cache removes the exactly matching old entry and wakes lookups of the same number;
a failure can only be logged, but it still must not hand the identity being released back to ordinary operations. The wakeup uses the wait queue of the removed slot itself,
and does not broadcast to other inodes. The single mutex of the identity table corresponds to Linux `inode_hash_lock`;
under the lock only the `(SuperBlock, inode number)` hash lookup and state transitions are done, while the initializer, waiting and the eviction hook
all run outside the lock. The key stores `Weak<SuperBlock>` and compares by its allocation address: it does not extend the lifetime of the superblock value,
but it keeps the `Arc` allocation until the slot is removed, so the address cannot be reused while the old identity is still hashed, and no new
superblock serial is needed. `VfsInode` likewise holds the superblock only weakly, to avoid a strong reference cycle with root/dcache;
filesystem callbacks perform a fallible upgrade on demand, and the bridge does not cache a second superblock owner.
An unhashed pseudo inode does not publish a `Freeing` slot, and its last reference runs the same eviction hook directly.
per-open
`FileOperations::release()` runs before final inode eviction and cannot replace the latter. The flags
type does not own resources; inode and filesystem-private resources are still the responsibility of the existing object lifetimes and filesystem callbacks.
When the last open file of a FIFO is closed, the inode pipe slot is cleared and the `Arc<PipeObject>` is
released with its last reference; hard links share the same active session because they share the same `VfsInode`.

## Known limitations

- The effective process capability set now covers DAC, sticky directories,
  xattr, chown/chmod, and related checks; LSM, ACL, user-namespace ID mapping,
  idmapped mounts, and file capabilities are not supported. Ambient
  capabilities are not a separate filesystem input; they affect a check only
  after becoming effective. `trusted.*` access and `security.*` mutation require
  effective `CAP_SYS_ADMIN`; user xattr modification in a sticky directory also
  requires directory ownership or effective `CAP_FOWNER`.
- Backends such as FAT that cannot natively express Unix UID/GID cannot fully persist the creator identity.
- The current POSIX rename path does not support `RENAME_WHITEOUT`.
- The superblock dentry cache has no Linux-style LRU/shrinker yet.
- Fast lookup is still mutex-based, without RCU or rename sequence validation; slow lookup already merges same-name concurrent lookups under the
  hashed candidate owner/waiter model.
- The cross-filesystem lock rank of layered filesystems has not been modeled.
- Mount topology synchronization and the superblock rename mutex are still different mechanisms.
- Filesystem types are currently all statically built-in implementations, with no Linux module autoload, reference counting or
  `unregister_filesystem()` lifecycle.
- `FsContext` passes only one page of opaque mount data; the concrete binary/text format and the supported option set belong to
  each filesystem type.

## Sequential record rendering errors

`SeqIterator::try_show` defaults to the existing `show` formatter and maps its
formatting failure to `Io`. Iterators that need a filesystem error may override
it, for example to preserve procfs `NoSuchProcess` when a weak target expires.
`SeqFile` discards the failed record and resets its iterator before returning the
error. Successful records use the existing per-open buffer and offset semantics;
no second snapshot or credential cache is introduced.

## Per-open generated-file snapshots

`SimpleFile` owns a file-operation object selected at construction. Ordinary
files use `GeneratedFileOperations`; `new_snapshot` selects the private
`SnapshotFileOperations`. Snapshot initialization, reads, write capability and
seeking belong to that object, without a mode flag in the generic simple file.
The generator remains shared with the inode's symlink operations.

Read-capable opens generate one immutable, private `SimpleFileSnapshot` in
`VfsFile::private_data`. Write-only opens neither generate nor retain bytes.
Separate opens capture independent generations. Normal descriptor sharing
retains the same file description and buffer; low-level clones that bypass
open are not supported initializers; reads and `SEEK_END` with missing state
return `InvalidData`.
The snapshot representation and constructor are not public API.

`SEEK_SET` and `SEEK_CUR` use only the descriptor position and work even for
write-only opens without generating content. `SEEK_END` uses that
open's immutable buffer length, never shared inode metadata. Negative results,
invalid whence and signed arithmetic overflow return `InvalidInput` without
changing the position. Seeking beyond EOF is allowed and reading there returns
zero. Tests cover differently sized generations, EOF, backward seeks and
invalid offsets. Snapshot operations do not advertise `CAN_WRITE`: write-mode
writes fail with `InvalidInput`; read-only writes fail with `BadFileDescriptor`.

## Pinned file pages for private mappings

`AddressSpace::with_pinned_file_page()` initializes a stable page object while
holding the invalidation read lock. The callback validates EOF and consumes the
pin through `map_readonly`, storing its optional `MappedFolio` content token
alongside an installed PTE. Before materialization, the provider rejects page
offsets at/beyond EOF and overflowing indices under the same lock, without
allocation or quota charge. The callback still validates its exact byte range;
its failure retains a valid initialized resident page and its cache quota for
ordinary reuse. Detachment later returns quota regardless of retained physical
references. Truncate submits
invalidation before detaching cache entries, but a busy
MM may queue that submission. Managed file PTEs retain independent physical
references through page-table retirement. Content tokens retain the stable
object for COW and can release their own references without waiting for TLB.

`PinnedFolio` and `MappedFolio` retain the same `Arc<Folio>` that owns its page
and content lock. Ordinary access uses a restricted `FolioGuard`; initialization
uses an owning `FolioRead` that can be transferred to completion;
cache replacement changes an index reference, not a published object's identity.
The original page remains copyable under its original lock after detachment,
without retaining its replacement. Copies do not by themselves prove current EOF
or freeze bytes against truncate. Pins and tokens share the stable page object;
its physical lifetime is independent of cache-residency quota.

## tmpfs quota integration with resident initialization

The inode address space retains the invalidate lock covering EOF, cache
reservation and read submission. The filesystem supplies one `CacheResidency`
provider when constructing the mapping. A winning cache identity is charged
before publication; removing that identity returns capacity once in the same
index transition. Pins, read batches, managed PTEs and retirement holds retain
physical pages independently and neither multiply nor extend filesystem quota.
Callbacks run in task context under the cache index lock and must not block,
reenter the mapping, or take locks whose holders wait for it. For memfs,
admission retains folio -> index -> quota limit ordering; removal decrements its
quota counter atomically. Remount/statfs never acquire cache locks. Speculative
physical storage is uncharged until cache publication.

The cache publishes non-uptodate reservations before filesystem I/O and keeps
its index lock out of I/O, preserving the current main concurrency contract.
Unconsumed readahead reservations detach while locked and return capacity at
that detachment, while their physical owners remain independently retained.


## Unrecoverable filesystem write failure

After an unrecoverable filesystem error, `SuperBlock::mark_read_only()` atomically sets the existing `RDONLY` bit,
preserving the other flags. It does not perform flush or transaction handling; the filesystem is responsible for stopping internal writes and for publishing the read-only state
before releasing mutation protection, so that the existing VFS write entry points and statfs export observe that state immediately.
The filesystem's reconfigure callback must reject restoring writability based on the existing failure state, and must not apply
other mount options before returning the error. `RDONLY` itself remains a mount flag and cannot replace the filesystem's internal write admission checks.

### Submission-based writeback batches

`AddressSpace::submit_cached_ranges` transfers bounded PageCache snapshots and
completion responsibility to the filesystem, sharing enumeration with synchronous
`writeback_cached_ranges`. Its budget counts pages handed to the callback; the
synchronous API uses the same attempted-page rule. Callback return does not mean completion.
The filesystem bounds in-flight work, drains I/O, completes folios in task context,
and protects inode/extent lifetime. Invalidate shared protection covers the call
and is not transferred to deferred completion. Filesystems therefore need their
own drain protocol. Raw `writepages` is submission in either mode: an
implementation may complete inline or return with owned I/O pending. Neither mode
expresses fsync/fdatasync intent. `WritebackControl` contains only the range,
submission mode, page budget and skipped count.

Cross-task completion uses `retain_writeback_completion` to retain the complete
`AddressSpace` and validate that the batch belongs to its embedded `i_pages`.
Synchronous completion may borrow the index; neither path creates an independent
index owner.

`AddressSpace` embeds its mapping `WritebackErrors`; `SuperBlock` embeds a separate
filesystem channel. Completion can borrow the mapping channel through its retained
VFS inode without allocating another Arc owner. Filesystems record real preparation,
submission, and completion errors in both channels before ending writeback or
dropping completion ownership. Capacity throttling is not an I/O failure.

Each channel keeps a generation, latest error, and observation flag. `record`
advances the generation and marks it unseen. `sample` skips only seen errors, so a
newly opened file still observes an unconsumed failure. `current_generation` reads
the current boundary without inheriting history or marking it seen. `check`
reports generation changes without consuming; `check_and_advance` marks the error
seen and advances only that observer.
Existing older cursors still see the failure. All operations require sleepable
task context, with no spinlock or IRQ/preemption-disable guard and no dependency
on a current process. A complete 2^64 generation wrap can alias an old cursor.

Each `VfsFile` samples separate mapping and filesystem cursors at open; fsync and
syncfs respectively check and advance them. Shared file descriptions share cursors;
independent opens do not consume each other's pending errors. Mapping observation
does not mark the filesystem channel seen. Successful retry never clears errors;
multiple unseen failures coalesce to the latest. Temporary synchronous writeback
passes snapshot `current_generation` before waiting for submission or existing
I/O, then use non-consuming checks for failures recorded during the pass. They
do not repeatedly inject old mapping history into syncfs after its filesystem
cursor has advanced. Historical errors remain pending for the appropriate file
cursor, including later opens while the mapping error is unseen. A direct error
takes precedence over recorded history, while required drains and cursor checks
still run. Superblock sync tries
all live inodes, all inode metadata, and the filesystem sync hook after an earlier
failure, retaining the first error. Filesystem abort checks remain authoritative,
and shutdown cannot skip remaining drain/flush stages because one stage failed.

## Buffered-write timestamp publication

Generic buffered writes and shared writable faults call `file_update_time()` outside folio
protection. `Kiocb::generic_file_write_iter_with_prepare` instead lets a filesystem
combine its admission checks and automatic timestamp publication in one callback.
It runs once under the inode data lock and before folio locking or data copying,
including for an empty request. It applies filesystem policy and generic size
limits in the required order, may narrow the accepted count, and must finish
fallible checks before publishing timestamps for a nonempty write; KVFS does not
repeat `file_update_time` for this path. This is a filesystem-owned preparation
contract, not an option for skipping timestamp semantics.
Filesystems may implement automatic `InodeOperations::update_time` with
resident attribute publication and deferred persistence; explicit setattr remains
a separate contract. The automatic callback owns comparison and publication and must
finish fallible checks before changing resident values. KVFS does not republish them.
The filesystem must retain metadata dirtiness through data-only
writeback and cover full fsync, syncfs/unmount and inode eviction.
## Generic quota ownership

Each `SuperBlock` owns one sleepable `Mutex<kquota::QuotaInfo<Arc<VfsInode>>>`. The state is
initially disabled for user, group and project quotas. `quota_info()` exposes
the canonical owner to filesystem integration; no filesystem-private duplicate
quota manager is needed. The core owns record caching, admission and quota
formats, while the concrete filesystem supplies direct backing-file I/O and
transactions. Each loaded class retains the canonical VFS inode in that
state, so filesystem integration needs no second class-to-inode array.
Suspension and failed disable retain that reference; successful disable returns
it for release after writeback and after leaving filesystem locks. Ext4 enables internal user/group/project usage accounting on mount.

Quota operations may sleep. Their backing-file I/O must bypass charging and
must not reenter the same quota owner or reacquire a caller-held filesystem
lock. Filesystem integration must establish one lock order and explicitly
flush/disable quota before final owner destruction.
The inode snapshot and eviction wait let a filesystem detach inode-held records
before releasing its quota cache. Snapshots are dropped without locks needed by
eviction, and the wait itself holds no filesystem mutation lock.

## Bounded private-fault readahead

`AddressSpace::readahead_for_fault` prefetches at most 16 missing file-cache
pages starting at a private mapping's fault offset. It is bounded by the inode's
visible length and an optional exclusive `file_end` supplied by the ELF loader.
A cached target does no work; only missing runs are submitted. The invalidate
lock is held for reading across EOF selection and submission, so concurrent
truncate cannot change the bound before stale pages are inserted. The helper
fills cache entries, not PTEs, and is optional: a returned error lets the caller
perform the original demand read.

## Final filesystem resource release

`SuperBlockOperations::put_super` runs after final ordinary inode writeback and
dcache eviction, under umount exclusion and without the lifecycle mutex. It lets
a filesystem disable quota and release other superblock-owned resources before
block-device ownership is dropped. Inode references must be dropped outside
filesystem mutation locks. `for_each_inode` visits a retained live-inode snapshot
without holding the registry lock, for serialized lifecycle initialization.
`FsContext::sync_filesystem` supplies writeback to reconfigure callbacks already
protected by umount exclusion, without recursively taking that lock.

## Read-only remount admission

The existing superblock umount lock is a read/write lock. Ordinary namespace,
attribute and xattr mutations hold its shared side through the backend call.
Writable open holds it until the existing inode write count is acquired;
that count remains held for the file's lifetime. Reconfigure and final shutdown
hold the exclusive side. Read-only remount pins the live inode snapshot,
waits for already-running cache evictions on their existing transition queues,
and rejects remaining writers or zero-link inodes with `EBUSY` before calling
the filesystem. Snapshot pins remain until the new flags are published.
This reuses inode counts and cache states without another removal counter.

The VFS cache binds and registers each successfully initialized inode before
cache publication. Filesystem inode constructors prepare identity and readable state;
write-dependent initialization belongs to writable-open and mutation callbacks
that participate in remount admission.

Special-file timestamp changes acquire shared write admission for the metadata
update because those files do not retain an inode writer count. After a
read-only policy is published, FIFO data I/O may continue while its timestamp
update is skipped. Ordinary writable files retain their existing admission
through the open-file writer count and do not recursively acquire this lock.

## Quota control and mutable inode policy

`SuperBlockOperations::quota_operations` selects filesystem control callbacks;
its default reports no support. `SuperBlock::quota_control` holds the existing
umount lock, validates lifecycle and class support, and invokes one command.
On/off use exclusive admission; other commands use shared admission.
`sync_for_quota` lets admitted callbacks drain writeback without reacquiring
shutdown exclusion. Quota state and retained files remain in `quota_info`.
`statfs` passes the queried dentry to the filesystem, allowing its result to
reflect a project-inherit directory's quota. File-attribute operations use the
inode's filesystem callbacks, with ownership and capability checks at the VFS
boundary and the inode mutation locks held across validation and publication.

`InodeAttributeOperations::flags` supplies filesystem-owned behavior flags from
resident state. The existing owned-attribute implementation stores construction
flags, while filesystem attribute implementations store their own flags. The
common inode wrapper retains no second behavior-flag field. This permits
quota-file protection to change without another attribute copy. Immutable policy is checked for write permission, cached
content writes, metadata changes, truncation, fallocate and namespace mutation;
NO_ATIME suppresses automatic access-time updates. File-data exclusion precedes
backend mutation locks when publishing protection.

`SuperBlockOperations::show_options` supplies filesystem-specific options to
proc mount reports. Mounted block-backed metadata uses the existing canonical
block-device identity, allowing tools to match st_dev to the source device.

Live inode cache invalidation is separate from final eviction. Under the
address-space invalidate write lock it synchronously revokes every registered
view's PTEs and completes TLB invalidation, then flushes dirty folios and removes
cache entries. A busy MM causes a retry after dropping that lock; no cached page
is detached while revocation remains deferred. Read faults publish shared PTEs
under the corresponding invalidate read lock.

`AddressSpace::invalidate_range()` is the byte-range counterpart for
backing-store transitions such as direct I/O. It holds the same invalidate write
lock, rounds the request outward to page boundaries so partial leading and
trailing pages are unmapped whole, and treats every covering view's synchronous
revocation as a precondition. A busy MM returns `Ok(false)` with no folio
discarded; a covering view without a notifier is rejected with
`OperationNotSupported`. Only after every view reports completion does it
discard clean folios in the range. A folio that stayed dirty is reported as
`ResourceBusy` after the clean ones were already discarded, matching Linux
partial invalidation.

Buffered `write()` and the mmap write fault publish their dirty state while
holding the folio lock, which cache discard also takes before marking a
detached identity, so neither path can lose a page between its residency check
and its dirty publication. The fault additionally holds the invalidate read
guard across its EOF recheck and PTE publication; `write()` runs under the
inode data lock instead, matching Linux `generic_perform_write`, which holds no
invalidate lock here. Truncate, range invalidation and mapping revocation all
take that data lock, so none of them interleaves with a buffered write.
Registered views cover page-aligned file ranges only: a partial tail
page is COW or zero-filled and owns no cache reference, so the view ends on the
page boundary instead of reaching into that anonymous tail. Executable images
register their private file views at load with a live invalidation handle, so the
same invalidation reaches text segments the loader maps directly from the cache.

Regular-file writes and writable open initialization hold shared superblock
lifecycle admission for the complete operation. Quota enable/disable hold its
exclusive side through writer initialization or reservation drain and teardown.
Shared write faults recheck inode mutability under a data read guard. MM callers
use nonblocking acquisition and retry a busy write fault outside the MM critical
section, preventing lock inversion with revocation.

Shared write-fault admission waits use the same superblock lifecycle and inode
data locks as the nonblocking fault checks. `VfsFile::wait_for_shared_mapping_write`
acquires them in that order outside MM serialization and releases them before
fresh fault lookup. It grants no write authority and does not cache policy.
The canonical lock wait queues allow the holder to run without polling budgets.


### Buffered reads into faultable destinations

Page-cache reads copy through `IovIterDest::copy_to_iter_nofault` while holding
the source folio. A short user copy advances the iterator and file offset only
by its successful prefix. After the folio and policy guards are released, the
read prepares the next destination prefix through `fault_in_writeable`. A retry
performs a fresh cache lookup and samples EOF under the folio lock again, so
invalidation or truncate during the wait cannot expose stale bytes. Permanent
faults return the completed prefix, or the error when no bytes were copied.
Kernel-memory short copies still finish the read without a fault retry.

An address-space view guard can extend its existing registration under MM
serialization. The registry mutex protects the VMA/object spans, preserving the
registration identity and invalidation notifier. Coordinates are validated before
publication; invalidation snapshots do not hold this mutex while entering MM.

## Bounded data write-and-wait

`AddressSpace::write_and_wait_range` samples the current mapping-error generation,
then captures the highest intersecting resident index under the existing page
index mutex. Clean pages carrying older WRITEBACK remain part of that target.
The helper clips the exclusive byte endpoint with overflow-safe arithmetic and
uses exactly that finite range for ALL submission and subsequent WRITEBACK wait.
A later append above the target cannot extend either stage. An empty resident
target bypasses raw submission; a file fsync still invokes the filesystem hook.
The bound is invocation-local and grants neither EOF authority nor a content
snapshot. There is no added persistent field, coordination owner or lock.

Submission errors do not skip waiting or the final nonconsuming error-generation
check; the first error remains authoritative. The invalidate read guard covers
submission only and is released before the generic wait. Filesystems still drain
their own detached/queued obligations before releasing backing mappings.

Live cache invalidation already owns invalidate exclusion for writing. It uses
the same data write-and-wait algorithm with direct submission under that existing
guard, before discarding cache pages. Target capture, completion waits and error
precedence are shared; failed data remains cached for retry.

`VfsFile::writeback_mapping`/`writeback_mapping_range`, inode/dentry sync, FIEMAP_SYNC
and superblock data sync use this data-completion boundary. They do not themselves
commit a journal or flush a device. Filesystem durability belongs to the file
fsync or superblock sync hook. Scan accounting and TOWRITE selection are owned by
pagecache; repeated dirtying through writable PTEs remains dependent on G.

### Buffered acceptance and live writeback EOF

A single-folio buffered write copies, dirties, accepts in `write_end` and
publishes visible i_size under one folio lock. A batched run of already-resident
pages copies and dirties each slot under its own folio lock, then releases the
whole run's unaccepted tail once through the batched-write tail-release hook
after those locks are released; the published count is decided by the generic
path and the release hook's return type cannot change it. The batch never
extends the visible size, so tail release publishes no size and the slot is
accepted as soon as it is dirty.
A successful-copy `write_end` hook may update reservations and inode metadata,
but cannot reacquire the folio, invalidate the cache or wait for writeback.
Cancellation releases only the units that the matching admission newly reserved,
never reservations owned by earlier writes. Copy-failure cancellation and
short-accept/error cache rollback run after releasing those guards. The writer
retains its inode data lock; completion never needs that lock.

All cache writeback adapters pass one control object and a live inode-size
supplier. Pagecache selects a finite target independently of EOF and samples
valid bytes after locking each candidate. No pending-size field, copied EOF,
pass owner or extra lifecycle lock is introduced. These buffered guarantees do
not capture stores through an already writable PTE.

## Direct I/O engine skeleton

`fs/kvfs/src/direct_io.rs` defines the backend contract for the generic direct
I/O engine. It mirrors Linux `iomap_begin`/`iomap_end`; the engine itself lives
in `kvfs` and will be wired into `read_iter`/`write_iter` in a later change.

- `DioMapping` returns one contiguous run with its full file bounds
  (`offset`, `length_bytes`) and device bounds (`device_offset`), not clamped
  to the request. `offset` is a file-space byte offset; `device_offset` is the
  backing device byte offset of the same first byte, so file byte `offset + k`
  maps to device byte `device_offset + k`. `DioMappingFlags` mirrors the flags
  a filesystem reports from `iomap_begin` (same bit values); the engine
  currently consumes `NEW`, which marks a run allocated by this write whose
  uncovered bytes must stay zero instead of exposing stale data.
- `DioDirection::{Read, Write}` states the request direction explicitly; Linux
  carries the same intent as the `IOMAP_WRITE` flag (no flag implies a read).
  On a write the filesystem chooses the concrete allocation form (initialized,
  unwritten, or fallback to buffered I/O) from its own format and inode state.
- `dio_end_io` is called exactly once per mapping returned by a write map
  call, including when `written_bytes` is zero. It settles
  `length_bytes - written_bytes` bytes (unreserve, keep unwritten), converts
  `[pos, pos + written_bytes)`, and extends the visible size to
  `pos + written_bytes`. Device errors are reported to the request issuer by
  the engine and do not need to be re-derived by the filesystem.
- The engine holds the inode data lock for the whole request (shared for reads
  and overwrites, exclusive when allocating or extending). `dio_map_blocks`
  runs under that lock; `dio_end_io` runs under the exclusive lock and
  publishes size with the `*_locked` helpers; the locking variants acquire the
  same lock and would deadlock there.
- Journal handles started by either callback must be stopped before the
  callback returns. `dio_map_blocks` must leave allocation metadata committed
  by then because the engine submits data bios only after it returns;
  `dio_end_io` must likewise finish its conversion and size publication before
  returning.
- No fixed per-bio byte limit exists. The engine submits one bio per mapping
  run and the block layer splits it by device limits, as buffered I/O and
  writeback do. The engine's owned bounce buffer may use an internal
  allocation stride, which does not affect splitting semantics.
- `dio_alignment` is the offset/length alignment in bytes; memory alignment is
  guaranteed by the engine's owned bounce buffer.

No call site uses the engine yet. `O_DIRECT` still falls back to buffered I/O,
and `FMode::CAN_ODIRECT` remains an informational open-time flag.
