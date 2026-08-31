# 9P Filesystem Design

## Scope

The `v9fs` crate is the concrete 9P filesystem implementation for X-Kernel's
VFS. Its location under `fs/filesystems/9p` matches the role of other concrete
filesystems: it translates VFS operations into calls to the transport-neutral
`p9` client in `net/9p`.

## Structure

- `fs.rs` owns filesystem mounting and superblock operations.
- `inode.rs` implements the 9P-backed VFS inode and file operations.
- `util.rs` contains conversions between 9P data and VFS types.

`Fs9pFilesystem::mount` receives a caller-provided transport and mount tag,
constructs the protocol session, and creates the root inode. Device discovery
and concrete transport implementation remain outside this crate.

## State and lifecycle

The mounted filesystem shares one synchronized protocol session among its
inodes. VFS objects hold the remote path information required by the current
client interface. The session and its transport live for the mounted
filesystem's lifetime.

This directory relocation does not change VFS behavior, public APIs, object
state, locking, or mount wiring.
