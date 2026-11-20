// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Byte-range invalidation of cached folios.

use kerrno::{KError, KResult};
use ktask::completion::CompletionWait;
use memaddr::PAGE_SIZE_4K;

use crate::{FolioBatch, MappingPages};

impl MappingPages {
    /// Discards clean cached folios in the byte range `[start, end)`, rounded
    /// outward to folio boundaries.
    ///
    /// Dirty folios stay resident so the caller can write them back first.
    /// Every folio in the range is visited: one dirty or busy folio does not
    /// stop removal of the rest.
    ///
    /// - `start`: inclusive byte offset.
    /// - `end`: exclusive byte offset; an empty or inverted range is a no-op.
    ///
    /// Returns: `Ok` when no cached folio in the range remained dirty. Clean
    /// folios are discarded in either outcome.
    ///
    /// Errors: `ResourceBusy` when a dirty folio remained resident; other
    /// errors propagate unchanged.
    ///
    /// Locking: the caller must not hold the mapping state lock or a folio
    /// lock. The owner serializes cache population and mapping revocation for
    /// its file-consistency contract. Detachment releases cache ownership and
    /// quota; managed PTEs retain their own backing until translation retirement.
    /// Task context only; may sleep waiting for writeback and returns
    /// `InvalidInput` before mutation outside sleepable task context.
    pub fn invalidate_pages_range(&self, start: u64, end: u64) -> KResult<()> {
        let wait = CompletionWait::prepare()?;
        // Decide emptiness on the byte range before rounding: outward rounding
        // maps a non-aligned empty or inverted range onto a folio.
        if start >= end {
            return Ok(());
        }
        let first = start / PAGE_SIZE_4K as u64;
        let last = end.div_ceil(PAGE_SIZE_4K as u64);

        let mut batch = FolioBatch::new();
        let mut cursor = Some(first);
        let mut saw_dirty = false;
        while cursor.is_some() {
            self.collect_folios_range(&mut cursor, Some(last), &mut batch);
            for candidate in batch.iter() {
                loop {
                    candidate.wait_writeback(&wait);
                    let mut folio = candidate.lock_with(&wait);
                    let mut state = self.state.lock();
                    if !state.is_current(folio.index(), candidate) {
                        break;
                    }
                    // Writeback may have restarted between waiting and taking
                    // the locks. Test and removal must be one critical section.
                    if folio.is_under_writeback() {
                        continue;
                    }
                    // Writeback completion is the only dirtying path that
                    // does not take the content lock, and this folio's
                    // writeback was drained above. Cache writers dirty
                    // through this lock, so the decision below is race-free.
                    if folio.is_dirty() {
                        saw_dirty = true;
                        break;
                    }
                    Self::discard_folio(&mut state, candidate, &mut folio);
                    break;
                }
            }
        }

        if saw_dirty {
            return Err(KError::ResourceBusy);
        }
        Ok(())
    }
}
