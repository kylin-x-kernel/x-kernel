// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio resize and final teardown.

use alloc::vec::Vec;

use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;

use crate::{MappingPages, mapping_pages::MappingPagesState};

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
        let dropped_folios = state
            .entries
            .range(first_truncated..)
            .map(|(index, folio)| (*index, folio.clone()))
            .collect::<Vec<_>>();
        // Explicit truncation owns the data discard, so clear dirty before
        // eviction rather than requiring writeback.
        for (_, folio) in &dropped_folios {
            folio.lock().clear_dirty();
        }
        state.entries.retain(|index, _| *index < first_truncated);
        state.dirty_marks.retain(|index| *index < first_truncated);

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
        let dropped_folios = {
            let mut state = self.state.lock();
            let dropped = state
                .entries
                .iter()
                .map(|(index, folio)| (*index, folio.clone()))
                .collect::<Vec<_>>();
            state.entries.clear();
            state.dirty_marks.clear();
            dropped
        };
        for (_, folio) in &dropped_folios {
            folio.lock().clear_dirty();
        }
        Ok(())
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
