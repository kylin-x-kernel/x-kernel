// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio resize and final teardown.

use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;

use crate::{FolioBatch, MappingPages, PageIndex, mapping_pages::MappingPagesState};

impl MappingPages {
    /// Updates cached folios for a change in the owning inode's visible size.
    pub fn resize_cached_folios(&self, old_len: u64, len: u64) {
        let mut state = self.state.lock();
        if len == old_len {
            return;
        }

        if len >= old_len {
            if len > old_len {
                self.zero_growth_tail(&mut state, old_len, len);
            }
            return;
        }

        let first_truncated = len.div_ceil(PAGE_SIZE_4K as u64);
        Self::remove_folios_from(&mut state, first_truncated);

        let tail_index = len / PAGE_SIZE_4K as u64;
        let tail_off = (len % PAGE_SIZE_4K as u64) as usize;
        if tail_off != 0
            && let Some(folio) = state.entries.get(&tail_index).cloned()
        {
            let mut folio = folio.lock();
            folio.data()[tail_off..].fill(0);
            folio.mark_dirty();
            drop(folio);
            state.dirty_marks.insert(tail_index);
        }
    }

    /// Drops all cached folios during final mapping teardown.
    ///
    /// Unlike ordinary invalidation, final teardown may discard dirty folios
    /// because no later backing-store writeback is possible.
    pub fn truncate_final(&self) -> KResult<()> {
        Self::remove_folios_from(&mut self.state.lock(), 0);
        Ok(())
    }

    fn remove_folios_from(state: &mut MappingPagesState, first_index: PageIndex) {
        let mut batch = FolioBatch::new();
        loop {
            for (index, folio) in state
                .entries
                .range(first_index..)
                .take(FolioBatch::CAPACITY)
            {
                batch.push(*index, folio.clone());
            }
            if batch.len() == 0 {
                break;
            }
            // The state lock preserves identity throughout each removal. The
            // owner authorizes discarding data, so clear dirty before eviction.
            for (index, folio) in batch.iter() {
                folio.lock().clear_dirty();
                state.entries.remove(index);
                state.dirty_marks.remove(index);
            }
            batch.clear();
        }
        state.dirty_marks.retain(|index| *index < first_index);
    }

    fn zero_growth_tail(&self, state: &mut MappingPagesState, old_len: u64, new_len: u64) {
        if old_len == 0 {
            return;
        }
        let tail_index = (old_len - 1) / PAGE_SIZE_4K as u64;
        let Some(folio) = state.entries.get(&tail_index).cloned() else {
            return;
        };
        let page_start = tail_index * PAGE_SIZE_4K as u64;
        let old_off = (old_len - page_start) as usize;
        let new_off = (new_len - page_start).min(PAGE_SIZE_4K as u64) as usize;
        if old_off >= new_off {
            return;
        }
        let mut folio = folio.lock();
        folio.data()[old_off..new_off].fill(0);
        folio.mark_dirty();
        drop(folio);
        state.dirty_marks.insert(tail_index);
    }
}
