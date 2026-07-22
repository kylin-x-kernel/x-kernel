// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode address-space objects.
//!
//! The page-cache and backing-store operation boundary hangs off
//! `inode->i_mapping`. This module provides the VFS-owned object for that
//! attachment point.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, PageCache, PageCacheKind, PageIndex, WritebackStats};
use vmobj::{
    FileObjectId, MappingView, MappingViewId, MappingViewNotifier, MappingViewRange,
    MappingViewSpec, ObjectInvalidateWork, VmObjectId, next_mapping_view_id,
};

use crate::{Kiocb, Mutex, NodeFlags, NodeType, VfsError, VfsResult, WeakVfsInode};

const MAX_READAHEAD_BYTES: usize = 128 * 1024;
static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);

struct AddressSpaceViewRegistration {
    address_space: Weak<AddressSpace>,
    id: MappingViewId,
}

/// Lifetime guard for a VMA registered in an inode address space.
#[derive(Clone)]
pub struct AddressSpaceViewGuard {
    inner: Arc<AddressSpaceViewRegistration>,
}

impl AddressSpaceViewGuard {
    /// Returns the stable registration id kept alive by this guard.
    pub fn id(&self) -> MappingViewId {
        self.inner.id
    }
}

impl Drop for AddressSpaceViewRegistration {
    fn drop(&mut self) {
        if let Some(address_space) = self.address_space.upgrade() {
            address_space.unregister_view(self.id);
        }
    }
}

struct RegisteredView {
    view: MappingView,
    notifier: Option<Arc<dyn MappingViewNotifier>>,
}

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
pub struct ReadaheadControl {
    address_space: Arc<AddressSpace>,
    start_index: PageIndex,
    count: usize,
}

impl ReadaheadControl {
    fn new(address_space: Arc<AddressSpace>, start_index: PageIndex, count: usize) -> Self {
        Self {
            address_space,
            start_index,
            count,
        }
    }

    pub fn start_index(&self) -> PageIndex {
        self.start_index
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Inserts one completed readahead folio if it is still absent.
    ///
    /// A foreground cache insertion wins over stale backing data.
    pub fn complete_folio(&self, index: PageIndex, offset: usize, src: &[u8]) -> VfsResult<bool> {
        let count = u64::try_from(self.count).map_err(|_| VfsError::InvalidInput)?;
        let end = self
            .start_index
            .checked_add(count)
            .ok_or(VfsError::InvalidInput)?;
        if index < self.start_index || index >= end {
            return Ok(false);
        }
        self.address_space
            .page_cache
            .filemap_add_folio(index, offset, src)
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

    /// Changes the file length and updates its inode address space.
    ///
    /// Implementations own the filesystem-specific ordering and must call
    /// [`AddressSpace::truncate_setsize`] exactly once after the backing inode
    /// is prepared and before freeing blocks that could still be reachable
    /// through cached folios. That helper publishes `i_size` and performs both
    /// mmap invalidation passes around cache truncation.
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

/// VFS address space for one inode.
pub struct AddressSpace {
    inode: WeakVfsInode,
    ops: Arc<dyn AddressSpaceOperations>,
    object_id: VmObjectId,
    views: Mutex<BTreeMap<MappingViewId, RegisteredView>>,
    page_cache: Arc<PageCache>,
}

impl AddressSpace {
    /// Creates an address space for `inode`.
    pub fn new(
        inode: WeakVfsInode,
        ops: Arc<dyn AddressSpaceOperations>,
        kind: PageCacheKind,
    ) -> Arc<Self> {
        Arc::new(Self {
            inode,
            ops,
            object_id: VmObjectId::File(FileObjectId::from_raw(
                NEXT_ADDRESS_SPACE_ID.fetch_add(1, Ordering::Relaxed),
            )),
            views: Mutex::new(BTreeMap::new()),
            page_cache: PageCache::new(kind),
        })
    }

    /// Creates the default address space for an inode with the given flags.
    pub fn new_default(
        inode: WeakVfsInode,
        ops: Arc<dyn AddressSpaceOperations>,
        inode_flags: NodeFlags,
    ) -> Arc<Self> {
        Self::new(inode, ops, address_space_mapping_kind(inode_flags))
    }

    /// Returns the inode owning this address space.
    pub fn inode(&self) -> Option<alloc::sync::Arc<crate::VfsInode>> {
        self.inode.upgrade()
    }

    /// Returns the stable VM identity of this inode address space.
    pub fn object_id(&self) -> VmObjectId {
        self.object_id
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.page_cache.nrpages()
    }

    /// Registers a VMA view of this inode address space.
    pub fn register_view(self: &Arc<Self>, spec: MappingViewSpec) -> AddressSpaceViewGuard {
        let id = next_mapping_view_id();
        self.views.lock().insert(
            id,
            RegisteredView {
                view: MappingView::new(
                    id,
                    spec.mm_id,
                    MappingViewRange {
                        vma_start: spec.vma_start,
                        vma_len: spec.vma_len,
                        object_start: spec.object_start,
                        object_len: spec.object_len,
                    },
                    spec.kind,
                ),
                notifier: spec.notifier,
            },
        );
        AddressSpaceViewGuard {
            inner: Arc::new(AddressSpaceViewRegistration {
                address_space: Arc::downgrade(self),
                id,
            }),
        }
    }

    fn unregister_view(&self, id: MappingViewId) {
        self.views.lock().remove(&id);
    }

    /// Runs `f` with the cached folio at `index`, if present.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        self.page_cache.with_folio(index, f)
    }

    /// Runs `f` with the folio at `index`, materializing it when absent.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        self.page_cache.with_folio_or_create(
            index,
            |index| {
                let mut folio = Folio::new_zeroed()?;
                self.read_folio(&mut folio, index)?;
                Ok(folio)
            },
            f,
        )
    }

    fn read_folio(&self, folio: &mut Folio, index: PageIndex) -> VfsResult<usize> {
        self.ops.read_folio(folio, index)
    }

    fn dirty_folio(&self, folio: &mut Folio) -> VfsResult<bool> {
        self.ops.dirty_folio(self, folio)
    }

    /// Performs a buffered read from this address space into an iterator.
    pub(crate) fn read_iter(
        self: &Arc<Self>,
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
        self: &Arc<Self>,
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

    fn prepare_read_window(
        self: &Arc<Self>,
        offset: u64,
        count: usize,
        file_len: u64,
    ) -> VfsResult<()> {
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
        if count > 1 && self.page_cache.cached_run_len(start_index, count) < count {
            self.readahead(ReadaheadControl::new(self.clone(), start_index, count))?;
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
        self.with_folio_or_create(index, |folio| {
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
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let _data_guard = inode.lock_data();
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
        let mut visible_len = self.visible_len()?;
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
            if prepared_end > visible_len {
                self.page_cache
                    .resize_cached_folios(visible_len, prepared_end);
                // Another iteration is reached only after `write_end` accepts
                // this full range and publishes the same EOF under data_lock.
                visible_len = prepared_end;
            }
            let copied = match self.with_folio_or_create(index, |folio| {
                let data = folio.data();
                let copied = iter.copy_from_iter(&mut data[page_off..page_off + step])?;
                if copied != 0 {
                    self.dirty_folio(folio)?;
                }
                Ok(copied)
            }) {
                Ok(copied) => copied,
                Err(error) => {
                    let _ = self.ops.write_end(self, WriteEndRequest::new(pos, step, 0));
                    return Err(error);
                }
            };
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
        self.ops.set_len(self, len)
    }

    /// Publishes `inode::i_size` and updates mapped/cache state in Linux order.
    ///
    /// Filesystem `set_len` implementations call this after preparing their
    /// backing-store mutation and before releasing blocks. Shrink performs
    /// `i_size_write`, unmap, cache truncation, then a second unmap so a
    /// concurrent private COW fault cannot survive beyond the new EOF.
    pub fn truncate_setsize(&self, len: u64) -> VfsResult<()> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let old_len = inode.size();
        inode.set_size(len);
        self.truncate_pagecache(old_len, len);
        Ok(())
    }

    /// Publishes the size accepted by `AddressSpaceOperations::write_end`.
    ///
    /// The caller must be the generic write path, which holds the inode data
    /// lock and has already prepared any cache bytes between the old EOF and
    /// the completed write. This is the Rust equivalent of an aops
    /// `write_end` implementation doing `i_size_write(mapping->host, len)`.
    pub fn write_end_set_size(&self, len: u64) -> VfsResult<()> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        if len > inode.size() {
            inode.set_size(len);
        }
        Ok(())
    }

    fn truncate_pagecache(&self, old_len: u64, len: u64) {
        let invalid_start = (len < old_len).then(|| {
            len.div_ceil(PAGE_SIZE_4K as u64)
                .saturating_mul(PAGE_SIZE_4K as u64)
        });
        if let Some(start) = invalid_start {
            self.invalidate_mappings_from(start);
        }
        self.page_cache.resize_cached_folios(old_len, len);
        if let Some(start) = invalid_start {
            self.invalidate_mappings_from(start);
        }
    }

    fn invalidate_mappings_from(&self, object_start: u64) {
        let mut hits = Vec::new();
        let mut notifiers = Vec::new();
        let mut object_end = object_start;
        for registered in self.views.lock().values() {
            if registered.view.object_end() <= object_start {
                continue;
            }
            let len =
                usize::try_from(registered.view.object_end() - object_start).unwrap_or(usize::MAX);
            let Some(hit) = registered.view.page_hit(object_start, len) else {
                continue;
            };
            object_end = object_end.max(registered.view.object_end());
            hits.push(hit.clone());
            if let Some(notifier) = &registered.notifier {
                notifiers.push((hit, notifier.clone()));
            }
        }
        if hits.is_empty() {
            return;
        }
        let len = usize::try_from(object_end - object_start).unwrap_or(usize::MAX);
        let work = ObjectInvalidateWork::new(self.object_id, object_start, len, hits);
        for (hit, notifier) in notifiers {
            notifier.invalidate(&work, &hit);
        }
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
            self.visible_len()?,
            control.range_start(),
            control.range_end(),
            control.nr_to_write(),
            &mut write_folio_fn,
        )?;
        control.account_stats(stats);
        Ok(())
    }

    /// Writes cached dirty ranges through the supplied backing-store writer.
    pub fn writeback_cached_ranges(
        &self,
        control: &mut WritebackControl,
        max_bytes: usize,
        mut write_range_fn: impl FnMut(u64, &[u8]) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let stats = self.page_cache.write_cache_pages(
            self.visible_len()?,
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
        let mut written = 0usize;
        while written < src.len() {
            let pos = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            let index = pos / PAGE_SIZE_4K as u64;
            let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
            let step = (src.len() - written).min(PAGE_SIZE_4K - page_off);
            self.with_folio_or_create(index, |folio| {
                folio.data()[page_off..page_off + step]
                    .copy_from_slice(&src[written..written + step]);
                folio.mark_dirty();
                Ok(())
            })?;
            written += step;
        }
        Ok(written)
    }

    fn visible_len(&self) -> VfsResult<u64> {
        self.inode()
            .map(|inode| inode.size())
            .ok_or(VfsError::InvalidInput)
    }
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("inode", &self.inode().map(|inode| inode.inode()))
            .field("object_id", &self.object_id())
            .field("nrpages", &self.nrpages())
            .finish()
    }
}

pub(crate) fn address_space_mapping_kind(inode_flags: NodeFlags) -> PageCacheKind {
    if inode_flags.contains(NodeFlags::ALWAYS_CACHE) {
        PageCacheKind::InMemory
    } else {
        PageCacheKind::FileBacked
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
    use alloc::{
        sync::{Arc, Weak},
        vec::Vec,
    };

    use ksync::Mutex;
    use pagecache::{Folio, PageCacheKind, PageIndex};
    use unittest::def_test;
    use vmobj::{MappingViewKind, MappingViewNotifier, ObjectViewHit};

    use super::*;
    use crate::{FileOperations, InodeOperations, NodePermission, Umode, VfsInode, VfsInodeInit};

    /// An `AddressSpaceOperations` that records whether `writepages` was
    /// called, and how many dirty folios were written back.
    struct TestOps {
        writepages_called: Mutex<bool>,
        writeback_count: Mutex<usize>,
    }

    struct TruncateOps;

    impl InodeOperations for TruncateOps {}
    impl FileOperations for TruncateOps {}

    impl AddressSpaceOperations for TruncateOps {
        fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
            mapping.truncate_setsize(len)
        }

        fn read_folio(&self, folio: &mut Folio, _index: PageIndex) -> VfsResult<usize> {
            folio.data().fill(0);
            Ok(folio.data().len())
        }
    }

    struct RecordingNotifier {
        address_space: Weak<AddressSpace>,
        observed_sizes: Mutex<Vec<u64>>,
    }

    impl MappingViewNotifier for RecordingNotifier {
        fn invalidate(&self, _work: &ObjectInvalidateWork, _hit: &ObjectViewHit) {
            let size = self
                .address_space
                .upgrade()
                .and_then(|mapping| mapping.inode())
                .map(|inode| inode.size())
                .expect("registered address space remains attached to its inode");
            self.observed_sizes.lock().push(size);
        }
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

    #[def_test]
    fn address_space_identity_is_owned_by_address_space() {
        let ops = TestOps::new_arc();
        let first = AddressSpace::new(
            Weak::new(),
            ops.clone() as Arc<dyn AddressSpaceOperations>,
            PageCacheKind::InMemory,
        );
        let second = AddressSpace::new(
            Weak::new(),
            ops as Arc<dyn AddressSpaceOperations>,
            PageCacheKind::InMemory,
        );

        assert_ne!(first.object_id(), second.object_id());
    }

    /// Verifies that final address-space teardown discards cached folios
    /// without forcing ordinary writeback.
    #[def_test]
    fn evict_discards_dirty_folios_without_writeback() {
        let ops = TestOps::new_arc();
        let address_space = AddressSpace::new(
            Weak::new(),
            ops.clone() as Arc<dyn AddressSpaceOperations>,
            PageCacheKind::InMemory,
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
            PageCacheKind::InMemory,
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

    #[def_test]
    fn truncate_publishes_i_size_before_two_mapping_invalidations() {
        let inode = VfsInode::new_file_with_address_space_and_flags(
            Arc::new(TruncateOps),
            NodeFlags::ALWAYS_CACHE,
            VfsInodeInit::new(
                1,
                (PAGE_SIZE_4K * 2) as u64,
                Umode::new(NodeType::RegularFile, NodePermission::default()),
            ),
        );
        let address_space = inode.address_space();
        address_space
            .page_cache
            .filemap_add_folio(1, 0, b"cached tail")
            .expect("cache tail page");
        let notifier = Arc::new(RecordingNotifier {
            address_space: Arc::downgrade(&address_space),
            observed_sizes: Mutex::new(Vec::new()),
        });
        let _view = address_space.register_view(MappingViewSpec {
            mm_id: 1,
            vma_start: 0x4000,
            vma_len: PAGE_SIZE_4K * 2,
            object_start: 0,
            object_len: PAGE_SIZE_4K * 2,
            kind: MappingViewKind::Private,
            notifier: Some(notifier.clone()),
        });

        inode.set_len(0).expect("truncate succeeds");

        assert_eq!(inode.size(), 0);
        assert_eq!(address_space.nrpages(), 0);
        assert_eq!(*notifier.observed_sizes.lock(), [0, 0]);
    }
}
