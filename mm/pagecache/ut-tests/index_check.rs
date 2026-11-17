// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test-only consistency checks for the derived writeback indexes.

use alloc::vec::Vec;

use crate::{MappingPages, PageIndex};

/// One disagreement between a resident folio's bits and the derived indexes.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum IndexMismatch {
    /// The dirty bit and the dirty index mark disagree.
    Dirty {
        /// Page index inside the mapping.
        index: PageIndex,
        /// Dirty bit observed under the folio lock.
        bit: bool,
        /// Whether `dirty_marks` lists the index.
        mark: bool,
    },
    /// The writeback bit and the writeback index mark disagree.
    Writeback {
        index: PageIndex,
        bit: bool,
        mark: bool,
    },
    /// A derived mark lists an index with no resident folio.
    Orphan {
        index: PageIndex,
        dirty: bool,
        writeback: bool,
    },
}

impl MappingPages {
    /// Returns every disagreement between resident folio bits and the derived
    /// indexes.
    ///
    /// Returns: one entry per inconsistent folio or orphaned mark; empty when
    /// both indexes match the resident folios.
    ///
    /// Locking: holds the mapping index lock and try-locks each resident folio.
    /// A busy folio is skipped because its owner may be inside a pagecache
    /// wrapper waiting for this index lock. Every dirtying path holds either
    /// the folio lock (cache writers) or the mapping index lock (writeback
    /// completion), so a folio locked under this index lock is quiescent.
    /// Test-only; do not call from production paths.
    pub(crate) fn index_mismatches(&self) -> Vec<IndexMismatch> {
        let mut mismatches = Vec::new();
        let state = self.state.lock();
        for (&index, folio) in &state.entries {
            let Some(locked) = folio.try_lock() else {
                continue;
            };
            let bit = locked.is_dirty();
            let mark = state.dirty_marks.contains(&index);
            if bit != mark {
                mismatches.push(IndexMismatch::Dirty { index, bit, mark });
            }
            let bit = locked.is_under_writeback();
            let mark = state.writeback_marks.contains(&index);
            if bit != mark {
                mismatches.push(IndexMismatch::Writeback { index, bit, mark });
            }
        }
        for &index in &state.dirty_marks {
            if !state.entries.contains_key(&index) {
                mismatches.push(IndexMismatch::Orphan {
                    index,
                    dirty: true,
                    writeback: state.writeback_marks.contains(&index),
                });
            }
        }
        for &index in &state.writeback_marks {
            if !state.entries.contains_key(&index) && !state.dirty_marks.contains(&index) {
                mismatches.push(IndexMismatch::Orphan {
                    index,
                    dirty: false,
                    writeback: true,
                });
            }
        }
        mismatches
    }

    /// Checks that both derived indexes match the resident folios.
    ///
    /// Panics: when any folio's dirty or writeback bit disagrees with its
    /// index mark, or a mark lists an absent folio; the message lists every
    /// mismatch.
    ///
    /// Locking: same as [`Self::index_mismatches`].
    pub fn verify_index_consistency(&self) {
        let mismatches = self.index_mismatches();
        assert!(
            mismatches.is_empty(),
            "inconsistent writeback indexes: {mismatches:?}"
        );
    }
}
