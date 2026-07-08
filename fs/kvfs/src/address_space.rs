// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode address-space objects.
//!
//! The page-cache and backing-store operation boundary hangs off
//! `inode->i_mapping`. This module provides the VFS-owned object for that
//! attachment point.

use alloc::sync::Arc;

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, Mapping, MappingKind, MappingOps, PageIndex, WritebackStats};

use crate::{Kiocb, NodeFlags, NodeType, VfsError, VfsResult, WeakVfsInode};

const MAX_READAHEAD_BYTES: usize = 128 * 1024;

/// Writeback range and mode for `AddressSpaceOperations::writepages`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WritebackControl {
    range_start: u64,
    range_end: u64,
    sync_mode: WritebackSyncMode,
    nr_to_write: usize,
    pages_skipped: usize,
    data_only: bool,
}

/// Whether a writeback pass is opportunistic or data-integrity oriented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritebackSyncMode {
    /// Start writeback without requiring all matching folios to be written.
    None,
    /// Write every matching dirty folio that can be written synchronously.
    All,
}

impl WritebackControl {
    pub const fn all(data_only: bool) -> Self {
        Self {
            range_start: 0,
            range_end: u64::MAX,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
            data_only,
        }
    }

    pub const fn from(range_start: u64, data_only: bool) -> Self {
        Self {
            range_start,
            range_end: u64::MAX,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
            data_only,
        }
    }

    pub fn range(range_start: u64, len: usize, data_only: bool) -> VfsResult<Self> {
        let range_end = range_start
            .checked_add(len as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(Self {
            range_start,
            range_end,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
            data_only,
        })
    }

    pub const fn with_sync_mode(mut self, sync_mode: WritebackSyncMode) -> Self {
        self.sync_mode = sync_mode;
        self
    }

    pub const fn with_nr_to_write(mut self, nr_to_write: usize) -> Self {
        self.nr_to_write = nr_to_write;
        self
    }

    pub const fn range_start(self) -> u64 {
        self.range_start
    }

    pub const fn range_end(self) -> u64 {
        self.range_end
    }

    pub const fn sync_mode(self) -> WritebackSyncMode {
        self.sync_mode
    }

    pub const fn nr_to_write(self) -> usize {
        self.nr_to_write
    }

    pub const fn pages_skipped(self) -> usize {
        self.pages_skipped
    }

    pub const fn is_data_only(self) -> bool {
        self.data_only
    }

    fn account_stats(&mut self, stats: WritebackStats) {
        self.nr_to_write = self.nr_to_write.saturating_sub(stats.pages_written);
        self.pages_skipped = self.pages_skipped.saturating_add(stats.pages_skipped);
    }
}

/// Readahead window for `AddressSpaceOperations::readahead`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadaheadControl {
    start_index: PageIndex,
    count: usize,
}

impl ReadaheadControl {
    pub const fn new(start_index: PageIndex, count: usize) -> Self {
        Self { start_index, count }
    }

    pub const fn start_index(self) -> PageIndex {
        self.start_index
    }

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
    pub const fn new(pos: u64, len: usize) -> Self {
        Self { pos, len }
    }

    pub const fn pos(self) -> u64 {
        self.pos
    }

    pub const fn len(self) -> usize {
        self.len
    }

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
    pub const fn new(pos: u64, len: usize, copied: usize) -> Self {
        Self { pos, len, copied }
    }

    pub const fn pos(self) -> u64 {
        self.pos
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

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
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NoSuchDevice)
    }

    /// Writes backing bytes at `offset`.
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NoSuchDevice)
    }

    /// Reads backing storage into a newly materialized folio.
    fn read_folio(&self, folio: &mut Folio, index: PageIndex) -> VfsResult<usize> {
        let page = folio.data();
        let offset = index
            .checked_mul(page.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        self.read_at(page, offset)
    }

    /// Writes all dirty pages known to this address space.
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
                let n = self.write_at(&data[written..valid_len], offset + written as u64)?;
                if n == 0 {
                    return Err(VfsError::WriteZero);
                }
                written += n;
            }
            Ok(())
        })
    }

    /// Changes the backing file length.
    fn set_len(&self, _mapping: &AddressSpace, _len: u64) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

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
        Err(VfsError::InvalidInput)
    }

    /// Completes a buffered write.
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

/// VFS address space for one inode.
pub struct AddressSpace {
    inode: WeakVfsInode,
    ops: Arc<dyn AddressSpaceOperations>,
    page_cache: Arc<Mapping>,
}

impl AddressSpace {
    /// Creates an address space for `inode`.
    pub fn new(
        inode: WeakVfsInode,
        ops: Arc<dyn AddressSpaceOperations>,
        kind: MappingKind,
        len: u64,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| {
            let mapping_ops: Arc<dyn MappingOps> = Arc::new(AddressSpacePageCacheOps {
                address_space: this.clone(),
            });
            Self {
                inode,
                ops,
                page_cache: Mapping::new(kind, len, mapping_ops),
            }
        })
    }

    /// Creates the default address space for an inode with the given flags.
    pub fn new_default(
        inode: WeakVfsInode,
        ops: Arc<dyn AddressSpaceOperations>,
        inode_flags: NodeFlags,
        len: u64,
    ) -> Arc<Self> {
        Self::new(inode, ops, address_space_mapping_kind(inode_flags), len)
    }

    /// Returns the inode owning this address space.
    pub fn inode(&self) -> Option<alloc::sync::Arc<crate::VfsInode>> {
        self.inode.upgrade()
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.page_cache.nrpages()
    }

    pub(crate) fn page_cache(&self) -> Arc<Mapping> {
        self.page_cache.clone()
    }

    fn read_folio(&self, folio: &mut Folio, index: PageIndex) -> VfsResult<usize> {
        self.ops.read_folio(folio, index)
    }

    fn dirty_folio(&self, folio: &mut Folio) -> VfsResult<bool> {
        self.ops.dirty_folio(self, folio)
    }

    /// Performs a buffered read from this address space into an iterator.
    pub(crate) fn read_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterDest<'_>,
    ) -> VfsResult<usize> {
        let file_len = self
            .inode()
            .map(|inode| inode.size())
            .ok_or(VfsError::InvalidInput)?;
        let pos = iocb.ki_pos();
        if iter.count() == 0 || pos >= file_len {
            return Ok(0);
        }

        let read = self.read_from_page_cache(iter, pos, file_len)?;
        iocb.advance(read);
        Ok(read)
    }

    fn read_from_page_cache(
        &self,
        iter: &mut IovIterDest<'_>,
        mut offset: u64,
        file_len: u64,
    ) -> VfsResult<usize> {
        let mut total = 0usize;
        while iter.count() != 0 && offset < file_len {
            self.prepare_read_window(offset, iter.count(), file_len)?;
            let copied = self.copy_folio_to_iter(iter, offset, file_len)?;
            if copied == 0 {
                break;
            }
            offset += copied as u64;
            total += copied;
            if copied < Self::copy_len(offset - copied as u64, file_len)? {
                break;
            }
        }
        Ok(total)
    }

    fn prepare_read_window(&self, offset: u64, count: usize, file_len: u64) -> VfsResult<()> {
        let count = u64::try_from(count).map_err(|_| VfsError::InvalidInput)?;
        let remaining =
            usize::try_from((file_len - offset).min(count)).map_err(|_| VfsError::InvalidInput)?;
        let read_len = remaining.min(MAX_READAHEAD_BYTES);
        if read_len == 0 {
            return Ok(());
        }

        let start_index = offset / PAGE_SIZE_4K as u64;
        let end = offset
            .checked_add(read_len as u64)
            .ok_or(VfsError::InvalidInput)?;
        let last_index = end.div_ceil(PAGE_SIZE_4K as u64);
        let count =
            usize::try_from(last_index - start_index).map_err(|_| VfsError::InvalidInput)?;
        if count > 1 && self.page_cache.cached_run_len(start_index, count) == 0 {
            self.readahead(ReadaheadControl::new(start_index, count))?;
        }
        Ok(())
    }

    fn copy_folio_to_iter(
        &self,
        iter: &mut IovIterDest<'_>,
        offset: u64,
        file_len: u64,
    ) -> VfsResult<usize> {
        let index = offset / PAGE_SIZE_4K as u64;
        let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
        let step = Self::copy_len(offset, file_len)?;
        self.page_cache.with_folio_or_create(index, |folio| {
            iter.copy_to_iter(&folio.data()[page_off..page_off + step])
        })
    }

    fn copy_len(offset: u64, file_len: u64) -> VfsResult<usize> {
        let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
        let end = offset
            .saturating_add((PAGE_SIZE_4K - page_off) as u64)
            .min(file_len);
        usize::try_from(end - offset).map_err(|_| VfsError::InvalidInput)
    }

    /// Performs a buffered write from an iterator into this address space.
    pub(crate) fn write_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<usize> {
        let count = self.write_checks(iocb, iter)?;
        if count == 0 {
            return Ok(0);
        }

        let written = self.perform_write(iocb.ki_pos(), iter, count)?;
        iocb.advance(written);
        Ok(written)
    }

    fn write_checks(&self, iocb: &Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        let mut count = iter.count();
        self.write_check_limits(iocb, &mut count)?;
        iter.truncate(count);
        Ok(iter.count())
    }

    fn write_check_limits(&self, iocb: &Kiocb<'_>, count: &mut usize) -> VfsResult<()> {
        if *count == 0 {
            return Ok(());
        }

        let max_file_size = iocb.file().max_file_size();
        let pos = iocb.ki_pos();
        if pos >= max_file_size {
            return Err(VfsError::FileTooLarge);
        }

        let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
        let count_u64 = count_u64.min(max_file_size - pos);
        *count = usize::try_from(count_u64).map_err(|_| VfsError::InvalidInput)?;
        Ok(())
    }

    fn perform_write(
        &self,
        offset: u64,
        iter: &mut IovIterSource<'_>,
        len: usize,
    ) -> VfsResult<usize> {
        let mut written = 0usize;
        while written < len {
            let pos = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            let index = pos / memaddr::PAGE_SIZE_4K as u64;
            let page_off = (pos % memaddr::PAGE_SIZE_4K as u64) as usize;
            let step = (len - written).min(memaddr::PAGE_SIZE_4K - page_off);
            let prepared_end = pos.checked_add(step as u64).ok_or(VfsError::InvalidInput)?;
            self.ops
                .write_begin(self, WriteBeginRequest::new(pos, step))?;
            if prepared_end > self.page_cache.len() {
                self.page_cache.set_len(prepared_end)?;
            }
            let copied = self.page_cache.with_folio_or_create(index, |folio| {
                let data = folio.data();
                let copied = iter.copy_from_iter(&mut data[page_off..page_off + step])?;
                if copied != 0 {
                    self.dirty_folio(folio)?;
                }
                Ok(copied)
            })?;
            let accepted = self
                .ops
                .write_end(self, WriteEndRequest::new(pos, step, copied))?;
            if accepted > copied {
                return Err(VfsError::InvalidInput);
            }
            if accepted < copied {
                iter.revert(copied - accepted)?;
            }
            if accepted == 0 {
                break;
            }
            written += accepted;
            if accepted < step {
                break;
            }
        }

        Ok(written)
    }

    /// Starts readahead for a range of folios.
    pub fn readahead(&self, control: ReadaheadControl) -> VfsResult<()> {
        self.ops.readahead(self, control)
    }

    /// Writes dirty pages belonging to this address space.
    pub fn writepages(&self, data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::all(data_only);
        self.writepages_control(&mut control)
    }

    /// Writes dirty pages intersecting `[start, start + len)`.
    pub fn writepages_range(&self, start: u64, len: usize, data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::range(start, len, data_only)?;
        self.writepages_control(&mut control)
    }

    /// Writes dirty pages from `start` through EOF.
    pub fn writepages_from(&self, start: u64, data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::from(start, data_only);
        self.writepages_control(&mut control)
    }

    /// Writes dirty pages according to an explicit control object.
    pub fn writepages_control(&self, control: &mut WritebackControl) -> VfsResult<()> {
        self.ops.writepages(self, control)
    }

    /// Changes the backing file length through this address space.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        self.ops.set_len(self, len)?;
        self.set_cached_len(len)
    }

    pub(crate) fn set_cached_len(&self, len: u64) -> VfsResult<()> {
        self.page_cache.set_len(len)?;
        Ok(())
    }

    /// Drops cached pages during final inode teardown.
    ///
    /// Ordinary invalidation still requires dirty data to be written back
    /// first, while final inode teardown may discard dirty cached pages because
    /// the inode is leaving the VFS lifecycle.
    pub fn evict(&self) -> VfsResult<()> {
        self.truncate_final()
    }

    /// Drops cached pages during final inode teardown.
    pub fn truncate_final(&self) -> VfsResult<()> {
        self.page_cache.truncate_final()?;
        Ok(())
    }

    pub(crate) fn writeback_cached_folios(
        &self,
        control: &mut WritebackControl,
        mut write_folio_fn: impl FnMut(PageIndex, &[u8], usize) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let stats = self.page_cache.writeback_until(
            control.range_start(),
            control.range_end(),
            control.nr_to_write(),
            &mut write_folio_fn,
        )?;
        control.account_stats(stats);
        Ok(())
    }

    /// Inserts backing-store bytes into one cached folio.
    pub fn cache_folio_range(&self, index: PageIndex, offset: usize, src: &[u8]) -> VfsResult<()> {
        self.page_cache.filemap_add_folio(index, offset, src)
    }

    /// Writes cached dirty ranges through the supplied backing-store writer.
    pub fn writeback_cached_ranges(
        &self,
        control: &mut WritebackControl,
        max_bytes: usize,
        mut write_range_fn: impl FnMut(u64, &[u8]) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let stats = self.page_cache.write_cache_pages(
            control.range_start(),
            control.range_end(),
            control.nr_to_write(),
            max_bytes,
            &mut write_range_fn,
        )?;
        control.account_stats(stats);
        Ok(())
    }

    #[cfg(unittest)]
    fn write_cached_bytes(&self, offset: u64, src: &[u8]) -> VfsResult<usize> {
        self.page_cache.write_from(offset, src)
    }
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("inode", &self.inode().map(|inode| inode.inode()))
            .field("page_cache", &self.page_cache.identity())
            .finish()
    }
}

pub(crate) fn address_space_mapping_kind(inode_flags: NodeFlags) -> MappingKind {
    if inode_flags.contains(NodeFlags::ALWAYS_CACHE) {
        MappingKind::InMemory
    } else {
        MappingKind::FileBacked
    }
}

struct AddressSpacePageCacheOps {
    address_space: alloc::sync::Weak<AddressSpace>,
}

impl AddressSpacePageCacheOps {
    fn address_space(&self) -> VfsResult<Arc<AddressSpace>> {
        self.address_space.upgrade().ok_or(VfsError::InvalidInput)
    }
}

impl MappingOps for AddressSpacePageCacheOps {
    fn instantiate_folio(&self, index: PageIndex) -> KResult<Folio> {
        let address_space = self.address_space()?;
        let mut folio = Folio::new_zeroed()?;
        address_space.read_folio(&mut folio, index)?;
        Ok(folio)
    }
}

pub(crate) fn default_address_space_operations(
    inode_flags: NodeFlags,
    node_type: NodeType,
) -> Arc<dyn AddressSpaceOperations> {
    if node_type == NodeType::RegularFile && inode_flags.contains(NodeFlags::ALWAYS_CACHE) {
        in_memory_address_space_operations()
    } else {
        empty_address_space_operations()
    }
}

pub(crate) fn in_memory_address_space_operations() -> Arc<dyn AddressSpaceOperations> {
    Arc::new(InMemoryAddressSpaceOperations)
}

pub(crate) fn empty_address_space_operations() -> Arc<dyn AddressSpaceOperations> {
    Arc::new(EmptyAddressSpaceOperations)
}

struct InMemoryAddressSpaceOperations;

impl AddressSpaceOperations for InMemoryAddressSpaceOperations {
    fn read_folio(&self, folio: &mut Folio, page_index: PageIndex) -> VfsResult<usize> {
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

    fn set_len(&self, _mapping: &AddressSpace, _len: u64) -> VfsResult<()> {
        Ok(())
    }

    fn write_begin(&self, _mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        Ok(())
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        crate::simple_write_end(mapping, request)
    }
}

struct EmptyAddressSpaceOperations;

impl AddressSpaceOperations for EmptyAddressSpaceOperations {
    fn read_folio(&self, _folio: &mut Folio, _index: PageIndex) -> VfsResult<usize> {
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

#[cfg(unittest)]
mod tests {
    use alloc::sync::{Arc, Weak};

    use ksync::Mutex;
    use pagecache::{Folio, MappingKind, PageIndex};
    use unittest::def_test;

    use super::*;

    /// An `AddressSpaceOperations` that records whether `writepages` was
    /// called, and how many dirty folios were written back.
    struct TestOps {
        writepages_called: Mutex<bool>,
        writeback_count: Mutex<usize>,
    }

    impl TestOps {
        fn new_arc() -> Arc<Self> {
            Arc::new(Self {
                writepages_called: Mutex::new(false),
                writeback_count: Mutex::new(0),
            })
        }
    }

    impl AddressSpaceOperations for TestOps {
        fn read_folio(&self, folio: &mut Folio, _index: PageIndex) -> VfsResult<usize> {
            folio.data().fill(0);
            Ok(folio.data().len())
        }

        fn writepages(
            &self,
            mapping: &AddressSpace,
            control: &mut WritebackControl,
        ) -> VfsResult<()> {
            *self.writepages_called.lock() = true;
            // Simulate writing dirty folios: count and clear them.
            mapping.writeback_cached_folios(control, |_index, _data, _valid_len| {
                *self.writeback_count.lock() += 1;
                Ok(())
            })
        }
    }

    /// Verifies that final address-space teardown discards cached folios
    /// without forcing ordinary writeback.
    #[def_test]
    fn evict_discards_dirty_folios_without_writeback() {
        let ops = TestOps::new_arc();
        let address_space = AddressSpace::new(
            Weak::new(),
            ops.clone() as Arc<dyn AddressSpaceOperations>,
            MappingKind::InMemory,
            4096,
        );
        address_space
            .write_cached_bytes(0, b"hello world")
            .expect("page-cache write should succeed");

        // Evict — this is the code path that was broken.
        address_space.evict().expect("evict should succeed");

        assert!(
            !*ops.writepages_called.lock(),
            "final eviction must not require ordinary writeback"
        );
        assert_eq!(
            *ops.writeback_count.lock(),
            0,
            "final eviction must not write back dirty folios"
        );
        assert_eq!(address_space.nrpages(), 0);
    }

    /// Verifies that `evict()` on a *clean* address space is harmless and
    /// does not spuriously write back clean folios.
    #[def_test]
    fn evict_on_clean_address_space_writes_back_zero_folios() {
        let ops = TestOps::new_arc();
        let address_space = AddressSpace::new(
            Weak::new(),
            ops.clone() as Arc<dyn AddressSpaceOperations>,
            MappingKind::InMemory,
            0,
        );

        // Evict on a clean cache should succeed.
        address_space.evict().expect("evict should succeed");

        assert!(
            !*ops.writepages_called.lock(),
            "final eviction must not call ordinary writeback"
        );
        assert_eq!(
            *ops.writeback_count.lock(),
            0,
            "evict on clean cache should not write back anything",
        );
    }
}
