// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident folio storage and its derived state indexes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kerrno::KResult;
use ksync::Mutex;

use crate::{Folio, FolioGuard, PageIndex};

/// Filesystem-owned capacity accounting for cache-owned physical pages.
///
/// Admission runs under the mapping's sleepable index and folio mutexes.
/// Release runs after physical deallocation, with neither mutex held. Both
/// callbacks require sleepable task context; a release may outlive the mapping.
/// They must not reenter a mapping, lock folios, perform I/O, or acquire locks
/// whose holders wait for this mapping. Each folio contains one page. A charge
/// belongs to the stable folio and survives cache detachment and all clones.
pub trait CacheResidency: Send + Sync {
    /// Admits one new page before publication, or fails without changing accounting.
    fn try_add_folio(&self) -> KResult<()>;

    /// Releases one charge after its physical allocation is freed.
    fn remove_folio(&self);
}

pub(super) struct MappingPagesState {
    pub(super) entries: BTreeMap<PageIndex, Arc<Folio>>,
    residency: Option<Arc<dyn CacheResidency>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    pub(super) dirty_marks: BTreeSet<PageIndex>,
    // Derived PAGECACHE_TAG_WRITEBACK index for waits.
    pub(super) writeback_marks: BTreeSet<PageIndex>,
}

impl MappingPagesState {
    pub(super) fn admit_folio(&self, folio: &mut FolioGuard<'_>) -> KResult<()> {
        self.residency.as_ref().map_or(Ok(()), |residency| {
            folio.charge_residency(residency.clone())
        })
    }

    pub(super) fn insert_folio(
        &mut self,
        index: PageIndex,
        folio: Arc<Folio>,
        locked: &mut FolioGuard<'_>,
    ) -> KResult<()> {
        debug_assert!(!self.entries.contains_key(&index));
        self.admit_folio(locked)?;
        self.entries.insert(index, folio);
        Ok(())
    }

    pub(super) fn remove_folio(&mut self, index: PageIndex, retained: &Arc<Folio>) {
        debug_assert!(self.is_current(index, retained));
        // The caller retains this exact object past the index critical section,
        // so removing the tree reference cannot deallocate or call the provider.
        self.entries.remove(&index);
    }

    pub(super) fn is_current(&self, index: PageIndex, candidate: &Arc<Folio>) -> bool {
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
    #[cfg(unittest)]
    pub(super) allocation_gate: Mutex<Option<Arc<crate::pagecache_unittest::AllocationGate>>>,
}

impl MappingPages {
    /// Creates an empty mapping page index.
    pub fn new() -> Self {
        Self::with_residency(None)
    }

    /// Creates an empty mapping with its filesystem's residency lifecycle.
    /// The provider is fixed before any entry can become visible.
    pub fn with_residency(residency: Option<Arc<dyn CacheResidency>>) -> Self {
        Self {
            #[cfg(unittest)]
            allocation_gate: Mutex::new(None),
            state: Mutex::new(MappingPagesState {
                entries: BTreeMap::new(),
                residency,
                dirty_marks: BTreeSet::new(),
                writeback_marks: BTreeSet::new(),
            }),
        }
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.state.lock().entries.len() as u64
    }

    pub(super) fn reconcile_dirty_folio(&self, candidate: &Arc<Folio>, folio: &FolioGuard<'_>) {
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
