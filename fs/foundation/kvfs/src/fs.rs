// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem traits and wrappers.
use alloc::sync::Arc;

use inherit_methods_macro::inherit_methods;

use crate::{DirEntry, Mutex, SuperBlockOperations, TypeMap, VfsResult};

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

/// Filesystem statistics returned by [`FilesystemOps::stat`].
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

/// Trait for legacy filesystem operations.
///
/// New VFS code should treat these methods as superblock operations and reach
/// them through [`SuperBlock`]. The trait is kept so existing filesystems can
/// migrate without a single tree-wide flag day.
pub trait FilesystemOps: Send + Sync + 'static {
    /// Gets the name of the filesystem
    fn name(&self) -> &str;

    /// Gets the root directory entry of the filesystem
    fn root_dir(&self) -> DirEntry;

    /// Returns statistics about the filesystem
    fn stat(&self) -> VfsResult<StatFs>;

    /// Flushes the filesystem, ensuring all data is written to disk
    fn flush(&self) -> VfsResult<()> {
        Ok(())
    }
}

impl<T: FilesystemOps> SuperBlockOperations for T {
    fn name(&self) -> &str {
        FilesystemOps::name(self)
    }

    fn root_dentry(&self) -> DirEntry {
        self.root_dir()
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        self.stat()
    }

    fn sync_fs(&self) -> VfsResult<()> {
        self.flush()
    }
}

/// VFS superblock object.
///
/// A superblock owns one filesystem instance and superblock-scoped
/// attachments. In Linux terms this is the `struct super_block` object; dentries
/// and inodes point into it, while open files do not own it.
pub struct SuperBlock {
    ops: Arc<dyn FilesystemOps>,
    data: Mutex<TypeMap>,
}

#[inherit_methods(from = "self.ops")]
impl SuperBlock {
    pub fn name(&self) -> &str;

    pub fn root_dir(&self) -> DirEntry;

    pub fn stat(&self) -> VfsResult<StatFs>;

    pub fn flush(&self) -> VfsResult<()>;
}

impl SuperBlock {
    /// Creates a superblock from legacy filesystem operations.
    pub fn new(ops: Arc<dyn FilesystemOps>) -> Arc<Self> {
        Arc::new(Self {
            ops,
            data: Mutex::default(),
        })
    }

    /// Returns the legacy operations object while filesystems are migrating.
    pub fn legacy_ops(&self) -> &Arc<dyn FilesystemOps> {
        &self.ops
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
    pub fn new(ops: Arc<dyn FilesystemOps>) -> Self {
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
