// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio access and generic buffered I/O.

use core::ops::ControlFlow;

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{FolioGuard, PageIndex, PinnedFolio};

use super::{AddressSpace, WriteBeginRequest, WriteEndRequest};
use crate::{Kiocb, Mutex, VfsError, VfsResult, file_ra::FileRaState};

fn write_prefix_or_error(written: usize, error: VfsError) -> VfsResult<usize> {
    if written == 0 {
        Err(error)
    } else {
        Ok(written)
    }
}

impl AddressSpace {
    /// Pins an initialized cache page while excluding truncate invalidation.
    ///
    /// The callback runs while the address-space invalidation read lock is
    /// held. Pages starting at or beyond EOF are rejected before allocation or
    /// admission. The callback must validate its exact byte range and publish the read-only
    /// PTE together with its file-mapping reference before returning. Truncate
    /// submits mapping invalidation before detaching the cache entry, but that
    /// submission may be deferred while an MM is busy. The mapping owner must
    /// retain the pin until PTE removal and TLB invalidation actually complete.
    /// Callback failure keeps a successfully initialized in-range page resident
    /// and charged, so another access can reuse it.
    pub fn with_pinned_file_page<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(PinnedFolio) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let _invalidate_guard = self.invalidate_lock.read();
        let offset = index
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::BadAddress)?;
        if offset >= self.visible_len()? {
            return Err(VfsError::BadAddress);
        }
        let pin = self
            .i_pages
            .pin_folio_or_create(index, |folio| self.ops.read_folio(self, folio))?;
        f(pin)
    }

    /// Runs `f` with the initialized resident folio at `index`, if present.
    /// Missing or non-uptodate folios are reported as `None`.
    pub fn with_folio<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(Option<&mut FolioGuard<'_>>) -> R,
    ) -> R {
        self.i_pages.with_folio(index, f)
    }

    /// Visits a bounded resident-page snapshot without populating holes.
    ///
    /// Holds one invalidate read guard through the batch, including EOF sampling
    /// and data copying in `visit`. At most [`pagecache::FolioBatch::CAPACITY`]
    /// pages are visited after a single index snapshot; absent pages are `None`.
    /// Retained initialization failures propagate instead of becoming holes.
    /// Callbacks must not reenter this mapping, change folio state, or wait for
    /// another folio. Returns the number visited, including a breaking page.
    ///
    /// # Errors
    ///
    /// Propagates index-range overflow, filesystem initialization, or callback
    /// errors. Earlier callback progress remains committed to its caller.
    pub fn visit_resident_folios(
        &self,
        start: PageIndex,
        count: usize,
        visit: impl FnMut(PageIndex, Option<&mut FolioGuard<'_>>) -> KResult<ControlFlow<()>>,
    ) -> KResult<usize> {
        let _invalidate = self.invalidate_lock.read();
        self.i_pages.visit_resident_folios(
            start,
            count,
            |folio| self.ops.read_folio(self, folio),
            visit,
        )
    }

    /// Runs `f` with the folio at `index`, materializing it when absent.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut FolioGuard<'_>) -> KResult<R>,
    ) -> KResult<R> {
        self.i_pages
            .with_folio_or_create(index, |folio| self.ops.read_folio(self, folio), f)
    }

    /// Prepares and dirties a cached folio for a shared write fault.
    ///
    /// The address-space invalidate lock excludes truncate and cache
    /// invalidation, while the folio lock covers the EOF recheck, filesystem
    /// preparation, dirtying, and the caller's PTE update.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::BadAddress`] if truncate moved the folio beyond EOF,
    /// or propagates filesystem preparation, dirtying, and PTE-update errors.
    pub(crate) fn page_mkwrite<R>(
        &self,
        index: PageIndex,
        prepare: impl FnOnce(u64, usize) -> VfsResult<()>,
        f: impl FnOnce(&mut FolioGuard<'_>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let pos = index
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)?;
        let _invalidate_guard = self.invalidate_lock.read();
        self.with_folio_or_create(index, |folio| {
            let file_len = inode.size();
            if pos >= file_len {
                return Err(VfsError::BadAddress);
            }
            let len = usize::try_from((file_len - pos).min(PAGE_SIZE_4K as u64))
                .map_err(|_| VfsError::InvalidInput)?;
            prepare(pos, len)?;
            self.dirty_folio(folio)?;
            f(folio)
        })
    }

    fn dirty_folio(&self, folio: &mut FolioGuard<'_>) -> VfsResult<bool> {
        self.ops.dirty_folio(self, folio)
    }

    /// Performs a buffered read from this address space into an iterator.
    pub(crate) fn read_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterDest<'_>,
    ) -> VfsResult<usize> {
        let read = self.read_with_ra(iter, iocb.ki_pos(), &iocb.file().readahead)?;
        iocb.advance(read);
        Ok(read)
    }

    fn read_with_ra(
        &self,
        iter: &mut IovIterDest<'_>,
        mut offset: u64,
        ra: &Mutex<FileRaState>,
    ) -> VfsResult<usize> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let mut total = 0usize;
        let mut prepared_end = offset;
        'read: while iter.count() != 0 {
            // Batch resident references independently of the per-open window.
            // A one-page read avoids collecting a batch at all.
            let page_remaining = PAGE_SIZE_4K - (offset % PAGE_SIZE_4K as u64) as usize;
            let batch = if iter.count() > page_remaining {
                let file_len = inode.size();
                if offset >= file_len {
                    break;
                }
                let end = offset.saturating_add(iter.count() as u64).min(file_len);
                let start = offset / PAGE_SIZE_4K as u64;
                let count = end.div_ceil(PAGE_SIZE_4K as u64) - start;
                Some(self.i_pages.get_folios_contig(start, count as usize))
            } else {
                None
            };
            for slot in 0..batch.as_ref().map_or(1, |batch| batch.len().max(1)) {
                let requested = iter.count();
                let page_remaining = PAGE_SIZE_4K - (offset % PAGE_SIZE_4K as u64) as usize;
                // Sample EOF after locking the folio, so a completed shrink cannot
                // expose stale bytes through an earlier file-size snapshot.
                let mut copy = |folio: &mut FolioGuard<'_>| {
                    let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
                    let step = inode
                        .size()
                        .saturating_sub(offset)
                        .min((PAGE_SIZE_4K - page_off) as u64)
                        as usize;
                    if step == 0 {
                        return Ok((0, None));
                    }
                    let copied = iter.copy_to_iter(&folio.data()[page_off..page_off + step])?;
                    let window = if copied != 0 {
                        self.readahead_after_copy(folio, offset, requested, ra)
                    } else {
                        None
                    };
                    Ok((copied, window))
                };
                let cached = match batch.as_ref() {
                    Some(batch) => batch.with_read_folio(slot, &mut copy),
                    None => self.with_folio(offset / PAGE_SIZE_4K as u64, |folio| folio.map(copy)),
                };
                let result = match cached {
                    Some(result) => result,
                    None => {
                        match self.read_cache_folio(offset, iter.count(), &mut prepared_end, ra) {
                            Ok(true) => continue 'read,
                            Ok(false) => break 'read,
                            Err(error) => Err(error),
                        }
                    }
                };
                let (copied, window) = match result {
                    Ok(result) => result,
                    Err(error) if total == 0 => return Err(error),
                    Err(_) => break 'read,
                };
                if let Some((range, marker)) = window {
                    // Both folio and policy guards are gone before invalidation or I/O.
                    self.advance_readahead(range, marker);
                }
                offset += copied as u64;
                total += copied;
                if copied < page_remaining || iter.count() == 0 {
                    break 'read;
                }
            }
        }
        if total != 0 {
            ra.lock().record_position(offset);
        }
        Ok(total)
    }

    #[cfg(unittest)]
    pub(super) fn read_from_page_cache(
        &self,
        iter: &mut IovIterDest<'_>,
        offset: u64,
    ) -> VfsResult<usize> {
        self.read_with_ra(
            iter,
            offset,
            &Mutex::new(FileRaState::new(block::BackingDevInfo::DEFAULT_RA_PAGES)),
        )
    }

    fn readahead_after_copy(
        &self,
        folio: &mut FolioGuard<'_>,
        offset: u64,
        count: usize,
        ra: &Mutex<FileRaState>,
    ) -> Option<(core::ops::Range<PageIndex>, Option<PageIndex>)> {
        if !folio.is_readahead() {
            return None;
        }
        // The caller holds the folio lock. Serialize eligibility, consumption
        // and window selection with EIO retreat; no policy holder waits for a
        // folio or performs I/O.
        let mut policy = ra.lock();
        if !policy.is_enabled() || !folio.take_readahead() {
            return None;
        }
        let index = folio.index();
        let request_pages = (offset
            .saturating_add(count as u64)
            .div_ceil(PAGE_SIZE_4K as u64)
            - index)
            .max(1);
        policy.on_marker(index, request_pages, |limit| {
            self.i_pages
                .next_miss_range(index + 1..index + 1 + limit)
                .map(|run| run.start)
        })
    }

    fn advance_readahead(&self, range: core::ops::Range<PageIndex>, marker: Option<PageIndex>) {
        let _invalidate = self.invalidate_lock.read();
        let Ok(file_len) = self.visible_len() else {
            return;
        };
        self.prepare_readahead(range, marker, file_len);
    }

    fn read_cache_folio(
        &self,
        offset: u64,
        count: usize,
        prepared_end: &mut u64,
        ra: &Mutex<FileRaState>,
    ) -> VfsResult<bool> {
        let _invalidate_guard = self.invalidate_lock.read();
        let file_len = self.visible_len()?;
        if offset >= file_len {
            return Ok(false);
        }
        if offset >= *prepared_end {
            let index = offset / PAGE_SIZE_4K as u64;
            let end = offset.saturating_add(count as u64).min(file_len);
            let request = end.div_ceil(PAGE_SIZE_4K as u64) - index;
            let (range, marker) = ra.lock().on_miss(index, request, |limit| {
                self.i_pages.preceding_cached_pages(index, limit)
            });
            *prepared_end = range.end.saturating_mul(PAGE_SIZE_4K as u64).min(file_len);
            self.prepare_readahead(range, marker, file_len);
        }
        let index = offset / PAGE_SIZE_4K as u64;
        if let Err(error) = self.with_folio_or_create(index, |_| Ok(())) {
            if error == VfsError::Io {
                ra.lock().shrink_after_io_error();
            }
            return Err(error);
        }
        Ok(true)
    }

    /// Performs a buffered write from an iterator into this address space.
    pub(crate) fn write_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<usize> {
        self.write_iter_with_checks(iocb, iter, |_, _| Ok(()))
    }

    pub(crate) fn write_iter_with_checks(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
        checks: impl FnOnce(u64, &mut usize) -> VfsResult<()>,
    ) -> VfsResult<usize> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let _data_guard = inode.lock_data();
        let mut count = iter.count();
        self.generic_write_check_limits(iocb, &mut count)?;
        if count != 0 {
            checks(iocb.ki_pos(), &mut count)?;
        }
        iter.truncate(count);
        let count = iter.count();
        if count == 0 {
            return Ok(0);
        }

        // Updating timestamps may acquire filesystem locks, so do it
        // before perform_write takes a folio lock or exposes dirty data.
        iocb.file().file_update_time()?;
        let written = self.perform_write(iocb.ki_pos(), iter, count)?;
        iocb.advance(written);
        Ok(written)
    }

    fn generic_write_check_limits(&self, iocb: &Kiocb<'_>, count: &mut usize) -> VfsResult<()> {
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

    // The caller holds the inode data lock through rollback to keep size and
    // competing writes stable. Rollback may wait for concurrent writeback;
    // filesystem callbacks have returned before cache shrink, so their locks
    // are released. Writeback completion must not depend on the data lock.
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
            if let Err(error) = self
                .ops
                .write_begin(self, WriteBeginRequest::new(pos, step))
            {
                return write_prefix_or_error(written, error);
            }
            let grew_cache = prepared_end > visible_len;
            if grew_cache {
                self.i_pages.resize_cached_folios(visible_len, prepared_end);
            }
            let mut copied = 0;
            let copy_result = self.with_folio_or_create(index, |folio| {
                let data = folio.data();
                copied = iter.copy_from_iter(&mut data[page_off..page_off + step])?;
                if copied != 0 {
                    self.dirty_folio(folio)?;
                }
                Ok(())
            });
            if let Err(error) = copy_result {
                iter.revert(copied)?;
                if let Err(cancel_error) =
                    self.ops.write_end(self, WriteEndRequest::new(pos, step, 0))
                {
                    log::warn!(
                        "failed to cancel buffered write at {pos} (len {step}): {cancel_error:?}"
                    );
                }
                if grew_cache {
                    self.i_pages.resize_cached_folios(prepared_end, visible_len);
                }
                return write_prefix_or_error(written, error);
            }
            let accepted = match self
                .ops
                .write_end(self, WriteEndRequest::new(pos, step, copied))
            {
                Ok(accepted) => accepted,
                Err(error) => {
                    iter.revert(copied)?;
                    if grew_cache {
                        self.i_pages.resize_cached_folios(prepared_end, visible_len);
                    }
                    return write_prefix_or_error(written, error);
                }
            };
            if accepted > copied {
                return Err(VfsError::InvalidInput);
            }
            if accepted < copied {
                iter.revert(copied - accepted)?;
            }
            // `write_end` publishes EOF as `pos + accepted`; roll the cache back
            // to that same end so unaccepted bytes are not left cached and dirty
            // beyond the visible size.
            let accepted_end = pos
                .checked_add(accepted as u64)
                .ok_or(VfsError::InvalidInput)?;
            if grew_cache && accepted_end < prepared_end {
                self.i_pages
                    .resize_cached_folios(prepared_end, accepted_end);
            }
            visible_len = visible_len.max(accepted_end);
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

    /// Runs a mapping operation while excluding truncate and cache invalidation.
    ///
    /// The callback may publish page-table entries and their backing references;
    /// both must be complete before this guard is released.
    pub fn with_invalidate_read<R>(&self, f: impl FnOnce() -> KResult<R>) -> KResult<R> {
        let _invalidate_guard = self.invalidate_lock.read();
        f()
    }
}
