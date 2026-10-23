// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio lookup and materialization.

use alloc::sync::Arc;
use core::ops::ControlFlow;

use kerrno::{KError, KResult};

use crate::{Folio, FolioBatch, FolioGuard, MappingPages, PageIndex, PinnedFolio};

impl MappingPages {
    fn get_or_create_folio(&self, index: PageIndex) -> KResult<Arc<Folio>> {
        loop {
            let mut state = self.state.lock();
            if let Some(folio) = state.entries.get(&index) {
                return Ok(folio.clone());
            }

            drop(state);
            #[cfg(unittest)]
            crate::pagecache_unittest::observe_allocation(self);
            let folio = match Folio::new_zeroed(index) {
                Ok(folio) => Arc::new(folio),
                Err(error) => {
                    // A competing publication wins even when our speculative
                    // physical allocation failed.
                    if let Some(folio) = self.state.lock().entries.get(&index) {
                        return Ok(folio.clone());
                    }
                    return Err(error);
                }
            };
            // This private object cannot block. Preserve folio -> index order
            // when attaching its allocation-lifetime capacity charge.
            let mut locked = folio.lock();
            state = self.state.lock();
            if state.entries.contains_key(&index) {
                // Another allocator may have published this index while the
                // index lock was dropped; reuse that identity without another
                // capacity charge.
                drop(state);
                drop(locked);
                // Dropping a losing allocation can enter the page allocator,
                // so keep it outside the mapping index critical section.
                drop(folio);
                continue;
            }

            if let Err(error) = state.insert_folio(index, folio.clone(), &mut locked) {
                drop(state);
                drop(locked);
                drop(folio);
                return Err(error);
            }
            drop(state);
            drop(locked);
            return Ok(folio);
        }
    }

    fn with_initialized_folio<R>(
        &self,
        index: PageIndex,
        read: impl FnOnce(&mut FolioGuard<'_>) -> KResult<()>,
        f: impl FnOnce(&Arc<Folio>, &mut FolioGuard<'_>) -> KResult<R>,
    ) -> KResult<R> {
        // This helper's lock order is folio -> mapping state. After waiting for
        // the folio, recheck the index because truncate may have detached it.
        let mut folio = self.get_or_create_folio(index)?;
        loop {
            let Some(mut locked) = folio.try_lock() else {
                #[cfg(unittest)]
                crate::pagecache_unittest::observe_read_wait(&folio);
                drop(folio.lock());
                folio = self.get_or_create_folio(index)?;
                continue;
            };

            let state = self.state.lock();
            if !state.is_current(index, &folio) {
                drop(state);
                drop(locked);
                folio = self.get_or_create_folio(index)?;
                continue;
            }
            drop(state);

            let was_dirty = locked.is_dirty();
            if !locked.is_uptodate() {
                read(&mut locked)?;
                locked.mark_uptodate();
                if was_dirty != locked.is_dirty() {
                    self.reconcile_dirty_folio(&folio, &locked);
                }
            }
            let was_dirty = locked.is_dirty();
            let result = f(&folio, &mut locked);
            if was_dirty != locked.is_dirty() {
                self.reconcile_dirty_folio(&folio, &locked);
            }
            return result;
        }
    }

    /// Initializes and pins one resident folio without exposing cache mutation.
    ///
    /// `read` runs without the mapping index lock and is called only when the
    /// folio is not up to date. The owner must exclude invalidation and validate
    /// EOF around this operation. The returned reference keeps the allocation
    /// alive if the cache entry is later evicted.
    pub fn pin_folio_or_create(
        &self,
        index: PageIndex,
        read: impl FnOnce(&mut FolioGuard<'_>) -> KResult<()>,
    ) -> KResult<PinnedFolio> {
        self.with_initialized_folio(index, read, |folio, _locked| {
            Ok(PinnedFolio::new(folio.clone()))
        })
    }

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
    pub fn with_folio<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(Option<&mut FolioGuard<'_>>) -> R,
    ) -> R {
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
        read: impl Fn(&mut FolioGuard<'_>) -> KResult<()>,
        mut visit: impl FnMut(PageIndex, Option<&mut FolioGuard<'_>>) -> KResult<ControlFlow<()>>,
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
        read: impl FnOnce(&mut FolioGuard<'_>) -> KResult<()>,
        f: impl FnOnce(Option<&mut FolioGuard<'_>>) -> KResult<R>,
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
        read: impl FnOnce(&mut FolioGuard<'_>) -> KResult<()>,
        f: impl FnOnce(&mut FolioGuard<'_>) -> KResult<R>,
    ) -> KResult<R> {
        self.with_initialized_folio(index, read, |_folio, locked| f(locked))
    }

    /// Publishes a fully initialized folio if its index is still absent.
    ///
    /// The owner must serialize backing I/O and publication against invalidation.
    /// A concurrent resident wins. On conflict, returns the incoming folio
    /// unchanged so the caller retains ownership of its data and dirty state.
    /// Dirty state is indexed in the same critical section as publication.
    #[cfg(unittest)]
    pub fn filemap_add_folio(&self, folio: Folio) -> Result<(), Folio> {
        let index = folio.index();
        // Preserve folio -> index lock order even for privately owned pages.
        let mut locked = folio.lock();
        let mut state = self.state.lock();
        if state.entries.contains_key(&index) {
            drop(locked);
            return Err(folio);
        }
        if state.admit_folio(&mut locked).is_err() {
            drop(locked);
            return Err(folio);
        }
        locked.mark_uptodate();
        if locked.is_dirty() {
            state.dirty_marks.insert(index);
        }
        drop(locked);
        state.entries.insert(index, Arc::new(folio));
        Ok(())
    }
}

unittest::path_mod_test!(filemap_unittest, "../ut-tests/filemap_unittest.rs");
