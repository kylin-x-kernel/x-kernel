// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio lookup and materialization.

use alloc::sync::Arc;

use kerrno::KResult;
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
                let folio = Arc::new(Mutex::new(Folio::new_zeroed(index)?));
                state = self.state.lock();
                if state.entries.contains_key(&index) {
                    // A concurrent resident owns initialization. Release the
                    // unused allocation outside the index lock and look it up.
                    drop(state);
                    drop(folio);
                    continue;
                }
                state.entries.insert(index, folio.clone());
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
    pub fn filemap_add_folio(&self, mut folio: Folio) -> Result<(), Folio> {
        let index = folio.index();
        let mut state = self.state.lock();
        if state.entries.contains_key(&index) {
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
