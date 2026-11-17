// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio access and generic buffered I/O.

use core::ops::ControlFlow;

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{FolioGuard, PageIndex, PinnedFolio};

use super::{
    AddressSpace, BatchWriteRunTail, BatchedWriteOperations, WriteBeginRequest, WriteEndRequest,
    WriteReservation,
};
use crate::{Kiocb, Mutex, VfsError, VfsResult, file_ra::FileRaState};

fn write_prefix_or_error(written: usize, error: VfsError) -> VfsResult<usize> {
    if written == 0 {
        Err(error)
    } else {
        Ok(written)
    }
}

/// Result of one contiguous-run attempt in the buffered write path.
enum BatchWriteResult {
    /// The backend does not batch, the run was not attempted, or nothing was
    /// published after cancelling its reservation; the caller must take the
    /// single-folio path from this pos.
    NoRun,
    /// Bytes published through one batched admission and tail-release pair;
    /// never zero.
    Committed(usize),
    /// The run published `committed` bytes before failing; the caller reports
    /// those bytes as a written prefix and surfaces `error` only when no bytes
    /// were committed in total.
    Failed { committed: usize, error: VfsError },
}

/// One buffered-write page step and its cache growth plan.
struct PageWriteState {
    pos: u64,
    copied: usize,
    grew_cache: bool,
    prepared_end: u64,
    visible_len: u64,
}

/// Copy or dirty publication failed after consuming `copied` source bytes.
struct CopyFailure {
    copied: usize,
    error: VfsError,
}

/// Outcome of one folio's buffered-write step.
enum FolioWriteResult {
    /// Copying and dirtying succeeded; the backend completion follows.
    Copied {
        copied: usize,
        completion: VfsResult<usize>,
    },
    /// Copying or dirty publication failed after consuming `copied` bytes.
    Failed(CopyFailure),
}

impl AddressSpace {
    /// Pins an initialized cache page while excluding truncate invalidation.
    ///
    /// The callback runs while the address-space invalidation read lock is
    /// held. Pages starting at or beyond EOF are rejected before allocation or
    /// admission. The callback must validate its exact byte range and publish the read-only
    /// PTE together with its file-mapping reference before returning. Truncate
    /// submits mapping invalidation before detaching the cache entry, but that
    /// submission may be deferred while an MM is busy. Managed publication gives
    /// the PTE an independent physical reference; its retirement belongs to the
    /// page table. A retained pin provides stable content identity, without a
    /// TLB-completion prerequisite for releasing its own reference.
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
    ) -> VfsResult<R> {
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
                        return Ok((0, 0, None));
                    }
                    let copied =
                        iter.copy_to_iter_nofault(&folio.data()[page_off..page_off + step])?;
                    let window = if copied != 0 {
                        self.readahead_after_copy(folio, offset, requested, ra)
                    } else {
                        None
                    };
                    Ok((copied, step.min(requested), window))
                };
                let cached = match batch.as_ref() {
                    Some(batch) => batch.with_read_folio(slot, &mut copy),
                    None => self.with_folio(offset / PAGE_SIZE_4K as u64, |folio| folio.map(copy)),
                };
                let result = match cached {
                    Ok(Some(result)) => result,
                    Err(error) => Err(error),
                    Ok(None) => {
                        match self.read_cache_folio(offset, iter.count(), &mut prepared_end, ra) {
                            Ok(true) => continue 'read,
                            Ok(false) => break 'read,
                            Err(error) => Err(error),
                        }
                    }
                };
                let (copied, requested, window) = match result {
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
                if copied < requested {
                    // A destination fault can enter the same inode. Release the
                    // source folio before waiting, then look up residency and EOF
                    // again; prefaulting never pins the destination mapping.
                    match iter.fault_in_writeable(requested - copied) {
                        Ok(true) => continue 'read,
                        Ok(false) => break 'read,
                        Err(error) if total == 0 => return Err(error),
                        Err(_) => break 'read,
                    }
                }
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
        self.write_iter_with_prepare(iocb, iter, |iocb, count| {
            iocb.generic_write_check_limits(count)?;
            if *count != 0 {
                iocb.file().file_update_time()?;
            }
            Ok(())
        })
    }

    pub(crate) fn write_iter_with_prepare(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
        prepare: impl FnOnce(&Kiocb<'_>, &mut usize) -> VfsResult<()>,
    ) -> VfsResult<usize> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let _data_guard = inode.lock_data();
        let mut count = iter.count();
        prepare(iocb, &mut count)?;
        iter.truncate(count);
        let count = iter.count();
        if count == 0 {
            return Ok(0);
        }

        let written = self.perform_write(iocb.ki_pos(), iter, count)?;
        iocb.advance(written);
        Ok(written)
    }

    /// Copies one page step into a locked folio and dirties it.
    ///
    /// - `folio`: caller-locked write target covering `[pos, pos + step)`.
    /// - `page_off`: byte offset of the step inside the page.
    /// - `step`: request length, never past the page end.
    /// - `iter`: source; advanced by exactly the copied prefix.
    ///
    /// Returns: the copied prefix on success; a failure carries the prefix
    /// consumed before it.
    ///
    /// Errors: copy and dirty errors are reported through the error value.
    ///
    /// Locking: caller holds `folio` locked and must not release it here.
    fn copy_and_dirty(
        &self,
        folio: &mut FolioGuard<'_>,
        page_off: usize,
        step: usize,
        iter: &mut IovIterSource<'_>,
    ) -> Result<usize, CopyFailure> {
        let copied = {
            let data = folio.data();
            match iter.copy_from_iter(&mut data[page_off..page_off + step]) {
                Ok(copied) => copied,
                Err(error) => return Err(CopyFailure { copied: 0, error }),
            }
        };
        if copied != 0
            && let Err(error) = self.dirty_folio(folio)
        {
            return Err(CopyFailure { copied, error });
        }
        Ok(copied)
    }

    /// Copies one page step and completes the backend hook for a single folio.
    ///
    /// - `folio`: caller-locked write target covering `range`.
    /// - `page_off`: byte offset of the step inside the page.
    /// - `range`: matching write-begin range, never past the page end.
    /// - `reservation`: admission token from the matching `write_begin`.
    /// - `iter`: source; advanced by exactly the copied prefix.
    ///
    /// Returns: the copied prefix paired with the backend `write_end` outcome,
    /// or [`FolioWriteResult::Failed`] when copying or dirtying failed first.
    ///
    /// Errors: copy and dirty errors are carried by `Failed`; the `write_end`
    /// error stays in `completion` so the caller can roll back.
    ///
    /// Locking: caller holds `folio` locked and must not release it here.
    fn copy_and_complete_folio(
        &self,
        folio: &mut FolioGuard<'_>,
        page_off: usize,
        range: WriteBeginRequest,
        reservation: &WriteReservation,
        iter: &mut IovIterSource<'_>,
    ) -> FolioWriteResult {
        match self.copy_and_dirty(folio, page_off, range.len(), iter) {
            Ok(copied) => FolioWriteResult::Copied {
                copied,
                completion: self.ops.write_end(
                    self,
                    WriteEndRequest::new(range, copied),
                    reservation,
                ),
            },
            Err(failure) => FolioWriteResult::Failed(failure),
        }
    }

    /// Rolls back source and cache state beyond `accepted`.
    ///
    /// - `pos`: file byte offset of the step.
    /// - `iter`: source iterator; rewound by the unaccepted tail.
    /// - `copied`: bytes copied into the folio, bounded by the validated step.
    /// - `accepted`: bytes the backend accepted; must not exceed `copied`.
    /// - `grew_cache`: whether the step extended the visible size.
    /// - `prepared_end`: EOF prepared by this step.
    ///
    /// Returns: the file byte offset accepted by the backend.
    ///
    /// Errors: `InvalidInput` when `accepted > copied`, or an iterator/cache
    /// rollback error.
    ///
    /// Locking: caller holds the inode data lock and no folio lock.
    fn rollback_unaccepted(
        &self,
        pos: u64,
        iter: &mut IovIterSource<'_>,
        copied: usize,
        accepted: usize,
        grew_cache: bool,
        prepared_end: u64,
    ) -> VfsResult<u64> {
        if accepted > copied {
            return Err(VfsError::InvalidInput);
        }
        if accepted < copied {
            iter.revert(copied - accepted)?;
        }
        let accepted_end = pos + accepted as u64;
        if grew_cache && accepted_end < prepared_end {
            self.i_pages
                .resize_cached_folios(prepared_end, accepted_end)?;
        }
        Ok(accepted_end)
    }

    /// Writes one contiguous run of already-resident whole pages under one
    /// batched admission and tail-release pair.
    ///
    /// Each slot copies and dirties under its own folio lock through the same
    /// copy boundary as the single-page path. `release_write_run_tail` then
    /// releases only the run's unaccepted tail; the published prefix is the
    /// copied bytes and the hook cannot renegotiate it. The run never extends
    /// the visible size.
    ///
    /// - `pos`: byte offset of the run; page aligned.
    /// - `remaining`: bytes left in this buffered write.
    /// - `visible_len`: current EOF; the run never grows past it.
    /// - `iter`: source copied per slot; reverted for bytes not committed.
    ///
    /// Returns: [`BatchWriteResult::NoRun`] when the backend does not support
    /// batched writes, no run can start, or nothing was published;
    /// [`BatchWriteResult::Committed`] with the published bytes, or
    /// [`BatchWriteResult::Failed`] with the first error.
    ///
    /// Errors: admission, dirty, tail-release and copy failures propagate as
    /// `Failed`.
    ///
    /// Locking: caller holds the inode data lock and no folio lock. Each slot
    /// takes and releases its own folio lock; may sleep while locking.
    fn write_batch_run(
        &self,
        pos: u64,
        remaining: usize,
        visible_len: u64,
        iter: &mut IovIterSource<'_>,
    ) -> BatchWriteResult {
        let Some(batched) = self.batched else {
            return BatchWriteResult::NoRun;
        };
        let page = PAGE_SIZE_4K as u64;
        let want = (remaining / PAGE_SIZE_4K).min(pagecache::FolioBatch::CAPACITY);
        if want < 2 {
            return BatchWriteResult::NoRun;
        }
        let Some(fit_pages) = visible_len
            .checked_sub(pos)
            .map(|inside| (inside / page) as usize)
        else {
            return BatchWriteResult::NoRun;
        };
        let fit = want.min(fit_pages);
        if fit < 2 {
            return BatchWriteResult::NoRun;
        }
        let start = pos / page;
        let (batch, run_len) = self.i_pages.get_folios_contig_with_run_len(start, fit);
        let run_len = run_len.min(batch.len());
        if run_len < 2 {
            return BatchWriteResult::NoRun;
        }
        let run_bytes = run_len * PAGE_SIZE_4K;
        let Ok(reservation) =
            batched.batch_write_begin(self, WriteBeginRequest::new(pos, run_bytes))
        else {
            // Fall back to the single-folio path so a smaller reservation can
            // retry, matching Linux's per-folio write_begin granularity. A real
            // failure is reported again by the single-folio write_begin.
            return BatchWriteResult::NoRun;
        };
        let mut committed = 0usize;
        for slot in 0..run_len {
            let folio_copy = self.i_pages.with_write_folio(&batch, slot, |folio| {
                Ok(self.copy_and_dirty(folio, 0, PAGE_SIZE_4K, iter))
            });
            let copied = match folio_copy {
                Ok(Some(Ok(copied))) => copied,
                Ok(Some(Err(failure))) => {
                    let revert_error = iter.revert(failure.copied).err();
                    // The release hook logs its own bookkeeping error; the
                    // copy/revert failure is the primary one to report.
                    let _ = self.release_run_tail(batched, pos, run_bytes, committed, &reservation);
                    let error = revert_error.unwrap_or(failure.error);
                    return BatchWriteResult::Failed { committed, error };
                }
                Ok(None) => break,
                Err(error) => {
                    // Reaching this arm means folio locking failed before the
                    // closure ran: copy and dirty errors arrive as `Ok(Err(_))`,
                    // so the iterator has not advanced and needs no rewind.
                    let _ = self.release_run_tail(batched, pos, run_bytes, committed, &reservation);
                    return BatchWriteResult::Failed { committed, error };
                }
            };
            committed += copied;
            if copied < PAGE_SIZE_4K {
                break;
            }
        }
        match (
            committed,
            self.release_run_tail(batched, pos, run_bytes, committed, &reservation),
        ) {
            (0, Some(error)) => BatchWriteResult::Failed {
                committed: 0,
                error,
            },
            (0, None) => BatchWriteResult::NoRun,
            // Published bytes are final: a tail-release bookkeeping failure
            // keeps the reported prefix, and `perform_write` returns it as
            // progress.
            (committed, Some(error)) => BatchWriteResult::Failed { committed, error },
            (committed, None) => BatchWriteResult::Committed(committed),
        }
    }

    /// Releases the unaccepted tail of one batched run.
    ///
    /// - `batched`: the backend's batched-write vtable.
    /// - `pos`: run start offset.
    /// - `run_bytes`: reserved run length.
    /// - `published`: bytes copied, dirtied and accepted by the run; final.
    /// - `reservation`: ranges this run's `batch_write_begin` newly reserved; the
    ///   hook releases the tail beyond `published` strictly inside it.
    ///
    /// Returns: the release hook's bookkeeping error, if any. Published bytes
    /// are final, so the caller keeps reporting its own accepted prefix.
    ///
    /// Errors: the hook's bookkeeping error is logged and returned; the iterator
    /// and published count are untouched.
    ///
    /// Locking: caller holds the inode data lock and no folio lock.
    fn release_run_tail(
        &self,
        batched: &dyn BatchedWriteOperations,
        pos: u64,
        run_bytes: usize,
        published: usize,
        reservation: &WriteReservation,
    ) -> Option<VfsError> {
        match batched.release_write_run_tail(
            self,
            BatchWriteRunTail::new(pos, run_bytes, published),
            reservation,
        ) {
            Ok(()) => None,
            Err(error) => {
                log::warn!("failed to release buffered write tail at {pos}: {error:?}");
                Some(error)
            }
        }
    }

    /// Commits one page's copied bytes from the backend `write_end` result and
    /// rolls back any unaccepted tail (iterator bytes and a cache growth window).
    ///
    /// - `page`: this step's offset, copied count and growth plan.
    /// - `write_end_result`: the backend `write_end` outcome, produced under the
    ///   copied folio's lock by the caller.
    /// - `iter`: source iterator, reverted for unaccepted bytes.
    ///
    /// Returns: the accepted length paired with the EOF published by this step.
    ///
    /// Errors: the backend `write_end` error after iterator and cache rollback,
    /// `InvalidInput` if a backend accepts more than was copied. The caller
    /// reports any already-committed prefix.
    ///
    /// Locking: caller holds the inode data lock and no folio lock.
    fn finish_page_write(
        &self,
        page: PageWriteState,
        write_end_result: VfsResult<usize>,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<(usize, u64)> {
        let PageWriteState {
            pos,
            copied,
            grew_cache,
            prepared_end,
            visible_len,
        } = page;
        let accepted = match write_end_result {
            Ok(accepted) => accepted,
            Err(error) => {
                iter.revert(copied)?;
                if grew_cache {
                    self.i_pages
                        .resize_cached_folios(prepared_end, visible_len)?;
                }
                return Err(error);
            }
        };
        let accepted_end =
            self.rollback_unaccepted(pos, iter, copied, accepted, grew_cache, prepared_end)?;
        Ok((accepted, accepted_end))
    }

    /// Writes, releases the unaccepted tail and rolls back one buffered-write
    /// page.
    ///
    /// - `index`: page index holding the target folio.
    /// - `page_off`: byte offset of the step inside that page.
    /// - `pos`: file byte offset of the step.
    /// - `step`: bytes requested for this step, never past the page end.
    /// - `visible_len`: current EOF, used to plan cache growth.
    /// - `iter`: source consumed by the copy and rewound for unaccepted bytes.
    ///
    /// Returns: the bytes the backend accepted paired with the EOF it published.
    /// A zero accepted count means the backend took no bytes at this position.
    ///
    /// Errors: the setup, copy, completion or cache-rollback error to report for
    /// this step; a failed copy first rewinds the iterator, cancels the
    /// reservation this step created and undoes an unfinished growth window, so
    /// the caller can still report an already written prefix.
    ///
    /// Locking: caller holds the inode data lock and no folio lock. The folio
    /// lock spans instantiation, copying, dirty publication and the backend
    /// `write_end`; cache discard takes the same folio lock before detaching the
    /// identity, matching Linux `generic_perform_write`, which holds no
    /// invalidate lock here. Rollback may wait for concurrent writeback.
    fn write_single_page(
        &self,
        index: PageIndex,
        page_off: usize,
        pos: u64,
        step: usize,
        visible_len: u64,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<(usize, u64)> {
        let prepared_end = pos.checked_add(step as u64).ok_or(VfsError::InvalidInput)?;
        let range = WriteBeginRequest::new(pos, step);
        let reservation = self.ops.write_begin(self, range)?;
        let grew_cache = prepared_end > visible_len;
        if grew_cache {
            self.i_pages
                .resize_cached_folios(visible_len, prepared_end)?;
        }
        let folio_step = self.with_folio_or_create(index, |folio| {
            Ok(self.copy_and_complete_folio(folio, page_off, range, &reservation, iter))
        });
        match folio_step {
            Ok(FolioWriteResult::Copied { copied, completion }) => self.finish_page_write(
                PageWriteState {
                    pos,
                    copied,
                    grew_cache,
                    prepared_end,
                    visible_len,
                },
                completion,
                iter,
            ),
            Ok(FolioWriteResult::Failed(failure)) => self.cancel_page_write(
                range,
                failure.copied,
                failure.error,
                visible_len,
                &reservation,
                iter,
            ),
            Err(error) => self.cancel_page_write(range, 0, error, visible_len, &reservation, iter),
        }
    }

    /// Reverts a page step whose copy or dirty publication failed.
    ///
    /// - `range`: matching write-begin range to cancel.
    /// - `copied`: bytes consumed from the source before the failure.
    /// - `error`: failure to report once rollback completes.
    /// - `visible_len`: EOF before this step, restored after rollback.
    /// - `reservation`: admission token of this step; cancels its tail.
    /// - `iter`: source iterator; rewound by `copied`.
    ///
    /// Returns: the failure, after the iterator, reservation and cache window
    /// are rolled back.
    ///
    /// Errors: the original `error`; a rewind failure replaces it first, then
    /// a cache-shrink failure.
    ///
    /// Locking: caller holds the inode data lock and no folio lock.
    fn cancel_page_write(
        &self,
        range: WriteBeginRequest,
        copied: usize,
        error: VfsError,
        visible_len: u64,
        reservation: &WriteReservation,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<(usize, u64)> {
        // Every rollback obligation runs even when an earlier step fails: the
        // reservation cancel must not depend on the iterator rewind.
        let revert_error = iter.revert(copied).err();
        if let Err(cancel_error) =
            self.ops
                .write_end(self, WriteEndRequest::new(range, 0), reservation)
        {
            log::warn!(
                "failed to cancel buffered write at {} (len {}): {cancel_error:?}",
                range.pos(),
                range.len()
            );
        }
        let prepared_end = range.pos() + range.len() as u64;
        let resize_error = if prepared_end > visible_len {
            self.i_pages
                .resize_cached_folios(prepared_end, visible_len)
                .err()
        } else {
            None
        };
        Err(revert_error.or(resize_error).unwrap_or(error))
    }

    // The caller holds the inode data lock through rollback to keep size and
    // competing writes stable. Each page step serializes its own copy, dirty
    // publication and acceptance under that page's folio lock; a batched run
    // releases its unaccepted tail once after its per-folio locks are released.
    // Rollback may wait for concurrent writeback; filesystem callbacks have
    // returned before cache shrink, so their locks are released. Writeback
    // completion must not depend on the data lock.
    pub(crate) fn perform_write(
        &self,
        offset: u64,
        iter: &mut IovIterSource<'_>,
        len: usize,
    ) -> VfsResult<usize> {
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        let mut written = 0usize;
        let mut visible_len = self.visible_len()?;
        while written < len {
            let pos = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            let index = pos / memaddr::PAGE_SIZE_4K as u64;
            let page_off = (pos % memaddr::PAGE_SIZE_4K as u64) as usize;
            let step = (len - written).min(memaddr::PAGE_SIZE_4K - page_off);
            if page_off == 0 && len - written >= 2 * PAGE_SIZE_4K {
                match self.write_batch_run(pos, len - written, visible_len, iter) {
                    BatchWriteResult::Committed(bytes) => {
                        written += bytes;
                        let end = pos
                            .checked_add(bytes as u64)
                            .ok_or(VfsError::InvalidInput)?;
                        visible_len = visible_len.max(end);
                        continue;
                    }
                    BatchWriteResult::Failed { committed, error } => {
                        return write_prefix_or_error(written + committed, error);
                    }
                    BatchWriteResult::NoRun => {}
                }
            }
            let (accepted, accepted_end) =
                match self.write_single_page(index, page_off, pos, step, visible_len, iter) {
                    Ok(progress) => progress,
                    Err(error) => return write_prefix_or_error(written, error),
                };
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
