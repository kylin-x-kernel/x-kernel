// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio resize and final teardown.

use alloc::sync::Arc;

use kerrno::KResult;
use ktask::completion::CompletionWait;
use memaddr::PAGE_SIZE_4K;

use crate::{
    Folio, FolioBatch, FolioGuard, MappingPages, PageIndex, mapping_pages::MappingPagesState,
};

impl MappingPages {
    /// Updates cached folios for a change in the owning inode's visible size.
    ///
    /// The owner must serialize size changes against writes and prevent new
    /// cache population beyond EOF. Concurrent writeback is allowed: shrink
    /// waits and rechecks residency and writeback under the cache locks before
    /// removal. Progress depends on writeback completing; the caller must not
    /// hold locks needed by its completion path.
    ///
    /// Task context only: waits hold neither the mapping state lock nor a folio
    /// lock. Growth dirties the retained tail without waiting for batch I/O.
    ///
    /// # Errors
    /// Returns `InvalidInput` before mutation outside sleepable task context.
    pub fn resize_cached_folios(&self, old_len: u64, len: u64) -> KResult<()> {
        let wait = CompletionWait::prepare()?;
        if len == old_len {
            return Ok(());
        }
        if len > old_len {
            self.zero_growth_tail(old_len, len, &wait);
            return Ok(());
        }

        let first_truncated = len.div_ceil(PAGE_SIZE_4K as u64);
        self.remove_ready_folios_from(first_truncated, None);
        let tail_off = (len % PAGE_SIZE_4K as u64) as usize;
        if tail_off != 0 {
            self.truncate_partial_folio(len / PAGE_SIZE_4K as u64, tail_off, &wait);
        }
        self.remove_folios_from(first_truncated, None, &wait);
        Ok(())
    }

    /// Discards all cached folios during final mapping teardown.
    ///
    /// The owner must exclude new cache population and writes. Task context only:
    /// waits for outstanding writeback before discarding even dirty folios.
    /// It also drains transferred initialization. This explicit drain differs
    /// from final container Drop, which only detaches ownership.
    ///
    /// # Errors
    /// Returns `InvalidInput` before mutation outside sleepable task context.
    pub fn truncate_final(&self) -> KResult<()> {
        let wait = CompletionWait::prepare()?;
        self.remove_ready_folios_from(0, None);
        self.remove_folios_from(0, None, &wait);
        Ok(())
    }

    // A cursor of None denotes exhaustion, including a folio at u64::MAX.
    // `end` is exclusive; None covers the whole suffix.
    pub(super) fn collect_folios_range(
        &self,
        cursor: &mut Option<PageIndex>,
        end: Option<PageIndex>,
        batch: &mut FolioBatch,
    ) {
        batch.clear();
        let Some(start) = *cursor else { return };
        let state = self.state.lock();
        *cursor = None;
        for (&index, folio) in state.entries.range(start..) {
            if end.is_some_and(|end| index >= end) {
                break;
            }
            batch.push(folio.clone());
            if batch.len() == FolioBatch::CAPACITY {
                *cursor = index.checked_add(1);
                break;
            }
        }
    }

    // Discard ready folios in `[first_index, end)`. None keeps the unbounded
    // suffix. Discarding before waiting keeps one busy folio from delaying the
    // rest or starting unnecessary I/O; the second pass handles folios skipped
    // here because they are locked or in writeback.
    fn remove_ready_folios_from(&self, first_index: PageIndex, end: Option<PageIndex>) {
        let mut cursor = Some(first_index);
        let mut batch = FolioBatch::new();
        while cursor.is_some() {
            self.collect_folios_range(&mut cursor, end, &mut batch);
            let mut state = self.state.lock();
            for candidate in batch.iter() {
                let Some(mut folio) = candidate.try_lock() else {
                    continue;
                };
                if !folio.is_under_writeback() && state.is_current(folio.index(), candidate) {
                    Self::discard_folio(&mut state, candidate, &mut folio);
                }
            }
        }
    }

    // Remove folios in `[first_index, end)` even if locked or in writeback.
    // None keeps the unbounded suffix.
    fn remove_folios_from(
        &self,
        first_index: PageIndex,
        end: Option<PageIndex>,
        wait: &CompletionWait,
    ) {
        let mut batch = FolioBatch::new();
        loop {
            // Restart at the boundary to include replacements installed while
            // locks were released. The owner excludes continuing population.
            // Success requires an empty range: a retry limit or a no-progress
            // exit could leave pages referencing blocks the owner will release.
            self.collect_folios_range(&mut Some(first_index), end, &mut batch);
            if batch.is_empty() {
                break;
            }
            for candidate in batch.iter() {
                loop {
                    candidate.wait_writeback(wait);
                    let mut folio = candidate.lock_with(wait);
                    let mut state = self.state.lock();
                    if !state.is_current(folio.index(), candidate) {
                        break;
                    }
                    // Writeback may have restarted between waiting and taking
                    // the locks. Test and removal must be one critical section.
                    if folio.is_under_writeback() {
                        continue;
                    }
                    Self::discard_folio(&mut state, candidate, &mut folio);
                    break;
                }
            }
        }
    }

    pub(super) fn discard_folio(
        state: &mut MappingPagesState,
        retained: &Arc<Folio>,
        folio: &mut FolioGuard<'_>,
    ) {
        let index = folio.index();
        folio.invalidate();
        state.remove_folio(index, retained);
        state.dirty_marks.remove(&index);
        state.writeback_marks.remove(&index);
    }

    fn zero_growth_tail(&self, old_len: u64, new_len: u64, wait: &CompletionWait) {
        let offset = (old_len % PAGE_SIZE_4K as u64) as usize;
        if offset == 0 {
            return;
        }
        let index = old_len / PAGE_SIZE_4K as u64;
        let end = (new_len - index * PAGE_SIZE_4K as u64).min(PAGE_SIZE_4K as u64) as usize;
        loop {
            let Some(candidate) = self.state.lock().entries.get(&index).cloned() else {
                return;
            };
            // Growth retains the folio. Writeback already owns its snapshot.
            let mut folio = candidate.lock_with(wait);
            let state = self.state.lock();
            if !state.is_current(index, &candidate) {
                continue;
            }
            if !folio.is_uptodate() {
                return;
            }
            drop(state);
            folio.data()[offset..end].fill(0);
            folio.mark_dirty();
            self.reconcile_dirty_folio(&candidate, &folio);
            return;
        }
    }

    fn truncate_partial_folio(&self, index: PageIndex, offset: usize, wait: &CompletionWait) {
        loop {
            let Some(candidate) = self.state.lock().entries.get(&index).cloned() else {
                return;
            };
            candidate.wait_writeback(wait);
            let mut folio = candidate.lock_with(wait);
            let state = self.state.lock();
            if !state.is_current(index, &candidate) || folio.is_under_writeback() {
                continue;
            }
            if !folio.is_uptodate() {
                return;
            }
            drop(state);
            folio.data()[offset..].fill(0);
            folio.mark_dirty();
            self.reconcile_dirty_folio(&candidate, &folio);
            return;
        }
    }
}

impl Drop for MappingPagesState {
    fn drop(&mut self) {
        // Active operations and writeback completions borrow this index.
        // Read tokens retain their page and backing resources independently.
        // Final storage destruction detaches all cache identities and releases
        // their residency charges; owners needing an I/O drain call
        // truncate_final() before releasing backing resources.
        for folio in self.entries.values() {
            debug_assert!(!folio.is_under_writeback());
            folio.invalidate();
        }
        let detached = core::mem::take(&mut self.entries);
        if let Some(residency) = &self.residency {
            for _ in 0..detached.len() {
                residency.remove_folio();
            }
        }
        drop(detached);
    }
}
