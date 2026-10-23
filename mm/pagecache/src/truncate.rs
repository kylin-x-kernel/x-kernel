// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio resize and final teardown.

use alloc::sync::Arc;

use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;

use crate::{
    Folio, FolioBatch, FolioGuard, MappingPages, PageIndex, mapping_pages::MappingPagesState,
    writeback::wait_on_writeback,
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
    pub fn resize_cached_folios(&self, old_len: u64, len: u64) {
        if len == old_len {
            return;
        }
        if len > old_len {
            self.zero_growth_tail(old_len, len);
            return;
        }

        let first_truncated = len.div_ceil(PAGE_SIZE_4K as u64);
        self.remove_ready_folios_from(first_truncated);
        let tail_off = (len % PAGE_SIZE_4K as u64) as usize;
        if tail_off != 0 {
            self.truncate_partial_folio(len / PAGE_SIZE_4K as u64, tail_off);
        }
        self.remove_folios_from(first_truncated);
    }

    /// Discards all cached folios during final mapping teardown.
    ///
    /// The owner must exclude new cache population and writes. Task context only:
    /// waits for outstanding writeback before discarding even dirty folios.
    pub fn truncate_final(&self) -> KResult<()> {
        self.remove_ready_folios_from(0);
        self.remove_folios_from(0);
        Ok(())
    }

    // A cursor of None denotes exhaustion, including a folio at u64::MAX.
    fn collect_truncate_folios(&self, cursor: &mut Option<PageIndex>, batch: &mut FolioBatch) {
        batch.clear();
        let Some(start) = *cursor else { return };
        let state = self.state.lock();
        *cursor = None;
        for (&index, folio) in state.entries.range(start..) {
            batch.push(folio.clone());
            if batch.len() == FolioBatch::CAPACITY {
                *cursor = index.checked_add(1);
                break;
            }
        }
    }

    // Discard ready folios before waiting, so one busy folio cannot delay
    // removal of the rest or allow them to start unnecessary I/O. The second
    // pass handles folios skipped here because they are locked or in writeback.
    fn remove_ready_folios_from(&self, first_index: PageIndex) {
        let mut cursor = Some(first_index);
        let mut batch = FolioBatch::new();
        while cursor.is_some() {
            self.collect_truncate_folios(&mut cursor, &mut batch);
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

    fn remove_folios_from(&self, first_index: PageIndex) {
        let mut batch = FolioBatch::new();
        loop {
            // Restart at the boundary to include replacements installed while
            // locks were released. The owner excludes continuing population.
            // Success requires an empty range: a retry limit or a no-progress
            // exit could leave pages referencing blocks the owner will release.
            self.collect_truncate_folios(&mut Some(first_index), &mut batch);
            if batch.is_empty() {
                break;
            }
            for candidate in batch.iter() {
                loop {
                    wait_on_writeback(candidate);
                    let mut folio = candidate.lock();
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

    fn zero_growth_tail(&self, old_len: u64, new_len: u64) {
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
            let mut folio = candidate.lock();
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

    fn truncate_partial_folio(&self, index: PageIndex, offset: usize) {
        loop {
            let Some(candidate) = self.state.lock().entries.get(&index).cloned() else {
                return;
            };
            wait_on_writeback(&candidate);
            let mut folio = candidate.lock();
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

impl Drop for MappingPages {
    fn drop(&mut self) {
        // Final ownership excludes mapping operations: synchronous writeback
        // borrows this container through completion. I/O waits belong to the
        // explicit truncate_final path, not field destruction. As with other
        // mapping operations, accessing these mutexes requires task context.
        let entries = {
            let mut state = self.state.lock();
            state.dirty_marks.clear();
            state.writeback_marks.clear();
            state.take_folios()
        };
        for entry in entries.values() {
            let mut folio = entry.lock();
            debug_assert!(!folio.is_under_writeback());
            // External references may outlive the container. Invalidate them
            // before Rust drops the entries and their derived marks.
            folio.invalidate();
        }
    }
}
