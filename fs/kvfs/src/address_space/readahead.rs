// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Readahead requests, window selection and filesystem dispatch.

use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, PageIndex};

use super::AddressSpace;
use crate::{VfsError, VfsResult};

const MAX_READAHEAD_BYTES: usize = 128 * 1024;

/// A synchronous readahead cursor over locked, resident, uninitialized folios.
///
/// The address space lends this control to the filesystem while holding its
/// invalidate lock for reading. Completion cannot outlive that callback.
/// A consumed folio that fails initialization remains non-uptodate for demand
/// retry. Unconsumed reservations are released when the callback returns.
pub struct ReadaheadControl<'a, 'folio> {
    address_space: &'a AddressSpace,
    index: PageIndex,
    folios: &'a mut dyn ExactSizeIterator<Item = &'folio mut Folio>,
}

impl ReadaheadControl<'_, '_> {
    /// Returns the address space whose backing storage should be read.
    pub fn mapping(&self) -> &AddressSpace {
        self.address_space
    }

    /// Returns the index of the next folio to consume.
    pub fn index(&self) -> PageIndex {
        self.index
    }

    /// Returns the number of folios not yet consumed.
    pub fn count(&self) -> usize {
        self.folios.len()
    }

    // Keep the default fallback on the same locked reservations as batch I/O.
    pub(super) fn read_folios(
        &mut self,
        mut read: impl FnMut(&AddressSpace, &mut Folio) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let mut result = Ok(());
        for folio in &mut *self.folios {
            self.index += 1;
            let attempt = read(self.address_space, folio);
            if attempt.is_ok() {
                folio.mark_uptodate();
            }
            // Attempt every reserved page, preserving the first read error.
            result = result.and(attempt);
        }
        result
    }

    /// Consumes the next folio and attempts to initialize it.
    ///
    /// `src` must contain one fully initialized page, including filesystem
    /// supplied zeroes for holes and the EOF tail. No new EOF sample is taken.
    /// The cursor advances even if the supplied page is incomplete; that folio
    /// remains non-uptodate for demand retry. An exhausted cursor is unchanged.
    pub fn complete_folio(&mut self, src: &[u8]) -> VfsResult<()> {
        let folio = self.folios.next().ok_or(VfsError::InvalidInput)?;
        self.index += 1;
        if src.len() != PAGE_SIZE_4K {
            return Err(VfsError::InvalidInput);
        }
        folio.data().copy_from_slice(src);
        folio.mark_uptodate();
        Ok(())
    }
}

impl AddressSpace {
    // Called with invalidate shared held, including EOF sampling, I/O and
    // initialization. Missing runs are installed and locked before filesystem
    // dispatch, so competing demand reads wait on the same resident folios.
    pub(super) fn sync_readahead(&self, offset: u64, count: usize, file_len: u64) -> u64 {
        let end = offset
            .saturating_add(count.min(MAX_READAHEAD_BYTES) as u64)
            .min(file_len);
        let last_index = end.div_ceil(PAGE_SIZE_4K as u64);
        let mut index = offset / PAGE_SIZE_4K as u64;
        while let Some(run) = self.i_pages.next_miss_range(index..last_index) {
            #[cfg(unittest)]
            super::tests::observe_readahead_run(self, &run);
            match self.i_pages.readahead_folios(run.clone(), |folios| {
                let mut control = ReadaheadControl {
                    address_space: self,
                    index: run.start,
                    folios,
                };
                // Consumed failures can be retried; the cache removes the
                // unconsumed suffix when this callback returns.
                let _ = if control.count() == 1 {
                    // A batch boundary must not shorten the window. Initialize
                    // a single reservation directly, without batch staging.
                    control.read_folios(|mapping, folio| self.ops.read_folio(mapping, folio))
                } else {
                    self.ops.readahead(&mut control)
                };
            }) {
                // Only the first candidate was occupied; later misses still need I/O.
                Ok(0) => index = run.start + 1,
                Ok(prepared) => index = run.start + prepared as u64,
                Err(_) => break,
            }
        }
        end
    }
}
