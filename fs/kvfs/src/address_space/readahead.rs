// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Readahead requests, window selection and filesystem dispatch.

use alloc::sync::Arc;

use memaddr::PAGE_SIZE_4K;
use pagecache::PageIndex;

use super::AddressSpace;
use crate::{VfsError, VfsResult};

const MAX_READAHEAD_BYTES: usize = 128 * 1024;

/// Readahead window for `AddressSpaceOperations::readahead`.
pub struct ReadaheadControl {
    address_space: Arc<AddressSpace>,
    start_index: PageIndex,
    count: usize,
}

impl ReadaheadControl {
    fn new(address_space: Arc<AddressSpace>, start_index: PageIndex, count: usize) -> Self {
        Self {
            address_space,
            start_index,
            count,
        }
    }

    pub fn start_index(&self) -> PageIndex {
        self.start_index
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Inserts one completed readahead folio if it is still absent.
    ///
    /// A foreground cache insertion wins over stale backing data.
    pub fn complete_folio(&self, index: PageIndex, offset: usize, src: &[u8]) -> VfsResult<bool> {
        let count = u64::try_from(self.count).map_err(|_| VfsError::InvalidInput)?;
        let end = self
            .start_index
            .checked_add(count)
            .ok_or(VfsError::InvalidInput)?;
        if index < self.start_index || index >= end {
            return Ok(false);
        }
        self.address_space
            .page_cache
            .filemap_add_folio(index, offset, src)
    }
}

impl AddressSpace {
    pub(super) fn prepare_read_window(
        self: &Arc<Self>,
        offset: u64,
        count: usize,
        file_len: u64,
    ) -> VfsResult<()> {
        let count = u64::try_from(count).map_err(|_| VfsError::InvalidInput)?;
        let remaining =
            usize::try_from((file_len - offset).min(count)).map_err(|_| VfsError::InvalidInput)?;
        let read_len = remaining.min(MAX_READAHEAD_BYTES);
        if read_len == 0 {
            return Ok(());
        }

        let start_index = offset / PAGE_SIZE_4K as u64;
        let end = offset
            .checked_add(read_len as u64)
            .ok_or(VfsError::InvalidInput)?;
        let last_index = end.div_ceil(PAGE_SIZE_4K as u64);
        let count =
            usize::try_from(last_index - start_index).map_err(|_| VfsError::InvalidInput)?;
        if count > 1 && self.page_cache.cached_run_len(start_index, count) < count {
            self.readahead(ReadaheadControl::new(self.clone(), start_index, count))?;
        }
        Ok(())
    }

    /// Starts readahead for a range of folios.
    pub fn readahead(&self, control: ReadaheadControl) -> VfsResult<()> {
        self.ops.readahead(self, control)
    }
}
