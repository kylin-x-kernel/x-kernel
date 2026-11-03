// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dirty-tag enumeration, writeback completion, and waiting.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

use kerrno::{KError, KResult};
use ksync::Mutex;
use ktask::completion::CompletionWait;
use memaddr::PAGE_SIZE_4K;

use crate::{
    Folio, FolioBatch, FolioGuard, MappingPages, PageIndex, mapping_pages::MappingPagesState,
};

/// Result of one contiguous filesystem writeback submission.
///
/// Success reports a completed byte prefix; failure reports a completed byte
/// prefix and its error. A short success leaves the suffix dirty for a later
/// pass. This protocol carries no pass budget or statistics.
pub type WritebackRangeResult = Result<usize, (usize, KError)>;

// Returns the covering half-open page range; empty byte ranges cover no pages.
fn byte_range_to_page_range(range: Range<u64>) -> (PageIndex, PageIndex) {
    if range.is_empty() {
        return (0, 0);
    }
    (
        range.start / PAGE_SIZE_4K as u64,
        (range.end - 1) / PAGE_SIZE_4K as u64 + 1,
    )
}

fn finish_writeback(state: &Mutex<MappingPagesState>, folio: &Arc<Folio>, is_complete: bool) {
    // Completion owns the writeback bit, not the content lock. A reader or
    // writer may retain that lock while I/O finishes on another task.
    let mut state = state.lock();
    let index = folio.index();
    let is_current = state.is_current(index, folio);
    if is_current && !is_complete {
        folio.set_dirty();
    }
    if is_current {
        state.writeback_marks.remove(&index);
        if folio.is_dirty() {
            state.dirty_marks.insert(index);
        } else {
            state.dirty_marks.remove(&index);
        }
    }
    // A tag scan must not observe the mark gone while the folio still carries
    // the completed writeback bit. Publish both before releasing the index.
    let has_waiters = folio.end_writeback();
    drop(state);
    if has_waiters {
        folio.wake_writeback();
    }
}

/// An immutable data snapshot and the obligation to finish its folios.
///
/// A filesystem may retain this batch after submission returns. Split it to
/// transfer the data to BIO while keeping completion in task context. Dropping
/// an unsubmitted batch redirties its folios. The batch borrows its owning
/// page index. Never drop or finish it in IRQ/BH
/// context, or while holding locks needed by PageCache/truncate.
pub struct WritebackBatch<'a> {
    offset: u64,
    data: Vec<u8>,
    completion: WritebackCompletion<'a>,
}

impl<'a> WritebackBatch<'a> {
    fn new(state: &'a Mutex<MappingPagesState>) -> Self {
        Self {
            offset: 0,
            data: Vec::new(),
            completion: WritebackCompletion {
                state,
                folios: FolioBatch::new(),
                bytes: 0,
            },
        }
    }

    fn submit(
        &mut self,
        submit: &mut impl FnMut(WritebackBatch<'a>, &mut usize) -> KResult<()>,
        budget: &mut usize,
    ) -> KResult<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let empty = Self::new(self.completion.state);
        submit(core::mem::replace(self, empty), budget)
    }

    /// Returns the first byte offset in the file.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the snapshot taken when writeback started.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Transfers snapshot storage and the exactly-once completion obligation.
    /// The caller must retain completion until every submitted I/O has ended.
    pub fn into_parts(self) -> (Vec<u8>, WritebackCompletion<'a>) {
        (self.data, self.completion)
    }
}

/// Owns folio writeback until data and required filesystem completion finish.
///
/// Completion is not a durability barrier. The filesystem reports only a
/// completed contiguous prefix, after any required metadata work. Uncompleted
/// folios are redirtied, while concurrent dirtying is preserved. Dropping this
/// object performs zero-prefix completion, so callers must drain submitted I/O
/// before dropping it, including on errors. All operations require task context.
/// Completion updates atomic folio flags and index marks without acquiring the
/// content lock. It may run on another task and carries no task-bound wait handle;
/// the index mutex still requires sleepable context.
pub struct WritebackCompletion<'a> {
    state: &'a Mutex<MappingPagesState>,
    folios: FolioBatch,
    bytes: usize,
}

impl WritebackCompletion<'_> {
    /// Ends writeback and returns the number of fully completed folios.
    ///
    /// # Errors
    /// An oversized prefix redirties the entire batch and returns InvalidInput.
    pub fn complete(mut self, completed_bytes: usize) -> KResult<usize> {
        if completed_bytes > self.bytes {
            return Err(KError::InvalidInput);
        }
        let mut completed_pages = 0;
        for (position, folio) in self.folios.iter().enumerate() {
            let folio_end = ((position + 1) * PAGE_SIZE_4K).min(self.bytes);
            let is_complete = folio_end <= completed_bytes;
            finish_writeback(self.state, folio, is_complete);
            completed_pages += usize::from(is_complete);
        }
        self.folios.clear();
        Ok(completed_pages)
    }
}

impl Drop for WritebackCompletion<'_> {
    fn drop(&mut self) {
        for folio in self.folios.iter() {
            finish_writeback(self.state, folio, false);
        }
    }
}

enum FolioMark {
    Dirty,
    Writeback,
}

impl MappingPages {
    /// Waits for tagged resident writeback discovered while scanning a byte range.
    ///
    /// Scans the half-open range once in ascending index order. Writeback started
    /// behind the cursor, including on replacement folios, may be missed. This
    /// is not a snapshot of all writeback at entry and does not guarantee that
    /// the range is free of writeback on return. It neither submits dirty folios
    /// nor reports I/O errors. The owner supplies submission, error handling and
    /// any serialization required by its operation; this is not a standalone
    /// fsync or truncate protocol.
    ///
    /// Holds bounded folio references but no mapping or folio lock while
    /// sleeping. Callers must recheck residency after a wake because a folio
    /// can be detached while writeback runs. Call only from sleepable task context.
    ///
    /// # Errors
    /// Returns [`KError::InvalidInput`] outside sleepable task context.
    pub fn wait_on_writeback_range(&self, range: Range<u64>) -> KResult<()> {
        let wait = CompletionWait::prepare()?;
        if range.is_empty() {
            return Ok(());
        }
        let (mut scan_index, end_index) = byte_range_to_page_range(range);
        let mut batch = FolioBatch::new();

        while scan_index < end_index {
            self.collect_marked_folios(
                FolioMark::Writeback,
                &mut scan_index,
                end_index,
                &mut batch,
            );
            for folio in batch.iter() {
                folio.wait_writeback(&wait);
            }
        }
        Ok(())
    }

    // Like filemap_get_folios_tag, collect one bounded window of resident refs.
    // Marks only select candidates; the mapping lock is released on return.
    fn collect_marked_folios(
        &self,
        mark: FolioMark,
        next_index: &mut PageIndex,
        end_index: PageIndex,
        batch: &mut FolioBatch,
    ) {
        batch.clear();
        let state = self.state.lock();
        let marks = match mark {
            FolioMark::Dirty => &state.dirty_marks,
            FolioMark::Writeback => &state.writeback_marks,
        };
        for index in marks.range(*next_index..end_index) {
            // end_index comes from an exclusive byte endpoint, so index + 1
            // is representable even for a range ending at u64::MAX.
            *next_index = *index + 1;
            if let Some(folio) = state.entries.get(index) {
                batch.push(folio.clone());
                if batch.is_full() {
                    return;
                }
            }
        }
        *next_index = end_index;
    }

    // A dirty tag only selects a candidate. Mapping identity and folio state
    // must be checked in one folio -> state critical section before I/O owns
    // the folio, otherwise truncate or replacement can detach a collected Arc.
    fn start_writeback_for_candidate<'a>(
        &self,
        index: PageIndex,
        candidate: &'a Arc<Folio>,
        pages_skipped: &mut usize,
        wait: &CompletionWait,
    ) -> Option<FolioGuard<'a>> {
        let mut folio = candidate.lock_with(wait);
        let mut state = self.state.lock();
        if !state.is_current(index, candidate) {
            // Replacement and detach update their own marks under the index
            // lock. This stale candidate has no authority over those marks.
            return None;
        }
        if !folio.is_dirty() {
            state.dirty_marks.remove(&index);
            return None;
        }
        if folio.is_under_writeback() {
            *pages_skipped = pages_skipped.saturating_add(1);
            return None;
        }

        folio.clear_dirty();
        folio.start_writeback();
        state.dirty_marks.remove(&index);
        state.writeback_marks.insert(index);
        drop(state);
        Some(folio)
    }

    /// Writes back dirty cached folios intersecting the half-open `range`.
    ///
    /// `nr_to_write` and `pages_skipped` exclusively borrow the owner's control
    /// fields. Completed pages consume budget immediately, and busy dirty pages
    /// increment skips, including when a later callback fails. There is no
    /// separate pass state to merge after return.
    ///
    /// Candidates are scanned in bounded ascending windows. Concurrent dirtying
    /// behind the cursor is left for a later pass; dirtying ahead may be observed.
    /// The callback runs with the candidate folio locked and must not re-enter
    /// this [`MappingPages`].
    pub fn writeback_until(
        &self,
        visible_len: u64,
        range: Range<u64>,
        nr_to_write: &mut usize,
        pages_skipped: &mut usize,
        write_folio_fn: &mut impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<()> {
        let wait = CompletionWait::prepare()?;
        if *nr_to_write == 0 || range.is_empty() {
            return Ok(());
        }
        let (mut scan_index, end_index) = byte_range_to_page_range(range);
        let mut candidates = FolioBatch::new();

        'scan: while scan_index < end_index {
            self.collect_marked_folios(
                FolioMark::Dirty,
                &mut scan_index,
                end_index,
                &mut candidates,
            );
            for folio in candidates.iter() {
                if *nr_to_write == 0 {
                    break 'scan;
                }
                let index = folio.index();
                let page_start = index
                    .checked_mul(PAGE_SIZE_4K as u64)
                    .ok_or(KError::InvalidInput)?;
                let valid_len = visible_len
                    .saturating_sub(page_start)
                    .min(PAGE_SIZE_4K as u64) as usize;
                if valid_len == 0 {
                    continue;
                }
                let Some(mut locked) =
                    self.start_writeback_for_candidate(index, folio, pages_skipped, &wait)
                else {
                    continue;
                };
                let result = write_folio_fn(index, &locked.data()[..valid_len], valid_len);
                drop(locked);
                if let Err(error) = result {
                    finish_writeback(&self.state, folio, false);
                    return Err(error);
                }
                finish_writeback(&self.state, folio, true);
                *nr_to_write -= 1;
            }
        }
        Ok(())
    }

    /// Writes back dirty cached folios in contiguous batches of at most 31 folios.
    ///
    /// The callback returns a completed byte prefix on success, or
    /// `(completed_prefix_bytes, error)` on failure. Only fully completed folios
    /// consume the borrowed budget. A short success leaves the suffix dirty;
    /// an oversized prefix returns `InvalidInput` and credits no pages. All started folios finish before an
    /// error is returned, and progress remains in the borrowed control fields.
    ///
    /// The byte and page limits may further shorten each batch. Enumeration is
    /// incremental as in [`Self::writeback_until`]. The callback runs without
    /// mapping or folio locks; completion revalidates each resident identity.
    /// The callback must not wait for or truncate folios in its submitted batch:
    /// their writeback completes only after the callback returns.
    pub fn write_cache_pages(
        &self,
        visible_len: u64,
        range: Range<u64>,
        nr_to_write: &mut usize,
        pages_skipped: &mut usize,
        max_bytes: usize,
        write_range_fn: &mut impl FnMut(u64, &[u8]) -> WritebackRangeResult,
    ) -> KResult<()> {
        self.scan_writeback(
            visible_len,
            range,
            nr_to_write,
            pages_skipped,
            max_bytes,
            &mut |batch, budget| {
                let result = write_range_fn(batch.offset(), batch.data());
                let (_, completion) = batch.into_parts();
                let (prefix, error) = match result {
                    Ok(prefix) => (prefix, None),
                    Err((prefix, error)) => (prefix, Some(error)),
                };
                *budget -= completion.complete(prefix)?;
                error.map_or(Ok(()), Err)
            },
        )
    }

    /// Transfers bounded dirty batches to the filesystem's submission owner.
    ///
    /// The budget counts folios handed to `submit`, including rejected batches;
    /// completion may occur after this scan returns. Unlike write_cache_pages,
    /// return does not establish I/O completion or report deferred errors. The
    /// owner bounds outstanding requests, drains on every exit, and provides
    /// error reporting and serialization against truncate. No cache locks are
    /// held during submission. Task context only.
    ///
    /// # Errors
    /// Returns the first submission/scan error; already transferred batches
    /// remain the caller's responsibility, and rejected batches must be dropped.
    pub fn submit_cache_pages<'a>(
        &'a self,
        visible_len: u64,
        range: Range<u64>,
        nr_to_write: &mut usize,
        pages_skipped: &mut usize,
        max_bytes: usize,
        submit: &mut impl FnMut(WritebackBatch<'a>) -> KResult<()>,
    ) -> KResult<()> {
        self.scan_writeback(
            visible_len,
            range,
            nr_to_write,
            pages_skipped,
            max_bytes,
            &mut |batch, budget| {
                *budget -= batch.completion.folios.len();
                submit(batch)
            },
        )
    }

    fn scan_writeback<'a>(
        &'a self,
        visible_len: u64,
        range: Range<u64>,
        nr_to_write: &mut usize,
        pages_skipped: &mut usize,
        max_bytes: usize,
        submit: &mut impl FnMut(WritebackBatch<'a>, &mut usize) -> KResult<()>,
    ) -> KResult<()> {
        let wait = CompletionWait::prepare()?;
        if *nr_to_write == 0 || range.is_empty() {
            return Ok(());
        }
        let (mut scan_index, end_index) = byte_range_to_page_range(range);
        let mut candidates = FolioBatch::new();

        let max_bytes = max_bytes.max(PAGE_SIZE_4K);
        let mut next_index = None;
        let mut batch = WritebackBatch::new(&self.state);

        'scan: while scan_index < end_index {
            self.collect_marked_folios(
                FolioMark::Dirty,
                &mut scan_index,
                end_index,
                &mut candidates,
            );
            for folio in candidates.iter() {
                if *nr_to_write == 0 {
                    break 'scan;
                }
                let index = folio.index();
                let page_start = index
                    .checked_mul(PAGE_SIZE_4K as u64)
                    .ok_or(KError::InvalidInput)?;
                let valid_len = visible_len
                    .saturating_sub(page_start)
                    .min(PAGE_SIZE_4K as u64) as usize;
                if valid_len == 0 {
                    continue;
                }

                let contiguous = next_index == Some(index);
                let would_overflow = batch.data.len().saturating_add(valid_len) > max_bytes;
                let would_exceed_pages = batch.completion.folios.len() >= *nr_to_write;
                if !batch.data.is_empty()
                    && (!contiguous
                        || would_overflow
                        || would_exceed_pages
                        || batch.completion.folios.is_full())
                {
                    batch.submit(submit, nr_to_write)?;
                    if *nr_to_write == 0 {
                        break 'scan;
                    }
                }

                let Some(mut locked) =
                    self.start_writeback_for_candidate(index, folio, pages_skipped, &wait)
                else {
                    batch.submit(submit, nr_to_write)?;
                    if *nr_to_write == 0 {
                        break 'scan;
                    }
                    next_index = None;
                    continue;
                };

                if batch.data.is_empty() {
                    batch.offset = page_start;
                }
                batch.data.extend_from_slice(&locked.data()[..valid_len]);
                batch.completion.bytes = batch.data.len();
                drop(locked);
                batch.completion.folios.push(folio.clone());
                next_index = Some(index + 1);
            }
        }

        batch.submit(submit, nr_to_write)?;
        Ok(())
    }
}
