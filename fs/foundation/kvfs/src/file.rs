// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open-file objects.
//!
//! `VfsFile` owns per-open state: opened path, inode, address-space view,
//! access mode, file offset, raw open flags, and file-private attachment
//! storage.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::LinuxError;

use crate::{
    AddressSpace, FileOperations, Location, Mutex, MutexGuard, NodeFlags, TypeMap, VfsError,
    VfsInode, VfsResult,
};

bitflags::bitflags! {
    /// Access mode flags for an opened VFS file.
    #[derive(Debug, Clone, Copy)]
    pub struct VfsFileFlags: u8 {
        /// File may be read.
        const READ = 1;
        /// File may be written.
        const WRITE = 2;
        /// File may be executed.
        const EXECUTE = 4;
        /// Writes append to the current end of file.
        const APPEND = 8;
        /// Path-only file descriptor.
        const PATH = 16;
    }
}

/// A byte range validated by VFS generic read/write checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsIoRange {
    offset: u64,
    len: usize,
}

impl VfsIoRange {
    /// Returns the starting byte offset.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the number of bytes that may be accessed.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether this range contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the exclusive byte end offset.
    pub fn end(&self) -> u64 {
        self.offset + self.len as u64
    }
}

/// Applies generic VFS read range checks.
///
/// Reads beyond `super_block::s_maxbytes` or EOF complete as a zero-length
/// access. Reads that cross either boundary are truncated before the page-cache
/// loop starts.
pub fn generic_read_range(
    location: &Location,
    offset: u64,
    count: usize,
    file_len: u64,
) -> VfsResult<Option<VfsIoRange>> {
    let max_file_size = location.super_block().max_file_size();
    if count == 0 || offset >= max_file_size || offset >= file_len {
        return Ok(None);
    }

    let count = u64::try_from(count).map_err(|_| VfsError::InvalidInput)?;
    let count = count
        .min(max_file_size - offset)
        .min(file_len.saturating_sub(offset));
    if count == 0 {
        return Ok(None);
    }

    Ok(Some(VfsIoRange {
        offset,
        len: usize::try_from(count).map_err(|_| VfsError::InvalidInput)?,
    }))
}

/// Applies generic VFS write range checks.
///
/// Writes starting at or beyond `super_block::s_maxbytes` fail with `EFBIG`.
/// Writes that start before the limit but cross it are truncated into a short
/// write before the page-cache loop starts.
pub fn generic_write_range(
    location: &Location,
    offset: u64,
    count: usize,
) -> VfsResult<Option<VfsIoRange>> {
    if count == 0 {
        return Ok(None);
    }

    let max_file_size = location.super_block().max_file_size();
    if offset >= max_file_size {
        return Err(VfsError::FileTooLarge);
    }

    let count = u64::try_from(count)
        .map_err(|_| VfsError::InvalidInput)?
        .min(max_file_size - offset);
    if count == 0 {
        return Ok(None);
    }

    Ok(Some(VfsIoRange {
        offset,
        len: usize::try_from(count).map_err(|_| VfsError::InvalidInput)?,
    }))
}

/// Checks a new inode size against this location's superblock maximum.
pub fn check_file_size(location: &Location, len: u64) -> VfsResult<()> {
    if len > location.super_block().max_file_size() {
        return Err(VfsError::FileTooLarge);
    }
    Ok(())
}

/// VFS-owned open-file state.
pub struct VfsFile {
    location: Location,
    inode: Arc<VfsInode>,
    address_space: Arc<AddressSpace>,
    operations: Arc<dyn FileOperations>,
    flags: VfsFileFlags,
    position: Option<Mutex<u64>>,
    open_flags: u32,
    nonblock: AtomicBool,
    private_data: Mutex<TypeMap>,
}

impl VfsFile {
    /// Creates a VFS file with no raw open flags.
    pub fn new(location: Location, flags: VfsFileFlags) -> Self {
        Self::with_open_flags(location, flags, 0)
    }

    /// Creates a VFS file with raw user-visible open flags.
    pub fn with_open_flags(location: Location, flags: VfsFileFlags, open_flags: u32) -> Self {
        let operations: Arc<dyn FileOperations> = Arc::new(NodeBackedFileOperations {
            location: location.clone(),
        });
        Self::with_operations(location, flags, open_flags, operations)
    }

    /// Creates a VFS file with explicit file operations.
    pub fn with_operations(
        location: Location,
        flags: VfsFileFlags,
        open_flags: u32,
        operations: Arc<dyn FileOperations>,
    ) -> Self {
        let position = (!location.flags().contains(NodeFlags::STREAM)).then(|| Mutex::new(0));
        let inode = location.vfs_inode().clone();
        let address_space = inode.address_space();

        Self {
            location,
            inode,
            address_space,
            operations,
            flags,
            position,
            open_flags,
            nonblock: AtomicBool::new(false),
            private_data: Mutex::default(),
        }
    }

    /// Returns the opened VFS location.
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// Returns the inode opened by this file.
    pub fn inode(&self) -> &Arc<VfsInode> {
        &self.inode
    }

    /// Returns this file's address space.
    pub fn address_space(&self) -> Arc<AddressSpace> {
        self.address_space.clone()
    }

    /// Returns the operations installed for this open file.
    pub fn operations(&self) -> &Arc<dyn FileOperations> {
        &self.operations
    }

    /// Returns this file's access mode flags.
    pub fn flags(&self) -> VfsFileFlags {
        self.flags
    }

    /// Returns whether this file descriptor is path-only.
    pub fn is_path(&self) -> bool {
        self.flags.contains(VfsFileFlags::PATH)
    }

    /// Checks that the file has the requested access mode.
    pub fn access(&self, flags: VfsFileFlags) -> VfsResult<()> {
        if self.flags.contains(flags) && !self.is_path() {
            Ok(())
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    /// Applies generic VFS read range checks for this open file.
    pub fn generic_read_range(
        &self,
        offset: u64,
        count: usize,
        file_len: u64,
    ) -> VfsResult<Option<VfsIoRange>> {
        generic_read_range(&self.location, offset, count, file_len)
    }

    /// Applies generic VFS write range checks for this open file.
    pub fn generic_write_range(&self, offset: u64, count: usize) -> VfsResult<Option<VfsIoRange>> {
        generic_write_range(&self.location, offset, count)
    }

    /// Checks a new inode size against this file's superblock maximum.
    pub fn check_file_size(&self, len: u64) -> VfsResult<()> {
        check_file_size(&self.location, len)
    }

    /// Returns whether the underlying node is always blocking.
    ///
    /// This is distinct from the open-file `O_NONBLOCK` state. Regular-file
    /// operations are blocking at the inode operation boundary.
    pub fn is_blocking(&self) -> bool {
        self.location.flags().contains(NodeFlags::BLOCKING)
    }

    /// Locks the current file offset, if this file tracks one.
    pub fn position_lock(&self) -> Option<MutexGuard<'_, u64>> {
        self.position.as_ref().map(Mutex::lock)
    }

    /// Locks the current file offset for operations that require seekability.
    pub fn position_lock_or_espipe(&self) -> VfsResult<MutexGuard<'_, u64>> {
        self.position_lock()
            .ok_or_else(|| VfsError::from(LinuxError::ESPIPE))
    }

    /// Returns the current file offset, if this file tracks one.
    pub fn position(&self) -> Option<u64> {
        self.position.as_ref().map(|position| *position.lock())
    }

    /// Sets the current file offset if this file tracks one.
    pub fn set_position(&self, position: u64) -> VfsResult<()> {
        *self.position_lock_or_espipe()? = position;
        Ok(())
    }

    /// Sets the open-file nonblocking flag.
    pub fn set_nonblocking(&self, flag: bool) {
        self.nonblock.store(flag, Ordering::Release);
    }

    /// Returns the open-file nonblocking flag.
    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Acquire)
    }

    /// Returns the raw user-visible open flags.
    pub fn open_flags(&self) -> u32 {
        self.open_flags
    }

    /// Access file-private attachment storage.
    pub fn private_data(&self) -> MutexGuard<'_, TypeMap> {
        self.private_data.lock()
    }
}

struct NodeBackedFileOperations {
    location: Location,
}

impl FileOperations for NodeBackedFileOperations {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.location.entry().as_file()?.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.location.entry().as_file()?.write_at(buf, offset)
    }

    fn fsync(&self, data_only: bool) -> VfsResult<()> {
        if self.location.is_dir() {
            self.location.sync(data_only)
        } else {
            self.location.entry().as_file()?.sync(data_only)
        }
    }
}
