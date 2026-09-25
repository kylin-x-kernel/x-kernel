// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded references used by mapping traversal and writeback.

use alloc::sync::Arc;

use ksync::Mutex;

use crate::Folio;

/// A bounded, ascending collection of mapping folio references.
///
/// References keep data alive but confer neither residency nor writeback
/// ownership. Callers must finish any started writeback before clearing the
/// batch; releasing references does not change folio state.
///
/// Entries follow mapping traversal order. The folio owns its index; batch
/// storage holds only references, as Linux folio_batch does.
pub(crate) struct FolioBatch {
    entries: [Option<Arc<Mutex<Folio>>>; Self::CAPACITY],
    len: usize,
}

impl FolioBatch {
    pub(crate) const CAPACITY: usize = 31;

    pub(crate) fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| None),
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
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
