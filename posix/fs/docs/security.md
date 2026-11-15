# posix-fs - Security and Reliability Analysis

## Overview

`posix-fs` is the boundary layer where user-space filesystem syscalls enter the kernel VFS, the fd table and device objects.
The main risks come from user pointers and path strings, file descriptor reuse, VFS magic-link follow,
the directory entry ABI layout, metadata permission semantics, mount flags compatibility and fd-to-fd data copying.

fd-backed kernel objects whose core is not filesystem state, such as `timerfd` and `eventfd`,
have been migrated to the standalone owner crate `kfd_objects`; this crate no longer bears responsibility for their state machines or security audit.

This crate contains a small amount of Rust `unsafe` code,
all of it used to construct Linux ABI structures, not to bypass VFS or fd table permissions.

## Trust Model

```text
user process
  │
  │ untrusted syscall arguments:
  │   path pointers, buffers, iovecs, fd numbers, flags, modes, offsets
  v
┌──────────────────────────────────────────────────────────┐
│ posix-fs                                                 │
│                                                          │
│ trust boundary                                           │
│  ├─ validates ABI flags and scalar ranges where needed    │
│  ├─ copies user strings/buffers through posix_types/osvm  │
│  ├─ resolves fd/path against current process context      │
│  ├─ delegates object semantics to kfd/kvfs/kfd_objects           │
│  └─ maps failures to KError/KResult                       │
└──────────────────────────────────────────────────────────┘
  │
  v
kfd resources / kvfs / device and pipe implementations
```

- User-space arguments are untrusted, including null pointers, invalid addresses, over-long paths, malicious flags, negative offsets and fd reuse.
- `posix-fs` trusts `posix_types` / `osvm` to perform bounds checks and error propagation on user addresses.
- `posix-fs` trusts `kfd` to remain concurrency-safe in fd lookup, duplication, closing and descriptor flags.
- `posix-fs` trusts `kvfs` / `kfd_objects` / device objects to perform the actual reads and writes and to implement the permission, directory and mount semantics.
- The current process context, the fd table, `FsStruct` and the user address space must come from the thread currently executing the syscall.

## External Boundaries / Attack Surface

| Boundary | Entry examples | Risk source |
|------|----------|----------|
| User path strings | `openat`, `linkat`, `statx`, `mount`, `move_mount` | Null pointers, strings not NUL-terminated, over-long strings, path traversal, symbolic link loops |
| mount data byte page | `mount(..., data)` | Bad pointers, crossing into unreadable pages, binary content, and malicious or unknown options interpreted by the specific filesystem |
| New mount API inputs | `fsopen`, `fsconfig`, `fsmount`, `move_mount`, `open_tree`, `mount_setattr` | Unregistered fs type, unknown commands/flags/`MOUNT_ATTR_*` bits, missing superblock, non-empty `from_path`, undersized `mount_attr`, unprivileged caller (`CAP_SYS_ADMIN` for all but the non-cloning `open_tree`) |
| User read/write buffers | `read`, `write`, `readlinkat`, `getdents64`, `statfs` | Bad addresses, short buffers, cross-page access failures, malformed kernel information written back |
| xattr names and values | `set/get/list/remove*xattr` | Missing NUL terminator, over-long name/value/list, non-UTF-8 suffix, short output buffers, namespace privilege violations |
| User iovec | `readv`, `writev`, `preadv2`, `pwritev2` | Too many iovecs, range overflow, wrong read/write direction |
| fd numbers | Almost all fd syscalls | Closed fds, type mismatch, fd reuse, confusion of `CLOEXEC`/non-blocking flags |
| Current process state | `chdir`, `openat`, `close_range` | Process resource lock, fs_struct updates, umask and current directory semantics |
| Mount namespace (new API) | `open_tree`, `mount_setattr` | Cloning or re-attribute a subtree before `CAP_SYS_ADMIN`; `O_PATH` vs read access; unknown `MOUNT_ATTR_*` bits |
| VFS magic link | `/proc/self/fd/<fd>`, `/proc/<pid>/fd/<fd>` | Cross-process fd access, target process exit, concurrent fd table changes, no-follow policy |
| VFS/device objects | `open`, `ioctl`, `mount`, `syncfs` | Device-specific ioctls, terminal objects, mount flags, differences between filesystem implementations |
| FIEMAP variable-length output | `ioctl(FS_IOC_FIEMAP)` | User-declared extent count, address of the array after the header, block mappings returned by the backend |
| fd-to-fd copying | `sendfile`, `copy_file_range`, `splice` | Wrong source/destination types, offset pointer TOCTOU, overlap within the same file, blocking semantics |

This crate does not directly access MMIO, PIO, DMA, firmware tables or architecture inline assembly.

## unsafe Code Inventory

| Location | Call path | Purpose | Invariant / safety rationale |
|------|----------|------|-------------------|
| `UserRead`/`UserWrite` implementations of `ioctl.rs::FiemapHeader` | `sys_ioctl(FS_IOC_FIEMAP)` | Copy the Linux FIEMAP header | The `repr(C)` structure contains only integers, the trailing explicit `reserved` covers the ABI alignment, and any bit pattern is valid; every byte belongs to an initialized field |
| `UserWrite` implementation of `ioctl.rs::FiemapExtent` | FIEMAP writer output | Copy a Linux FIEMAP extent | All ABI reserved regions of the `repr(C)` structure are covered by explicit integer arrays and zeroed on every output, so there is no uninitialized padding |
| `UserRead`/`UserWrite` implementations of `ioctl.rs::Fsxattr` | File-attribute ioctl | Copies the file-attribute ABI | `repr(C)` integers and explicit byte arrays cover all fields; reserved fields are zeroed on output |
| `notify.rs::entry_notify_wanted` (`transmute::<usize, WatchProbe>`) | `open`/`mkdir`/`mknod`/`symlink`/`link`/`unlink`/`rmdir`/`rename` entry, the "no watch anywhere" probe | Restores the installed `fn` pointer from its integer form | Only `set_watch_probe` writes the static, storing `probe as usize` -- the address of a `fn` item whose signature matches exactly -- so every non-zero value is such a code address; `0` is the "not installed" sentinel and is excluded before the cast; the `Release` store pairs with the `Acquire` load. Several syscall entries may install concurrently, but each stores the same typed value, so the invariant rests on the typed setter parameter, the sentinel and the ordering, not on a single writer. Failure consequence: if that invariant is broken the call jumps to a non-function address (UB), so no other path may write the static |
| `notify.rs::notify_entry` (`transmute::<usize, EntryNotifier>`) | `notify_entry_of_path` / `notify_entry`, directory-entry events | Restores the entry notifier the same way | Same argument: the only writer is `set_entry_notifier`, the signature matches the cast type, the `0` sentinel is excluded first, and the store/load pair is `Release`/`Acquire` |
| `notify.rs::notify_modified` (`transmute::<usize, ContentNotifier>`) | successful `write`/`writev`/positioned writes, `sendfile`/`copy_file_range`/`splice` with positive progress, `open(O_TRUNC)`, `truncate`/`ftruncate`, and `fallocate` -> `notify_modified` | Restores the installed content-event router | Only `set_content_notifier` writes `CONTENT`, storing a `ContentNotifier` function pointer as `usize`; the signature matches the cast type, `0` is the "not installed" sentinel and is excluded before the cast; the `Release` store pairs with the `Acquire` load. Concurrent installers store the same typed value. Failure consequence: if that invariant is broken the call jumps to a non-function address (UB), so no other path may write the static |
| `notify.rs::notify_self_object` (`transmute::<usize, SelfNotifier>`) | `link`'s `IN_ATTRIB`, self-events of removal and rename | Restores the self-event notifier | Same argument: the only writer is `set_self_notifier` |
| `notify.rs::retire_object` (`transmute::<usize, RetireNotifier>`) | retirement of an object's watches when it disappears | Restores the retirement notifier | Same argument: the only writer is `set_retire_notifier` |



A newly added `unsafe` block must satisfy all of the following:

- It is preceded by a `SAFETY:` comment;
- This inventory records the call path, the invariant being protected and the failure consequence;
- It does not pass user pointers raw to the underlying objects;
- It does not construct object references in a way that bypasses VFS/fd table permission checks.

### getcwd User Output Boundary

A NULL output address in `getcwd` is not a query mode; after the path and capacity checks are complete,
all actual output goes through `UserPtr::write_vm_slice`, and a bad address returns `EFAULT`.
The capacity is received as a `usize`, but only the actual path length is copied, so a huge capacity does not trigger an allocation of that size.
A short capacity returns `ERANGE` before any write, including when a NULL address and a short capacity occur together.
The regression tests use a separate process's `FsStruct` and mappings managed by `TestUserArray`,
and check that a short buffer is not written, that normal output does not go past the end of the path, and the error return for an unmapped address.

### New mount API input validation

The new mount API (`mount_api.rs`) validates every user-controlled branch before touching namespace state.
Five entries enforce `CAP_SYS_ADMIN` via `require_mount_privilege` (the same authority as legacy `mount(2)`);
`open_tree` requires it only on its cloning branch, because Linux calls `may_mount()` only for `detached`
(see that entry below). The legacy `mount` entry is described in its own row above:

- `fsopen`: `flags & !FSOPEN_CLOEXEC != 0` → `InvalidInput`; the fs type name is bounded by `PATH_MAX` and must be
  registered (`kvfs::get_filesystem_type`), otherwise `NoSuchDevice`. The fs-context fd is an `anon_inodefs` anonymous inode.
- `fsconfig`: unknown `cmd` → `InvalidInput`; `FSCONFIG_CMD_CREATE` is rejected with `InvalidInput` once a superblock already
  exists; the `source` key is stored as the mount source (not folded into the option string). Key/value strings are bounded by `PATH_MAX`.
- `fsmount`: `flags & !FSMOUNT_CLOEXEC != 0` → `InvalidInput`; the context must already hold a superblock, otherwise `InvalidInput`.
- `move_mount`: `flags & !MOVE_MOUNT_F_EMPTY_PATH != 0` → `InvalidInput`; a non-empty `from_path` → `InvalidInput`
  (only detached `fsmount` mounts are supported). The target path is resolved before the `fsmount` mount is consumed, so a
  failed lookup leaves the descriptor reusable; a failed attach restores the mount instead of spending the fd.
- `open_tree`: the flag word admits exactly `OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_EMPTY_PATH | AT_RECURSIVE |
  AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT`, and `AT_RECURSIVE` without `OPEN_TREE_CLONE` is `InvalidInput` — the same word and
  the same rejection as `SYSCALL_DEFINE3(open_tree)`. `OPEN_TREE_CLONE` (the only branch that clones a subtree) requires
  `CAP_SYS_ADMIN`; the non-cloning form does not, matching Linux, where only the `detached` branch calls `may_mount()`.
  The descriptor is opened `O_PATH`, so it names the mount without granting read access to what it covers.
- `mount_setattr`: the flag word admits only `AT_EMPTY_PATH | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT`
  (Linux rejects anything else); `usize` above `PAGE_SIZE_4K` is `E2BIG` and one below `MOUNT_ATTR_SIZE_VER0` (32) is
  `InvalidInput`; bytes past the published struct must be zero (`E2BIG`), so a caller relying on a field this kernel does not
  implement is refused instead of having its request truncated. An `attr_set`/`attr_clr` bit outside
  `MOUNT_SETATTR_VALID_FLAGS` is `InvalidInput` rather than a silently ignored request.
  `check_mount_attr_atime` applies the field's enum semantics: `MOUNT_ATTR__ATIME` must be all-or-nothing in `attr_clr`, an
  atime value in `attr_set` requires the group in `attr_clr`, and only RELATIME/NOATIME/STRICTATIME are representable, so a
  combination such as `NOATIME | STRICTATIME` is `InvalidInput` rather than being applied as relatime. `propagation` admits the
  four `MS_*` propagation flags, at most one per call. `MOUNT_ATTR_IDMAP` follows `build_mount_idmapped` exactly: clearing an
  idmap or an oversized descriptor is `InvalidInput`, an unresolvable descriptor is `BadFileDescriptor`, and a descriptor that
  resolves is `InvalidInput` because no descriptor in this kernel names a user namespace - so the request is refused at the same
  points Linux refuses it, rather than with an `EOPNOTSUPP` of its own. The whole call requires `CAP_SYS_ADMIN`, as `may_mount()` gates it in `SYSCALL_DEFINE5(mount_setattr)`.

## Memory Safety Invariants

1. **User memory is accessed only through wrapper types**:
   Syscall entries read and write user addresses using `UserConstPtr`, `UserPtr`, `VmBytes`, `VmBytesMut`
   and `IoVectorBuf`, and never dereference user pointers directly.
2. **Directory output uses bounded checked copies**:
   `getdents64` copies records through `UserPtr` without using the user capacity
   as an allocation size. A fault preserves earlier completed records.
3. **fd objects are held through `Arc<dyn FileLike>`**:
   An object obtained from fd lookup is kept alive by its reference count for the duration of the current operation.
4. **Type-dependent operations downcast and validate first**:
   Paths such as `ftruncate`, `fstatfs`, positional I/O and directory seek first confirm that the object is a `File` or
   `Directory`; pipe-specific operations confirm that a `PipeObject` exists in `VfsFile::private_data`,
   and a type mismatch returns an error.
5. **Ranges and overflow must be handled explicitly**:
   `fallocate` returns `InvalidInput` for negative offset/len and for `offset + len` overflow;
   the `lseek` directory offset uses `checked_add_signed`;
   positional I/O rejects negative offsets;
   fd-to-fd copying has the VFS update the kernel-side offset, which is then written back through `UserPtr`.
6. **Temporary ABI structures must not leak uninitialized memory**:
   `statfs` explicitly initializes all fields with a safe struct literal, and `f_spare` is zero;
   `getdents64` writes initialized fields, names and NUL bytes, leaving padding untouched.
7. **FIEMAP variable-length output must use checked arithmetic**:
   `fm_extent_count` is first bounded by the Linux UAPI total byte limit, and the header address, the array byte count and each entry's
   offset all use `checked_add`/`checked_mul`; the reserved fields of every output extent are explicitly zeroed.
8. **mount data may be passed down only as a bounded kernel byte page**:
   The syscall entry copies one opaque buffer of 4 KiB base-page size and forcibly zeroes the last byte, imposing no UTF-8
   or early-NUL constraint; `FsContext` does not store user pointers, and if a filesystem wants to retain
   options after `get_tree()` returns, it must copy only the parsed state.

### fchdir descriptor boundary

`fchdir` performs an fd table lookup for every input; negative values (including `AT_FDCWD`) and closed fds
return `EBADF`. A non-directory returns `ENOTDIR`; for a directory, execution continues with the search permission check of the current credentials.
The cwd is written only after full validation succeeds; the special base-directory markers of path syscalls are not treated as valid fds.

## Permission and semantic invariants

Read and write syscalls restart on an interrupt-request signal only when waiting on recognized files, and the conversion must happen after the completed amount is aggregated.
Not every EINTR may be upgraded, and the conversion must not be applied unconditionally in the low-level helpers shared by copy-type syscalls;
otherwise output that has already been committed could cause the entire request to be replayed. A positive completed amount and ordinary errors must not be overwritten.

A copy request first commits the actual completed amount, and only then writes back the user offset; an EFAULT on write-back may coexist with the already output
data, and the request must not be replayed on that basis. The read-ahead position of the source file is not published directly, and a pipe source keeps the unaccepted
suffix. Position lock ordering and pipe consumption are managed uniformly by KVFS, and syscalls do not hold these locks while writing back to user memory.
A zero completed amount does not always exempt the write-back: a sendfile transfer error, as well as EOF in sendfile/splice,
may still return EFAULT because the offset is not writable; copy_file_range writes back only after a positive completion.

1. `O_CLOEXEC`, `FD_CLOEXEC` and `close_range(CLOEXEC)` must affect only descriptor flags
   and must not change the underlying `FileLike` object. `close_range` validates the unsigned range at the syscall boundary
   before converting to signed fd numbers; accepting `UINT_MAX` must not let an empty range skip the requested file-table isolation.
2. `O_NONBLOCK` and `F_SETFL(O_NONBLOCK)` must modify the open file description's
   `VfsFile::f_flags`, and `nonblocking()` must be derived only from that field.
3. `AT_EMPTY_PATH` is a necessary condition for empty-path resolution; only an actual empty string combined with
   `AT_FDCWD` may select the cwd holding a reference, and all other cases look up the fd table.
   The internal `None` form keeps resolving by fd only, so that `fstat/fchmod/fchown/f*xattr`
   reusing the resolver do not accidentally gain the ability to access or modify the cwd.
4. A non-empty absolute `*at` pathname must ignore `dirfd`; only a relative pathname allows
   a `dirfd` lookup failure to affect the result.
5. `AT_SYMLINK_NOFOLLOW` / `O_NOFOLLOW` must affect symlinks and VFS magic-link follow.
6. `mount` must not silently ignore unsupported operation flags that change semantics.
7. An ordinary new mount may only look up registered filesystem types exactly; the syscall layer must not maintain a construction branch separate from
   `/proc/filesystems`.
8. Creating files and directories must apply the current umask in `FsStruct`; the written umask must be truncated to
   `0777`, and `CLONE_FS` must share it together with root/pwd.
9. A `chroot` target must be a directory, and the caller must hold effective
   `CAP_SYS_CHROOT`.
10. `chown/chmod/utimensat` must pass the same credential snapshot to VFS metadata
   authorization and must not call the backend `setattr` directly.
11. `F_ADD_SEALS` may only add memfd seals monotonically and must honor `F_SEAL_SEAL`.
12. Xattr input name/value must first be copied into a bounded kernel buffer; the cumulative length of the list name sink must not
    exceed `XATTR_LIST_MAX`. A `size == 0` query must not access the output pointer or materialize the name sequence, and a non-zero short buffer
    must return `ERANGE`; raw set flags must not enter KVFS.
13. `mount`/`umount2` must validate mount privilege with the one credential snapshot captured at the syscall entry before changing mount namespace state;
    an unauthorized call returns `EPERM` before mount point state validation,
    corresponding to Linux's `CAP_SYS_ADMIN` gate in `may_mount()`.
14. Mount authorization happens only at the syscall boundary: `mount_new`, `attach_bind`,
    `remount`, `reconfigure_mount` and `detach` of `kvfs::MntNamespace` keep explicit calling contracts and introduce no implicit
    current-process query; in-kernel callers are responsible for ensuring they are authorized.

### readlink length input boundary

The user-supplied `readlink` / `readlinkat` length is interpreted according to the Linux syscall `int` ABI,
rather than treating the whole register directly as a `usize`. A non-positive value returns `EINVAL`, without reading the pathname
or writing the output buffer; a positive-value copy still uses `UserPtr::write_vm_slice` and does not introduce raw pointer access.

### openat2 input validation

`sys_openat2` copies and validates the user `struct open_how` before it reads
the pathname or resolves the directory fd, so malformed requests fail with the
argument error rather than a later `EFAULT`/`EBADF`:

- `read_open_how`: `size < OPEN_HOW_SIZE_VER0` (24) is `EINVAL`; `size > PATH_MAX`
  is `E2BIG`; any non-zero byte past the known fields is `E2BIG`
  (`copy_struct_from_user` semantics).
- `Openat2How::validate` (kvfs `OpenHow::from_openat2`): unknown `flags` or
  `resolve` bits are `EINVAL`; `mode` bits outside `0o7777` are `EINVAL`; a
  non-zero `mode` without `O_CREAT` is `EINVAL`; `RESOLVE_BENEATH |
  RESOLVE_IN_ROOT` is `EINVAL`; `OPENAT2_REGULAR` with `O_PATH`/`O_DIRECTORY` is
  `EINVAL`; contradictory `O_DIRECTORY|O_CREAT` / invalid `O_PATH` combinations
  are `EINVAL` before the `RESOLVE_CACHED` rule; `RESOLVE_CACHED` with
  `O_CREAT`/`O_TRUNC` is `EAGAIN`.
- The pathname is bounded to `PATH_MAX - 1` payload bytes; a longer or
  unterminated name is `ENAMETOOLONG`, invalid UTF-8 is `EILSEQ`.
- `RESOLVE_BENEATH` with an absolute pathname is `EXDEV` before the dirfd is
  consulted.

## Thread safety

The metadata and mount ID of `statx` come from the same target held after a single resolution; when obtaining the identity,
the user path is not read again. The newly added field returns the existing integer mount ID, does not expose kernel pointers, and does not change
path authorization. Errors from resolution, getattr and user write-back propagate along the original `KResult` path, and the complete metadata is finally
written back through the existing `UserPtr::write_vm`.

The reserved request bits and mutually exclusive sync flags of `statx` are rejected before the user pathname is accessed and the target is looked up; an illegal request must not be reported as successful because a parameter was
silently ignored; these failures do not write the output buffer. Other unknown mask
bits are not rejected outright, in order to preserve the extensibility of Linux query requests.

| Resource | Concurrency condition | Risk |
|------|----------|------|
| fd table | Protected by the internal lock of the current process resources | A close/dup/lookup race can cause fd reuse or object leaks |
| `FsStruct` | Accessed through the process fs_struct lock | `chdir`, `chroot`, umask and relative path resolution run concurrently; `CLONE_FS` shares the same object |
| Directory offset | Protected by the `Directory::offset` lock | `getdents64` and directory `lseek` update it concurrently |
| `FileLike` object | Handled by the specific file, pipe or device implementation | Blocking and non-blocking state and offset update semantics |
| procfs magic-link target | The procfs/kfd snapshot holds a reference to the target `FileLike` | Lifetime when the target process exits or the fd is closed |

`posix-fs` does not define a global lock order across resources.
New code should shorten lock-holding intervals,
and avoid calling complex VFS/device operations that may access the same resource again while holding the directory offset or fd table lock.

## Threat Analysis

| ID | Threat Description | Boundary | Impact Level | Trigger Condition | Mitigation |
|------|----------|------|----------|----------|----------|
| T-01 | Bad user pointer causes kernel out-of-bounds read/write | User path/buffer/iovec | High | Directly dereferencing a user address or ignoring copy errors | Uniformly use `UserPtr`, `UserConstPtr`, `VmBytes`, `IoVectorBuf` and propagate `KResult` |
| T-02 | `getdents64` count causes kernel OOM or record overflow | User output / ABI | High | Treating user capacity as an allocation size, or advancing before copy succeeds | Allocation-free per-entry copies, aligned capacity checks and checked address arithmetic |
| T-03 | `statfs` leaks uninitialized kernel stack data to user space | User output buffer / ABI layout | High | C ABI struct padding uninitialized | `statfs` initializes all fields including `f_spare` with a struct literal; the four supported 64-bit ABI layouts have no implicit padding, and the layout and copyout must be rechecked when the ABI changes |
| T-04 | `/proc/<pid>/fd/<fd>` magic-link bypasses normal path resolution to access objects it should not | VFS magic-link | High | procfs follow does not preserve fd lifetime or lacks a permission model | The procfs fd entry holds the target object via an fd snapshot; follow to a non-VFS target returns unsupported; the cross-process permission policy still needs tightening |
| T-05 | `O_NOFOLLOW` ignored, so a symlink or magic-link is followed | Path flags | High | open/at flags not passed to the resolution layer | The syscall layer uniformly converts flags to `LookupFlags`, and `kvfs::namei` handles final/non-final symlinks and magic-links per `LookupIntent` |
| T-06 | `fallocate` offset/len overflow corrupts file size or range operations | scalar arguments / VFS | High | Negative values or `offset + len` overflow | Validate non-negative and use `checked_add` |
| T-07 | `copy_file_range` overlapping copy within the same file corrupts data | fd-to-fd copy | Medium | Same-file overlap check not implemented | Listed as a limitation in the design document; non-zero flags are rejected at the boundary; equivalence to Linux is currently not claimed |
| T-08 | mount flags layered incorrectly, filesystem type dispatch drifts, or unsupported semantics silently ignored | mount flags / filesystem type | High | User space combines move, shared/slave/unbindable propagation or the recursive-bind bit with bind/remount/new mount, or syscall/procfs each maintain their own support set | `path_mount` uniformly rejects unimplemented bits before any operation dispatch, then generates superblock/per-mount flags independently; `MS_PRIVATE[|MS_REC]` succeeds only on a mount root, since the KVFS mount tree inherently does not propagate; an initial bind inherits the source flags as Linux does and ignores ordinary request bits, while bind remount replaces the flags; an ordinary remount keeps only the default atime mask; new mounts and `/proc/filesystems` share the KVFS registry |
| T-09 | File-lock conflict or close race breaks application mutual exclusion | fd_ops / KVFS | Medium | Wrong owner, range, or close release | The inode lock context serializes range updates; blocking waits support signal cancellation and POSIX deadlock detection; close cleans up per files owner, and the fd is rechecked after locking |
| T-10 | Creating a file with the wrong owner causes wrong DAC semantics | open/create | High | Fixed root owner, or ignoring a setgid parent directory | The syscall passes the current `Cred`, and the backend initializes fsuid/fsgid and the inherited group with `inode_init_owner()` |
| T-11 | `faccessat2` permission check uses the wrong identity | access | High | Defaulting wrongly to filesystem IDs, or `AT_EACCESS` forcibly overriding an explicit fs ID | By default a real-ID credential is constructed; `AT_EACCESS` uses the current credential's `fsuid/fsgid` directly, for both the full traversal and the final check |
| T-12 | Device ioctl arguments misinterpreted | ioctl / device object | Medium | The syscall layer wrongly handles device-private commands | `FIONBIO` is handled in this layer and other commands are forwarded to `FileLike::ioctl`; common isatty probe errors do not flood warnings |
| T-13 | memfd seals forged through a non-shmem fd or a regular file | fcntl / shmem | Medium | `F_ADD_SEALS` / `F_GET_SEALS` do not validate the fd object type and inode state | fcntl first obtains the `kvfs::VfsFile`, and the shmem inode state then decides whether it exists |
| T-14 | fcntl duplication ignores the minimum number or exceeds the soft limit | fd_ops / kfd | Medium | The `F_DUPFD*` minimum is negative, over the limit, or the range is full | Validate the source FD first, then validate the Linux signed-int minimum and allocate within the soft limit and physical capacity; failure changes neither existing slots nor CLOEXEC. |
| T-14 | One pathname operation mixes multiple credential snapshots | syscall/VFS | High | Path components query the current task one by one while a credential is committed in between | The syscall entry obtains an `Arc<Cred>` only once and passes it along the whole operation |
| T-15 | `ftruncate` redoes pathname DAC wrongly after a UID change | fd/VFS | Medium | An fd operation calls `Path::truncate(cred)` | `VfsFile::truncate()` only verifies the open write mode, then uses the opened-file authority |
| T-16 | A non-owner bypasses the VFS to modify mode, owner or times directly | metadata | High | The syscall calls the backend `setattr` directly | The syscall only does ABI conversion, and `Path::chown/chmod/set_times` uniformly enforce owner, group and write authorization |
| T-17 | Unprivileged user changes the process root | chroot | High | Only target-directory searchability is checked | After directory DAC, additionally require effective `CAP_SYS_CHROOT` |
| T-18 | Unprivileged user creates a device node through `mknodat` | namespace / device | High | The special inode is created from the mode alone | Character/block devices require effective `CAP_MKNOD`; other node types still apply umask and VFS directory authorization |
| T-19 | Absolute `*at` path wrongly accesses an invalid `dirfd` | pathname / fd | Medium | `dirfd` is resolved before determining whether the pathname is absolute | All non-empty pathname entry points reuse `with_fs_at`; an absolute path forcibly selects the process cwd snapshot and KVFS starts from the root |
| T-20 | `mknodat` authorization outside the lock races with the final name state | namespace / device | High | The syscall does privilege/DAC first, and then the VFS looks up or creates | The syscall only converts the ABI; KVFS performs the final lookup, authorization, mode preparation and callback in positive-first order under the parent directory's exclusive lock |
| T-21 | Illegal mknod type masked by an invalid dirfd | syscall / fd | Medium | `may_mknod` runs only after entering `with_fs_at` | After copying the pathname, call the namei-layer `may_mknod()` immediately; resolving the relative-path `dirfd` is allowed only after validation succeeds |
| T-22 | umask has two owners or is polluted by high bits | process / fs context | High | process runtime and FsStruct each keep a copy, or `sys_umask` does not truncate | `FsStruct` is the sole owner, `replace_umask` uniformly applies `mask & 0777`; fork/clone and the pathname snapshot both copy or share that object |
| T-23 | A namespace syscall looks up the final name twice and operates on a different object of the same name | pathname / namespace | High | The syscall resolves the complete target first or confirms the target is absent outside the lock, then Path looks it up again by name | link/symlink/unlink/rmdir call the dedicated `Filename::*_at`; Dentry performs the single final lookup under the parent directory's exclusive lock |
| T-24 | `linkat` wrongly follows the source symlink by default or silently accepts unknown flags | pathname flags | Medium | The generic resolve helper follows by default, or the syscall only logs a warning | The syscall accepts only `AT_SYMLINK_FOLLOW | AT_EMPTY_PATH`; no-follow is used explicitly by default, and unknown bits return `EINVAL` |
| T-25 | FIFO open re-resolves the pathname after the first error | pathname/open | High | The target is renamed or replaced by a symlink between two lookups | The syscall open calls `Filename::open_with_flags_at` only once; FIFO dispatch completes on the resolved inode already authorized by KVFS |
| T-26 | FIEMAP count or address computation overflow causes out-of-bounds write or kernel data leak | ioctl / user output | High | Trusting `fm_extent_count`, using unchecked pointer arithmetic, or copying uninitialized reserved fields | Limit the maximum count, use checked arithmetic for all address operations, and explicitly initialize all fields of the output structure; user addresses are written only through `UserPtr` |
| T-27 | `listxattr` name aggregation overflows or a size query causes unbounded intermediate allocation | xattr / user output | Medium | Collecting attributes and values first, then building multiple complete name copies, or leaving the cumulative length unchecked | The KVFS borrowed-name sink writes directly into a single `XattrListWriter`; checked_add per item and `XATTR_LIST_MAX` limit, and `size == 0` only counts |
| T-28 | mount data causes unbounded user-memory reads, wrongly rejects binary data, or carries a transient user pointer into the filesystem | mount data / filesystem type | High | The generic layer parses a `void *` as a path string, or retains the user pointer in the backend | The syscall boundary copies one zero-filled 4 KiB opaque byte page and forcibly zeroes the last byte; `FsContext` only borrows a kernel slice, and the backend stores only the parsed mount state |

Impact level definitions:

- High: may lead to memory corruption, kernel information leaks, privilege escalation, or unauthorized access.
- Medium: may lead to data corruption, incorrect compatibility semantics, service unavailability, or broken application synchronization.
- Low: mainly leads to imprecise error codes, performance degradation, or log noise.

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Path read failure | The user pointer is invalid or the string is inaccessible | The syscall returns an address-related `KError` | The current operation fails | 3 | A `load_string` failure is propagated directly |
| F-02 | fd type mismatch | A file write on a directory, or a pipe operation on a non-pipe | Returns `BadFileDescriptor`, `InvalidInput`, or an underlying error | The application receives an error equivalent to the Linux errno | 3 | Use `get_file` followed by object type checks (for example `is_dir`) and downcast validation |
| F-03 | User output buffer too small | `getdents64` cannot fit even one record, or the `getcwd` size is insufficient | Returns `InvalidInput` or `OutOfRange` | The caller can retry with a larger buffer | 4 | Compute the length before writing |
| F-04 | Device open failure | The device file operations reject the open or fail to initialize | open returns an error | Terminal-related programs cannot open the target device | 3 | The error is propagated; device semantics are maintained by the corresponding device implementation |
| F-05 | The backend zero-fill write of `fallocate` returns 0 | The underlying filesystem cannot advance the write | Returns `WriteZero` | The operation fails, but the file must not loop forever | 2 | `write_zeros_range` detects a 0-byte write |
| F-06 | `sendfile`/`splice` encounters `WouldBlock` | A non-blocking source temporarily has no data | If part has been written, partial progress is returned; otherwise an error is returned | The caller can poll and retry | 4 | `kvfs::transfer_files` preserves partial-write semantics |
| F-07 | `mount` requests an unknown filesystem type or an invalid backing source | The name is not registered, or the source is not a usable block-special path | Returns the corresponding error such as `NoSuchDevice`, `ENOTBLK`, or `ENXIO` | User-space mount fails | 3 | Exact lookup in the KVFS registry; nodev/device-backed types all go through `FsContext`, and the device source is validated by `get_tree_bdev` |
| F-08 | The `syncfs` target is not a file or directory | The fd refers to a pipe/socket/device | Returns `InvalidInput` | The current sync request fails | 4 | Only flush filesystem objects after the downcast |
| F-09 | `copy_file_range` semantics are incomplete | The overlap and regular-file checks are TODO | Data results inconsistent with Linux are possible | Copy behavior of related applications is abnormal | 2 | Non-zero flags are explicitly rejected; the other restrictions need additional tests before implementation |
| F-10 | `fcntl` unsupported cmd returns success | Compatibility placeholder | The application wrongly concludes that some control operations have taken effect | Behavioral differences may result | 2 | A warning is logged; commands with security impact should be explicitly implemented or rejected |
| F-11 | `close_range(UNSHARE)` cannot obtain the files owner | The process has exited and has detached from the fd table owner | `unshare_fd_table` returns `NoSuchProcess` | The current syscall fails, and the detached fd table is not reinstalled | 3 | `sys_close_range` propagates the resource-layer error as a user-space syscall error through `?` |
| F-12 | FIEMAP output capacity is insufficient or a bad user page is encountered midway | The caller provides a small array or an unwritable address | Returns the count already collected or `BadAddress` | The current query fails, and the filesystem state is unchanged | 3 | `FiemapExtentInfo` stops normally once capacity is reached; the writer writes each entry through `UserPtr` and propagates copy faults |
| F-13 | Mount data copy failure | The first byte is unreadable, or the whole-page read fails and the byte-by-byte recovery also cannot read the first byte | `mount(2)` returns `BadAddress` before the superblock is created | The current mount does not happen and no partial topology is left behind | 3 | Try a whole-page copy first; on failure, recover the readable prefix and zero-fill the tail; the error is propagated only when zero bytes are readable |

Severity definitions:

- 1: Fatal; may cause a kernel crash, memory corruption, or unrecoverable data corruption.
- 2: Serious; functionality is unavailable, semantics are wrong, or a reboot/manual recovery is required.
- 3: Moderate; a single syscall fails or the application can degrade gracefully.
- 4: Minor; the caller can retry or the impact is limited.

## Fault Management

- All syscall entry points return `KResult<isize>`,
  and errors are mapped to Linux errno by the upper layer of `core/ksyscall`.
- Bad fds, bad paths, illegal flags, and unsupported modes preferentially return an explicit `KError`.
- I/O errors such as `WouldBlock`, `WriteZero`, and `BrokenPipe` are propagated by the underlying object.
- For common probing ioctl failures, for example `TCGETS` / `TIOCGWINSZ` on a non-terminal fd,
  no warning is logged, to avoid log noise.
- Unsupported `fcntl` arguments currently log a warning but return success;
  this is a compatibility placeholder and must not be extended to new commands with security impact. `F_ADD_SEALS` and
  `F_GET_SEALS` are already explicitly wired to the shmem object state.
- This crate has no unified retry mechanism;
  user space or the upper-level syscall scheduler is responsible for deciding whether to retry based on errno.

## Privacy Analysis

`posix-fs` handles pathnames, directory entry names, file content buffers, fd numbers, mount points, and device control parameters.
The module itself does not persist this data,
but debug logs record some paths, fds, flags, modes, and ioctl cmds.
New logging should avoid emitting file contents, user buffer data, or sensitive cross-process path relationships.

`read`, `write`, `sendfile`, `copy_file_range`, and `splice`
move user file contents within the kernel,
but the contents are not parsed by this crate or stored in global state.

## Known Limitations

1. Credential DAC uses effective capabilities and Linux mode semantics, but
   LSM, ACL, user-namespace ID mapping, and idmapped mounts are not implemented.
2. Backends such as FAT that cannot express a Unix owner cannot fully store the creator UID/GID.
3. File locks are advisory locks; they do not block reads and writes that do not take part in locking, and do not implement leases or network lock protocols.
4. `copy_file_range` regular-file type, same-inode overlap and cross-superblock admission checks are performed by the KVFS transfer layer; there is no zero-copy path.
5. `mount` supports registered nodev filesystems, block-backed types wired to `get_tree_bdev()`,
   non-recursive bind, ordinary read-only remount, bind remount, and handoff of one opaque mount data page for ordinary new mounts;
   it supports idempotent `MS_PRIVATE[|MS_REC]` and `MS_SLAVE[|MS_REC]` on a mount root (the two behave equivalently when there is no shared
   propagation group), and supports `pivot_root` and `umount2(MNT_DETACH)`; it does not support move,
   recursive bind with mounted subtrees (`MS_BIND|MS_REC` is accepted only when the source subtree has no mounts, otherwise it returns
   `EINVAL`), shared/unbindable propagation, filesystem-specific reconfigure, or force
   unmount. The generic layer's support for passing binary data does not mean a concrete filesystem implements the corresponding format or options.
6. `preadv2` / `pwritev2` currently support only `flags == 0`;
   non-zero flags return `Unsupported`.
7. memfd `F_ADD_SEALS` / `F_GET_SEALS` are wired in; shared writable mmap and
   `mprotect(PROT_WRITE)` seal enforcement are performed by `mm/filemap`.
8. POSIX ACLs and LSMs are not implemented; the final security policy for
   `security.*`/`system.*` is constrained by this gap. KExt4 currently exposes
   only ordinary `user.*`, `trusted.*`, and `security.*` xattrs, and KVFS
   requires effective `CAP_SYS_ADMIN` to set/remove `security.*` until complete
   concrete LSM hooks are connected.

## Audit Checklist

When modifying `posix-fs`, verify:

- [ ] New user pointer accesses go only through `UserPtr`, `UserConstPtr`, `VmBytes`, or `IoVectorBuf`.
- [ ] New `unsafe` blocks have a `SAFETY:` comment and are added to the unsafe inventory in this document.
- [ ] New path resolution entry points correctly handle absolute paths ignoring `dirfd`, `AT_EMPTY_PATH`,
  `AT_SYMLINK_NOFOLLOW`, and `AT_FDCWD`.
- [ ] New open/stat/namei entry points make explicit the difference between ordinary paths and procfd paths.
- [ ] A failed fd table modification does not leave a half-created object or wrong descriptor flags.
- [ ] Paths involving offset/len check negative values, overflow, and 0-length semantics.
- [ ] FIEMAP flags are explicitly supported at the ABI boundary or rejected according to the `EBADR` rule, and all reserved output fields are initialized.
- [ ] Cross-fd copy paths check source and destination types, blocking semantics, and the write-back of the user offset.
- [ ] Metadata and access changes stay in sync with the `kcred` credential model.
- [ ] Each multi-step pathname operation takes the credential snapshot only once and passes it through the entire resolution process.
- [ ] Pathname and descriptor operations do not confuse the caller's DAC at call time with the open-file authority.
- [ ] New mount/umount flags are either fully implemented or explicitly rejected.
- [ ] `MS_PRIVATE`/`MS_SLAVE` propagation requests are validated against their respective flag sets (`MS_REC`/`MS_SILENT` allowed,
  any other combination gives `EINVAL`); `MS_REC` is still rejected when not accompanied by `MS_BIND` or propagation bits.
- [ ] `pivot_root` completes all URL validation before the call, and, following `chroot_fs_refs()` semantics, redirects the `FsStruct` of every
  process whose root/pwd is exactly the old root to `new_root` (not only the caller); `umount2(MNT_DETACH)` detaches via `detach_tree` rather than silently unmounting synchronously.
- [ ] Is mount data copied with bounds at the syscall boundary as one page of opaque bytes, with the filesystem storing only the parsed state?
- [ ] New filesystem types are wired in through the KVFS registry, without adding concrete crate dependencies at the syscall layer or
  a name table separate from `/proc/filesystems`.
- [ ] New logging does not emit file contents or sensitive user buffers.
- [ ] Does the xattr list check the cumulative length item by item, use only one output buffer, and only count when `size == 0`?
- [ ] If a current known limitation is fixed, update this document and `design.md` accordingly.

The ioctl entry passes the current caller's credential snapshot through KVFS.
The block dispatcher authorizes `BLKROSET` using effective `CAP_SYS_ADMIN`
before copying the argument, returning EACCES on failure. The opening credential
does not authorize later ioctl callers.

Content-change notification uses inode event masks only to avoid unnecessary
routing work. Permissions remain checked at registration and the notifier
revalidates subscriptions under its own lock. Parent/name/linkage snapshots use
one location guard; callbacks run after it is released. The registry pins inode
objects through routing removal, preventing address reuse. Bind aliases share
object subscriptions. Special files do not report parent content events, and
`IN_EXCL_UNLINK` is applied per watch using the particular dentry's unlink state.

## Disk quota control

Commands, quota types, format IDs, record identities, information masks and
numeric ranges are validated at the syscall or core administrative boundary.
Classic unknown record-valid bits are ignored; information masks are strict.
Only selected input fields are converted, so irrelevant fields cannot reject an
otherwise valid update. Quota carriers explicitly represent ABI padding and
initialize every byte copied to user memory.

The current subjective credential authorizes changes and enumeration. Own-user
queries compare effective UID; group queries compare effective GID and
supplementary groups, independently of filesystem GID. Descriptor access
retains descriptor error ordering and target filesystem lifetime. Superblock
admission serializes quota enable/disable against reconfiguration and shutdown.
File-attribute ioctl copies use the typed user-memory interface. VFS checks
inode ownership and capabilities before passing validated attributes to the
filesystem; output padding is initialized before copyout.

Quota format and information queries build owned ABI snapshots under the quota
state mutex and release it before copying to user memory. Record and extended
status queries follow the same boundary: a user destination may fault into file
allocation and quota accounting, so no quota-state or record guard spans copyout.
