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

pub(super) struct PageCacheInner {
    pub(super) pages: BTreeMap<PageIndex, Arc<Mutex<Folio>>>,
    // Rust's `BTreeMap` has no per-entry marks, so this derived index carries
    // the enumeration role of Linux `PAGECACHE_TAG_DIRTY`. `Folio::dirty`
    // remains the authoritative dirty state.
    pub(super) dirty_pages: BTreeSet<PageIndex>,
}

/// Cache storage owned by one inode address space.
pub struct PageCache {
    kind: PageCacheKind,
    pub(super) inner: Mutex<PageCacheInner>,
}

impl PageCache {
    /// Creates cache storage with source-specific materialization operations.
    pub fn new(kind: PageCacheKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            inner: Mutex::new(PageCacheInner {
                pages: BTreeMap::new(),
                dirty_pages: BTreeSet::new(),
            }),
        })
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
        self.inner.lock().pages.len() as u64
    }

    pub(super) fn reconcile_dirty_folio(&self, index: PageIndex, folio: &Arc<Mutex<Folio>>) {
        let mut inner = self.inner.lock();
        let is_current = inner
            .pages
            .get(&index)
            .is_some_and(|current| Arc::ptr_eq(current, folio));
        if !is_current {
            return;
        }
        if folio.lock().is_dirty() {
            inner.dirty_pages.insert(index);
        } else {
            inner.dirty_pages.remove(&index);
        }
    }
}
