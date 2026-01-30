//! Filesystem traits and wrappers.
use alloc::sync::Arc;

use inherit_methods_macro::inherit_methods;

use crate::{DirEntry, VfsResult};

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

/// Trait for filesystem operations.
pub trait FilesystemOps: Send + Sync {
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

/// A reference-counted filesystem wrapper.
#[derive(Clone)]
pub struct Filesystem {
    ops: Arc<dyn FilesystemOps>,
}

#[inherit_methods(from = "self.ops")]
impl Filesystem {
    pub fn name(&self) -> &str;

    pub fn root_dir(&self) -> DirEntry;

    pub fn stat(&self) -> VfsResult<StatFs>;
}

impl Filesystem {
    /// Create a new filesystem wrapper from an implementation object.
    pub fn new(ops: Arc<dyn FilesystemOps>) -> Self {
        Self { ops }
    }
}
