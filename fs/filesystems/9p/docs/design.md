# v9fs — Design Document

## Prepared transport handoff

Mount consumes KBox<dyn P9Transport> supplied by its caller and hands it directly
to P9Client before negotiation. No transport-owner allocation is performed after
remote attach. Session-owner allocation and remote fid cleanup retain their
existing contracts; client Arc and protocol buffers are not migrated here.


## Prepared session owner

After negotiation and root metadata validation, mount prepares the session in a
KERNEL-policy KBox before handing it to KVFS. Transport calls have returned and
no transport guard is retained at this allocation. Callers must use sleepable,
reclaim-safe task context with IRQs/preemption enabled and no non-sleepable RCU
section. A failed owner allocation is returned before a local superblock is published.

## Positioning

`fs/filesystems/9p` is the concrete 9P network filesystem, corresponding to Linux `fs/9p`. It owns the KVFS adaptation for mount, superblock,
inode, namespace, open-file and directory-iteration; `net/9p` owns the 9P client, fid,
protocol encoding/decoding and the connection interface.

## Background

After a rename/unlink, a remote object is still identified by its fid. Re-walking by absolute path would change the open-file identity,
so, like Linux, v9fs must have the inode/dentry and the opened file hold fids in different roles.

## Scope

The code is split by responsibility following Linux:

| X-Kernel | Linux | Responsibility |
| --- | --- | --- |
| `fs.rs` | `vfs_super.c`, `v9fs.c` | mount session and superblock operations |
| `inode.rs` | `vfs_inode.c`, `vfs_inode_dotl.c` | inode, metadata and namespace operations |
| `file.rs` | `vfs_file.c` | open, read/write, fsync and folio reads |
| `dir.rs` | `vfs_dir.c` | opened directory iteration |
| `util.rs` | dotl/VFS conversion helpers | device number and `d_type` conversion |

## Architecture

The kernel unit tests are located in `ut-tests/file.rs`, `ut-tests/inode.rs` and `ut-tests/util.rs`.
Each file is mounted into the private `tests` module of the corresponding source file via `unittest::path_mod_test!`,
and is compiled only under `cfg(unittest)`. The current AArch64 verification configuration must enable both `KFEAT_FS_9P=y`
and `KFEAT_DRIVER_VIRTIO_9P=y`, so as to include `v9fs` and the VirtIO 9P device support it requires.
These tests are independent of the protocol client tests in `net/9p`.

```text
SuperBlock private_data
        │
        v
V9fsSession ─────► Arc<P9Client> ─────► P9Transport
                         ▲
                         │
V9fsInode ───────► unopened Arc<P9Fid>
                         │ zero-name walk + open
                         v
VfsFile private_data ─► opened Arc<P9Fid>
```

`V9fsSession` corresponds to Linux `v9fs_session_info` and keeps only the mount-level client. `V9fsInode` stores no absolute
path, no redundant node type and no outer filesystem lock; the node type and identity come from the qid/attributes.

Linux keeps the lookup fid in dentry private data. KVFS currently has no dentry-private slot, so v9fs
keeps the same unopened fid in `V9fsInode`. This is the minimal structural difference needed to preserve object identity; no
path cache or second-layer fid wrapper is added for it.

## Calling Constraints / Execution Context

Mount, lookup, namespace operations, I/O and the release of the last fid reference all wait synchronously on the transport, and may only run in
sleepable task context after the scheduler and allocator are available. A current userspace process is not required,
but they must not run in IRQ context or while holding a spinlock. Concurrent entry is allowed (the code is reentrant), and protocol requests are serialized by the shared client.

## Algorithm Flow

### mount and inode

The boot/driver glue first selects the concrete device and establishes the transport connection, then calls `v9fs::mount()`.
v9fs creates the `P9Client`, negotiates 9P2000.L, attaches the root fid, obtains and validates the root attributes, and then publishes the
superblock. The block size is computed by a rule equivalent to Linux `fls(maxdata - 1)`, and the maximum file size uses KVFS
`MAX_LFS_FILESIZE`.

The inode number uses the Linux 64-bit `QID2INO(qid.path) = qid.path + 2`. The current implementation corresponds to Linux's default
cacheless mode: each successful lookup builds an inode from the returned fid, and different lookups are not merged by inode number;
a hard link instead reuses the VFS inode already held by the source dentry.

Attributes are fetched via `TGETATTR`, and the fid qid, the attribute qid and the VFS node type are verified to be consistent.
Explicit changes to mode, owner, size, atime and mtime are converted into a single `TSETATTR`; atime/mtime
use explicitly specified times. ctime can only be requested to be updated by the server, and `RSETATTR` does not return its actual value, so the
client's ctime is never published as a confirmed result; the externally visible getattr still reads the times from the server.

Automatic access/modification updates follow Linux `generic_update_time`: the
`update_time` callback publishes local inode atime or mtime/ctime directly, and
KVFS does not publish them again. It sends no remote `TSETATTR` and does not
translate automatic updates into server-side current-time requests.

### open, I/O and lifecycle

Every VFS open performs a zero-name walk from the inode's unopened fid to obtain an independent fid, selects the access mode/status flags following Linux's
`v9fs_open_to_dotl_flags()` rules, and then issues `TLOPEN`. The opened fid is stored in `VfsFile` as typed private data; read, write, readdir and fsync on regular files always take the fid from that object; symlink reads go through `read_link()` and use the inode's unopened fid.
write updates the inode size from the number of bytes the server confirmed written, and uses KVFS's
`grow_size_after_backing_write()` to keep the backing write and the local size publication under consistent responsibility.

Therefore, after a rename/unlink, an already opened file does not depend on the old path and still refers to the same server object. When the file is released, the typed
private data drops the last `Arc<P9Fid>`, which is clunked by `P9Fid::drop`; v9fs does not implement a duplicate release state machine.

### Page cache and mmap

Regular-file inodes install v9fs's own `AddressSpaceOperations` through
`new_file_with_address_space_and_flags()`, keeping `NodeFlags::NON_CACHEABLE` so
plain I/O and truncate keep their existing fid/TSETATTR paths. Only `read_folio`
is implemented: it derives the file offset from `folio.index()`, walks a
zero-name clone of the inode's unopened fid to get a private fid, opens it with
`TLOPEN` and reads the folio window, zero-filling the tail past end of file as
the KVFS contract requires. A clone is mandatory because faults run concurrently
and `P9Fid::read` only accepts an opened fid.

The client shortens each TREAD to the negotiated msize or iounit, which may be
smaller than a folio, so `read_folio` loops over the remaining window and treats
a zero-length reply before the window is covered as `VfsError::Io`; the folio
stays non-uptodate instead of caching a hole. This path lets executables and
shared libraries on 9P demand-page like Linux `v9fs_vfs_read_folio()`.

The callback consumes the existing `FolioRead` initialization token. This
cacheless server adapter performs its fid reads synchronously, then completes
the token with the final result. Whole-window success includes zeroing the EOF
tail; every failure leaves the folio non-uptodate and releases its lock for retry.
No separate read state or completion owner is introduced.

Plain `read()`/`write()` still use the opened fid's `TREAD`/`TWRITE` and bypass
the page cache. Writable `MAP_SHARED` remains unsupported because there is no
`write_begin`/`write_end`/`writepages` implementation.

Truncate stays on the `TSETATTR` path because the inode is `NON_CACHEABLE`. A
successful size change is therefore published through KVFS's
`update_size_after_backing_change()`, which resizes or drops cached folios and
invalidates mappings past the new EOF before the operation returns. Without
that publication a folio cached by `read_folio` could keep serving
pre-truncate bytes.

Directory iteration converts the server qid into an inode number, and uses the server `d_off` as the next
`DirContext` position, rather than simulating the remote offset with a local array index.

### namespace operations

- lookup walks a single child name on the parent fid;
- create/mkdir/mknod/symlink perform the dotl operation on the parent fid, then walk the child to obtain an unopened fid;
- link sends `dfid, oldfid, name` and reuses the source VFS inode;
- unlink/rmdir prefer `TUNLINKAT`, and fall back to `TREMOVE` on a cloned child fid only for `EOPNOTSUPP`;
- rename prefers `TRENAMEAT`, and falls back to `TRENAME` on the source fid only for `EOPNOTSUPP`;
- all operations use object fids and do not construct or update absolute path strings.

`statfs` uses `TSTATFS` on the root inode fid; it falls back to
`simple_statfs` only when the server explicitly reports unsupported. `sync_fs` has no superblock-owned dirty state; the durability of opened objects is expressed by
per-fid `TFSYNC`. The range-aware VFS callback forwards `data_only` unchanged.
9P fsync has no byte-range operand, so partial and empty VFS ranges conservatively
synchronize the whole opened fid; no local writeback or second sync hook is added.

## Concurrency Model

v9fs itself adds no global or session-level lock. The inode/file fids of one mount share the `P9Client`, and the client's
internal mutex serializes transport requests and protocol allocation state; the lifetimes of VFS objects and fids are expressed by `Arc`.

## Design Decisions

- Only 9P2000.L cacheless filesystem semantics are implemented;
- No KVFS dentry-private fid slot, atomic-open contract, xattr, or remote-lock support yet;
- the page cache only serves read faults; no writable shared mappings or writeback;
- transport discovery and VirtIO/kclass glue stay in the existing boot/driver layer; this change does not refactor them.

## Drop / Resource Release

The superblock private data holds the mount client; the inode holds the lookup fid; the typed private
data of an opened file holds an independent opened fid. When each host object releases its last `Arc<P9Fid>`, the protocol layer clunks it, and v9fs
does not duplicate clunk state or implement an extra release wrapper.

File fsync first writes and waits for the requested local mapping range, then
issues the protocol's whole-fid TFSYNC. This follows Linux
`v9fs_file_fsync_dotl()` and prevents server success from hiding unsent local
mmap data. Cacheless 9P currently has no dirty-folio writeback provider: dirty
shared mappings return the local writeback error before TFSYNC; clean or
non-overlapping ranges still reach the server.
