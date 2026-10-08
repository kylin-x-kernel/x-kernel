// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem callbacks and default address-space operations.

use pagecache::Folio;

use super::{AddressSpace, ReadaheadControl, WritebackControl};
use crate::{NodeFlags, NodeType, VfsError, VfsResult};

/// Buffered write setup request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteBeginRequest {
    pos: u64,
    len: usize,
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
/// Implementations should be tied to the owning inode/superblock state, not to
/// one open file instance.
pub trait AddressSpaceOperations: Send + Sync + 'static {
    /// Initializes a locked resident folio from backing storage synchronously.
    ///
    /// The folio may be newly allocated or retained after an earlier failed
    /// read. Initialize its entire contents on every successful attempt;
    /// do not rely on contents left by a failed attempt.
    ///
    /// Success means the entire folio is initialized, including zeroes for
    /// holes, unwritten extents and the tail beyond EOF. The filesystem owns
    /// the read boundary and must report incomplete backing reads as errors;
    /// a later EOF sample must not reinterpret an already completed read.
    /// The cache marks the folio uptodate only after success. On failure it
    /// remains non-uptodate for retry, without exposing partial contents.
    ///
    /// Runs with the folio locked and the cache index lock released. Do not
    /// reenter the mapping or acquire inode data or invalidate exclusive locks:
    /// callers may already hold them or the invalidate shared lock.
    fn read_folio(&self, _mapping: &AddressSpace, _folio: &mut Folio) -> VfsResult<()> {
        Err(VfsError::NoSuchDevice)
    }

    /// Writes all dirty pages known to this address space.
    ///
    /// [`AddressSpace`] holds its invalidate lock for reading across this
    /// callback, including backing-store I/O. Implementations may block, but
    /// must not enter the inode truncate path or otherwise require the
    /// invalidate lock exclusively.
    /// Buffered-write rollback may wait for writeback while holding the inode
    /// data lock, so this callback and its I/O completion must not acquire it.
    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.writeback_cached_folios(control, |_, _, _| Err(VfsError::NoSuchDevice))
    }

    /// Marks a folio dirty.
    fn dirty_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<bool> {
        let was_dirty = folio.is_dirty();
        folio.mark_dirty();
        Ok(!was_dirty)
    }

    /// Initializes a locked resident run synchronously, completing folios in order.
    ///
    /// The invalidate shared lock covers reservation and initialization.
    /// Folios are already locked; do not reenter their mapping or acquire
    /// the inode data lock or invalidate exclusive lock here.
    /// Consumed failures remain non-uptodate for demand `read_folio` retry;
    /// unconsumed reservations are removed when the callback returns.
    ///
    /// The default consumes all reservations through `read_folio`, marking
    /// successful pages uptodate and continuing after errors. It returns the
    /// first error after attempting the entire run.
    fn readahead(&self, control: &mut ReadaheadControl<'_, '_>) -> VfsResult<()> {
        control.read_folios(|mapping, folio| self.read_folio(mapping, folio))
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
    fn read_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        let page = folio.data();
        page.fill(0);
        Ok(())
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        // In-memory files have no backing store; the page cache is the
        // storage. Explicit writeback only marks cached data clean.
        mapping.writeback_cached_folios(control, |_, _, _| Ok(()))
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
    fn read_folio(&self, _mapping: &AddressSpace, _folio: &mut Folio) -> VfsResult<()> {
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
