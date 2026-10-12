// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded references shared by cached reads, writeback and mapping traversal.

use alloc::sync::Arc;

use ksync::Mutex;

use crate::Folio;

/// A bounded, ascending collection of mapping folio references.
///
/// References keep allocations alive without guaranteeing residency. Clearing
/// releases references only; callers must first finish any started writeback.
pub struct FolioBatch {
    entries: [Option<Arc<Mutex<Folio>>>; Self::CAPACITY],
    len: usize,
}

impl FolioBatch {
    /// Maximum references retained in one batch.
    pub const CAPACITY: usize = 31;

    pub(crate) fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| None),
            len: 0,
        }
    }

    /// Returns the number of references collected.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no references were collected.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reads immutable bytes from the folio at a batch position.
    ///
    /// `slot` is a traversal position. Calls `read` under the folio lock without
    /// a mapping lock; it must not re-enter the mapping. Absent, non-uptodate or invalidated
    /// entries return `None` without calling `read`; callers must resolve them again.
    pub fn with_read_folio<R>(&self, slot: usize, read: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let folio = self.entries.get(slot)?.as_ref()?;
        let mut locked = folio.lock();
        if locked.is_invalidated() || !locked.is_uptodate() {
            return None;
        }
        Some(read(locked.data()))
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == Self::CAPACITY
    }

    /// Appends a reference in traversal order, with space available.
    pub(crate) fn push(&mut self, folio: Arc<Mutex<Folio>>) {
        self.entries[self.len] = Some(folio);
        self.len += 1;
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<Mutex<Folio>>> {
        self.entries[..self.len].iter().flatten()
    }

    /// Releases references only; folio flags remain owned by the algorithm.
    pub(crate) fn clear(&mut self) {
        for entry in &mut self.entries[..self.len] {
            *entry = None;
        }
        self.len = 0;
    }
}
