// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked ext4 storage primitives.
//!
//! The current implementation provides disk decoding, filesystem block I/O,
//! feature negotiation, explicit journal recovery, metadata-buffer/JBD2
//! mutation, extent tree updates, bitmap allocation, a PageCache-facing
//! ordered-data regular-file writeback baseline, and a regular-file
//! truncate/orphan recovery baseline. Buffered writeback can
//! overwrite initialized extents, allocate blocks for holes, convert unwritten
//! extents after data flush, and grow the on-disk size through journaled inode
//! metadata. `Ext4Inode::disk_size()` is the core equivalent of Linux ext4
//! `i_disksize`; KVFS keeps the VFS-visible size separately while writeback or
//! prepared truncate work is pending. Truncate uses a prepare/finish token so
//! VFS can run generic PageCache resize between Linux-style `i_disksize`
//! commit and ext4 extent/orphan completion. Grow zeros any mapped old EOF tail
//! before committing the larger disk size; sparse ranges continue to read as
//! zeroes. Shrink uses the legacy ext4 orphan list, zeros the partial EOF block,
//! removes tail extents, releases blocks, updates extent-backed `i_blocks`, and
//! lets explicit recovery finish both `nlink > 0` regular-file truncation and
//! `nlink == 0` final eviction from the legacy orphan list, including when the
//! journal itself is already clean. Filesystems carrying the `huge_file`
//! feature can mutate ordinary inodes that retain 512-byte-sector `i_blocks`
//! accounting; inodes using `EXT4_HUGE_FILE_FL` block-unit accounting and the
//! ext4 orphan-file format remain unsupported.
//!
//! The first R7 namespace baseline can create regular files and directories in
//! linear directories: it allocates the inode, inserts or
//! extends a dirent block, initializes mkdir `.`/`..` data, updates parent
//! metadata, journals the metadata, and rolls back the whole transaction on
//! failure. R7.3-R7.7 add unlink/rmdir, hard link, rename, symlink,
//! special-file creation, HTree hash insertion, leaf split, dx checksum update,
//! and one-block linear-to-indexed conversion, including dirent removal,
//! link-count updates, deferred extent-backed zero-link eviction, directory `..`
//! updates, bitmap/counter updates, and e2fsck-checked journal commits.
//! The live KVFS adapter exposes regular-file buffered writeback and
//! two-phase `set_len` through the inode `AddressSpace`, and exposes the R7
//! namespace mutation baseline through KVFS inode operations. Namespace remove
//! persists nlink/orphan state without releasing an inode that still has a live
//! VFS identity; final `SuperBlockOperations::evict_inode` releases xattrs,
//! extents, and the inode bitmap after the last reference is gone. Core mutation
//! results refresh the shared VFS inode attributes. The R8 write strategy adds
//! per-group order-bucket free extent caches, bridge-side delayed-allocation
//! reservation with statfs accounting, writeback-time physical allocation,
//! normalized unwritten preallocation, EOF preallocation discard after the
//! last writer has no delayed reservations, and
//! Linux-style Orlov inode group selection with flex group, threshold,
//! quadratic-probe, and fallback stages. Folio invalidate/reclaim reservation
//! release and dirty throttling remain KVFS/PageCache accounting extensions.
//! The R9 xattr baseline adds journaled inode-body and single external
//! block set/remove for `user.*`, `trusted.*`, `security.*`, and opaque POSIX
//! ACL xattrs. The live bridge under `fs/bridges/kext4_vfs` exposes the current
//! KVFS superblock, inode, address-space, and file-operation surface; live xattr
//! hooks still wait for a shared KVFS xattr API. This is still not a complete
//! Linux ext4 write mount: mmap/direct I/O coherence, freeze/unmount lifecycle
//! hooks, a background checkpoint worker, oversized xattr, EA-inode, inline-data
//! write contracts, and Linux-style errseq/forced-readonly reporting remain
//! later stages. The core R10 sync baseline can flush checkpointed metadata and
//! device state. Metadata commits feed a pending checkpoint queue with
//! synchronous drain/failure-retain semantics, and the current KVFS bridge wires
//! that through `SuperBlockOperations::sync_fs`.
//! Crate-internal tests keep a raw allocated-data overwrite helper for
//! validating storage plumbing; it is not exported as an ext4 API.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(not(target_os = "none"))]
extern crate std;

mod balloc;
mod bitmap_allocator;
mod buffer;
mod dir;
mod dirhash;
mod disk;
mod error;
mod extent;
mod file;
mod ialloc;
mod inode;
mod io;
mod jbd2;
mod journal;
mod mballoc;
mod namei;
mod orphan;
mod superblock;
mod truncate;
mod types;
mod xattr;

pub use dir::{DirectoryEntry, Ext4DirEntryRef, Ext4DirPos, Ext4DirSink};
pub use disk::{
    BlockGroupDescriptor, CompatFeatures, DirectoryFileType, FeatureSet, IncompatFeatures,
    JournalFields, ReadOnlyCompatFeatures, Superblock,
};
pub use error::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, FeatureClass, UnsupportedKind,
};
pub use extent::BlockMapping;
pub use file::Ext4SyncIntent;
pub use inode::{
    Ext4DeviceId, Ext4Inode, Ext4InodeMetadataUpdate, Ext4Timestamp, InodeKind, SymlinkStorage,
};
pub use namei::{Ext4NamespaceCreate, Ext4NamespaceLink, Ext4NamespaceRemove, Ext4NamespaceRename};
pub use superblock::{
    Ext4Filesystem, Ext4RecoveryReport, Ext4StatFs, FilesystemLayout, JournalLocation,
    JournalStatus,
};
pub use truncate::Ext4PreparedTruncate;
pub use types::{
    BlockCount, BlockGroupNumber, FilesystemBlock, InodeNumber, LogicalBlock, PhysicalBlock,
};
pub use xattr::{Ext4Xattr, Ext4XattrNamespace};
