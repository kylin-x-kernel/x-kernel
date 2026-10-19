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

use crate::{Folio, PageIndex};

/// Filesystem-owned admission and removal accounting for one mapping.
///
/// Callbacks run under the mapping's sleepable index mutex, in task context.
/// They must not reenter a mapping, lock folios, perform I/O, or acquire locks
/// whose holders wait for this mapping. Each resident folio contains one page.
/// The mapping retains one provider, shared by all its entries.
pub trait CacheResidency: Send + Sync {
    /// Admits one new cache identity, or fails without changing its accounting.
    fn try_add_folio(&self) -> KResult<()>;

    /// Releases `count` cache identities after removal or final destruction.
    /// Physical folio references may outlive their cache identities.
    fn remove_folios(&self, count: usize);
}

pub(super) struct MappingPagesState {
    pub(super) entries: BTreeMap<PageIndex, Arc<Mutex<Folio>>>,
    residency: Option<Arc<dyn CacheResidency>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    pub(super) dirty_marks: BTreeSet<PageIndex>,
    // Derived PAGECACHE_TAG_WRITEBACK index for waits.
    pub(super) writeback_marks: BTreeSet<PageIndex>,
}

impl MappingPagesState {
    pub(super) fn admit_folio(&self) -> KResult<()> {
        self.residency
            .as_ref()
            .map_or(Ok(()), |residency| residency.try_add_folio())
    }

    pub(super) fn insert_folio(
        &mut self,
        index: PageIndex,
        folio: Arc<Mutex<Folio>>,
    ) -> KResult<()> {
        debug_assert!(!self.entries.contains_key(&index));
        self.admit_folio()?;
        self.entries.insert(index, folio);
        Ok(())
    }

    pub(super) fn remove_folio(&mut self, index: PageIndex) {
        if self.entries.remove(&index).is_some()
            && let Some(residency) = &self.residency
        {
            residency.remove_folios(1);
        }
    }

    pub(super) fn take_folios(&mut self) -> BTreeMap<PageIndex, Arc<Mutex<Folio>>> {
        let entries = core::mem::take(&mut self.entries);
        if let Some(residency) = &self.residency {
            residency.remove_folios(entries.len());
        }
        entries
    }

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
