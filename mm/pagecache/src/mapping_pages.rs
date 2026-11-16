// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident folio storage and its derived state indexes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kerrno::KResult;
use ksync::{Mutex, SpinConfig};

use crate::{Folio, FolioGuard, PageIndex};

/// Filesystem-owned accounting for pages resident in one cache mapping.
///
/// Admission and release run in task context as part of serialized mapping-index
/// transitions. Admission also holds the candidate folio lock; final index
/// destruction calls release while the state is exclusively owned. Providers
/// must not acquire either lock recursively. The charge follows cache membership,
/// not the lifetime of a `Folio`, PTE, or physical allocation. Providers must not
/// sleep, reenter this mapping, or take locks whose holders wait for it. Each
/// folio contains one page.
pub trait CacheResidency: Send + Sync {
    /// Admits one new page before publication, or fails without changing accounting.
    fn try_add_folio(&self) -> KResult<()>;

    /// Releases one charge after its cache identity is detached.
    fn remove_folio(&self);
}

pub(super) struct MappingPagesState {
    pub(super) entries: BTreeMap<PageIndex, Arc<Folio>>,
    pub(super) residency: Option<Arc<dyn CacheResidency>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    pub(super) dirty_marks: BTreeSet<PageIndex>,
    // Derived PAGECACHE_TAG_WRITEBACK index for waits.
    pub(super) writeback_marks: BTreeSet<PageIndex>,
    // Derived PAGECACHE_TAG_TOWRITE selection, never an inode or folio state copy.
    pub(super) towrite_marks: BTreeSet<PageIndex>,
}

impl MappingPagesState {
    pub(super) fn admit_folio(&self) -> KResult<()> {
        self.residency
            .as_ref()
            .map_or(Ok(()), |residency| residency.try_add_folio())
    }

    pub(super) fn insert_folio(&mut self, index: PageIndex, folio: Arc<Folio>) -> KResult<()> {
        debug_assert!(!self.entries.contains_key(&index));
        self.admit_folio()?;
        self.entries.insert(index, folio);
        Ok(())
    }

    pub(super) fn remove_folio(&mut self, index: PageIndex, retained: &Arc<Folio>) {
        debug_assert!(self.is_current(index, retained));
        self.towrite_marks.remove(&index);
        if self.entries.remove(&index).is_some()
            && let Some(residency) = &self.residency
        {
            residency.remove_folio();
        }
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
            // Cached hits only traverse the tree and retain a folio. Yielding
            // after a few pauses makes unrelated readers pay scheduler latency
            // for these short holds. Bound the pauses, then park as usual for
            // longer mutations or a descheduled owner.
            state: Mutex::with_config(
                MappingPagesState {
                    entries: BTreeMap::new(),
                    residency,
                    dirty_marks: BTreeSet::new(),
                    writeback_marks: BTreeSet::new(),
                    towrite_marks: BTreeSet::new(),
                },
                SpinConfig {
                    max_spins: 10,
                    spin_before_yield: 10,
                },
            ),
        }
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.state.lock().entries.len() as u64
    }

    /// Republishes one locked folio's dirty bit into the writeback index.
    ///
    /// - `candidate`: folio identity whose bit changed; must still be resident.
    /// - `folio`: its locked guard; the bit is authoritative.
    ///
    /// Locking: takes the mapping index lock; the folio must stay locked so the
    /// bit cannot change before this update. A detached or replaced identity is
    /// ignored.
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
