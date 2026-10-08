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
    /// Returns the number reserved, or an allocation error if none were prepared.
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
            let folio = match Folio::new_zeroed(index) {
                Ok(folio) => folio,
                Err(error) if batch.len() == 0 => return Err(error),
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
        for (folio, guard) in batch.iter().zip(&locked) {
            let index = guard.as_ref().unwrap().index();
            if state.entries.contains_key(&index) {
                break;
            }
            state.entries.insert(index, folio.clone());
            count += 1;
        }
        drop(state);
        if count == 0 {
            return Ok(0);
        }
        let consumed = {
            let mut folios = locked[..count]
                .iter_mut()
                .map(|guard| guard.as_deref_mut().unwrap());
            read(&mut folios);
            count - folios.len()
        };
        // Unconsumed pages never leave our locked reservation lifecycle:
        // detach before unlocking, so waiters must find a fresh resident.
        let mut state = self.state.lock();
        for (candidate, guard) in batch.iter().zip(&mut locked).take(count).skip(consumed) {
            let folio = guard.as_deref_mut().unwrap();
            if state.is_current(folio.index(), candidate) {
                Self::discard_folio(&mut state, folio);
            }
        }
        drop(state);
        Ok(count)
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
