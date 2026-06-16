// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS operation families.
//!
//! These traits name the ownership boundaries for superblock-wide operations,
//! inode namespace operations, open-file operations, and address-space/page-cache
//! operations. `SuperBlockOperations` is the owned superblock boundary; the
//! other families still provide adapters while filesystems migrate away from
//! legacy node traits.

use crate::{
    DirEntry, DirEntrySink, DirNode, FileNode, Metadata, MetadataUpdate, NodePermission, NodeType,
    StatFs, VfsResult,
};

/// Superblock-wide filesystem operations.
///
/// A superblock owns one mounted filesystem instance and the state shared by
/// all inodes in that instance. This boundary must not contain per-open-file
/// state.
pub trait SuperBlockOperations: Send + Sync + 'static {
    /// Returns the filesystem type name.
    fn name(&self) -> &str;

    /// Returns the root dentry for this superblock.
    fn root_dentry(&self) -> DirEntry;

    /// Returns filesystem statistics.
    fn statfs(&self) -> VfsResult<StatFs>;

    /// Writes back superblock-owned dirty state.
    fn sync_fs(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Returns the maximum regular-file byte offset supported by this superblock.
    fn max_file_size(&self) -> u64 {
        crate::MAX_LFS_FILESIZE
    }
}

/// Inode metadata and namespace operations.
///
/// This groups inode-scoped namespace operations, plus the current directory
/// iteration hook that still needs to move out of the compatibility
/// `DirNodeOps` trait.
pub trait InodeOperations: Send + Sync {
    /// Returns metadata for this inode.
    fn metadata(&self) -> VfsResult<Metadata>;

    /// Updates inode metadata.
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    /// Looks up a child dentry below `dir`.
    fn lookup(&self, _dir: &DirEntry, _name: &str) -> VfsResult<DirEntry> {
        Err(crate::VfsError::NotADirectory)
    }

    /// Creates a child dentry below `dir`.
    fn create(
        &self,
        _dir: &DirEntry,
        _name: &str,
        _node_type: NodeType,
        _permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        Err(crate::VfsError::NotADirectory)
    }

    /// Links `source` into `dir` under `name`.
    fn link(&self, _dir: &DirEntry, _name: &str, _source: &DirEntry) -> VfsResult<DirEntry> {
        Err(crate::VfsError::NotADirectory)
    }

    /// Unlinks a child name below `dir`.
    fn unlink(&self, _dir: &DirEntry, _name: &str) -> VfsResult<()> {
        Err(crate::VfsError::NotADirectory)
    }

    /// Renames a child from one directory to another.
    fn rename(
        &self,
        _old_dir: &DirEntry,
        _old_name: &str,
        _new_dir: &DirEntry,
        _new_name: &str,
    ) -> VfsResult<()> {
        Err(crate::VfsError::NotADirectory)
    }

    /// Reads directory entries for this inode.
    fn read_dir(
        &self,
        _dir: &DirEntry,
        _offset: u64,
        _sink: &mut dyn DirEntrySink,
    ) -> VfsResult<usize> {
        Err(crate::VfsError::NotADirectory)
    }
}

/// Open-file operations.
///
/// This is the future home for open-file behavior. The current compatibility
/// path still routes most calls through `FileNodeOps`.
pub trait FileOperations: Send + Sync {
    /// Reads file data at `offset`.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize>;

    /// Writes file data at `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize>;

    /// Flushes an open file description.
    fn flush(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Synchronizes this file's data and optionally metadata.
    fn fsync(&self, data_only: bool) -> VfsResult<()>;
}

/// Adapter from an existing file node to the new file-ops shape.
pub struct FileNodeFileOperations<'a> {
    file: &'a FileNode,
}

impl<'a> FileNodeFileOperations<'a> {
    /// Creates an adapter over `file`.
    pub fn new(file: &'a FileNode) -> Self {
        Self { file }
    }
}

impl FileOperations for FileNodeFileOperations<'_> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.file.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.file.write_at(buf, offset)
    }

    fn fsync(&self, data_only: bool) -> VfsResult<()> {
        self.file.sync(data_only)
    }
}

/// Page-cache and backing-store operations for an inode address space.
///
/// This is the target boundary for page-cache and backing-store operations.
/// Buffered I/O and mmap should converge here instead of reaching through
/// byte-level `read_at`/`write_at` methods.
///
/// Implementations should be tied to the owning inode/superblock state, not to
/// one open file instance.
pub trait AddressSpaceOperations: Send + Sync + 'static {
    /// Reads one page from backing storage into `page`.
    fn read_page(&self, page_index: u64, page: &mut [u8]) -> VfsResult<usize>;

    /// Writes one page from `page` to backing storage.
    fn write_page(&self, page_index: u64, page: &[u8]) -> VfsResult<usize>;

    /// Writes all dirty pages known to this address space.
    fn writepages(&self, data_only: bool) -> VfsResult<()>;

    /// Invalidates cached pages starting at `page_index`.
    fn invalidate_from(&self, page_index: u64) -> VfsResult<()>;
}

/// Adapter from an existing directory node to the new inode-ops shape.
///
/// This keeps PR2 incremental: callers can target `InodeOperations` while old
/// filesystems continue to implement `DirNodeOps`.
pub struct DirNodeInodeOperations<'a> {
    dir: &'a DirNode,
}

impl<'a> DirNodeInodeOperations<'a> {
    /// Creates an adapter over `dir`.
    pub fn new(dir: &'a DirNode) -> Self {
        Self { dir }
    }
}

impl InodeOperations for DirNodeInodeOperations<'_> {
    fn metadata(&self) -> VfsResult<Metadata> {
        self.dir.metadata()
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.dir.update_metadata(update)
    }

    fn lookup(&self, _dir: &DirEntry, name: &str) -> VfsResult<DirEntry> {
        self.dir.lookup(name)
    }

    fn create(
        &self,
        _dir: &DirEntry,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        self.dir.create(name, node_type, permission)
    }

    fn link(&self, _dir: &DirEntry, name: &str, source: &DirEntry) -> VfsResult<DirEntry> {
        self.dir.link(name, source)
    }

    fn unlink(&self, _dir: &DirEntry, name: &str) -> VfsResult<()> {
        let entry = self.dir.lookup(name)?;
        self.dir.unlink(name, entry.is_dir())
    }

    fn rename(
        &self,
        _old_dir: &DirEntry,
        old_name: &str,
        new_dir: &DirEntry,
        new_name: &str,
    ) -> VfsResult<()> {
        self.dir.rename(old_name, new_dir.as_dir()?, new_name)
    }

    fn read_dir(
        &self,
        _dir: &DirEntry,
        offset: u64,
        sink: &mut dyn DirEntrySink,
    ) -> VfsResult<usize> {
        self.dir.read_dir(offset, sink)
    }
}
