# pipefs — Design Document

## Positioning

`pipefs` is the concrete pseudo filesystem that owns anonymous pipes: the `pipefs` type, the hidden mount, the superblock, the default
dentry operations, and the pipe inode created for each pipe. The KVFS `pipe` module retains the data path shared by anonymous pipes and
pathname FIFOs, `PipeObject`, and the shared file operations; the syscall layer only calls
`pipefs::create_pipe_files()`.

## Background

Linux `fs/pipe.c` uses a dedicated `pipefs` for anonymous pipes rather than `anon_inodefs`. The dedicated inode holds
`pipe_inode_info`, and the dynamic name is `pipe:[ino]`. X-Kernel adopts the same ownership: each anonymous pipe has a
unique VFS inode, and the inode private data and the private data of both files point to the same `PipeObject`.

## Scope

The kernel unit tests are in `ut-tests/lib.rs`; they are mounted as the `tests` module by
`unittest::path_mod_test!` in `src/lib.rs` and compiled under `cfg(unittest)`.

- `src/lib.rs`: pipefs initialization, pipe inode/file creation, and dynamic dentry names.
- `kvfs::pipe`: the shared pipe/FIFO data structures and file operation algorithms; they are not part of this crate's
  filesystem instance state.
- `docs/design.md`, `docs/security.md`: lifecycle and reliability contracts.

## Architecture

```text
static pipefs type / s_op / s_d_op
                 |
                 v
       Once<PipeFs> { hidden Mount }
                 |
        create_pipe_files(cred, flags)
                 |
                 v
     unique VfsInode --private--> PipeObject
                 |                    ^
                 +-- write VfsFile ---+
                 +-- read  VfsFile ---+
```

## Calling Constraints / Execution Context

`init_pipefs()` runs on the sleepable execution path of boot, before the pipe syscalls and parallel tests. Initialization
allocates VFS objects, so it must not be called in interrupt context or while holding a spinlock.

`create_pipe_files()` allocates the pipe state, the inode, the dentry, and two files, and may acquire sleepable
locks, so it may only be called from normal task context. The interface takes the credential explicitly and does not rely on a reverse current-task
lookup; it is reentrant and does not depend on CPU-local state or device mappings.

## State Machine

The global filesystem only has `Uninitialized -> Published`. The state machine of each created `PipeObject` (reader, writer,
file count, and wait queues) is defined by `kvfs::pipe`; `pipefs` is only responsible for establishing, once, its ownership relationship with the inode and
files.

## Algorithm Flow

1. At boot, a hidden mount is created and published through KVFS `new_pseudo_super_block()` using the static type, the shared simple `s_op`, the pipefs
   `s_magic`, and the static `s_d_op`.
2. The syscall calls `create_pipe_files()` and passes typed status flags; pipefs retains only
   `NONBLOCK` and derives the read-only and write-only access modes itself.
3. A `PipeObject` is created with initial `files=2`, `readers=1`, `writers=1`.
4. A unique FIFO-mode inode is created with `get_next_ino()`; the owner comes from the fsuid/fsgid of the passed-in credential, and
   the inode private data holds the same `PipeObject`.
5. The write file is created on the pipefs hidden mount, and then the read file is cloned; the private data of both files
   holds the same pipe object and they use the shared anonymous-pipe file operation table.
6. The inode uses `kvfs::metadata_only_inode_operations()`: the owner/mode/timestamp of the pipe
   all persist in the KVFS-resident inode, and `setattr` accepts updates as-is, so `fchown`/`fchmod` applied to an
   anonymous pipe (runc calls `fchown` on container stdio pipes) publish the metadata correctly instead of being rejected with the inode-op
   default `EOPNOTSUPP`.
7. The dentry automatically inherits the pipefs default table, and the path is displayed as `pipe:[inode-number]`.

## Concurrency Model

`Once<PipeFs>` is written only during initialization. After publication the hidden mount is immutable; creations of different pipes share no
inode or `PipeObject`. Concurrency over the buffer, readers/writers, and wait queues of the same pipe
is managed by the state lock in `kvfs::pipe`, and pipefs adds no mount-level lock.

## Design Decisions

- Anonymous pipes no longer borrow the `anon_inodefs` singleton; a dedicated inode identity aligns with Linux pipefs.
- `PipeFs` holds only the counterpart of Linux `pipe_mnt` and does not duplicate the pipe session or the operation table.
- The inode private data and the file private data hold the same `Arc<PipeObject>`; this expresses the ownership in which Linux `inode->i_pipe`
  and `file->private_data` point to the same object, and is not two copies of pipe state.
- The pipe/FIFO data path remains shared by KVFS, avoiding duplicating the algorithms for anonymous pipes and pathname FIFOs.
- The pipe inode uses `kvfs::metadata_only_inode_operations()` (see step 6 above), so `fchown`/`fchmod`/`utimensat` updates to owner/mode/timestamps are kept in the resident inode and are visible through `fstat`. A size update is rejected with `EINVAL`, matching Linux, which refuses truncate on non-regular files.
- Anonymous pipe file operations maintain I/O timestamps consistently with pathname FIFOs: a successful write updates
  mtime/ctime, and a successful read updates atime (the corresponding behavior of Linux `fs/pipe.c` `pipe_write`/`pipe_read`),
  relying on the common fallback layer above to persist them.

## Drop / Resource Release

The hidden mount is retained for the lifetime of the system. When the read/write files are released, the shared pipe operations update the
reader/writer/file counts; the last file released drops the last file-private reference, the inode is released along with its dentry/
file references, and finally the inode-private `PipeObject` is released. There is no additional pipefs inode registry.
