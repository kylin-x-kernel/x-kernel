// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem callbacks and default address-space operations.

use alloc::vec::Vec;

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
    ///
    /// The caller must validate that the range's exclusive end fits in `u64`
    /// before passing it to a write-begin hook.
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

/// Buffered write completion for a previously prepared byte range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteEndRequest {
    range: WriteBeginRequest,
    copied: usize,
}

impl WriteEndRequest {
    /// Creates a completion for the matching write-begin range.
    ///
    /// `range` must be the admitted range with its end already validated.
    /// `copied` is the byte count copied into the cache, at most `range.len()`;
    /// zero cancels the prepared write without publishing a new file size.
    pub const fn new(range: WriteBeginRequest, copied: usize) -> Self {
        Self { range, copied }
    }

    /// Returns the starting file byte offset.
    pub const fn pos(self) -> u64 {
        self.range.pos()
    }

    /// Returns the requested byte count.
    pub const fn len(self) -> usize {
        self.range.len()
    }

    /// Returns whether the requested byte range is empty.
    pub const fn is_empty(self) -> bool {
        self.range.is_empty()
    }

    /// Returns the number of bytes copied into the cache.
    pub const fn copied(self) -> usize {
        self.copied
    }
}

/// Byte ranges a successful [`AddressSpaceOperations::write_begin`] newly
/// reserved for the matching buffered write.
///
/// The generic write path threads this token into the matching
/// [`AddressSpaceOperations::write_end`] or
/// [`BatchedWriteOperations::release_write_run_tail`] so cancellation releases
/// exactly the units this write owns and never reservations established by
/// earlier writes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WriteReservation {
    ranges: Vec<(u64, u64)>,
}

impl WriteReservation {
    /// Creates an empty reservation for a write that reserved nothing.
    pub const fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    /// Creates a reservation owning the newly reserved half-open byte ranges.
    pub fn from_ranges(ranges: Vec<(u64, u64)>) -> Self {
        Self { ranges }
    }

    /// Returns the newly reserved half-open byte ranges in ascending order.
    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// Returns whether this write_begin newly reserved nothing.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
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

    /// Prepares a per-folio buffered write and reports what this call newly
    /// reserved.
    ///
    /// - `request`: the byte range the generic path intends to copy, with a
    ///   validated exclusive end and never past one page; batched runs use
    ///   [`BatchedWriteOperations::batch_write_begin`] instead.
    ///
    /// Returns: a [`WriteReservation`] listing the half-open byte ranges this
    /// call newly established. A filesystem that reuses existing state returns
    /// an empty token; the generic path threads it unchanged into the matching
    /// [`Self::write_end`] so cancellation releases only these units.
    ///
    /// Errors: `InvalidInput` when buffered writes are unsupported; other
    /// errors propagate unchanged.
    fn write_begin(
        &self,
        _mapping: &AddressSpace,
        _request: WriteBeginRequest,
    ) -> VfsResult<WriteReservation> {
        Err(VfsError::InvalidInput)
    }

    /// Completes or cancels one per-folio buffered write.
    ///
    /// The generic path runs copying, dirty publication and this hook under the
    /// same folio lock, so the hook decides the accepted byte count while no
    /// reader or writeback can observe that folio's new state. The hook may
    /// update reservations and inode metadata, but must not reacquire the
    /// folio, invalidate or truncate the mapping, or wait for writeback.
    /// Copy-failure cancellation runs after releasing the folio lock, passing
    /// `copied == 0` for a previously successful [`Self::write_begin`] call;
    /// cache rollback and its waits also run unlocked.
    ///
    /// The hook releases the unaccepted tail strictly inside `reservation`:
    /// ranges that reservation does not list belong to earlier writes and their
    /// dirty data must keep their space and quota guarantees. When accepting
    /// bytes, return at most [`WriteEndRequest::copied`]. Before returning
    /// success for a nonempty accepted prefix, publish its end with
    /// [`AddressSpace::write_end_set_size`]; that helper owns the growth check
    /// against the inode's current size. Zero accepted bytes must not grow the
    /// file. The caller holds the inode data lock and advances its write cursor
    /// assuming the accepted size is already published. Batched runs do not
    /// reach this hook; they release their unaccepted tail through
    /// [`BatchedWriteOperations::release_write_run_tail`], whose return type
    /// cannot change a published byte count.
    fn write_end(
        &self,
        _mapping: &AddressSpace,
        request: WriteEndRequest,
        _reservation: &WriteReservation,
    ) -> VfsResult<usize> {
        let _ = request;
        Err(VfsError::InvalidInput)
    }

    /// Returns the batched-write vtable, or `None` when buffered writes for
    /// this mapping must always run one page at a time.
    ///
    /// [`AddressSpace`] samples this once at construction; the generic write
    /// path never attempts a batched run for a `None` mapping. The vtable only
    /// amortizes admission and run-tail release: slots complete by copying and
    /// dirtying, and a run's published prefix is fixed before the tail release.
    fn batched_write_ops(&self) -> Option<&'static dyn BatchedWriteOperations> {
        None
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

/// Tail-release request for one batched buffered-write run.
///
/// `published` bytes were copied and dirtied under their own folio locks and
/// are final: the release hook releases resources for the remainder and
/// cannot renegotiate the published count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchWriteRunTail {
    pos: u64,
    len_bytes: usize,
    published: usize,
}

impl BatchWriteRunTail {
    /// Creates a tail-release request for one published run.
    ///
    /// - `pos`: starting file byte offset of the run.
    /// - `len_bytes`: reserved byte count of the run.
    /// - `published`: byte count already copied and dirtied; final.
    pub const fn new(pos: u64, len_bytes: usize, published: usize) -> Self {
        Self {
            pos,
            len_bytes,
            published,
        }
    }

    /// Returns the starting file byte offset of the run.
    pub const fn pos(self) -> u64 {
        self.pos
    }

    /// Returns the reserved byte count of the run.
    pub const fn len_bytes(self) -> usize {
        self.len_bytes
    }

    /// Returns the byte count already copied and dirtied; final.
    pub const fn published(self) -> usize {
        self.published
    }
}

/// Batched-write admission and tail release, available only to backends that
/// declare it through [`AddressSpaceOperations::batched_write_ops`].
///
/// The generic path runs one contiguous run of already-resident, uptodate,
/// in-EOF whole pages through `batch_write_begin` once, per-slot copy and dirty
/// under each folio lock, and `release_write_run_tail` once after those locks
/// are released. Data acceptance is the per-slot copy and dirty step; the
/// return type of `release_write_run_tail` makes rejecting or shortening
/// published bytes unrepresentable.
pub trait BatchedWriteOperations: Send + Sync + 'static {
    /// Reserves a batched write run and reports what this call newly reserved.
    ///
    /// - `request`: the whole run the generic path intends to copy; ranges are
    ///   page-aligned and never past the visible size.
    ///
    /// Returns: a [`WriteReservation`] listing the half-open byte ranges this
    /// call newly established; subranges already reserved by earlier writes
    /// are not owned by this run and never appear in the token.
    ///
    /// Errors: any failure here aborts the batch attempt before publication,
    /// so the generic path falls back to per-folio writes and reports this
    /// hook's error nowhere else.
    fn batch_write_begin(
        &self,
        mapping: &AddressSpace,
        request: WriteBeginRequest,
    ) -> VfsResult<WriteReservation>;

    /// Releases the unaccepted tail of one published batched run.
    ///
    /// - `tail`: the run range with its final published byte count; the
    ///   implementation must not treat any published byte as rejected.
    /// - `reservation`: ranges this run's `batch_write_begin` newly reserved;
    ///   the tail beyond the published prefix is released strictly inside it.
    ///
    /// Returns: `Ok(())` once the tail release completes. The published count
    /// is decided by the generic path and is not an output here.
    ///
    /// Errors: limited to tail-release bookkeeping failures (for example an
    /// accounting-invariant violation); they abort the run's report and must
    /// never be used to express disagreement with published data — such
    /// objections belong in `batch_write_begin`, before publication.
    ///
    /// Locking: caller holds the inode data lock and no folio lock. The hook
    /// may take mount and inode-state locks, but must not reacquire a folio,
    /// invalidate or truncate the mapping, or wait for writeback.
    fn release_write_run_tail(
        &self,
        mapping: &AddressSpace,
        tail: BatchWriteRunTail,
        reservation: &WriteReservation,
    ) -> VfsResult<()>;
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

    fn write_begin(
        &self,
        _mapping: &AddressSpace,
        _request: WriteBeginRequest,
    ) -> VfsResult<WriteReservation> {
        Ok(WriteReservation::new())
    }

    fn write_end(
        &self,
        mapping: &AddressSpace,
        request: WriteEndRequest,
        _reservation: &WriteReservation,
    ) -> VfsResult<usize> {
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
