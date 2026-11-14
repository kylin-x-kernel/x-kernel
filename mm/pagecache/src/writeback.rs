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
    Folio, FolioBatch, FolioGuard, MappingPages, PageIndex, WritebackControl, WritebackSyncMode,
    mapping_pages::MappingPagesState,
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
        control: &mut WritebackControl,
        submit: &mut impl FnMut(&mut WritebackControl, WritebackBatch<'a>) -> KResult<()>,
    ) -> KResult<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let pages = self.page_count();
        let skipped = control.pages_skipped();
        control.charge(pages);
        let empty = Self::new(self.completion.state);
        let result = submit(control, core::mem::replace(self, empty));
        let max_skipped = skipped.saturating_add(pages);
        if control.pages_skipped() < skipped || control.pages_skipped() > max_skipped {
            return Err(KError::InvalidInput);
        }
        if result == Err(KError::WouldBlock)
            && (control.sync_mode() != WritebackSyncMode::None
                || control.pages_skipped() != max_skipped)
        {
            return Err(KError::InvalidInput);
        }
        result
    }

    /// Returns the number of prepared folios in this batch.
    pub fn page_count(&self) -> usize {
        self.completion.folios.len()
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
    /// Retains the complete mapping owner for completion after submission returns.
    /// `pages` must always project the same embedded page index from that owner.
    ///
    /// # Errors
    /// Returns `InvalidInput` if the owner does not contain this batch's index.
    pub fn into_owned<O>(
        mut self,
        owner: Arc<O>,
        pages: fn(&O) -> &MappingPages,
    ) -> KResult<OwnedWritebackCompletion<O>> {
        if !core::ptr::eq(self.state, &pages(&owner).state) {
            return Err(KError::InvalidInput);
        }
        Ok(OwnedWritebackCompletion {
            owner,
            pages,
            folios: core::mem::replace(&mut self.folios, FolioBatch::new()),
            bytes: self.bytes,
        })
    }

    /// Ends writeback and returns the number of fully completed folios.
    ///
    /// # Errors
    /// An oversized prefix redirties the entire batch and returns InvalidInput.
    pub fn complete(mut self, completed_bytes: usize) -> KResult<usize> {
        complete_folios(self.state, &mut self.folios, self.bytes, completed_bytes)
    }
}

impl Drop for WritebackCompletion<'_> {
    fn drop(&mut self) {
        for folio in self.folios.iter() {
            finish_writeback(self.state, folio, false);
        }
    }
}

/// An asynchronous completion retaining the address-space owner, not a separately
/// owned page index. Complete or drop only in task context after data I/O ends.
pub struct OwnedWritebackCompletion<O> {
    owner: Arc<O>,
    pages: fn(&O) -> &MappingPages,
    folios: FolioBatch,
    bytes: usize,
}

impl<O> OwnedWritebackCompletion<O> {
    /// Ends writeback with the same prefix and redirty rules as borrowed completion.
    ///
    /// # Errors
    /// An oversized prefix redirties the entire batch and returns `InvalidInput`.
    pub fn complete(mut self, completed_bytes: usize) -> KResult<usize> {
        complete_folios(
            &(self.pages)(&self.owner).state,
            &mut self.folios,
            self.bytes,
            completed_bytes,
        )
    }
}

impl<O> Drop for OwnedWritebackCompletion<O> {
    fn drop(&mut self) {
        for folio in self.folios.iter() {
            finish_writeback(&(self.pages)(&self.owner).state, folio, false);
        }
    }
}

fn complete_folios(
    state: &Mutex<MappingPagesState>,
    folios: &mut FolioBatch,
    bytes: usize,
    completed_bytes: usize,
) -> KResult<usize> {
    if completed_bytes > bytes {
        return Err(KError::InvalidInput);
    }
    let mut completed_pages = 0;
    for (position, folio) in folios.iter().enumerate() {
        let folio_end = ((position + 1) * PAGE_SIZE_4K).min(bytes);
        let is_complete = folio_end <= completed_bytes;
        finish_writeback(state, folio, is_complete);
        completed_pages += usize::from(is_complete);
    }
    folios.clear();
    Ok(completed_pages)
}

#[derive(Clone, Copy)]
enum FolioMark {
    ToWrite,
    Dirty,
    Writeback,
}

impl MappingPages {
    /// Captures the finite resident-page target within a half-open byte range.
    ///
    /// The existing tree includes clean folios with in-flight writeback. Only
    /// its highest intersecting index is sampled, under the index lock; no
    /// reference or lock survives this call. The bound limits later submission
    /// and waiting, not file size or the contents selected by either operation.
    /// Requires sleepable task context, without a current-process requirement.
    ///
    /// # Errors
    /// Returns `InvalidInput` for a backwards range or nonsleepable context.
    pub fn capture_writeback_range(&self, range: Range<u64>) -> KResult<Range<u64>> {
        if range.start > range.end {
            return Err(KError::InvalidInput);
        }
        if !ktask::is_sleepable_context() {
            return Err(KError::InvalidInput);
        }
        if range.is_empty() {
            return Ok(range);
        }
        let (start_index, end_index) = byte_range_to_page_range(range.clone());
        let state = self.state.lock();
        let Some((&last_index, _)) = state.entries.range(start_index..end_index).next_back() else {
            return Ok(range.start..range.start);
        };
        let end = last_index
            .saturating_add(1)
            .saturating_mul(PAGE_SIZE_4K as u64)
            .min(range.end);
        Ok(range.start..end)
    }

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
            FolioMark::ToWrite => &state.towrite_marks,
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

    // A pass selects a finite target before preparing data. TOWRITE belongs to
    // the mapping, not this invocation; never clear another pass's selection.
    fn select_writeback(&self, control: &WritebackControl) -> KResult<(PageIndex, PageIndex)> {
        let target = self.capture_writeback_range(control.range_start()..control.range_end())?;
        let (start, end) = byte_range_to_page_range(target);
        if control.sync_mode() == WritebackSyncMode::All {
            let mut cursor = start;
            while cursor < end {
                let mut indexes = [0; FolioBatch::CAPACITY];
                let mut count = 0;
                let mut state = self.state.lock();
                for index in state.dirty_marks.range(cursor..end).take(indexes.len()) {
                    indexes[count] = *index;
                    count += 1;
                }
                if count == 0 {
                    break;
                }
                for index in &indexes[..count] {
                    state.towrite_marks.insert(*index);
                }
                cursor = indexes[count - 1] + 1;
            }
        }
        Ok((start, end))
    }

    // The filesystem supplies live EOF; the cache never owns another size.
    // Flush unsent WRITEBACK owners before a lock or I/O wait can depend on them.
    fn prepare_writeback_candidate<'a>(
        &self,
        candidate: &'a Arc<Folio>,
        mode: WritebackSyncMode,
        live_eof: &mut impl FnMut() -> KResult<u64>,
        wait: &CompletionWait,
        mut flush_before_wait: impl FnMut() -> KResult<()>,
    ) -> KResult<Option<(FolioGuard<'a>, usize)>> {
        loop {
            let mut folio = match candidate.try_lock() {
                Some(folio) => folio,
                None => {
                    flush_before_wait()?;
                    candidate.lock_with(wait)
                }
            };
            let index = candidate.index();
            // A single-folio buffered write finished acceptance and size
            // publication under this same folio lock. A buffered batch run
            // never grows the file, so its data acceptance is the per-folio
            // copy and dirty step that already ran under this folio's lock, and
            // its shared settlement hook releases reservation tails only.
            // Sampling metadata here therefore sees every accepted byte either
            // way.
            let page_start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(KError::InvalidInput)?;
            let valid_len = live_eof()?
                .saturating_sub(page_start)
                .min(PAGE_SIZE_4K as u64) as usize;
            let mut state = self.state.lock();
            if !state.is_current(index, candidate) {
                return Ok(None);
            }
            if !folio.is_dirty() {
                state.dirty_marks.remove(&index);
                return Ok(None);
            }
            if folio.is_under_writeback() {
                drop(state);
                drop(folio);
                if mode == WritebackSyncMode::None {
                    return Ok(None);
                }
                flush_before_wait()?;
                candidate.wait_writeback(wait);
                continue;
            }
            if valid_len == 0 {
                return Ok(None);
            }
            folio.clear_dirty();
            folio.start_writeback();
            state.dirty_marks.remove(&index);
            state.towrite_marks.remove(&index);
            state.writeback_marks.insert(index);
            return Ok(Some((folio, valid_len)));
        }
    }

    /// Writes selected folios through a synchronous, folio-locked callback.
    ///
    /// The live EOF supplier reads owner metadata without acquiring a folio or
    /// index lock. All adapters charge attempted pages, including failures.
    /// NONE stops on budget exhaustion or error; ALL completes the finite
    /// selected set and returns its first error. Existing writeback is skipped
    /// by NONE and waited/revalidated by ALL. Requires sleepable task context.
    pub fn writeback_until(
        &self,
        control: &mut WritebackControl,
        live_eof: &mut impl FnMut() -> KResult<u64>,
        write_folio: &mut impl FnMut(PageIndex, &[u8], usize) -> KResult<()>,
    ) -> KResult<()> {
        if control.nr_to_write() == 0 {
            return Ok(());
        }
        let wait = CompletionWait::prepare()?;
        let (mut cursor, end) = self.select_writeback(control)?;
        let mode = control.sync_mode();
        let mark = if mode == WritebackSyncMode::All {
            FolioMark::ToWrite
        } else {
            FolioMark::Dirty
        };
        let mut candidates = FolioBatch::new();
        let mut first_error = None;
        'scan: while cursor < end {
            self.collect_marked_folios(mark, &mut cursor, end, &mut candidates);
            for candidate in candidates.iter() {
                if mode == WritebackSyncMode::None && control.nr_to_write() == 0 {
                    break 'scan;
                }
                let result =
                    self.prepare_writeback_candidate(candidate, mode, live_eof, &wait, || Ok(()));
                let (mut folio, valid_len) = match result {
                    Ok(Some(prepared)) => prepared,
                    Ok(None) => continue,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        if mode == WritebackSyncMode::None {
                            break 'scan;
                        }
                        continue;
                    }
                };
                control.charge(1);
                let result = write_folio(candidate.index(), &folio.data()[..valid_len], valid_len);
                drop(folio);
                finish_writeback(&self.state, candidate, result.is_ok());
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                    if mode == WritebackSyncMode::None {
                        break 'scan;
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Writes contiguous snapshots with synchronous prefix completion.
    ///
    /// Completion acknowledges data only; it never changes the invocation's
    /// attempted-work budget. The incomplete suffix is redirtied for retry.
    pub fn write_cache_pages(
        &self,
        control: &mut WritebackControl,
        live_eof: &mut impl FnMut() -> KResult<u64>,
        max_bytes: usize,
        write_range: &mut impl FnMut(u64, &[u8]) -> WritebackRangeResult,
    ) -> KResult<()> {
        self.submit_cache_pages(control, live_eof, max_bytes, &mut |_, batch| {
            let result = write_range(batch.offset(), batch.data());
            let (_, completion) = batch.into_parts();
            let (prefix, error) = match result {
                Ok(prefix) => (prefix, None),
                Err((prefix, error)) => (prefix, Some(error)),
            };
            let completed = completion.complete(prefix).map(|_| ());
            error.map_or(completed, Err)
        })
    }

    /// Hands snapshots and exactly-once completion to the filesystem.
    ///
    /// The callback borrows this invocation's control only until it returns.
    /// A whole unsent NONE decline accounts the batch as skipped, restores all
    /// its folios before returning, and returns WouldBlock. Other failures are
    /// errors. ALL must wait filesystem admission rather than decline.
    ///
    /// Existing asynchronous consumers serialize the entire selection through
    /// callback return and rollback with their inode submission mutex. Workers
    /// must not acquire it. The owner retains mappings through deferred I/O,
    /// drains before freeing blocks, and publishes deferred errors separately.
    pub fn submit_cache_pages<'a>(
        &'a self,
        control: &mut WritebackControl,
        live_eof: &mut impl FnMut() -> KResult<u64>,
        max_bytes: usize,
        submit: &mut impl FnMut(&mut WritebackControl, WritebackBatch<'a>) -> KResult<()>,
    ) -> KResult<()> {
        if control.nr_to_write() == 0 {
            return Ok(());
        }
        let wait = CompletionWait::prepare()?;
        let (mut cursor, end) = self.select_writeback(control)?;
        let mode = control.sync_mode();
        let mark = if mode == WritebackSyncMode::All {
            FolioMark::ToWrite
        } else {
            FolioMark::Dirty
        };
        let mut candidates = FolioBatch::new();
        let mut batch = WritebackBatch::new(&self.state);
        let mut next_index = None;
        let mut first_error = None;
        let max_bytes = max_bytes.max(PAGE_SIZE_4K);
        'scan: while cursor < end {
            self.collect_marked_folios(mark, &mut cursor, end, &mut candidates);
            for (slot, candidate) in candidates.iter().enumerate() {
                let index = candidate.index();
                let is_full = batch.completion.folios.is_full()
                    || batch.data.len().saturating_add(PAGE_SIZE_4K) > max_bytes
                    || (mode == WritebackSyncMode::None
                        && batch.page_count() >= control.nr_to_write());
                if !batch.data.is_empty()
                    && (next_index != Some(index) || is_full)
                    && !can_continue_writeback(
                        mode,
                        batch.submit(control, submit),
                        &mut first_error,
                    )
                {
                    break 'scan;
                }
                if mode == WritebackSyncMode::None && control.nr_to_write() == 0 {
                    break 'scan;
                }
                let prepared =
                    self.prepare_writeback_candidate(candidate, mode, live_eof, &wait, || {
                        if !can_continue_writeback(
                            mode,
                            batch.submit(control, submit),
                            &mut first_error,
                        ) || (mode == WritebackSyncMode::None && control.nr_to_write() == 0)
                        {
                            return Err(KError::WouldBlock);
                        }
                        Ok(())
                    });
                let (mut folio, valid_len) = match prepared {
                    Ok(Some(prepared)) => prepared,
                    Ok(None) => continue,
                    Err(error) => {
                        if mode == WritebackSyncMode::None && error == KError::WouldBlock {
                            break 'scan;
                        }
                        first_error.get_or_insert(error);
                        if mode == WritebackSyncMode::None {
                            break 'scan;
                        }
                        continue;
                    }
                };
                if batch.data.is_empty() {
                    batch.offset = index * PAGE_SIZE_4K as u64;
                    let mut max_pages = max_bytes / PAGE_SIZE_4K;
                    if mode == WritebackSyncMode::None {
                        max_pages = max_pages.min(control.nr_to_write());
                    }
                    let pages = candidates.contiguous_pages_from(slot, max_pages);
                    // Size from the already collected run. Geometric rounding
                    // preserves the old Vec capacity bound across scan windows,
                    // without repeatedly reallocating/copying contiguous pages.
                    let capacity = if valid_len < PAGE_SIZE_4K {
                        valid_len
                    } else {
                        (pages * PAGE_SIZE_4K).next_power_of_two()
                    };
                    batch.data.reserve(capacity);
                }
                batch.data.extend_from_slice(&folio.data()[..valid_len]);
                batch.completion.bytes = batch.data.len();
                drop(folio);
                batch.completion.folios.push(candidate.clone());
                next_index = Some(index + 1);
                // A later growing EOF must start its own payload at its real
                // page offset, never immediately after this partial page.
                if valid_len < PAGE_SIZE_4K
                    && !can_continue_writeback(
                        mode,
                        batch.submit(control, submit),
                        &mut first_error,
                    )
                {
                    break 'scan;
                }
            }
        }
        can_continue_writeback(mode, batch.submit(control, submit), &mut first_error);
        first_error.map_or(Ok(()), Err)
    }
}

// Like writeback_iter's saved error: integrity passes keep preparing selected
// work after an error; opportunistic passes stop, with capacity decline benign.
fn can_continue_writeback(
    mode: WritebackSyncMode,
    result: KResult<()>,
    first_error: &mut Option<KError>,
) -> bool {
    let Err(error) = result else {
        return true;
    };
    if !(mode == WritebackSyncMode::None && error == KError::WouldBlock) {
        first_error.get_or_insert(error);
    }
    mode == WritebackSyncMode::All
}
