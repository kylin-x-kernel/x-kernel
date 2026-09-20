// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Writeback control and filesystem dispatch.

use pagecache::{PageIndex, WritebackRangeOutcome, WritebackStats};

use super::AddressSpace;
use crate::{VfsError, VfsResult};

/// Writeback range and mode for `AddressSpaceOperations::writepages`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WritebackControl {
    range_start: u64,
    range_end: u64,
    sync_mode: WritebackSyncMode,
    nr_to_write: usize,
    pages_skipped: usize,
    data_only: bool,
}

/// Whether a writeback pass is opportunistic or data-integrity oriented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritebackSyncMode {
    /// Start writeback without requiring all matching folios to be written.
    None,
    /// Write every matching dirty folio that can be written synchronously.
    All,
}

impl WritebackControl {
    pub const fn all(data_only: bool) -> Self {
        Self {
            range_start: 0,
            range_end: u64::MAX,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
            data_only,
        }
    }

    pub const fn from(range_start: u64, data_only: bool) -> Self {
        Self {
            range_start,
            range_end: u64::MAX,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
            data_only,
        }
    }

    pub fn range(range_start: u64, len: usize, data_only: bool) -> VfsResult<Self> {
        let range_end = range_start
            .checked_add(len as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(Self {
            range_start,
            range_end,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
            data_only,
        })
    }

    pub const fn with_sync_mode(mut self, sync_mode: WritebackSyncMode) -> Self {
        self.sync_mode = sync_mode;
        self
    }

    pub const fn with_nr_to_write(mut self, nr_to_write: usize) -> Self {
        self.nr_to_write = nr_to_write;
        self
    }

    pub const fn range_start(self) -> u64 {
        self.range_start
    }

    pub const fn range_end(self) -> u64 {
        self.range_end
    }

    pub const fn sync_mode(self) -> WritebackSyncMode {
        self.sync_mode
    }

    pub const fn nr_to_write(self) -> usize {
        self.nr_to_write
    }

    pub const fn pages_skipped(self) -> usize {
        self.pages_skipped
    }

    pub const fn is_data_only(self) -> bool {
        self.data_only
    }

    fn account_stats(&mut self, stats: WritebackStats) {
        self.nr_to_write = self.nr_to_write.saturating_sub(stats.pages_written);
        self.pages_skipped = self.pages_skipped.saturating_add(stats.pages_skipped);
    }
}

impl AddressSpace {
    /// Writes dirty pages belonging to this address space.
    pub fn writepages(&self, data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::all(data_only);
        self.writepages_control(&mut control)
    }

    /// Writes dirty pages intersecting `[start, start + len)`.
    pub fn writepages_range(&self, start: u64, len: usize, data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::range(start, len, data_only)?;
        self.writepages_control(&mut control)
    }

    /// Writes dirty pages from `start` through EOF.
    pub fn writepages_from(&self, start: u64, data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::from(start, data_only);
        self.writepages_control(&mut control)
    }

    /// Writes dirty pages according to an explicit control object.
    pub fn writepages_control(&self, control: &mut WritebackControl) -> VfsResult<()> {
        let _invalidate_guard = self.invalidate_lock.read();
        self.ops.writepages(self, control)
    }

    pub(crate) fn writeback_cached_folios(
        &self,
        control: &mut WritebackControl,
        mut write_folio_fn: impl FnMut(PageIndex, &[u8], usize) -> VfsResult<()>,
    ) -> VfsResult<()> {
        let stats = self.page_cache.writeback_until(
            self.visible_len()?,
            control.range_start(),
            control.range_end(),
            control.nr_to_write(),
            &mut write_folio_fn,
        )?;
        control.account_stats(stats);
        Ok(())
    }

    /// Writes cached dirty ranges through the supplied backing-store writer.
    ///
    /// The writer reports a completed byte prefix even when a later
    /// filesystem transaction fails. Complete folios in that prefix finish
    /// writeback normally; only the boundary folio and suffix are redirtied.
    pub fn writeback_cached_ranges(
        &self,
        control: &mut WritebackControl,
        max_bytes: usize,
        mut write_range_fn: impl FnMut(u64, &[u8]) -> VfsResult<WritebackRangeOutcome>,
    ) -> VfsResult<()> {
        let outcome = self.page_cache.write_cache_pages(
            self.visible_len()?,
            control.range_start(),
            control.range_end(),
            control.nr_to_write(),
            max_bytes,
            &mut write_range_fn,
        )?;
        control.account_stats(outcome.stats());
        outcome.error().map_or(Ok(()), Err)
    }
}
