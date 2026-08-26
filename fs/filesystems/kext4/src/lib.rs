// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked ext4 filesystem objects and storage algorithms.
//!
//! The crate owns ext4-private algorithms and their direct KVFS operation
//! objects. The current implementation provides disk decoding, filesystem block I/O,
//! feature negotiation, explicit journal recovery, metadata-buffer/JBD2
//! mutation, extent tree updates, bitmap allocation, a PageCache-facing
//! ordered-data regular-file writeback baseline, and a regular-file
//! truncate/orphan recovery baseline. Mount negotiation requires the ext4
//! extents feature because legacy direct/indirect block maps currently have a
//! checked read path but no complete mutation contract. Buffered writeback can
//! overwrite initialized extents, allocate blocks for holes, convert unwritten
//! extents after data flush, and grow the on-disk size through journaled inode
//! metadata. A live writeback cursor reads the current extent before each
//! mapping or allocation run, charges journal credits for the actual work, and
//! restarts the transaction at a durable filesystem-block boundary when the
//! current handle cannot be extended. If a later transaction fails, the core
//! reports its completed byte prefix through KVFS so PageCache can clean only
//! complete prefix folios and retain the boundary and suffix for retry.
//! `Ext4Inode::disk_size()` is the core equivalent of Linux ext4
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
//! results update the inode component composed into the sole resident KVFS
//! inode; KVFS generic attribute operations read and update that same state,
//! so there is no second cached inode snapshot to refresh. KVFS owns
//! `I_NEW`/`I_FREEING`-equivalent lookup and eviction; KExt4 owns no resident
//! inode cache or lifecycle. Namespace mutation receives the already resident
//! child/moved/replaced private state instead of reloading it by number.
//! Recovery runs before VFS identity publication and uses temporary private
//! state with the normal truncate/eviction algorithms. The R8 write strategy adds
//! per-group order-bucket free extent caches, interval-based delayed-allocation
//! reservation with core-owned inode/mount accounting, writeback-time physical allocation,
//! normalized unwritten preallocation, EOF preallocation discard after the
//! last writer has no delayed reservations, and
//! Linux-style Orlov inode group selection with flex group, threshold,
//! quadratic-probe, and fallback stages. Folio invalidate/reclaim reservation
//! release and dirty throttling remain KVFS/PageCache accounting extensions.
//! The R9 xattr baseline adds journaled inode-body and single external
//! block set/remove for `user.*`, `trusted.*`, `security.*`, and opaque POSIX
//! ACL xattrs. The direct KVFS operation implementation exposes
//! `user.*`, `trusted.*`, and `security.*` through KVFS xattr operations,
//! including atomic create/replace policy and ctime synchronization. Opaque ACL
//! bytes remain a core-only foundation until KVFS has ACL permission and
//! inheritance semantics. This is still not a complete
//! Linux ext4 write mount: mmap/direct I/O coherence, freeze/unmount lifecycle
//! hooks, a background checkpoint worker, oversized xattr, EA-inode, inline-data
//! write contracts, and Linux-style errseq/forced-readonly reporting remain
//! later stages. The core R10 sync baseline can flush checkpointed metadata and
//! device state. One mount-lifetime journal identity owns the internal mapping,
//! transaction engine, and FIFO checkpoint queue. Pending work shares the
//! committed transaction instead of cloning it or retaining a second journal
//! owner. Expected mutation failures are validated before the first metadata
//! access; a normal error after metadata publication is treated as an invariant
//! violation and aborts the journal without rewinding metadata already published
//! by this or earlier successful operations. Handles retain only credits and
//! buffer/revoke membership and return accounting failures through explicit
//! stop. Durable commit is separate from home-block checkpoint,
//! while queue progress remains synchronously driven until N2 introduces
//! background execution. Precise per-inode sync tids still require KVFS
//! runtime-inode storage, so the current sync path conservatively commits the
//! running transaction. The KVFS integration wires filesystem drain through
//! `SuperBlockOperations::sync_fs`.
//! Crate-internal tests keep a raw allocated-data overwrite helper for
//! validating storage plumbing; it is not exported as an ext4 API.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[macro_use]
extern crate klogger;

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
#[cfg(test)]
mod linux_image_tests;
mod mballoc;
mod namei;
mod orphan;
mod superblock;
mod sync;
mod truncate;
mod types;
mod vfs;
mod xattr;

pub use dir::{DirectoryEntry, Ext4DirEntryRef, Ext4DirPos, Ext4DirSink};
pub use disk::{
    BlockGroupDescriptor, CompatFeatures, DirectoryFileType, FeatureSet, IncompatFeatures,
    JournalFields, ReadOnlyCompatFeatures, Superblock,
};
pub use error::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, FeatureClass, UnsupportedKind,
};
pub use extent::{BlockMapping, BlockMappingFlags};
pub use file::{Ext4SyncIntent, Ext4WritebackError, Ext4WritebackResult};
pub use inode::{
    Ext4DeviceId, Ext4Inode, Ext4InodeMetadataUpdate, Ext4InodeStat, Ext4Timestamp, InodeKind,
};
pub(crate) use superblock::Ext4SbInfo;
pub use superblock::{
    Ext4RecoveryReport, Ext4StatFs, FilesystemLayout, JournalLocation, JournalStatus,
};
pub use types::{
    BlockCount, BlockGroupNumber, FilesystemBlock, InodeNumber, LogicalBlock, PhysicalBlock,
};
use vfs::{Ext4MountOptions, Ext4SuperOperations};
pub use xattr::{
    Ext4Xattr, Ext4XattrNameRef, Ext4XattrNameSink, Ext4XattrNamespace, Ext4XattrSetMode,
};

fn ext4_get_tree(
    context: &mut kvfs::FsContext<'_>,
    lookup_root: &kvfs::Path,
    lookup_pwd: &kvfs::Path,
) -> kvfs::VfsResult<alloc::sync::Arc<kvfs::SuperBlock>> {
    let options = *context.private::<Ext4MountOptions>()?;
    kvfs::get_tree_bdev(context, lookup_root, lookup_pwd, move |super_block| {
        Ext4SuperOperations::fill_super(super_block, options)
    })
}

fn ext4_reconfigure(context: &mut kvfs::FsContext<'_>) -> kvfs::VfsResult<()> {
    let options = *context.private::<Ext4MountOptions>()?;
    let super_block = context.super_block()?;
    if let Some(statfs_mode) = options.statfs_mode {
        sync::write_lock(super_block.private::<sync::RwLock<Ext4SbInfo>>()?)
            .set_statfs_mode(statfs_mode);
    }
    Ok(())
}

static FS_CONTEXT_OPERATIONS: kvfs::FsContextOperations =
    kvfs::FsContextOperations::with_reconfigure(ext4_get_tree, ext4_reconfigure);

fn init_fs_context(context: &mut kvfs::FsContext<'_>) -> kvfs::VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    context.set_private(Ext4MountOptions::parse(context.data())?);
    Ok(())
}

static FILE_SYSTEM_TYPE: kvfs::FileSystemType =
    kvfs::FileSystemType::device_backed("ext4", init_fs_context);

#[macros::register_init]
fn init_ext4_fs() {
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE).expect("ext4 filesystem type must register once");
}
