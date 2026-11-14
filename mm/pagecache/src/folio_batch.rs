// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded references shared by cached reads, writeback and mapping traversal.

use alloc::sync::Arc;

use kerrno::KResult;

use crate::{Folio, FolioGuard};

/// A bounded, ascending collection of mapping folio references.
///
/// References keep allocations alive without guaranteeing residency. Clearing
/// releases references only; callers must first finish any started writeback.
pub struct FolioBatch {
    entries: [Option<Arc<Folio>>; Self::CAPACITY],
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

    /// Counts at most `max_pages` consecutive indexes from a collected slot.
    /// The caller obtains `slot` from this batch's iterator.
    pub(crate) fn contiguous_pages_from(&self, slot: usize, max_pages: usize) -> usize {
        let end = self.len.min(slot.saturating_add(max_pages));
        let mut folios = self.entries[slot..end].iter().flatten();
        let Some(first) = folios.next() else {
            return 0;
        };
        let mut previous = first.index();
        let mut pages = 1;
        for folio in folios {
            if previous.checked_add(1) != Some(folio.index()) {
                break;
            }
            previous = folio.index();
            pages += 1;
        }
        pages
    }

    /// Accesses an initialized folio at a batch position for buffered reading.
    ///
    /// `slot` is a traversal position. Calls `read` under the folio lock without
    /// a mapping lock. The callback may consume a readahead marker, but must not
    /// modify data, dirty state or residency, or wait for another folio. Absent,
    /// non-uptodate or invalidated entries return `None` without calling `read`;
    /// callers must resolve them again.
    ///
    /// # Errors
    /// Waiting returns `InvalidInput` outside sleepable task context or
    /// `Interrupted` for a pending uncatchable kill. Neither calls `read`.
    pub fn with_read_folio<R>(
        &self,
        slot: usize,
        read: impl FnOnce(&mut FolioGuard<'_>) -> R,
    ) -> KResult<Option<R>> {
        let Some(folio) = self.entries.get(slot).and_then(Option::as_ref) else {
            return Ok(None);
        };
        let mut locked = folio.lock_killable()?;
        if locked.is_invalidated() || !locked.is_uptodate() {
            return Ok(None);
        }
        Ok(Some(read(&mut locked)))
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == Self::CAPACITY
    }

    /// Returns the reference collected at a batch slot, if any.
    pub(crate) fn folio_at(&self, slot: usize) -> Option<&Arc<Folio>> {
        self.entries.get(slot).and_then(Option::as_ref)
    }

    /// Appends a reference in traversal order, with space available.
    pub(crate) fn push(&mut self, folio: Arc<Folio>) {
        self.entries[self.len] = Some(folio);
        self.len += 1;
    }

    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: push initializes each slot before \
                  increasing len, and clear resets len after removing all entries"
    )]
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = &Arc<Folio>> {
        self.entries[..self.len]
            .iter()
            .map(|entry| entry.as_ref().unwrap())
    }

    /// Releases references only; folio flags remain owned by the algorithm.
    pub(crate) fn clear(&mut self) {
        for entry in &mut self.entries[..self.len] {
            *entry = None;
        }
        self.len = 0;
    }
}
