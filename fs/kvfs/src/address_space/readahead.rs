// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Readahead requests, window selection and filesystem dispatch.

use memaddr::PAGE_SIZE_4K;
use pagecache::{FolioRead, PageIndex};

use super::AddressSpace;
use crate::{VfsError, VfsResult};

const MAX_FAULT_READAHEAD_PAGES: usize = 16;

/// A readahead submission cursor over locked, resident, uninitialized folios.
///
/// The address space lends this control to the filesystem while holding its
/// invalidate lock for reading. Consumed tokens can outlive the callback;
/// they retain their allocation and folio lock until completion. Truncation
/// waits on that lock before modifying or removing a submitted page.
/// A consumed folio that fails initialization remains non-uptodate for demand
/// retry. Unconsumed reservations are released when the callback returns.
pub struct ReadaheadControl<'a> {
    address_space: &'a AddressSpace,
    index: PageIndex,
    folios: &'a mut dyn ExactSizeIterator<Item = FolioRead>,
}

impl ReadaheadControl<'_> {
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
        mut read: impl FnMut(&AddressSpace, FolioRead) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let mut result = Ok(());
        while let Some(folio) = self.next_folio() {
            let attempt = read(self.address_space, folio);
            // Attempt every reserved page, preserving the first read error.
            result = result.and(attempt);
        }
        result
    }

    /// Transfers the next folio's initialization responsibility to the caller.
    ///
    /// The returned owner must be completed or dropped after I/O releases the
    /// page. A deferred completion must not need the inode data lock, invalidate
    /// lock or mapping index: truncation may hold those while waiting for it.
    pub fn next_folio(&mut self) -> Option<FolioRead> {
        let folio = self.folios.next()?;
        self.index += 1;
        Some(folio)
    }

    /// Consumes the next folio and attempts to initialize it.
    ///
    /// `src` must contain one fully initialized page, including filesystem
    /// supplied zeroes for holes and the EOF tail. No new EOF sample is taken.
    /// The cursor advances even if the supplied page is incomplete; that folio
    /// remains non-uptodate for demand retry. An exhausted cursor is unchanged.
    pub fn complete_folio(&mut self, src: &[u8]) -> VfsResult<()> {
        let mut folio = self.next_folio().ok_or(VfsError::InvalidInput)?;
        if src.len() != PAGE_SIZE_4K {
            return Err(VfsError::InvalidInput);
        }
        folio.data().copy_from_slice(src);
        folio.complete(Ok(()))
    }
}

impl AddressSpace {
    /// Prefetches a bounded window of missing file-cache pages for a private
    /// read fault.
    ///
    /// At most 16 pages starting at the page containing `file_offset` are
    /// selected below both the inode's visible length and the optional
    /// exclusive `file_end` (used by loaders to bound a PT_LOAD file prefix).
    /// A cached target performs no work. Only missing runs are submitted to the
    /// backing `readahead` operation; cached pages are never overwritten and no
    /// PTE is installed. This is an optional optimization: a returned error
    /// must not replace the caller's ordinary demand read.
    ///
    /// The invalidate lock is held for reading across size selection and
    /// submission. The page-cache index lock is not held across backend I/O.
    pub fn readahead_for_fault(&self, file_offset: u64, file_end: Option<u64>) -> VfsResult<()> {
        let start_index = file_offset / PAGE_SIZE_4K as u64;
        if self
            .i_pages
            .next_miss_range(start_index..start_index.saturating_add(1))
            .is_none()
        {
            return Ok(());
        }

        let _invalidate_guard = self.invalidate_lock.read();
        let file_len = self.visible_len()?.min(file_end.unwrap_or(u64::MAX));
        if file_offset >= file_len {
            return Ok(());
        }
        let end_index = start_index
            .saturating_add(MAX_FAULT_READAHEAD_PAGES as u64)
            .min(file_len.div_ceil(PAGE_SIZE_4K as u64));
        self.prepare_readahead(start_index..end_index, None, file_len);
        Ok(())
    }

    // Called with invalidate shared held through EOF sampling and submission.
    // Completion may follow later under the transferred folio lock. Missing
    // runs are resident before dispatch, so competing reads wait on those folios.
    pub(super) fn prepare_readahead(
        &self,
        range: core::ops::Range<PageIndex>,
        marker: Option<PageIndex>,
        file_len: u64,
    ) {
        let last_index = range.end.min(file_len.div_ceil(PAGE_SIZE_4K as u64));
        let mut index = range.start;
        while let Some(run) = self.i_pages.next_miss_range(index..last_index) {
            #[cfg(unittest)]
            super::tests::observe_readahead_run(self, &run);
            match self.i_pages.readahead_folios(run.clone(), |folios| {
                let mut marked = folios.map(|mut folio| {
                    if Some(folio.index()) == marker {
                        folio.mark_readahead();
                    }
                    folio
                });
                let mut control = ReadaheadControl {
                    address_space: self,
                    index: run.start,
                    folios: &mut marked,
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
    }
}

#[cfg(unittest)]
impl AddressSpace {
    pub(super) fn sync_readahead(&self, offset: u64, count: usize, file_len: u64) -> u64 {
        let end = offset.saturating_add(count as u64).min(file_len);
        self.prepare_readahead(
            offset / PAGE_SIZE_4K as u64..end.div_ceil(PAGE_SIZE_4K as u64),
            None,
            file_len,
        );
        end
    }
}
