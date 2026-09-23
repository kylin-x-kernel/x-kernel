// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dirty-tag enumeration and synchronous writeback completion.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

use kerrno::{KError, KResult};
use ksync::{Mutex, MutexGuard};
use memaddr::PAGE_SIZE_4K;

use crate::{Folio, FolioBatch, MappingPages, PageIndex};

/// Result of one contiguous filesystem writeback submission.
///
/// Success reports a completed byte prefix; failure reports a completed byte
/// prefix and its error. A short success leaves the suffix dirty for a later
/// pass. This protocol carries no pass budget or statistics.
pub type WritebackRangeResult = Result<usize, (usize, KError)>;

impl MappingPages {
    fn finish_writeback(&self, index: PageIndex, folio: &Arc<Mutex<Folio>>, is_complete: bool) {
        let mut state = self.state.lock();
        let is_current = state
            .entries
            .get(&index)
            .is_some_and(|current| Arc::ptr_eq(current, folio));
        let mut locked = folio.lock();
        if is_current && !is_complete {
            locked.mark_dirty();
        }
        locked.end_writeback();
        if !is_current {
            return;
        }
        if locked.is_dirty() {
            state.dirty_marks.insert(index);
        } else {
            state.dirty_marks.remove(&index);
        }
    }

    // Like filemap_get_folios_tag: advance monotonically, holding references
    // only for one bounded window. Concurrent changes are revalidated at start.
    fn collect_dirty_folios(
        &self,
        next_index: &mut PageIndex,
        end_index: PageIndex,
        batch: &mut FolioBatch,
    ) {
        batch.clear();
        let state = self.state.lock();
        for index in state.dirty_marks.range(*next_index..end_index) {
            // end_index comes from an exclusive byte endpoint, so index + 1
            // is representable even for a range ending at u64::MAX.
            *next_index = *index + 1;
            if let Some(folio) = state.entries.get(index) {
                batch.push(*index, folio.clone());
                if batch.is_full() {
                    return;
                }
            }
        }
        *next_index = end_index;
    }

    // A dirty tag only selects a candidate. Mapping identity and folio state
    // must be checked in one state -> folio critical section before I/O owns
    // the folio, otherwise truncate or replacement can detach a collected Arc.
    fn start_writeback_for_candidate<'a>(
        &self,
        index: PageIndex,
        candidate: &'a Arc<Mutex<Folio>>,
        pages_skipped: &mut usize,
    ) -> Option<MutexGuard<'a, Folio>> {
        let mut state = self.state.lock();
        let Some(current) = state.entries.get(&index).cloned() else {
            state.dirty_marks.remove(&index);
            return None;
        };

        if !Arc::ptr_eq(&current, candidate) {
            let is_current_dirty = current.lock().is_dirty();
            if is_current_dirty {
                state.dirty_marks.insert(index);
            } else {
                state.dirty_marks.remove(&index);
            }
            return None;
        }

        let mut folio = candidate.lock();
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
        if *nr_to_write == 0 || range.is_empty() {
            return Ok(());
        }
        let mut scan_index = range.start / PAGE_SIZE_4K as u64;
        let end_index = (range.end - 1) / PAGE_SIZE_4K as u64 + 1;
        let mut candidates = FolioBatch::new();

        'scan: while scan_index < end_index {
            self.collect_dirty_folios(&mut scan_index, end_index, &mut candidates);
            for &(index, ref folio) in candidates.iter() {
                if *nr_to_write == 0 {
                    break 'scan;
                }
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
                    self.start_writeback_for_candidate(index, folio, pages_skipped)
                else {
                    continue;
                };
                let result = write_folio_fn(index, &locked.data()[..valid_len], valid_len);
                drop(locked);
                if let Err(error) = result {
                    self.finish_writeback(index, folio, false);
                    return Err(error);
                }
                self.finish_writeback(index, folio, true);
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
    pub fn write_cache_pages(
        &self,
        visible_len: u64,
        range: Range<u64>,
        nr_to_write: &mut usize,
        pages_skipped: &mut usize,
        max_bytes: usize,
        write_range_fn: &mut impl FnMut(u64, &[u8]) -> WritebackRangeResult,
    ) -> KResult<()> {
        if *nr_to_write == 0 || range.is_empty() {
            return Ok(());
        }
        let mut scan_index = range.start / PAGE_SIZE_4K as u64;
        let end_index = (range.end - 1) / PAGE_SIZE_4K as u64 + 1;
        let mut candidates = FolioBatch::new();

        let max_bytes = max_bytes.max(PAGE_SIZE_4K);
        let mut batch_start = 0u64;
        let mut next_index = None;
        let mut batch_data = Vec::new();
        let mut batch_folios = FolioBatch::new();

        let flush_batch = |batch_start: u64,
                           batch_data: &mut Vec<u8>,
                           batch_folios: &mut FolioBatch,
                           write_range_fn: &mut dyn FnMut(u64, &[u8]) -> WritebackRangeResult,
                           nr_to_write: &mut usize|
         -> KResult<()> {
            if batch_data.is_empty() {
                return Ok(());
            }
            let (completed_bytes, error) = match write_range_fn(batch_start, batch_data) {
                Ok(completed_bytes) => (completed_bytes, None),
                Err((completed_bytes, error)) => (completed_bytes, Some(error)),
            };
            let (completed_bytes, error) = if completed_bytes > batch_data.len() {
                (0, Some(KError::InvalidInput))
            } else {
                (completed_bytes, error)
            };

            for (position, (index, folio)) in batch_folios.iter().enumerate() {
                // A contiguous submission contains full pages followed by
                // at most one EOF tail. No per-entry byte length is needed.
                let folio_end = ((position + 1) * PAGE_SIZE_4K).min(batch_data.len());
                let is_complete = folio_end <= completed_bytes;
                self.finish_writeback(*index, folio, is_complete);
                if is_complete {
                    *nr_to_write -= 1;
                }
            }
            batch_folios.clear();
            batch_data.clear();
            error.map_or(Ok(()), Err)
        };

        'scan: while scan_index < end_index {
            self.collect_dirty_folios(&mut scan_index, end_index, &mut candidates);
            for &(index, ref folio) in candidates.iter() {
                if *nr_to_write == 0 {
                    break 'scan;
                }
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
                let would_overflow = batch_data.len().saturating_add(valid_len) > max_bytes;
                let would_exceed_pages = batch_folios.len() >= *nr_to_write;
                if !batch_data.is_empty()
                    && (!contiguous
                        || would_overflow
                        || would_exceed_pages
                        || batch_folios.is_full())
                {
                    flush_batch(
                        batch_start,
                        &mut batch_data,
                        &mut batch_folios,
                        write_range_fn,
                        nr_to_write,
                    )?;
                    if *nr_to_write == 0 {
                        break 'scan;
                    }
                }

                let Some(mut locked) =
                    self.start_writeback_for_candidate(index, folio, pages_skipped)
                else {
                    flush_batch(
                        batch_start,
                        &mut batch_data,
                        &mut batch_folios,
                        write_range_fn,
                        nr_to_write,
                    )?;
                    if *nr_to_write == 0 {
                        break 'scan;
                    }
                    next_index = None;
                    continue;
                };

                if batch_data.is_empty() {
                    batch_start = page_start;
                }
                batch_data.extend_from_slice(&locked.data()[..valid_len]);
                drop(locked);
                batch_folios.push(index, folio.clone());
                next_index = Some(index + 1);
            }
        }

        flush_batch(
            batch_start,
            &mut batch_data,
            &mut batch_folios,
            write_range_fn,
            nr_to_write,
        )?;
        Ok(())
    }
}
