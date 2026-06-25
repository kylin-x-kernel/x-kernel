// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS operation families.
//!
//! These traits name the ownership boundaries for superblock-wide operations,
//! inode namespace operations, open-file operations, and address-space/page-cache
//! operations.

use pagecache::{Folio, PageIndex};

use crate::{
    AddressSpace, DirEntry, DirEntrySink, Metadata, MetadataUpdate, NodePermission, NodeType,
    StatFs, VfsError, VfsResult,
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
/// This groups inode-scoped namespace operations and directory iteration.
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
/// This is the operation boundary for open-file behavior.
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

/// Writeback range and mode for `AddressSpaceOperations::writepages`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WritebackControl {
    range_start: u64,
    range_end: u64,
    data_only: bool,
}

impl WritebackControl {
    /// Creates a full-address-space writeback request.
    pub const fn all(data_only: bool) -> Self {
        Self {
            range_start: 0,
            range_end: u64::MAX,
            data_only,
        }
    }

    /// Creates a writeback request from `range_start` through EOF.
    pub const fn from(range_start: u64, data_only: bool) -> Self {
        Self {
            range_start,
            range_end: u64::MAX,
            data_only,
        }
    }

    /// Creates a bounded writeback request for `[range_start, range_start + len)`.
    pub fn range(range_start: u64, len: usize, data_only: bool) -> VfsResult<Self> {
        let range_end = range_start
            .checked_add(len as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(Self {
            range_start,
            range_end,
            data_only,
        })
    }

    /// Returns the first byte offset covered by this request.
    pub const fn range_start(self) -> u64 {
        self.range_start
    }

    /// Returns the exclusive byte end offset.
    pub const fn range_end(self) -> u64 {
        self.range_end
    }

    /// Returns whether metadata writeback may be skipped.
    pub const fn is_data_only(self) -> bool {
        self.data_only
    }
}

/// Readahead window for `AddressSpaceOperations::readahead`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadaheadControl {
    start_index: PageIndex,
    count: usize,
}

impl ReadaheadControl {
    /// Creates a readahead window starting at `start_index`.
    pub const fn new(start_index: PageIndex, count: usize) -> Self {
        Self { start_index, count }
    }

    /// Returns the first folio index in the window.
    pub const fn start_index(self) -> PageIndex {
        self.start_index
    }

    /// Returns the number of folios requested.
    pub const fn count(self) -> usize {
        self.count
    }
}

/// Buffered write setup request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteBeginRequest {
    pos: u64,
    len: usize,
}

impl WriteBeginRequest {
    /// Creates a buffered-write setup request for `[pos, pos + len)`.
    pub const fn new(pos: u64, len: usize) -> Self {
        Self { pos, len }
    }

    /// Returns the starting byte offset.
    pub const fn pos(self) -> u64 {
        self.pos
    }

    /// Returns the requested write length.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether the requested write length is zero.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Buffered write completion request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteEndRequest {
    pos: u64,
    len: usize,
    copied: usize,
}

impl WriteEndRequest {
    /// Creates a buffered-write completion request.
    pub const fn new(pos: u64, len: usize, copied: usize) -> Self {
        Self { pos, len, copied }
    }

    /// Returns the starting byte offset.
    pub const fn pos(self) -> u64 {
        self.pos
    }

    /// Returns the original requested write length.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether the original requested write length is zero.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Returns the bytes copied into the page cache.
    pub const fn copied(self) -> usize {
        self.copied
    }
}

/// Direct-I/O operation passed to `AddressSpaceOperations::direct_io`.
pub enum DirectIoRequest<'a> {
    /// Reads file data into a kernel buffer.
    Read { offset: u64, buf: &'a mut [u8] },
    /// Writes file data from a kernel buffer.
    Write { offset: u64, buf: &'a [u8] },
}

/// Page-cache and backing-store operations for an inode address space.
///
/// This is the target boundary for page-cache and backing-store operations.
/// Buffered I/O and mmap should converge here instead of reaching through
/// byte-level `read_at`/`write_at` methods.
///
/// Implementations should be tied to the owning inode/superblock state, not to
/// one open file instance.
/// Inode teardown is driven by VFS inode/superblock lifetime code, not by this
/// operation family.
pub trait AddressSpaceOperations: Send + Sync + 'static {
    /// Reads backing storage into a newly materialized folio.
    fn read_folio(&self, folio: &mut Folio, index: PageIndex) -> VfsResult<usize>;

    /// Writes all dirty pages known to this address space.
    fn writepages(&self, mapping: &AddressSpace, control: WritebackControl) -> VfsResult<()>;

    /// Marks a folio dirty.
    fn dirty_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<bool> {
        let was_dirty = folio.is_dirty();
        folio.mark_dirty();
        Ok(!was_dirty)
    }

    /// Starts readahead for the supplied folio window.
    fn readahead(&self, _mapping: &AddressSpace, _control: ReadaheadControl) -> VfsResult<()> {
        Ok(())
    }

    /// Prepares a buffered write.
    fn write_begin(&self, _mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        Ok(())
    }

    /// Completes a buffered write.
    fn write_end(&self, _mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        Ok(request.copied())
    }

    /// Invalidates cached pages starting at `page_index`.
    fn invalidate_folio(
        &self,
        _mapping: &AddressSpace,
        _folio: &mut Folio,
        _offset: usize,
        _len: usize,
    ) -> VfsResult<()> {
        Ok(())
    }

    /// Releases a clean folio if the filesystem has no private attachment left.
    fn release_folio(&self, _mapping: &AddressSpace, _folio: &Folio) -> VfsResult<bool> {
        Ok(true)
    }

    /// Performs direct I/O for this address space.
    fn direct_io(
        &self,
        _mapping: &AddressSpace,
        _request: DirectIoRequest<'_>,
    ) -> VfsResult<usize> {
        Err(VfsError::NoSuchDevice)
    }
}
