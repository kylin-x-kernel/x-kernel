// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem callbacks and default address-space operations.

use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, PageIndex};

use super::{AddressSpace, ReadaheadControl, WritebackControl};
use crate::{NodeFlags, NodeType, VfsError, VfsResult};

/// Buffered write setup request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteBeginRequest {
    pos: u64,
    len: usize,
}

/// Shared writable-mapping fault preparation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageMkwriteRequest {
    pos: u64,
    len: usize,
}

impl PageMkwriteRequest {
    /// Creates a request for the valid file bytes in one cached folio.
    pub const fn new(pos: u64, len: usize) -> Self {
        Self { pos, len }
    }

    /// Returns the file byte offset at which preparation starts.
    pub const fn pos(self) -> u64 {
        self.pos
    }

    /// Returns the number of valid file bytes to prepare.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether the request contains no valid file bytes.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl WriteBeginRequest {
    /// Creates a request for a buffered-write byte range.
    pub const fn new(pos: u64, len: usize) -> Self {
        Self { pos, len }
    }

    /// Returns the starting file byte offset.
    pub const fn pos(self) -> u64 {
        self.pos
    }

    /// Returns the requested byte count.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether the requested byte range is empty.
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
    /// Creates a request for a buffered-write byte range.
    pub const fn new(pos: u64, len: usize, copied: usize) -> Self {
        Self { pos, len, copied }
    }

    /// Returns the starting file byte offset.
    pub const fn pos(self) -> u64 {
        self.pos
    }

    /// Returns the requested byte count.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether the requested byte range is empty.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Returns the number of bytes copied into the cache.
    pub const fn copied(self) -> usize {
        self.copied
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
    /// Reads backing bytes at `offset`.
    fn read_at(&self, _mapping: &AddressSpace, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NoSuchDevice)
    }

    /// Writes backing bytes at `offset`.
    fn write_at(&self, _mapping: &AddressSpace, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NoSuchDevice)
    }

    /// Reads backing storage into a newly materialized folio.
    fn read_folio(
        &self,
        mapping: &AddressSpace,
        folio: &mut Folio,
        index: PageIndex,
    ) -> VfsResult<usize> {
        let page = folio.data();
        let offset = index
            .checked_mul(page.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        self.read_at(mapping, page, offset)
    }

    /// Writes all dirty pages known to this address space.
    ///
    /// [`AddressSpace`] holds its invalidate lock for reading across this
    /// callback, including backing-store I/O. Implementations may block, but
    /// must not call [`AddressSpace::set_len`] or otherwise require the
    /// invalidate lock exclusively.
    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.writeback_cached_folios(control, |index, data, valid_len| {
            if valid_len == 0 {
                return Ok(());
            }
            let offset = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(VfsError::InvalidInput)?;
            let mut written = 0usize;
            while written < valid_len {
                let n =
                    self.write_at(mapping, &data[written..valid_len], offset + written as u64)?;
                if n == 0 {
                    return Err(VfsError::WriteZero);
                }
                written += n;
            }
            Ok(())
        })
    }

    /// Changes the file length and updates its inode address space.
    ///
    /// [`AddressSpace`] invokes this callback while holding its invalidate lock
    /// exclusively. Implementations own the filesystem-specific ordering and
    /// must call [`AddressSpace::truncate_setsize`] exactly once after the
    /// backing inode is prepared and before freeing blocks that could still be
    /// reachable through cached folios. That helper publishes `i_size` and
    /// performs both mmap invalidation passes around cache truncation.
    fn set_len(&self, _mapping: &AddressSpace, _len: u64) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    /// Marks a folio dirty.
    fn dirty_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<bool> {
        let was_dirty = folio.is_dirty();
        folio.mark_dirty();
        Ok(!was_dirty)
    }

    /// Prepares filesystem mapping state before a shared folio becomes writable.
    ///
    /// The address-space invalidate lock is held for reading and the target
    /// folio is locked while this callback runs.
    fn page_mkwrite(&self, _mapping: &AddressSpace, _request: PageMkwriteRequest) -> VfsResult<()> {
        Ok(())
    }

    /// Starts readahead for the supplied folio window.
    fn readahead(&self, _mapping: &AddressSpace, _control: ReadaheadControl) -> VfsResult<()> {
        Ok(())
    }

    /// Prepares a buffered write.
    ///
    /// If copying into the page cache fails after this hook succeeds,
    /// [`Self::write_end`] is called with `copied == 0` so the filesystem can
    /// release reservations or other per-write state established here.
    fn write_begin(&self, _mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    /// Completes or cancels a buffered write.
    ///
    /// A `copied == 0` request can be a cancellation for a previously
    /// successful [`Self::write_begin`] call. When accepting bytes that extend
    /// the file, the implementation must publish the new visible size with
    /// [`AddressSpace::write_end_set_size`].
    fn write_end(&self, _mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let _ = request;
        Err(VfsError::InvalidInput)
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
}

pub(crate) fn default_address_space_operations(
    inode_flags: NodeFlags,
    node_type: NodeType,
) -> &'static dyn AddressSpaceOperations {
    if node_type == NodeType::RegularFile && inode_flags.contains(NodeFlags::ALWAYS_CACHE) {
        in_memory_address_space_operations()
    } else {
        empty_address_space_operations()
    }
}

fn in_memory_address_space_operations() -> &'static dyn AddressSpaceOperations {
    &IN_MEMORY_ADDRESS_SPACE_OPERATIONS
}

pub(crate) fn empty_address_space_operations() -> &'static dyn AddressSpaceOperations {
    &EMPTY_ADDRESS_SPACE_OPERATIONS
}

struct InMemoryAddressSpaceOperations;

static IN_MEMORY_ADDRESS_SPACE_OPERATIONS: InMemoryAddressSpaceOperations =
    InMemoryAddressSpaceOperations;

impl AddressSpaceOperations for InMemoryAddressSpaceOperations {
    fn read_folio(
        &self,
        _mapping: &AddressSpace,
        folio: &mut Folio,
        page_index: PageIndex,
    ) -> VfsResult<usize> {
        let _ = page_index;
        let page = folio.data();
        page.fill(0);
        Ok(page.len())
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        // In-memory files have no backing store; the page cache is the
        // storage. Explicit writeback only marks cached data clean.
        mapping.writeback_cached_folios(control, |_, _, _| Ok(()))
    }

    fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        mapping.truncate_setsize(len)
    }

    fn write_begin(&self, _mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        Ok(())
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        crate::libfs::simple_write_end(mapping, request)
    }
}

struct EmptyAddressSpaceOperations;

static EMPTY_ADDRESS_SPACE_OPERATIONS: EmptyAddressSpaceOperations = EmptyAddressSpaceOperations;

impl AddressSpaceOperations for EmptyAddressSpaceOperations {
    fn read_folio(
        &self,
        _mapping: &AddressSpace,
        _folio: &mut Folio,
        _index: PageIndex,
    ) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    fn writepages(
        &self,
        _mapping: &AddressSpace,
        _control: &mut WritebackControl,
    ) -> VfsResult<()> {
        Ok(())
    }
}
