// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio lookup and materialization.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;

use crate::{Folio, MappingPages, PageIndex};

impl MappingPages {
    /// Runs `f` with the cached folio at `index`, if present.
    ///
    /// Dirty-state changes made by `f` are reflected in the cache's writeback
    /// index before this method returns.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        let folio = self.state.lock().entries.get(&index).cloned();
        if let Some(folio) = folio {
            let (result, needs_dirty_reconciliation) = {
                let mut locked = folio.lock();
                let was_dirty = locked.is_dirty();
                let result = f(Some(&mut locked));
                let is_dirty = locked.is_dirty();
                (result, was_dirty != is_dirty)
            };
            if needs_dirty_reconciliation {
                self.reconcile_dirty_folio(index, &folio);
            }
            result
        } else {
            f(None)
        }
    }

    /// Runs `f` with a folio at `index`, materializing one on demand.
    ///
    /// Dirty-state changes made by `f` are reflected in the cache's writeback
    /// index before this method returns.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        instantiate_folio: impl FnOnce(PageIndex) -> KResult<Folio>,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        let folio = {
            let mut state = self.state.lock();
            if let Some(folio) = state.entries.get(&index) {
                folio.clone()
            } else {
                let mut folio = instantiate_folio(index)?;
                folio.set_warn_on_dirty_drop(self.warn_on_dirty_drop());
                let folio = Arc::new(Mutex::new(folio));
                state.entries.insert(index, folio.clone());
                folio
            }
        };
        let (result, needs_dirty_reconciliation) = {
            let mut locked = folio.lock();
            let was_dirty = locked.is_dirty();
            let result = f(&mut locked);
            let is_dirty = locked.is_dirty();
            (result, was_dirty != is_dirty)
        };
        if needs_dirty_reconciliation {
            self.reconcile_dirty_folio(index, &folio);
        }
        result
    }

    /// Adds one page-cache folio from backing-store data if it is still absent.
    ///
    /// This is the page-cache insertion side used by readahead after the
    /// backing filesystem has already filled a byte range.
    pub fn filemap_add_folio(&self, index: PageIndex, offset: usize, src: &[u8]) -> KResult<bool> {
        let end = offset.checked_add(src.len()).ok_or(KError::InvalidInput)?;
        if end > PAGE_SIZE_4K {
            return Err(KError::InvalidInput);
        }
        let mut folio = Folio::new_zeroed()?;
        folio.data()[offset..end].copy_from_slice(src);
        folio.set_warn_on_dirty_drop(self.warn_on_dirty_drop());
        let mut state = self.state.lock();
        if state.entries.contains_key(&index) {
            return Ok(false);
        }
        state.entries.insert(index, Arc::new(Mutex::new(folio)));
        Ok(true)
    }
}
