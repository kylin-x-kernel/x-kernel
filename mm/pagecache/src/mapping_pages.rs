// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident folio storage and its derived state indexes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use ksync::Mutex;

use crate::{Folio, PageIndex};

pub(super) struct MappingPagesState {
    pub(super) entries: BTreeMap<PageIndex, Arc<Mutex<Folio>>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    pub(super) dirty_marks: BTreeSet<PageIndex>,
    // Derived PAGECACHE_TAG_WRITEBACK index for waits.
    pub(super) writeback_marks: BTreeSet<PageIndex>,
}

impl MappingPagesState {
    pub(super) fn is_current(&self, index: PageIndex, candidate: &Arc<Mutex<Folio>>) -> bool {
        self.entries
            .get(&index)
            .is_some_and(|current| Arc::ptr_eq(current, candidate))
    }
}

/// Resident folios and dirty/writeback marks embedded in one inode address space.
///
/// This is the mapping's page index, corresponding to Linux `i_pages`.
/// The owning address space supplies filesystem operations, size and
/// invalidation serialization; this container does not own that state.
/// Operations and final destruction require sleepable task context.
pub struct MappingPages {
    pub(super) state: Mutex<MappingPagesState>,
}

impl MappingPages {
    /// Creates an empty mapping page index.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MappingPagesState {
                entries: BTreeMap::new(),
                dirty_marks: BTreeSet::new(),
                writeback_marks: BTreeSet::new(),
            }),
        }
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.state.lock().entries.len() as u64
    }

    pub(super) fn reconcile_dirty_folio(&self, candidate: &Arc<Mutex<Folio>>, folio: &Folio) {
        let mut state = self.state.lock();
        let index = folio.index();
        if !state.is_current(index, candidate) {
            return;
        }
        if folio.is_dirty() {
            state.dirty_marks.insert(index);
        } else {
            state.dirty_marks.remove(&index);
        }
    }
}

impl Default for MappingPages {
    fn default() -> Self {
        Self::new()
    }
}
