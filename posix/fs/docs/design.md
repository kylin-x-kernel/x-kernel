# posix-fs - Design Document

## Positioning

`posix-fs` is the POSIX/Linux filesystem syscall compatibility crate of x-kernel.
It converts user-space entry points such as `openat(2)`, `read(2)`, `write(2)`, `stat(2)`, `mkdir(2)`,
`link(2)`, `mount(2)`, `fcntl(2)`, and `ioctl(2)`
into operations on the kernel-internal fd table, per-process filesystem context, VFS nodes, and device objects.

The target audience is developers who maintain the `core/ksyscall` filesystem dispatch path, the `kfd` per-process fd table,
the `kvfs` VFS layer, and the terminal and device-file compatibility paths.

## Background

POSIX filesystem syscalls touch user pointers, path strings, the process current working directory,
file descriptors, VFS nodes, device nodes, and the interoperation paths with fd-backed objects all at once.
`posix-fs` gathers these Linux ABI details into a single crate,
so that the syscall dispatch table is only responsible for exporting entry points,
while the underlying VFS and fd table remain abstractions oriented toward kernel objects.

Fd-backed kernel objects such as `timerfd` and `eventfd`, whose core is not VFS or filesystem state,
are no longer owned by `posix-fs`, and have been migrated to the standalone owner crate `kfd_objects`.

The current implementation aims to be compatible with commonly used Linux user-space programs,
giving priority to covering the common paths and explicitly rejecting modes that are not yet supported.
File locks are implemented through the inode lock context of KVFS; capabilities not yet covered are recorded in the known limitations.

## Scope

Source files involved:

```text
posix/fs/
├── Cargo.toml
├── src/
│   ├── lib.rs          # crate entry point and syscall re-export
│   ├── path.rs         # AT_*, dirfd, VFS magic-link and empty-path resolution
│   ├── open.rs         # open/openat/openat2 argument conversion and fd install
│   ├── io.rs           # read/write/lseek/fallocate/sendfile/splice and other I/O
│   ├── fd_ops.rs       # close/dup/close_range/fcntl/flock
│   ├── dir.rs          # chdir/chroot/mkdir/getdents64/getcwd
│   ├── namei.rs        # link/unlink/symlink/readlink/rename
│   ├── notify.rs       # directory-entry notification hook for inotify
│   ├── metadata.rs     # chown/chmod/utime/utimensat
│   ├── xattr.rs        # set/get/list/remove xattr syscall families
│   ├── mount.rs        # mount/umount2
│   ├── stat.rs         # stat/fstatat/statx/access/statfs
│   ├── ioctl.rs        # ioctl dispatch, file attributes and FIEMAP ABI handling
│   ├── quota.rs        # quotactl/quotactl_fd (with quota/xfs.rs)
│   ├── sync.rs         # sync/syncfs
│   ├── file.rs         # stdio fd installation (add_stdio)
│   └── watch.rs        # watch target resolution (resolve_watch_target)
└── docs/
    ├── design.md
    └── security.md
```

## Architecture

```text
core/ksyscall
    │ dispatches filesystem syscalls
    v
┌──────────────────────────────────────────────────────────┐
│ posix-fs                                                 │
│                                                          │
│  ABI parsing                                             │
│   ├─ UserConstPtr/UserPtr string and buffer access        │
│   ├─ Linux flags, modes, stat/statfs/ioctl structures     │
│   └─ errno-oriented KResult handling                      │
│                                                          │
│  Process context                                         │
│   ├─ kprocess::current_user_process()                     │
│   ├─ kprocess::current_files()                            │
│   └─ fs_context::FsStruct { root, pwd, umask, in_exec }    │
│                                                          │
│  Internal object dispatch                                │
│   ├─ kfd::FileLike                                       │
│   ├─ kvfs::{Filename, VfsFile, MetadataUpdate, MountFlags}  │
│   ├─ kvfs::FileSystemType registry                         │
│   ├─ fs_context::FsStruct                                │
│   └─ kvfs::pipe::PipeObject interop for splice/fcntl     │
└──────────────────────────────────────────────────────────┘
```

| Submodule | Responsibility |
|--------|------|
| `path` | Uniformly handles absolute paths ignoring `dirfd`, `AT_FDCWD`, `AT_EMPTY_PATH`, `AT_SYMLINK_NOFOLLOW`, `O_NOFOLLOW` and VFS magic-link follow |
| `open` | Hands Linux open arguments to `kvfs::Filename` and installs the returned file into the current process fd table; does not re-resolve special-file pathnames. `openat2` additionally copies and validates `struct open_how` (`read_open_how`), enforces `RESOLVE_*` constraints via kvfs namei, and selects the dirfd/cwd base (a plain absolute pathname ignores dirfd) |
| `io` | Handles ordinary, vectored, positioned and fd-to-fd data-transfer syscalls |
| `fd_ops` | Maintains fd lifecycle, duplication, `CLOEXEC`, non-blocking flags and some `fcntl` behavior |
| `dir` | Maintains the current directory, root directory, directory/node creation and `linux_dirent64` output; `mknodat` only converts ABI arguments, and the creation policy is enforced by KVFS |
| `namei` | Handles namespace mutations such as links, deletion, symlinks and renames |
| `metadata` | Modifies owner, permissions and timestamps |
| `xattr` | Implements set/get/list/remove xattr for three target forms (pathname, no-follow and fd), copies bounded input names and values, and handles list size queries/`ERANGE` with a single bounded writer |
| `mount` | Maps Linux mount flags to `kvfs::MountFlags`, looks up the implementation by registered `FileSystemType`, and handles non-recursive bind and remount |
| `stat` | Converts VFS metadata, access checks and statfs information |
| `ioctl` | Handles `FIONBIO`, inode FIEMAP and file attributes, and forwards other commands to `FileLike::ioctl`; unrecognized commands return `ENOTTY` and are only logged with `debug!` |
| `sync` | Forwards sync requests to the filesystem, or to the filesystem of the open object |

## Content modification notifications

`notify::notify_modified` reports content changes to the parent-directory watch
with an entry name and to the file's own watches without a name. Writes and
transfers report once after positive request progress; empty and failed writes
do not report. Successful truncation, truncating opens of existing regular
files and allocation requests also report `IN_MODIFY`. `O_PATH` opens do not
truncate or report modifications, even with `O_TRUNC`.

Position and backend locks are released before delivery. Transfer offset
copyout follows notification, so a later user-memory fault does not retract
an already completed mutation. The installed content router uses inode event
masks to reject unrelated subscriptions before registry lookup. It snapshots the
actual dentry parent, name and unlink state together, copying a name only for an
interested parent. Unrelated writes can still inspect the local location lock.

The router suppresses special-file parent access/modify events and applies
`IN_EXCL_UNLINK` per watch to content events. Inode references identify watches
across bind aliases; namespace event records use the same object identity.
Concurrent registrations may or may not receive an event; registrations
completed before the mutation are included.

## Calling Constraints / Execution Context

`posix-fs` is a syscall-layer crate, and its entry points run by default in the context of the current user process thread.
Most functions depend on the following context:

- the current thread can be obtained through the current-thread interface of `kprocess`;
- the current process has an accessible process runtime state, fd resources, and `FsStruct`;
- user pointers can access the current address space through `posix_types` / `osvm`;
- the calling path is allowed to block, allocate, and enter the VFS, fd-backed objects, or device objects;
- the scheduler, process resource locks, VFS, and memory mappings have already been initialized.

This crate is not suitable for use as an API in interrupt context or in the early boot stage.
Path resolution, fd table access, VFS I/O, directory traversal, and fd-to-fd copying may all allocate,
take locks, wait on underlying objects, or return `WouldBlock`.

## State Machine

### statx Mount Identity

`sys_statx` reuses the target obtained by a single `resolve_at`, reads the metadata and
`VfsMount::mount_id()` at the same time, fills in `stx_mnt_id`, and declares `STATX_MNT_ID` valid.
For an empty-path fd and when following a procfs magic link, the mount identity of the target is returned, and the path is not re-resolved.
This field uses the existing mount ID and does not add to or change the mount lifecycle.

The `STATX__RESERVED` reserved bits in a `statx` request, and the combination that specifies both
`AT_STATX_FORCE_SYNC` / `AT_STATX_DONT_SYNC`, return
`EINVAL` before pathname copying and target resolution, which applies to ordinary pathnames, empty strings, and invalid pathname addresses.
This error takes precedence over `ENOENT` for an empty path and `EFAULT` for a user address. A zero mask and other unknown request bits
are still accepted as query hints, preserving forward compatibility of the interface; this validation does not change the sync policy or the result mask.

### fd Lifecycle

```text
open/pipe/dup
    │ add_file_like / duplicate_file_like
    v
Open fd
    │ F_SETFD / close_range(CLOEXEC)
    v
Open fd with descriptor flags
    │ close / close_range
    v
Closed fd
```

`posix-fs` does not store the fd table directly,
but operates on the current process resources through `kprocess::current_files()`.
### Path Resolution

```text
user path pointer
    │ load_string / nullable path handling
    v
raw path string or AT_EMPTY_PATH
    │ path.rs helpers
    ├─ normal path ────────────────> kvfs::namei lookup
    ├─ empty path + AT_EMPTY_PATH ─> cwd (AT_FDCWD) or dirfd fd object
    └─ magic-link component ───────> kvfs::namei typed follow
```

`path.rs` does not own procfd string parsing.
`/proc/<pid>/fd/<fd>` is exposed by procfs as a VFS magic-link node, and `path.rs`
is only responsible for converting syscall flags into `LookupFlags` and `LookupIntent`. The rules for ordinary symlinks, magic-links, `NO_MAGIC_LINKS`, and
`O_NOFOLLOW` on final and non-final components are all enforced uniformly by `kvfs::namei`.

For an empty string combined with `AT_EMPTY_PATH` and `dirfd == AT_FDCWD`, the `Path` of the current
directory is cloned through a `with_fs` snapshot, and then the lock is released before reading or modifying metadata; `.` is not re-resolved, and search permission on the
current directory is not required. `None` keeps the existing convention of resolving by fd only: `fstat/fchmod/fchown`
and the fd forms of the xattr interfaces reuse the resolver through it, and `AT_FDCWD` must not be mistaken for a real
descriptor. This change does not extend the range in which each syscall accepts a NULL user pathname.

### File Data Flow

For non-positional read and write requests, after the object and the iterator have aggregated the completed amount, only the explicit file-wait interruption marker is converted to
`ERESTARTSYS`, and signal delivery decides, based on the handler flags, whether to restart or return EINTR. The conversion is located in the whole-request
helper rather than in each syscall entry point: `read/write/readv/writev` and
`preadv2/pwritev2(offset=-1, flags=0)` share this exit. Positive values, zero, and ordinary errors are left unchanged.
Copy-type syscalls use the underlying VFS operations directly, and apply the same conversion only after the completed amount of the entire copy request has been aggregated;
if there is already output, a positive value is returned first, and a single underlying wait must not be escalated into a replay of the whole request. Positional I/O and objects that have not been wired to the marker
keep the original interruption contract.

```text
user buffer / iovec
    │ VmBytes / VmBytesMut / IoVectorBuf
    v
FileLike::read/write
    │
    ├─ kvfs::VfsFile offset or read_at/write_at
    ├─ kvfs::VfsFile directory position for lseek/getdents64
    ├─ kfd_objects::PipeReadEnd / PipeWriteEnd
    └─ device-specific FileLike implementation
```

`sendfile`, `copy_file_range`, and `splice` hand kernel-space offsets to
`kvfs::transfer_files`, and the VFS manages buffered transfer, file positions, and pipe consumption.
The syscall layer retains argument validation, writing back the user offset, and the restart decision for the whole request.
`sendfile` writes back the explicit offset after the transfer returns, including on EOF and errors;
`splice` writes back only after a non-negative result, destination first and then source, and returns on the first write-back error;
`copy_file_range` writes back only after a positive result, and attempts both write-backs.
A write-back failure may return EFAULT, but does not roll back output that has already occurred.

## Algorithm Flow

### `openat`

1. Read the path string from the user pointer.
2. Obtain a complete `FsStruct` snapshot containing root, pwd, and umask through `with_fs_at`.
3. Pass the Linux open flags, the raw mode, and the snapshot umask to the open entry point of `kvfs::Filename`;
   the umask is not applied early at the syscall layer.
4. `kvfs` internally constructs the namei open arguments and uses `LookupIntent::Open` and
   `LookupFlags` to handle `O_NOFOLLOW`, `O_PATH`, ordinary symlinks, and VFS
   magic-links.
5. For ordinary paths, call `with_fs_at(dirfd, filename, ...)`: an absolute path starts from root and does not access
   `dirfd`; only a relative path validates and uses `dirfd`.
6. The special open semantics of device nodes are handled by the corresponding VFS/device file operations.
7. Set the object and descriptor flags according to `O_NONBLOCK` and `O_CLOEXEC`.
8. Add the opened file to the current process's fd table and return the fd.

### `openat2`

1. `read_open_how` copies `struct open_how` with Linux `copy_struct_from_user`
   semantics: `size < OPEN_HOW_SIZE_VER0` is `EINVAL`, `size > PATH_MAX` is
   `E2BIG`, and non-zero reserved bytes are `E2BIG`.
2. `Openat2How::validate` applies the `build_open_flags` rules (unknown flag or
   resolve bits, mode outside `0o7777`, mode without `O_CREAT`,
   `BENEATH|IN_ROOT`, `OPENAT2_REGULAR` with `O_PATH`/`O_DIRECTORY`, and
   `RESOLVE_CACHED` with `O_CREAT`/`O_TRUNC`) before the pathname or dirfd is
   touched.
3. The base is the dirfd (or cwd for `AT_FDCWD`) only for a relative pathname or
   an anchored request; a plain absolute pathname uses the process root and
   ignores dirfd. For `RESOLVE_IN_ROOT`/`RESOLVE_BENEATH` the directory is passed
   as both root and base, and kvfs namei owns rebasing/clamping.
4. `Filename::open_with_openat2_at` runs the constrained namei walk and the
   opened file is installed into the fd table.

`RESOLVE_CACHED` is a cache-only constraint: a dentry-cache miss, or a request
with `O_CREAT`/`O_TRUNC`, returns `EAGAIN`. Symlink expansion whose target is not
cached may still consult the filesystem, so the cached-only guarantee is a
superset rather than a strict Linux `LOOKUP_CACHED` match.

### fd resolution in `fchdir`

`fchdir` obtains the directory object directly from the current resource table; it does not reuse `with_fs`, which provides the `AT_FDCWD`
semantics for path lookup. The obtained `Arc<VfsFile>` keeps the object alive; only after confirming the directory type and checking
search permission is the current process's cwd updated. An `O_PATH` directory descriptor is still usable; an invalid fd,
a non-directory, or a permission failure leaves cwd unchanged.

### `resolve_at`

1. `None` or an empty path must be accompanied by `AT_EMPTY_PATH`; otherwise `NotFound` is returned.
2. A non-empty path enters `kvfs::namei`, carrying the `LookupIntent` and
   `LookupFlags` that correspond to the syscall.
3. An absolute non-empty path does not read `dirfd`; a relative non-empty path uses `dirfd` to select the base.
4. namei handles final/non-final symlinks, magic-links,
   `AT_SYMLINK_NOFOLLOW`, and `NO_MAGIC_LINKS` within a single path walk.
5. When `Some("")`, `AT_EMPTY_PATH`, and `AT_FDCWD` all appear together, the cwd `Path` is cloned through the context snapshot of `with_fs`;
   `.` is not resolved, and no additional cwd search-permission check is performed.
   All other empty paths and the internal `None` form obtain the `Path` from the real fd.

### xattr syscall family

1. `setxattr/lsetxattr/fsetxattr`, `get*`, `list*`, and `remove*` share four semantic helpers;
   the pathname versions follow the final symlink, `l*` does not follow it, and `f*` uses the fd's `Path` directly.
2. An xattr name is copied as raw bytes subject to the Linux 255-byte limit, without requiring UTF-8; the set value and the list
   are each subject to a 64 KiB limit.
3. `XATTR_CREATE`/`XATTR_REPLACE` are converted to `XattrSetFlags` at the syscall boundary, and unknown bits return
   `EINVAL`; the two known bits may be set together, and the resulting four existence policies are enforced by the filesystem within the same mutation
   lock/transaction.
4. For `get/list`, `size == 0` returns only the required length; a non-zero buffer that is too small returns `ERANGE`, and the list output is
   a complete sequence of NUL-separated names. List writes the KVFS name sink directly into the single bounded kernel buffer;
   when `size == 0` it only accumulates the length and does not materialize the name sequence.
5. User pointers are copied only in `posix-fs`; KVFS and filesystem backends do not receive user addresses.

### `mkdirat`

`mkdirat` passes the raw permission bits and the `FsStruct` umask to
`Filename::mkdir_at()`. This entry point completes the final lookup under the parent directory's namespace lock, and then `Path::vfs_mkdir()` performs the mount check, parent directory DAC, mode preparation, and the filesystem callback.
If the path already resolves to an existing object, including directory paths such as `/`, `.`, and `..` that have no ordinary final component,
`AlreadyExists` is returned, corresponding to Linux `EEXIST`.

### `mknodat`

The syscall entry point only copies the pathname, and truncates or converts the 32-bit ABI mode/device into
`Umode` and `DeviceId`; before any `dirfd` access it then calls
`may_mknod()` in the namei layer to perform Linux type validation, and writes the validated `NodeType` back into the type bits of the
existing `Umode`. `Umode::mknod_node_type()` only performs pure type decoding and does not depend on
VFS errno. The normalized mode, the `FsStruct` umask, and the credential snapshot are passed to
`Filename::mknod_at()`:

1. The syscall boundary rejects directories, symbolic links, and invalid types according to the raw `S_IFMT` bits before path resolution;
2. The final lookup is completed under the parent directory's exclusive namespace lock;
3. A positive dentry returns `AlreadyExists` first, and only a negative dentry proceeds to check mount write,
   `MAY_WRITE | MAY_EXEC`, and the character/block device privilege;
4. It shares `Path::vfs_create()` / `Path::vfs_mknod()` with open-create, and prepares the mode in the order of setgid and then
   umask;
5. A regular file uses the exclusive create callback, FIFO/socket clear the device, and
   character/block devices retain the ABI device.

Creating a character or block device requires effective `CAP_MKNOD`.

### `linkat`, `symlinkat`, `unlinkat`

These namespace syscalls no longer perform a one-off final lookup outside the lock through the generic `create_at()` or a full target lookup.
The syscall copies and validates the ABI arguments, and then calls the corresponding
`Filename::link_at()`, `Filename::symlink_at()`, `Filename::unlink_at()`, or
`Filename::rmdir_at()`:

1. Filename resolves the parent and the type of the final component;
2. Dentry performs the single final lookup under the parent's exclusive namespace lock;
3. The errno for positive/negative, trailing slash, and special final components is decided on that final object;
4. Path enforces the mount, DAC, cross-mount, sticky, and mountpoint policies;
5. VfsInode only enters the filesystem callback.

`linkat` accepts only `AT_SYMLINK_FOLLOW | AT_EMPTY_PATH`; by default it does not follow a final symlink on the source,
and only `AT_SYMLINK_FOLLOW` enables following.

### exec path source

`execve` no longer parses the procfd string separately or provides a dedicated magic-link helper:

1. The syscall layer reads the user path and then constructs a `LookupContext`.
2. A non-empty path enters `kvfs::namei` with `LookupIntent::Exec` and the follow-final flags.
3. Ordinary paths, procfd magic-links, APK magic source style display paths, and the later
   `AT_EMPTY_PATH`/`fexecve` entry points should all converge on the same set of namei/open-executable
   semantics.
4. The syscall layer passes the resolved `Location` together with the user display path to
   `process/kexec::ExecRequest::from_resolved_with_display()`; the loader no longer looks up the target object again from the
   procfs string.

### `getcwd` output contract

`getcwd` follows the raw Linux syscall and takes the capacity as an unsigned machine word; it first produces the path including the NUL,
then compares the capacity, and finally copies through the user memory interface. Insufficient capacity returns `ERANGE`;
sufficient capacity but an invalid output address returns `EFAULT`. The libc `getcwd(NULL, 0)` automatic allocation
extension is implemented in user space, and the kernel never returns success for a NULL buffer.

`getcwd_tests` in `dir.rs` creates an independent process and user mappings through `#[def_test(user)]`,
and calls the syscall entry point directly to verify the capacity, the returned byte count, the write range, and the error priority;
the test cases are executed automatically by the existing kernel unittest flow.

### `getdents64`

1. Allocate a temporary kernel `DirBuffer` according to the length given by the user.
2. Obtain the directory fd and lock the directory offset.
3. Iterate over the directory entries and write each entry in the ABI `linux_dirent64` layout.
4. Stop iterating when the buffer cannot hold a complete record.
5. If directory entries exist but the user buffer cannot hold even one, return `InvalidInput`.
6. Write the temporary buffer back to user space and return the number of bytes actually written.

### `fallocate`

1. Validate `offset`/`len` as non-negative and check `offset + len` overflow.
2. Accept only the mode combinations the implementation supports; append-only
   inodes admit plain preallocation only and immutable inodes return `EPERM`,
   decided before the node-type dispatch and the `EFBIG` bound.
3. Plain preallocation (mode 0 / `KEEP_SIZE`) hands the full flags to the backend
   first. Only a backend that declares no fallocate capability (empty mask) is
   emulated on `EOPNOTSUPP` (a trailing zero-byte write for non-KEEP_SIZE, a
   no-op for KEEP_SIZE); a backend that declares modes but still answers
   `EOPNOTSUPP` is rejecting the file, so the error propagates instead of a
   silent success. A prefix committed by the backend, whether the call fully
   succeeded or partially failed, is published as the visible size by the VFS
   exit.
4. `UNSHARE_RANGE`, `COLLAPSE_RANGE`, `INSERT_RANGE` hand the flags to the
   backend, whose `supported_fallocate_modes` mask accepts them or returns
   `EOPNOTSUPP`.
5. `PUNCH_HOLE`, `ZERO_RANGE` route to the backend when it declares the mode;
   otherwise they fall back to chunked zero writes, which free no blocks and
   leave the range allocated.

### `FS_IOC_FIEMAP`

1. First check whether the inode exposes the FIEMAP capability; if unsupported, return
   `EOPNOTSUPP` before accessing the user header, preserving Linux error priority.
2. Copy the 32-byte Linux `fiemap` header through `UserPtr`, validate that `fm_extent_count` does not exceed
   the array limit expressible in the UAPI, and use checked arithmetic to validate the address range of the extent array following the header.
3. Wrap the raw flags, the capacity, and the safe user output writer into `FiemapExtentInfo`; the inode operation
   decides which flags are supported, and incompatible bits are written back through `fm_flags` with `EBADR` returned.
4. The writer constructs each 56-byte `fiemap_extent` one at a time, explicitly zeroes all reserved fields, and writes it back through `UserPtr`;
   when the capacity is zero, `FiemapExtentInfo` only counts the extents.
5. Whether the backend succeeds or fails, the current `fm_flags` and `fm_mapped_extents` are written back first, and only then is the result propagated.

### `mount`

1. `sys_mount()` copies the nullable source/fs type and target, and copies the nullable filesystem-specific
   mount data into the kernel as an opaque byte buffer the size of one 4 KiB base page; it does not require the input
   to be NUL-terminated or valid UTF-8, and only reserves the last byte as the forced NUL for text parsers. When the whole-page copy fails, following Linux
   legacy mount semantics, the readable prefix is recovered byte by byte, and `EFAULT` is returned only if the first byte is unreadable; the remaining tail stays zero-filled.
   User pointers are accessed only at the syscall boundary; the process/credential context is then captured once and `do_mount()` is entered.
2. `do_mount()` resolves the target against the current `FsStruct`, then hands the resolved `Path` to
   `path_mount()`; this corresponds to the Linux layering of syscall, `do_mount()` and `path_mount()`.
3. `path_mount()` discards the old mount magic, rejects `MS_NOUSER`, then, before operation dispatch, checks the mount privilege against the credential snapshot
   captured for this syscall (an unprivileged call returns `EPERM`, corresponding to `may_mount()` in Linux
   `path_mount()`), and uniformly rejects the currently unimplemented move, shared/unbindable
   propagation and recursive-bind bits with a mounted subtree; `MS_PRIVATE`/`MS_SLAVE` (there is no shared propagation
   group, so the two behave equivalently) and non-recursive bind are supported; then, from the same
   `MS_*` argument, the superblock
   flags and the per-mount `MountFlags` are generated separately. When remount does not explicitly specify
   an atime option, it preserves the target mount's current atime flags; the other user-settable per-mount flags
   are replaced wholesale by this request, corresponding to Linux `set_mount_attributes()`.
4. A validated request is dispatched as `MS_REMOUNT|MS_BIND`, plain `MS_REMOUNT`, `MS_BIND`, or an ordinary new mount, in that order;
   the corresponding operations call the namespace object methods
   `reconfigure_mount()`, `remount()`, `attach_bind()` or
   `mount_new()`, respectively.
5. A non-recursive bind requires a non-empty source; after resolving the source it clones the source mount; the copy inherits the source mount's
   flags, and the ordinary mount flags of the initial `MS_BIND` are not applied to the copy. `MS_BIND | MS_REC` is accepted only when the source
   subtree has no mounts (in which case the result is the same as cloning a single mount); otherwise it returns `EINVAL` instead of silently
   flattening the mount tree.
6. Like Linux `do_new_mount()`, an ordinary mount looks up the KVFS
   `FileSystemType` registry by canonical type name, then constructs an `FsContext` containing the source, mount data, superblock flags and credential,
   and then calls `MntNamespace::mount_new()`. The registry and `/proc/filesystems`
   use the same static descriptor identity.
7. `mount_new()` enters either nodev or
   `get_tree_bdev -> fill_super` through the type object's synchronous `get_tree()`; the filesystem parses the mount data and copies the state that must be retained after mounting
   into its own mount state, after which the VFS grafts the detached mount onto the target.
8. `MS_PRIVATE`/`MS_SLAVE` with an optional `MS_REC` require the target to be a mount root; KVFS currently does not model
   shared propagation groups, so slave and private behave equivalently, and both succeed as idempotent operations (each
   validating illegal flag combinations); shared and unbindable requests are still explicitly rejected.
9. After resolving the two paths, `sys_pivot_root()` executes Linux
   `do_pivot_root()` semantics on `MntNamespace::pivot_root()` (`put_old` may coincide with `new_root`; after the pre-failure checks, the fs root
   and pwd are updated in a single step), and then, following Linux `chroot_fs_refs()` semantics, walks all live processes and redirects every `FsStruct` whose root/pwd is exactly the old root
   to `new_root`.
10. `sys_umount2()` supports `MNT_DETACH`: it detaches the whole mount subtree via `detach_tree()` without a
    busy check (an OCI runtime uses it to unmount the old root after `pivot_root`); `MNT_FORCE`,
    `MNT_EXPIRE` and `UMOUNT_NOFOLLOW` are still rejected.

### readlink length ABI

`readlink` / `readlinkat` receive the length as a signed 32-bit integer at the syscall entry;
the dispatch layer takes the low 32 bits of the register, and the entry first rejects non-positive values before accessing the path and the output buffer.
Only a validated positive value is converted to `usize` and handed to the copy logic; the return value does not count the NUL terminator,
preserving the truncation semantics for short buffers.

The in-kernel unit tests in `namei.rs` pass non-positive lengths and a NULL address directly to the readlink entry,
ensuring that length rejection precedes user memory access; a separate guest probe adds checks for register truncation and successful copying.

## Concurrency model

`posix-fs` itself holds no global state.
Concurrency control is the responsibility of the underlying resource objects:

- the fd table and descriptor flags are protected by internal locks in `kfd`/process resources;
- `FsStruct` serializes updates to the current directory, root directory, umask and resolution base through a lock in the process state;
  it is the sole owner of the umask, and `CLONE_FS` shares root, pwd and umask together by sharing the same `Arc<Mutex<FsStruct>>`.
  A non-shared fork copies the state with `clone_for_process()`;
  a single `*at` operation uses a full `snapshot()`, which does not reset the umask to its default value;
- the directory stream offset is protected by the `Directory::offset` lock;
- the internal concurrency of files, pipes, devices, mountpoints and VFS nodes is the responsibility of their respective implementations;
- user memory access is performed by `posix_types` / `osvm` under the current process address space.

Readers of this document need to pay special attention to cross-object operations:
`renameat2` resolves the old directory and the new directory and then hands them to the VFS rename;
file-to-file copy locks positions in object-address order, and takes the lock only once for the same open file; the pipe path first waits for
data or space, then acquires the non-pipe file position lock inside the pipe lock, and does not hold a position lock while waiting across a pipe;
the open semantics of device nodes are handled by the VFS/device file operations themselves.
New code should avoid entering complex paths that may call back into the same resource while holding the fd table or directory offset lock.

## Design decisions

### Separating the syscall compatibility layer from the VFS

`posix-fs` handles only the Linux ABI, the current process context and error propagation,
and leaves the actual file semantics to `kvfs`, `kfd`, `kfd_objects` and device objects.
This avoids duplicating filesystem implementations in the syscall layer,
at the cost that syscall compatibility behavior must clearly record which parts are guaranteed by this crate and which by the underlying objects.

### Separating fd-backed objects from filesystem state

Exposing an object to user space through an fd does not automatically mean that the object belongs to the filesystem owner.
`timerfd` and `eventfd` live in `kfd_objects`, and their core state belongs to the timer runtime and the event
counter, respectively. The pipe lives in `kvfs::pipe`: an anonymous pipe and a pathname FIFO share state equivalent to
`pipe_inode_info`, while the pathname FIFO session is held by the inode and therefore belongs to fs-core,
and cannot be owned by the process fd-object layer. The responsibility `posix-fs` retains for these objects is limited to
the interoperation wiring between syscalls such as `splice` and `fcntl(F_*PIPE_SZ)` and the underlying objects.

### procfd as a VFS magic-link

`/proc/<pid>/fd/<fd>` is exposed by procfs as a magic-link node, rather than as a
string-parsing special case inside `posix-fs`.
`posix-fs` decides whether to follow the final component according to the syscall flags,
while the actual target fd snapshot, the readlink display and the follow behavior are accomplished jointly by procfs/kfd/kvfs.
This preserves live procfd object semantics while avoiding duplicating procfs path rules in the syscall layer.

### Compatibility boundary of mount operations

The ABI entry of `sys_mount`, the target lookup of `do_mount` and the flags
normalization/operation dispatch of `path_mount` correspond to the same-named layers in Linux; no extra persistent state object is added for request parameters. It implements non-recursive
bind mount, ordinary remount and bind remount. The remount target must be the root path of a mount registered in the current mount namespace;
an ordinary directory returns `InvalidInput`. An ordinary remount passes the
superblock flags and the per-mount flags separately, and a bind remount updates only the target mount flags; a remount that does not explicitly give an
atime option preserves only the target's current atime mask, and the other user-settable flags are rebuilt from the request;
when an ordinary remount does not give `MS_RDONLY`, it requests switching the superblock to read-write. This is raw Linux
`mount(2)` semantics; whether user-space `mount(8)` fills in the old options is its own policy.
The initial bind inherits the source mount flags and does not apply the ordinary mount flags of the same call; modifying the bind mount flags
requires a second `MS_REMOUNT|MS_BIND` call.
An ordinary new mount no longer depends on specific crates such as devfs, procfs or bpffs; `path_mount` only resolves the canonical
type and uses the KVFS registry, corresponding to Linux entering the filesystem type's creation entry after `get_fs_type()`.
This keeps the set of supported mounts and `/proc/filesystems` on the same source of truth.
An ordinary new mount passes one page of bounded, kernel-owned opaque mount data to the filesystem
type through `FsContext`; the generic syscall/VFS does not interpret UTF-8, commas or filesystem options. The concrete filesystem decides whether that page is a text
or a binary representation, and when parsing an unknown or unimplemented option it must, according to its own compatibility policy, either fail explicitly or record it as a
genuine compatibility no-op. Filesystem-specific remount reconfigure is still not modeled.
`MS_MOVE`, `MS_SHARED` and `MS_UNBINDABLE` return `InvalidInput` before any operation dispatch, and an ordinary new mount carrying `MS_REC` also returns `InvalidInput`; `MS_PRIVATE`/`MS_SLAVE` are accepted, and `MS_BIND|MS_REC` is accepted only when the source subtree has no mounts. Therefore `MS_BIND|MS_MOVE` and `MS_REMOUNT|MS_SHARED` never execute a partial request.
`sys_umount2` returns `InvalidInput` for `MNT_FORCE`, `MNT_EXPIRE` and `UMOUNT_NOFOLLOW`; `MNT_DETACH` is supported.
Explicit rejection is safer than silent ignoring,
because silent ignoring would lead user space to believe it has obtained semantics such as asynchronous unmount or bind mount.
`mount` privilege is decided at the syscall boundary from the credential
snapshot captured for that call: `path_mount()` checks after `MS_NOUSER` and
before operation dispatch, while `sys_umount2()` checks after target lookup and
before namespace detach. An unresolvable target therefore still reports the
lookup error first; any target a non-privileged caller can resolve yields
`EPERM`, and unimplemented operations such as `MS_MOVE` also yield `EPERM` for
non-privileged callers before dispatch. The current requirement is effective
`CAP_SYS_ADMIN`; user-namespace and idmapped-mount authorization semantics are
not implemented. `MntNamespace::{mount_new, attach_bind, remount,
reconfigure_mount, detach}` retain explicit caller contracts, with authorization
owned by the caller.

### File locks

`fcntl` converts the user range into absolute offsets, validates the fd, the open mode and the zero `l_pid` of OFD locks,
and then calls KVFS's record lock operations. The POSIX owner is the identity of the shared fd table, and the OFD owner is the
`VfsFile` identity; `F_GETLK` returns the actual conflict.
The traditional `F_GETLK` rejects `F_UNLCK` before range conversion; `F_OFD_GETLK` allows using it to query one's own lock.
`F_SETLKW` is an interruptible wait, and signals are handled as `ERESTARTSYS`; after a POSIX lock succeeds, the fd is rechecked
to handle concurrent close/replace. In the current main, the fd table binding still belongs to the process; when the binding is replaced during the wait,
the late lock of the old owner is cleared, and the lock is retried on the current table with the original absolute range. This PR does not change the thread file table,
exec or task identity model.
`flock` uses a separate whole-file lock set. Closing any fd that refers to the same inode releases the
POSIX locks of that files owner; OFD/flock locks are cleaned up when the open file is finally released.

### fcntl minimum-based duplication

`F_DUPFD` and `F_DUPFD_CLOEXEC` convert the unsigned-long argument to a signed
32-bit minimum, matching Linux `do_fcntl`, then call
`ProcessResources::duplicate_file_from`. Source validation precedes minimum
validation; allocation selects the lowest free FD at/above the minimum and below
the soft `RLIMIT_NOFILE` and physical capacity. The CLOEXEC variant sets only the
new descriptor's flag. Ordinary `dup` and fixed-target `dup2`/`dup3` keep their
existing paths.

### memfd sealing fcntl

`fd_ops` handles `F_GET_SEALS` and `F_ADD_SEALS`:

```text
sys_fcntl(F_GET_SEALS/F_ADD_SEALS)
  -> current_files().get_file()
  -> kvfs::VfsFile::shmem_seal_bits() / add_shmem_seals()
  -> inode-scoped ShmemObjectState
```

These commands are valid only for shmem-backed VFS files. A non-VFS file, or a
regular file without `ShmemObjectState`, returns an error. `F_ADD_SEALS` allows
seals to be added only monotonically; once `F_SEAL_SEAL` is set, further additions
are rejected.

### Current credentials and VFS authorization

On entry, each pathname syscall calls `kprocess::current_cred()` to obtain a committed
`Arc<Cred>`, and passes the same snapshot to `Filename`, `Nameidata` and `Path`. Ordinary open, path
traversal, creation, deletion, link, rename, pathname truncate and exec checks use that object's
`fsuid/fsgid` and supplementary groups; the VFS does not query the current task in the reverse direction.

A new inode is initialized by the filesystem creation callback using `kvfs::inode_init_owner()`: the UID is `fsuid`,
the GID is normally `fsgid`, it inherits the parent GID under a setgid parent directory, and a new subdirectory propagates the setgid bit.

`faccessat2` selects the checking identity before path resolution. By default,
`Cred::for_access()` uses the real IDs and adjusts the effective capability set
according to the Linux `access_override_creds` rules (unless
`SECURE_NO_SETUID_FIXUP`: a nonzero real UID clears effective capabilities,
while a zero real UID sets effective equal to permitted). `AT_EACCESS` uses the
current committed credential directly and preserves its `fsuid`/`fsgid` and
capabilities. The selected object is used for both intermediate-directory search
and the final inode check; supplementary groups remain unchanged.

`chown/chmod/utimensat` also capture a single credential snapshot at entry,
then `Path` performs Linux-style owner, group, write-permission, and
explicit-time authorization. `chroot` additionally requires effective
`CAP_SYS_CHROOT` after the target-directory search check.

`truncate(path)` uses the credentials at the time of the call for pathname write DAC; `ftruncate(fd)` verifies that the open file
has write mode and then uses the open file authority directly, without re-running
pathname DAC if the caller later changes its UID. This corresponds to the division of semantics between Linux `vfs_truncate()` and `do_ftruncate()`.

### Copy-style syscalls use an intermediate buffer

The KVFS transfer entry point is responsible for file buffering and pipe consumption; the syscall does not loop over object reads and writes by itself.
A short write is added to the accumulated count and then returned; a later transfer error must not overwrite the amount already completed. Only a file wait with zero completed amount that is interrupted
may request a signal restart. A failure of the explicit-offset write-back is an independent error after the transfer and is not subject to this accumulation rule.
There is no zero-copy yet; the regular-file type, same-inode overlap and cross-superblock admission checks of `copy_file_range` are already done
in the check phase of the KVFS transfer layer (see `fs/kvfs/docs/design.md`).
`copy_file_range` currently accepts only `flags == 0`;
nonzero flags return `InvalidInput` at the syscall boundary.

## Drop / Resource release

`posix-fs` has no custom `Drop` types.
Resource release depends on the lower-level objects:

- `close`/`close_range` remove the `Arc<dyn FileLike>` from the current process fd table;
- the two range arguments of `close_range` follow the Linux `unsigned int` ABI and accept `UINT_MAX`
  as an open upper bound; the upper bound is clamped when converted to the signed fd table. An empty range beyond the fd table's numbering domain still retains
  the file-table isolation effect of `CLOSE_RANGE_UNSHARE`, and an invalid order or unknown flags are not treated as success.
- fd duplication and the open path share file, directory, pipe or device objects through `Arc`;
- temporary `Vec`, path `String`, `CString` and intermediate I/O buffers are released when the function returns;
- the mount/unmount topology is managed by `kvfs::Path` and `Mount`; when the last `VfsMount` is released, it returns the
  superblock active reference, and shutdown runs when the last reference goes away.

## Known limitations

1. Credential DAC uses effective capabilities and Linux mode semantics, but
   LSM, ACL, user-namespace ID mapping, and idmapped mounts are not implemented.
2. Backends such as FAT that cannot express Unix UID/GID cannot fully persist the creator identity.
3. File locks provide local advisory semantics and do not implement leases or network filesystem lock protocols.
4. `copy_file_range` has no zero-copy path; a cross-superblock pairing can only be
   served by the target's native copy primitive, otherwise the same superblock is required.
5. `mount` does not yet support move, recursive bind with mounted subtrees, shared/unbindable propagation, or filesystem-specific
   reconfigure beyond the read-only policy.
   An ordinary new mount already passes one page of opaque mount data, but the concrete text or binary format and options are still selectively implemented by
   each filesystem type; both nodev and device-backed types are created through registry/FsContext.
6. `sendfile` retains the 32-bit range limit for a non-null offset, reflecting legacy interface compatibility constraints.
7. `F_ADD_SEALS` / `F_GET_SEALS` are wired into shmem object state; shared
   writable mmap and `mprotect(PROT_WRITE)` seal enforcement are
   performed by `mm/filemap`.
8. POSIX ACL still lacks permission computation, mode synchronization and inheritance; KExt4 does not expose the opaque ACL bytes in core
   as ordinary `system.posix_acl_*` xattrs.

## Kernel unit-test layout

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/xattr.rs`: mounted by `src/xattr.rs` as `tests`.
- `ut-tests/ioctl.rs`: mounted by `src/ioctl.rs` as `tests`.
- `ut-tests/stat.rs`: mounted by `src/stat.rs` as `tests`.
- `ut-tests/fd_ops.rs`: mounted by `src/fd_ops.rs` as `tests`.
- `ut-tests/file.rs`: mounted by `src/file.rs` as `tests`.
- `ut-tests/path.rs`: mounted by `src/path.rs` as `tests`.
- `ut-tests/mount.rs`: mounted by `src/mount.rs` as `tests`.
- `ut-tests/namei.rs`: mounted by `src/namei.rs` as `readlink_tests`.
- `ut-tests/io_restart_tests.rs`: mounted by `src/io.rs` as `restart_tests`.
- `ut-tests/io_transfer_count_tests.rs`: mounted by `src/io.rs` as `transfer_count_tests`.
- `ut-tests/io_copy_file_range_checks_tests.rs`: mounted by `src/io.rs` as `copy_file_range_checks_tests`.

## Disk quota syscalls

`quota.rs` implements quotactl and quotactl_fd. It decodes command/type, resolves
the mounted block-device superblock or descriptor, checks the current caller,
and converts classic quota records between 1-KiB limits and native byte counts.
`quota/xfs.rs` converts the extended status/record ABI, whose space units are
512-byte blocks. Both paths use the same superblock quota operation table and
canonical state. Reservations are included in reported usage. Enumeration
returns the least stored ID at or above the requested ID without wrapping.
System quota files retain accounting while quota-on and quota-off commands
toggle enforcement; external files continue through their activation callbacks.

Queries for format, information, status and synchronization are unprivileged;
user/effective-group owners may query their records. Other operations require
CAP_SYS_ADMIN. A null special pathname with Q_SYNC visits mounted supporting
filesystems. A filesystem without quota callbacks returns ENOSYS.

Quota format and information queries build owned ABI snapshots under the quota
state mutex and release it before copying to user memory. Record and extended
status queries follow the same boundary: a user destination may fault into file
allocation and quota accounting, so no quota-state or record guard spans copyout.
