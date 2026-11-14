// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio lookup and materialization.

use alloc::sync::Arc;
use core::ops::ControlFlow;

use kerrno::{KError, KResult};

use crate::{Folio, FolioBatch, FolioGuard, FolioRead, MappingPages, PageIndex, PinnedFolio};

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
            let locked = folio.lock()?;
            state = self.state.lock();
            if state.entries.contains_key(&index) {
                // Another allocator may have published this index while the
                // index lock was dropped; reuse that identity without another
                // cache-residency charge.
                drop(state);
                drop(locked);
                // Dropping a losing allocation can enter the page allocator,
                // so keep it outside the mapping index critical section.
                drop(folio);
                continue;
            }

            if let Err(error) = state.insert_folio(index, folio.clone()) {
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
        mut read: impl FnMut(FolioRead) -> KResult<()>,
        f: impl FnOnce(&Arc<Folio>, &mut FolioGuard<'_>) -> KResult<R>,
    ) -> KResult<R> {
        // After waiting for the folio, recheck the invalidation marker because
        // truncate may have detached it before the lock was acquired.
        let mut folio = self.get_or_create_folio(index)?;
        loop {
            let Some(mut locked) = folio.try_lock() else {
                drop(folio.lock_killable()?);
                folio = self.get_or_create_folio(index)?;
                continue;
            };

            // Discard marks the detached identity before removing it from the
            // index, so a set flag is equivalent to an `is_current` mismatch
            // without taking the index lock a second time.
            if locked.is_invalidated() {
                drop(locked);
                folio = self.get_or_create_folio(index)?;
                continue;
            }

            if !locked.is_uptodate() {
                locked = read_locked_folio(&folio, locked, &mut read)?;
                if locked.is_invalidated() {
                    drop(locked);
                    folio = self.get_or_create_folio(index)?;
                    continue;
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
    /// `read` receives initialization ownership without the mapping index lock
    /// only when the folio is not up to date. It may transfer that ownership to
    /// completion; this method waits for completion and checks residency again.
    /// The owner must exclude invalidation (through the invalidate lock or the
    /// inode data lock) and validate EOF around this operation.
    /// The returned reference keeps the allocation alive after cache eviction.
    /// Waiting is killable: `Interrupted` leaves any submitted read owned by
    /// its completion token. Invalid waiting context returns `InvalidInput`.
    pub fn pin_folio_or_create(
        &self,
        index: PageIndex,
        read: impl FnMut(FolioRead) -> KResult<()>,
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
    ///
    /// # Errors
    /// Waiting returns [`KError::InvalidInput`] outside sleepable task context
    /// or [`KError::Interrupted`] for a pending uncatchable kill, before `f` runs.
    pub fn with_folio<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(Option<&mut FolioGuard<'_>>) -> R,
    ) -> KResult<R> {
        loop {
            let state = self.state.lock();
            let Some(folio) = state.entries.get(&index).cloned() else {
                drop(state);
                return Ok(f(None));
            };
            let Some(mut locked) = folio.try_lock() else {
                drop(state);
                drop(folio.lock_killable()?);
                continue;
            };
            drop(state);
            let was_dirty = locked.is_dirty();
            let result = f(locked.is_uptodate().then_some(&mut locked));
            let needs_dirty_reconciliation = was_dirty != locked.is_dirty();
            if needs_dirty_reconciliation {
                self.reconcile_dirty_folio(&folio, &locked);
            }
            return Ok(result);
        }
    }

    /// Runs `write` on one resident batch slot under its folio lock for buffered writing.
    ///
    /// - `batch`: a snapshot from [`Self::get_folios_contig_with_run_len`].
    /// - `slot`: traversal position within `batch`; an out-of-range slot is a miss.
    /// - `write`: copies data into the locked folio and may dirty it; it must not
    ///   wait for another folio or change residency.
    ///
    /// Returns: `Ok(Some(result))` when `write` ran, `Ok(None)` when the entry is
    /// absent, non-uptodate or invalidated. Dirty-state changes made by `write`
    /// are reflected in the cache's writeback index on both the success and the
    /// failure path, before this method returns.
    ///
    /// Errors: waiting returns [`KError::InvalidInput`] outside sleepable task
    /// context or [`KError::Interrupted`] for a pending uncatchable kill, before
    /// `write` runs; `write` errors propagate with the folio lock released.
    ///
    /// Locking: takes the folio lock and holds it across `write`; reconciles the
    /// dirty index under the mapping index lock while the folio stays locked. No
    /// mapping lock is held during `write`. The caller must exclude invalidation
    /// (through the inode data lock). May sleep while locking.
    pub fn with_write_folio<R>(
        &self,
        batch: &FolioBatch,
        slot: usize,
        write: impl FnOnce(&mut FolioGuard<'_>) -> KResult<R>,
    ) -> KResult<Option<R>> {
        let Some(folio) = batch.folio_at(slot) else {
            return Ok(None);
        };
        let mut locked = folio.lock_killable()?;
        if locked.is_invalidated() || !locked.is_uptodate() {
            return Ok(None);
        }
        let was_dirty = locked.is_dirty();
        let result = write(&mut locked);
        if was_dirty != locked.is_dirty() {
            self.reconcile_dirty_folio(folio, &locked);
        }
        result.map(Some)
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
        read: impl Fn(FolioRead) -> KResult<()>,
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
                let cached = batch.with_read_folio(slot, |folio| visit(index, Some(folio)))?;
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
        mut read: impl FnMut(FolioRead) -> KResult<()>,
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
                drop(folio.lock_killable()?);
                continue;
            };
            drop(state);
            if !locked.is_uptodate() {
                locked = read_locked_folio(&folio, locked, &mut read)?;
                if !self.state.lock().is_current(index, &folio) {
                    continue;
                }
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
    /// An absent folio is installed before initialization. The folio lock
    /// serializes readers; the mapping index lock is released before `read`.
    /// `read` owns initialization and must complete or drop its token, possibly
    /// from another context. A successful submission is followed by a wait and
    /// an uptodate check; failed completion returns [`KError::Io`]. Failed reads
    /// leave a non-uptodate folio that a later reader can retry. After detach,
    /// `read` can be invoked again for the replacement identity.
    /// The owner must exclude invalidation (through the invalidate lock or the
    /// inode data lock) and recheck EOF around this operation.
    /// Neither callback may reenter the mapping; dirty changes made by `f`
    /// are reconciled before releasing the folio lock.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        read: impl FnMut(FolioRead) -> KResult<()>,
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
        let Some(mut locked) = folio.try_lock() else {
            return Err(folio);
        };
        let mut state = self.state.lock();
        if state.entries.contains_key(&index) {
            drop(locked);
            return Err(folio);
        }
        if state.admit_folio().is_err() {
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

// Read submission and completion are separate: only the initialization owner
// publishes uptodate. The caller rechecks residency after acquiring this guard.
fn read_locked_folio<'a>(
    folio: &'a Arc<Folio>,
    locked: FolioGuard<'a>,
    read: &mut impl FnMut(FolioRead) -> KResult<()>,
) -> KResult<FolioGuard<'a>> {
    // Validate before handing initialization to the filesystem. Completion may
    // outlive this call; a signal only cancels this reader's wait, not the I/O.
    let wait = ktask::completion::CompletionWait::prepare()?;
    read(locked.into_read(folio))?;
    let locked = folio.lock_killable_with(&wait)?;
    if !locked.is_uptodate() {
        return Err(KError::Io);
    }
    Ok(locked)
}

unittest::path_mod_test!(filemap_unittest, "../ut-tests/filemap_unittest.rs");
