// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Superblock state and filesystem statistics.
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use hashbrown::HashMap;
use klazy::Lazy;

use crate::{Dentry, Mutex, VfsInode, VfsResult, WritebackControl};

static SUPER_BLOCK_REGISTRY: Lazy<SuperBlockRegistry> = Lazy::new(SuperBlockRegistry::new);

/// Returns the VFS-wide superblock registry.
pub fn super_block_registry() -> &'static SuperBlockRegistry {
    Lazy::force(&SUPER_BLOCK_REGISTRY)
}

/// Synchronizes all live superblocks known to the VFS.
pub fn sync_filesystems() -> VfsResult<()> {
    super_block_registry().sync_filesystems()
}

#[derive(Debug, Default)]
struct SuperBlockSet {
    super_blocks: HashMap<usize, Weak<SuperBlock>>,
}

impl SuperBlockSet {
    fn register(&mut self, super_block: &Arc<SuperBlock>) {
        self.super_blocks.insert(
            Arc::as_ptr(super_block) as usize,
            Arc::downgrade(super_block),
        );
    }

    fn live_super_blocks(&mut self) -> Vec<Arc<SuperBlock>> {
        let mut live = Vec::new();
        self.super_blocks
            .retain(|_, super_block| match super_block.upgrade() {
                Some(super_block) => {
                    live.push(super_block);
                    true
                }
                None => false,
            });
        live
    }
}

/// VFS-wide owner for live superblocks.
#[derive(Debug, Default)]
pub struct SuperBlockRegistry {
    super_blocks: Mutex<SuperBlockSet>,
}

impl SuperBlockRegistry {
    fn new() -> Self {
        Self::default()
    }

    fn register(&self, super_block: &Arc<SuperBlock>) {
        self.super_blocks.lock().register(super_block);
    }

    /// Returns a snapshot of live superblocks and prunes dropped ones.
    pub fn live_super_blocks(&self) -> Vec<Arc<SuperBlock>> {
        self.super_blocks.lock().live_super_blocks()
    }

    /// Synchronizes all live superblocks.
    pub fn sync_filesystems(&self) -> VfsResult<()> {
        for super_block in self.live_super_blocks() {
            super_block.sync_fs()?;
        }
        Ok(())
    }
}

/// Superblock-wide filesystem operations.
///
/// A superblock owns one mounted filesystem instance and the state shared by
/// all inodes in that instance. This boundary must not contain per-open-file
/// state.
pub trait SuperBlockOperations: Send + Sync + 'static {
    /// Returns the filesystem type name.
    fn name(&self) -> &str;

    /// Returns the root dentry for this superblock.
    fn root_dentry(&self) -> Dentry;

    /// Returns filesystem statistics.
    fn statfs(&self) -> VfsResult<StatFs>;

    /// Writes back superblock-owned dirty state.
    fn sync_fs(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Writes back filesystem-owned metadata for one inode.
    fn write_inode(&self, _inode: &VfsInode, _control: &mut WritebackControl) -> VfsResult<()> {
        Ok(())
    }

    /// Returns the maximum regular-file byte offset supported by this superblock.
    fn max_file_size(&self) -> u64 {
        crate::MAX_LFS_FILESIZE
    }

    /// Releases filesystem-owned inode state during final inode teardown.
    ///
    /// The default drops the inode page cache without treating final teardown
    /// as ordinary writeback.
    fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        default_evict_inode(inode)
    }
}

/// Releases the VFS-owned state for an inode with no filesystem-specific
/// teardown requirements.
pub fn default_evict_inode(inode: &VfsInode) -> VfsResult<()> {
    inode.address_space().truncate_final()
}

/// Large-file page-cache limit for 64-bit VFS offsets.
pub const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;

bitflags::bitflags! {
    /// Filesystem-wide flags reported through `statfs`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct StatFsFlags: u32 {
        /// Filesystem is read-only.
        const RDONLY = 0x1;
        /// Ignore set-user-ID and set-group-ID bits.
        const NOSUID = 0x2;
        /// Disallow access to device special files.
        const NODEV = 0x4;
        /// Disallow program execution.
        const NOEXEC = 0x8;
        /// `f_flags` support is implemented.
        const VALID = 0x20;
        /// Do not update file access times.
        const NOATIME = 0x400;
        /// Do not update directory access times.
        const NODIRATIME = 0x800;
        /// Update access time relative to mtime/ctime.
        const RELATIME = 0x1000;
        /// Do not follow symlinks.
        const NOSYMFOLLOW = 0x2000;
    }
}

/// Filesystem statistics returned by [`SuperBlockOperations::statfs`].
pub struct StatFs {
    /// Filesystem type identifier.
    pub fs_type: u32,
    /// Fundamental block size (bytes).
    pub block_size: u32,
    /// Total data blocks in the filesystem.
    pub blocks: u64,
    /// Free blocks in the filesystem.
    pub blocks_free: u64,
    /// Blocks available to unprivileged users.
    pub blocks_available: u64,

    /// Total file count.
    pub file_count: u64,
    /// Free file count.
    pub free_file_count: u64,

    /// Maximum filename length.
    pub name_length: u32,
    /// Fragment size (bytes).
    pub fragment_size: u32,
    /// Mount flags in effect.
    pub mount_flags: StatFsFlags,
}

/// VFS superblock object.
///
/// A superblock owns one filesystem instance and the live inode set attached to
/// that instance. Inodes own their address spaces; superblock-wide writeback
/// reaches page cache state through those inodes, matching Linux's
/// `super_block` -> `inode` -> `address_space` layering.
pub struct SuperBlock {
    ops: Arc<dyn SuperBlockOperations>,
    root: Dentry,
    max_file_size: u64,
    inodes: Mutex<Vec<Weak<VfsInode>>>,
}

impl SuperBlock {
    /// Returns the filesystem type name.
    pub fn name(&self) -> &str {
        self.ops.name()
    }

    /// Returns the root dentry for this superblock.
    pub fn root_dir(self: &Arc<Self>) -> Dentry {
        self.root.clone()
    }

    /// Returns filesystem statistics for this superblock.
    pub fn stat(&self) -> VfsResult<StatFs> {
        self.ops.statfs()
    }

    /// Creates a superblock from superblock operations.
    pub fn new(ops: Arc<dyn SuperBlockOperations>) -> Arc<Self> {
        let root = ops.root_dentry();
        let max_file_size = ops.max_file_size().min(MAX_LFS_FILESIZE);
        let super_block = Arc::new(Self {
            ops,
            root: root.clone(),
            max_file_size,
            inodes: Mutex::default(),
        });
        root.bind_super_block(&super_block);
        super_block_registry().register(&super_block);
        super_block
    }

    /// Returns this superblock's maximum regular-file size.
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    /// Tracks an inode attached to this superblock.
    pub(crate) fn register_inode(&self, inode: &Arc<VfsInode>) {
        let mut inodes = self.inodes.lock();
        inodes.retain(|existing| match existing.upgrade() {
            Some(existing) => !Arc::ptr_eq(&existing, inode),
            None => false,
        });
        inodes.push(Arc::downgrade(inode));
    }

    fn live_inodes(&self) -> Vec<Arc<VfsInode>> {
        let mut live = Vec::new();
        self.inodes.lock().retain(|inode| match inode.upgrade() {
            Some(inode) => {
                live.push(inode);
                true
            }
            None => false,
        });
        live
    }

    /// Starts writeback for dirty inode page-cache state on this superblock.
    fn writeback_inodes(inodes: &[Arc<VfsInode>], data_only: bool) -> VfsResult<()> {
        for inode in inodes {
            inode.sync(data_only)?;
        }
        Ok(())
    }

    /// Writes back filesystem-owned metadata for live inodes on this superblock.
    fn writeback_inode_metadata(&self, inodes: &[Arc<VfsInode>], data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::all(data_only);
        for inode in inodes {
            self.ops.write_inode(inode.as_ref(), &mut control)?;
        }
        Ok(())
    }

    /// Synchronizes inode page-cache state and then filesystem-owned state.
    pub fn sync_fs(&self) -> VfsResult<()> {
        let inodes = self.live_inodes();
        Self::writeback_inodes(&inodes, false)?;
        self.writeback_inode_metadata(&inodes, false)?;
        self.ops.sync_fs()
    }

    /// Releases filesystem-owned inode state during final inode teardown.
    pub(crate) fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        self.ops.evict_inode(inode)
    }
}

impl core::fmt::Debug for SuperBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SuperBlock")
            .field("name", &self.name())
            .finish()
    }
}
