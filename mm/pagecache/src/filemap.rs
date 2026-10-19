// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio lookup and materialization.

use alloc::sync::Arc;
use core::ops::ControlFlow;

use kerrno::{KError, KResult};
use ksync::Mutex;

use crate::{Folio, FolioBatch, MappingPages, PageIndex};

impl MappingPages {
    /// Collects up to `count` contiguous resident folios starting at `start`.
    ///
    /// Stops at the first hole or [`FolioBatch::CAPACITY`] without loading pages.
    /// References keep allocations alive; batch access runs outside the mapping lock.
    pub fn get_folios_contig(&self, start: PageIndex, count: usize) -> FolioBatch {
        self.get_folios_contig_with_run_len(start, count.min(FolioBatch::CAPACITY))
            .0
    }

    /// Collects references and counts the contiguous resident prefix in one traversal.
    ///
    /// Returns at most [`FolioBatch::CAPACITY`] references and a count up to `count`,
    /// checking entries beyond capacity without retaining them. This mapping-lock
    /// snapshot does not guarantee residency during later access.
    pub fn get_folios_contig_with_run_len(
        &self,
        start: PageIndex,
        count: usize,
    ) -> (FolioBatch, usize) {
        let mut batch = FolioBatch::new();
        if count == 0 {
            return (batch, 0);
        }
        let mut run_len = 0;
        let state = self.state.lock();
        for (&index, folio) in state.entries.range(start..).take(count) {
            if start.checked_add(run_len as u64) != Some(index) {
                break;
            }
            if !batch.is_full() {
                batch.push(folio.clone());
            }
            run_len += 1;
        }
        (batch, run_len)
    }

    /// Runs `f` with the initialized resident folio at `index`, if present.
    ///
    /// Waits for any current folio holder and rechecks residency. An absent or
    /// non-uptodate folio is reported as `None`; partial read data is not exposed.
    /// Dirty-state changes made by `f` are reflected in the cache's writeback
    /// index before this method returns.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        loop {
            let state = self.state.lock();
            let Some(folio) = state.entries.get(&index).cloned() else {
                drop(state);
                return f(None);
            };
            let Some(mut locked) = folio.try_lock() else {
                drop(state);
                #[cfg(unittest)]
                crate::pagecache_unittest::observe_read_wait(&folio);
                drop(folio.lock());
                continue;
            };
            drop(state);
            let was_dirty = locked.is_dirty();
            let result = f(locked.is_uptodate().then_some(&mut locked));
            let needs_dirty_reconciliation = was_dirty != locked.is_dirty();
            if needs_dirty_reconciliation {
                self.reconcile_dirty_folio(&folio, &locked);
            }
            return result;
        }
    }

    /// Visits a bounded snapshot of resident folios and holes in page order.
    ///
    /// Visits at most [`FolioBatch::CAPACITY`] pages, returning the number
    /// visited, including the page on which `visit` breaks. One index lookup
    /// collects all resident references; initialized hits subsequently take
    /// only their folio lock. Holes are passed as `None` without allocation.
    /// A non-uptodate or invalidated snapshot entry is resolved again, and
    /// initialization failures propagate without visiting that page.
    ///
    /// The owner must exclude invalidation and sample EOF inside `visit`.
    /// Callbacks must not reenter this mapping or wait for another folio;
    /// `visit` must not modify data, dirty state, or residency. Task context
    /// only. A hole describes the initial snapshot, even if a concurrent writer
    /// subsequently creates that identity.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] if the bounded index range overflows,
    /// or an error from initialization or `visit`. Earlier visits remain valid.
    pub fn visit_resident_folios(
        &self,
        start: PageIndex,
        count: usize,
        read: impl Fn(&mut Folio) -> KResult<()>,
        mut visit: impl FnMut(PageIndex, Option<&mut Folio>) -> KResult<ControlFlow<()>>,
    ) -> KResult<usize> {
        let count = count.min(FolioBatch::CAPACITY);
        if count == 0 {
            return Ok(0);
        }
        let end = start
            .checked_add(count as u64)
            .ok_or(KError::InvalidInput)?;
        let mut batch = FolioBatch::new();
        let mut indices = [0; FolioBatch::CAPACITY];
        {
            let state = self.state.lock();
            for (&index, folio) in state.entries.range(start..end) {
                indices[batch.len()] = index;
                batch.push(folio.clone());
            }
        }
        let mut slot = 0;
        for (offset, index) in (start..end).enumerate() {
            let result = if slot < batch.len() && indices[slot] == index {
                let cached = batch.with_read_folio(slot, |folio| visit(index, Some(folio)));
                slot += 1;
                match cached {
                    Some(result) => result,
                    None => self.with_resident_folio(index, &read, |folio| visit(index, folio)),
                }
            } else {
                visit(index, None)
            };
            if result?.is_break() {
                return Ok(offset + 1);
            }
        }
        Ok(count)
    }

    /// Accesses an existing folio, retrying initialization without creating holes.
    ///
    /// The owner excludes invalidation and rechecks EOF in `f`. A missing
    /// identity is passed as `None`; an initialization failure is propagated
    /// without calling `f`. Neither callback may reenter this mapping.
    fn with_resident_folio<R>(
        &self,
        index: PageIndex,
        read: impl FnOnce(&mut Folio) -> KResult<()>,
        f: impl FnOnce(Option<&mut Folio>) -> KResult<R>,
    ) -> KResult<R> {
        loop {
            let state = self.state.lock();
            let Some(folio) = state.entries.get(&index).cloned() else {
                drop(state);
                return f(None);
            };
            let Some(mut locked) = folio.try_lock() else {
                drop(state);
                drop(folio.lock());
                continue;
            };
            drop(state);
            if !locked.is_uptodate() {
                read(&mut locked)?;
                locked.mark_uptodate();
            }
            let was_dirty = locked.is_dirty();
            let result = f(Some(&mut locked));
            if was_dirty != locked.is_dirty() {
                self.reconcile_dirty_folio(&folio, &locked);
            }
            return result;
        }
    }

    /// Runs `f` with an initialized resident folio, reading it if necessary.
    ///
    /// An absent folio is installed before initialization. The folio mutex
    /// serializes readers; the mapping index lock is released before `read`.
    /// Failed reads leave a non-uptodate folio that a later reader can retry.
    /// The owner must exclude invalidation and recheck EOF around this operation.
    /// Neither callback may reenter the mapping; dirty changes made by `f`
    /// are reconciled before releasing the folio lock.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        read: impl FnOnce(&mut Folio) -> KResult<()>,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        loop {
            let mut state = self.state.lock();
            let folio = if let Some(folio) = state.entries.get(&index) {
                folio.clone()
            } else {
                drop(state);
                #[cfg(unittest)]
                crate::pagecache_unittest::observe_allocation(self);
                let allocated = Folio::new_zeroed(index).map(|folio| Arc::new(Mutex::new(folio)));
                state = self.state.lock();
                if state.entries.contains_key(&index) {
                    // Another allocator published while the index lock was
                    // released. Even allocation failure must reuse its entry.
                    drop(state);
                    drop(allocated);
                    continue;
                }
                let folio = allocated?;
                if let Err(error) = state.insert_folio(index, folio.clone()) {
                    // Releasing unused physical storage may enter the
                    // allocator; keep it outside the cache index mutex.
                    drop(state);
                    drop(folio);
                    return Err(error);
                }
                folio
            };
            let Some(mut locked) = folio.try_lock() else {
                drop(state);
                // Wait without the index lock, then look up residency again:
                // truncation may have detached this folio while we waited.
                #[cfg(unittest)]
                crate::pagecache_unittest::observe_read_wait(&folio);
                drop(folio.lock());
                continue;
            };
            drop(state);
            if !locked.is_uptodate() {
                read(&mut locked)?;
                locked.mark_uptodate();
            }
            let was_dirty = locked.is_dirty();
            let result = f(&mut locked);
            let needs_dirty_reconciliation = was_dirty != locked.is_dirty();
            if needs_dirty_reconciliation {
                self.reconcile_dirty_folio(&folio, &locked);
            }
            return result;
        }
    }

    /// Publishes a fully initialized folio if its index is still absent.
    ///
    /// The owner must serialize backing I/O and publication against invalidation.
    /// A concurrent resident wins. On conflict, returns the incoming folio
    /// unchanged so the caller retains ownership of its data and dirty state.
    /// Dirty state is indexed in the same critical section as publication.
    #[cfg(unittest)]
    pub fn filemap_add_folio(&self, mut folio: Folio) -> Result<(), Folio> {
        let index = folio.index();
        let mut state = self.state.lock();
        if state.entries.contains_key(&index) {
            return Err(folio);
        }
        if state.admit_folio().is_err() {
            return Err(folio);
        }
        folio.mark_uptodate();
        if folio.is_dirty() {
            state.dirty_marks.insert(index);
        }
        state.entries.insert(index, Arc::new(Mutex::new(folio)));
        Ok(())
    }
}

unittest::path_mod_test!(filemap_unittest, "../ut-tests/filemap_unittest.rs");
