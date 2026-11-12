# anon_inodefs — Design Document

## Positioning

`anon_inodefs` is the concrete pseudo filesystem for kernel anonymous files. It owns the `FileSystemType`, the hidden mount, the superblock, the default dentry operations and the singleton inode of `anon_inodefs`;
callers such as eventfd, epoll, timerfd, signalfd, pidfd, sockets and BPF objects only request the creation of an
open file. KVFS provides only the generic superblock, dentry, inode and `alloc_file_pseudo()` mechanisms,
and does not own this concrete filesystem.

## Background

Linux `fs/anon_inodes.c` creates files for kernel objects that need no independent inode state, using one kernel mount and one shared inode. X-Kernel keeps the same semantic layering, and uses an `AnonInodeFs` object to combine the global mount and the singleton inode found in Linux;
the static filesystem, superblock and dentry operation
tables are not part of this object, and are not allocated repeatedly per mount or per file.

## Scope

The kernel unit tests are located in `ut-tests/lib.rs`, are mounted as the `tests` module by
`unittest::path_mod_test!` in `src/lib.rs`, and are compiled under `cfg(unittest)`.

- `src/lib.rs`: hidden filesystem initialization, the shared inode, dynamic dentry names and file creation.
- `docs/design.md`: object hierarchy, boot order and lifetimes.
- `docs/security.md`: input boundaries, concurrent initialization and failure modes.

## Architecture

```text
static FileSystemType / s_op / s_d_op
                 |
                 v
Once<AnonInodeFs> { hidden Mount, singleton VfsInode }
                 |
                 +-- get_file(name, fops, private, mode, flags, cred)
                           |
                           v
                 per-call Dentry + VfsFile
                           |
                           +-- shared singleton inode
                           +-- caller-owned file private data
```

The counterpart of the superblock's default `s_d_op` makes the root and the dentries created by `alloc_file_pseudo()` automatically inherit
the same static table. `AnonInodeFs` therefore does not hold a dentry-operation `Arc`, and each file does not hold a
parallel operation owner.

## Calling Constraints / Execution Context

`init_anon_inodefs()` must be called on the sleepable boot execution path, before any ordinary task or parallel test creates
anonymous files. Initialization allocates VFS objects and acquires sleepable locks, so it must not be called in interrupt context or while holding a
spinlock.

`get_file()` may be called from ordinary task context. It needs to allocate a dentry/file, so it must not be called in interrupt
context. The interface does not read the current task in reverse; the caller
explicitly passes in an open credential snapshot, so kernel threads and tests can use a controlled credential.

## State Machine

```text
Uninitialized -- init_anon_inodefs() --> Published
Published     -- get_file() ----------> Published
Uninitialized -- global()/get_file() -> panic
```

There is no runtime unload state. After `Once` is published, the mount and the singleton inode remain valid for the lifetime of the system.

## Algorithm Flow

Initialization phase:

1. Create the superblock through KVFS `new_pseudo_super_block()` with the static `FileSystemType`, the shared simple `s_op`,
   `s_magic` and the default `s_d_op`.
2. The generic helper creates the pseudo-fs root inode/dentry, and the superblock binds the default dentry operations.
3. Create the hidden root mount with `NODEV | NOEXEC`.
4. Create one shared anonymous inode with no file type and permissions `0600` through KVFS `get_next_ino()`.
5. Publish the mount and the inode to `Once<AnonInodeFs>` in one step.

When creating a file, only the per-file dentry/file is allocated, `O_ACCMODE | O_NONBLOCK` is preserved, and the caller's
private object is installed into the counterpart of `file->private_data`. `getattr` obtains metadata from the path's unique VFS inode,
then clears the file-type bits, preserving the historical semantics of the Linux user ABI for anonymous inodes.

## Concurrency Model

Initialization is executed serially by boot; `Once` is responsible for publication visibility. After publication, the `AnonInodeFs` fields are immutable,
and concurrent `get_file()` calls share only the mount and the singleton inode, each creating independent dentry/file/private
state, so no filesystem-level mutex is needed. Internal synchronization of VFS objects is the responsibility of KVFS.

## Design Decisions

- The concrete `anon_inodefs` resides in `fs/filesystems`, and is not a KVFS module or a facade re-export.
- `AnonInodeFs` combines only the hidden mount and singleton inode that actually exist in Linux; the operation
  tables use static objects and are not placed in runtime state.
- All ordinary anonymous files share one inode; the semantics of Linux
  `anon_inode_create_getfile()`, which needs an independent inode/security context, are not yet implemented, and must not be faked by copying singleton state.
- `AnonInodeOperations` explicitly overrides `setattr`: owner/mode/time updates keep the rejection semantics
  (`EOPNOTSUPP`) and do not inherit the KVFS common fallback layer. The premise of the common fallback layer is that inode identity and
  metadata are both held by the inode itself, whereas the singleton is referenced jointly by the anonymous fds of eventfd/epoll/timerfd/signalfd/pidfd/socket/BPF of all processes; accepting a single
  fallback update would rewrite the shared uid/gid/mode/timestamps that every process sees when it calls `fstat` on its own anonymous fd.
  Size updates keep the common fallback layer's `EINVAL`, so that `ftruncate` on a non-regular file stays on the documented errno,
  consistent with anonymous pipes.
- Initialization explicitly happens at boot, rather than performing complex VFS construction on the first access of an arbitrary runtime caller.

## Drop / Resource Release

The global filesystem object is not released at runtime. When each `VfsFile` is released, its own operation table performs
the kernel object cleanup, after which the per-file private `Arc`, the dentry and the file reference are released; the shared inode
and the hidden mount remain held by `AnonInodeFs`.
