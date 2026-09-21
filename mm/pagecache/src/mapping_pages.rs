// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident folio storage and its derived dirty index.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use ksync::Mutex;

use crate::{Folio, PageIndex};

/// Page-cache storage class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCacheKind {
    /// tmpfs/shmem-style in-memory object.
    InMemory,
    /// Regular inode-backed file object.
    FileBacked,
}

pub(super) struct MappingPagesState {
    pub(super) entries: BTreeMap<PageIndex, Arc<Mutex<Folio>>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    pub(super) dirty_marks: BTreeSet<PageIndex>,
}

/// Resident folios and dirty marks embedded in one inode address space.
///
/// This is the mapping's page index, corresponding to Linux `i_pages`.
/// The owning address space supplies filesystem operations, size and
/// invalidation serialization; this container does not own that state.
pub struct MappingPages {
    kind: PageCacheKind,
    pub(super) state: Mutex<MappingPagesState>,
}

impl MappingPages {
    /// Creates an empty mapping page index with the existing drop-warning policy.
    pub fn new(kind: PageCacheKind) -> Self {
        Self {
            kind,
            state: Mutex::new(MappingPagesState {
                entries: BTreeMap::new(),
                dirty_marks: BTreeSet::new(),
            }),
        }
    }

    pub(super) fn warn_on_dirty_drop(&self) -> bool {
        matches!(self.kind, PageCacheKind::FileBacked)
    }

    /// Returns the mapping kind.
    pub const fn kind(&self) -> PageCacheKind {
        self.kind
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.state.lock().entries.len() as u64
    }

    pub(super) fn reconcile_dirty_folio(&self, index: PageIndex, folio: &Arc<Mutex<Folio>>) {
        let mut state = self.state.lock();
        let is_current = state
            .entries
            .get(&index)
            .is_some_and(|current| Arc::ptr_eq(current, folio));
        if !is_current {
            return;
        }
        if folio.lock().is_dirty() {
            state.dirty_marks.insert(index);
        } else {
            state.dirty_marks.remove(&index);
        }
    }
}
