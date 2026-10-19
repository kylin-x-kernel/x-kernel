// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Missing-run lookup for readahead windows.

use alloc::sync::Arc;
use core::ops::Range;

use kerrno::KResult;
use ksync::{Mutex, MutexGuard};

use crate::{Folio, FolioBatch, MappingPages, PageIndex};

impl MappingPages {
    /// Installs and locks an absent prefix of `range` for synchronous readahead.
    ///
    /// Reserves at most one bounded folio batch. All folios are resident before
    /// `read` runs, with the index lock released. The callback must mark complete
    /// folios uptodate. Consuming the iterator transfers a folio to the read
    /// attempt: failed attempts may remain resident for demand-read retry.
    /// Unconsumed reservations are removed while still locked, before waiting
    /// readers can initialize them. Those readers must look up residency again.
    /// The owner must exclude invalidation throughout this call. The callback
    /// must not reenter the mapping or acquire locks needed by truncation.
    /// Returns the reserved prefix. Allocation or admission failures after a
    /// nonempty prefix are partial success; an error is returned only when no
    /// identity was reserved. Demand reads report a later admission failure.
    pub fn readahead_folios(
        &self,
        range: Range<PageIndex>,
        read: impl FnOnce(&mut dyn ExactSizeIterator<Item = &mut Folio>),
    ) -> KResult<usize> {
        let mut batch = FolioBatch::new();
        for index in range {
            if batch.is_full() || self.state.lock().entries.contains_key(&index) {
                break;
            }
            #[cfg(unittest)]
            crate::pagecache_unittest::observe_allocation(self);
            let folio = match Folio::new_zeroed(index) {
                Ok(folio) => folio,
                Err(error) if batch.is_empty() => {
                    return if self.state.lock().entries.contains_key(&index) {
                        Ok(0)
                    } else {
                        Err(error)
                    };
                }
                Err(_) => break,
            };
            batch.push(Arc::new(Mutex::new(folio)));
        }
        let count = batch.len();
        if count == 0 {
            return Ok(0);
        }
        // These folios are private until publication, so locking cannot wait.
        let mut locked: [Option<MutexGuard<'_, Folio>>; FolioBatch::CAPACITY] =
            core::array::from_fn(|_| None);
        for (slot, folio) in locked.iter_mut().zip(batch.iter()) {
            *slot = Some(folio.lock());
        }
        let mut state = self.state.lock();
        let mut count = 0;
        let mut charge_error = None;
        // Every batch folio has a guard in the matching leading slot.
        for (folio, guard) in batch.iter().zip(locked.iter().flatten()) {
            let index = guard.index();
            if state.entries.contains_key(&index) {
                break;
            }
            if let Err(error) = state.insert_folio(index, folio.clone()) {
                charge_error = Some(error);
                break;
            }
            count += 1;
        }
        drop(state);
        if count == 0 {
            return charge_error.map_or(Ok(0), Err);
        }
        let consumed = {
            #[expect(
                clippy::unwrap_used,
                reason = "CG-021-no-panic-on-reachable-failure: the first batch.len() slots were \
                          locked above and count is at most batch.len(); the callback needs an \
                          exact-size iterator"
            )]
            let mut folios = locked[..count]
                .iter_mut()
                .map(|guard| guard.as_deref_mut().unwrap());
            read(&mut folios);
            count - folios.len()
        };
        // Unconsumed pages never leave our locked reservation lifecycle:
        // detach before unlocking, so waiters must find a fresh resident.
        let mut state = self.state.lock();
        let guards = locked.iter_mut().flatten();
        for (candidate, guard) in batch.iter().zip(guards).take(count).skip(consumed) {
            let folio: &mut Folio = guard;
            if state.is_current(folio.index(), candidate) {
                Self::discard_folio(&mut state, folio);
            }
        }
        drop(state);
        Ok(count)
    }

    /// Counts the contiguous resident history immediately before `index`.
    /// This is a bounded policy hint; residency may change after returning.
    pub fn preceding_cached_pages(&self, index: PageIndex, limit: u64) -> u64 {
        let state = self.state.lock();
        let mut next = index;
        for (&cached, _) in state
            .entries
            .range(index.saturating_sub(limit)..index)
            .rev()
        {
            if cached + 1 != next {
                break;
            }
            next = cached;
        }
        index - next
    }

    /// Finds the first contiguous absent run within `range`.
    ///
    /// The result is a hint, not a reservation. Publication must still resolve
    /// concurrent insertions under the mapping state lock.
    pub fn next_miss_range(&self, range: Range<PageIndex>) -> Option<Range<PageIndex>> {
        if range.is_empty() {
            return None;
        }
        let state = self.state.lock();
        let mut start = range.start;
        for (&index, _) in state.entries.range(range.clone()) {
            if index > start {
                return Some(start..index);
            }
            start = index + 1;
        }
        (start < range.end).then_some(start..range.end)
    }
}
