// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem callbacks and default address-space operations.

use pagecache::{FolioGuard, FolioRead};

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
    /// Takes responsibility for initializing a locked resident folio.
    ///
    /// The folio may be newly allocated or retained after an earlier failed
    /// read. Initialize its entire contents on every successful attempt;
    /// do not rely on contents left by a failed attempt. The same index can
    /// be submitted again after failure or replacement; each token is a distinct
    /// initialization responsibility. Release attempt-local resources on every
    /// completion, including failure, rather than deduplicating by index.
    ///
    /// Returning success means the read was submitted or completed. Complete
    /// the token only after the entire folio is initialized, including zeroes for
    /// holes, unwritten extents and the tail beyond EOF. The filesystem owns
    /// the read boundary and must report incomplete backing reads as errors;
    /// a later EOF sample must not reinterpret an already completed read.
    /// `FolioRead::complete` publishes uptodate on success; failure or dropping
    /// the token leaves it non-uptodate for retry without exposing partial data.
    /// Submission failure must release the token before returning the error.
    /// Submission errors propagate unchanged. After accepted submission, a
    /// failed completion is observed as `VfsError::Io`: the folio records
    /// initialization success, not a second copy of the backing I/O result.
    ///
    /// Runs with the folio locked and the cache index lock released. Do not
    /// reenter the mapping or acquire inode data or invalidate exclusive locks:
    /// callers may already hold them or the invalidate shared lock. A deferred
    /// completion must not require the mapping index, inode data or invalidate
    /// locks: their holders can wait for this folio. It must own every backing
    /// resource it needs after this callback returns.
    fn read_folio(&self, _mapping: &AddressSpace, _folio: FolioRead) -> VfsResult<()> {
        Err(VfsError::NoSuchDevice)
    }

    /// Submits dirty pages selected by the supplied control.
    ///
    /// Return may precede owned I/O completion, including for ALL. Data waiting
    /// and filesystem metadata/journal/device persistence are separate duties.
    ///
    /// [`AddressSpace`] holds its invalidate lock across this callback, normally
    /// for reading; live cache invalidation owns it exclusively. Implementations
    /// may block, but must not enter the inode truncate path or otherwise require
    /// the invalidate lock exclusively.
    /// Buffered-write rollback may wait for writeback while holding the inode
    /// data lock, so this callback and its I/O completion must not acquire it.
    /// I/O completion must also avoid invalidate locks: live cache invalidation
    /// retains exclusive admission while waiting for it.
    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.writeback_cached_folios(control, |_, _, _| Err(VfsError::NoSuchDevice))
    }

    /// Marks a folio dirty.
    fn dirty_folio(&self, _mapping: &AddressSpace, folio: &mut FolioGuard<'_>) -> VfsResult<bool> {
        let was_dirty = folio.is_dirty();
        folio.mark_dirty();
        Ok(!was_dirty)
    }

    /// Submits initialization for a locked resident run in cursor order.
    ///
    /// The invalidate shared lock covers reservation and submission.
    /// Folios are already locked; do not reenter their mapping or acquire
    /// the inode data lock or invalidate exclusive lock here.
    /// Consumed failures remain non-uptodate for demand `read_folio` retry;
    /// unconsumed reservations are removed when the callback returns.
    ///
    /// Consumed tokens follow `read_folio`'s completion and resource contracts.
    /// The default submits every reservation through `read_folio`, continuing
    /// after errors and returning the first submission error.
    fn readahead(&self, control: &mut ReadaheadControl<'_>) -> VfsResult<()> {
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
        _folio: &mut FolioGuard<'_>,
        _offset: usize,
        _len: usize,
    ) -> VfsResult<()> {
        Ok(())
    }

    /// Releases a clean folio if the filesystem has no private attachment left.
    fn release_folio(&self, _mapping: &AddressSpace, _folio: &FolioGuard<'_>) -> VfsResult<bool> {
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
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        let page = folio.data();
        page.fill(0);
        folio.complete(Ok(()))
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
    fn read_folio(&self, _mapping: &AddressSpace, _folio: FolioRead) -> VfsResult<()> {
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
