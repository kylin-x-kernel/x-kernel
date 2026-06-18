// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem traits and wrappers.
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use inherit_methods_macro::inherit_methods;

use crate::{AddressSpace, DirEntry, Mutex, SuperBlockOperations, TypeMap, VfsResult};

/// Large-file page-cache limit for 64-bit VFS offsets.
pub const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;

/// Mount read-only.
pub const ST_RDONLY: u32 = 0x1;
/// Ignore set-user-ID and set-group-ID bits.
pub const ST_NOSUID: u32 = 0x2;
/// Disallow access to device special files.
pub const ST_NODEV: u32 = 0x4;
/// Disallow program execution.
pub const ST_NOEXEC: u32 = 0x8;
/// `f_flags` support is implemented.
pub const ST_VALID: u32 = 0x20;
/// Do not update file access times.
pub const ST_NOATIME: u32 = 0x400;
/// Do not update directory access times.
pub const ST_NODIRATIME: u32 = 0x800;
/// Update access time relative to mtime/ctime.
pub const ST_RELATIME: u32 = 0x1000;
/// Do not follow symlinks.
pub const ST_NOSYMFOLLOW: u32 = 0x2000;

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
    pub mount_flags: u32,
}

/// VFS superblock object.
///
/// A superblock owns one filesystem instance and superblock-scoped
/// attachments. Dentries and inodes point into it, while open files do not own
/// it.
pub struct SuperBlock {
    ops: Arc<dyn SuperBlockOperations>,
    max_file_size: u64,
    address_spaces: Mutex<Vec<Weak<AddressSpace>>>,
    data: Mutex<TypeMap>,
}

impl SuperBlock {
    /// Returns the filesystem type name.
    pub fn name(&self) -> &str {
        self.ops.name()
    }

    /// Returns the root dentry for this superblock.
    pub fn root_dir(&self) -> DirEntry {
        self.ops.root_dentry()
    }

    /// Returns filesystem statistics for this superblock.
    pub fn stat(&self) -> VfsResult<StatFs> {
        self.ops.statfs()
    }
}

impl SuperBlock {
    /// Creates a superblock from superblock operations.
    pub fn new(ops: Arc<dyn SuperBlockOperations>) -> Arc<Self> {
        let max_file_size = ops.max_file_size().min(MAX_LFS_FILESIZE);
        Arc::new(Self {
            ops,
            max_file_size,
            address_spaces: Mutex::default(),
            data: Mutex::default(),
        })
    }

    /// Returns the superblock operation family.
    pub fn operations(&self) -> &Arc<dyn SuperBlockOperations> {
        &self.ops
    }

    /// Returns this superblock's maximum regular-file size.
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    /// Registers an inode address space owned by this superblock.
    pub fn register_address_space(&self, address_space: &Arc<AddressSpace>) {
        let mut address_spaces = self.address_spaces.lock();
        let mut exists = false;
        address_spaces.retain(|existing| {
            let Some(existing) = existing.upgrade() else {
                return false;
            };
            if Arc::ptr_eq(&existing, address_space) {
                exists = true;
            }
            true
        });
        if !exists {
            address_spaces.push(Arc::downgrade(address_space));
        }
    }

    fn live_address_spaces(&self) -> Vec<Arc<AddressSpace>> {
        let mut address_spaces = self.address_spaces.lock();
        let mut live = Vec::new();
        address_spaces.retain(|address_space| {
            let Some(address_space) = address_space.upgrade() else {
                return false;
            };
            live.push(address_space);
            true
        });
        live
    }

    /// Writes back dirty page-cache state owned by this superblock.
    pub fn writeback_address_spaces(&self, data_only: bool) -> VfsResult<()> {
        for address_space in self.live_address_spaces() {
            address_space.writepages(data_only)?;
        }
        Ok(())
    }

    /// Synchronizes VFS page-cache state and then filesystem-owned state.
    pub fn sync_fs(&self) -> VfsResult<()> {
        self.writeback_address_spaces(false)?;
        self.ops.sync_fs()
    }

    /// Access superblock-scoped attachment storage.
    pub fn data(&self) -> crate::MutexGuard<'_, TypeMap> {
        self.data.lock()
    }
}

impl core::fmt::Debug for SuperBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SuperBlock")
            .field("name", &self.name())
            .finish()
    }
}

/// A reference-counted filesystem wrapper.
#[derive(Clone)]
pub struct Filesystem {
    super_block: Arc<SuperBlock>,
}

#[inherit_methods(from = "self.super_block")]
impl Filesystem {
    pub fn name(&self) -> &str;

    pub fn root_dir(&self) -> DirEntry;

    pub fn stat(&self) -> VfsResult<StatFs>;
}

impl Filesystem {
    /// Create a new filesystem wrapper from an implementation object.
    pub fn new(ops: Arc<dyn SuperBlockOperations>) -> Self {
        Self {
            super_block: SuperBlock::new(ops),
        }
    }

    /// Create a filesystem wrapper from an existing superblock.
    pub fn from_super_block(super_block: Arc<SuperBlock>) -> Self {
        Self { super_block }
    }

    /// Returns the VFS superblock for this filesystem.
    pub fn super_block(&self) -> &Arc<SuperBlock> {
        &self.super_block
    }
}
