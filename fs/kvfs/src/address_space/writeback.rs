// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Writeback control and filesystem dispatch.

use pagecache::{PageIndex, WritebackBatch, WritebackRangeResult};

use super::AddressSpace;
use crate::{VfsError, VfsResult};

/// Writeback request and authoritative progress for one `writepages` operation.
///
/// Cache algorithms borrow the budget and skip fields in place. Synchronous
/// adapters count completed pages; submission adapters count handed-off pages.
/// Earlier progress remains accounted even if a later write fails.
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
    /// Selects all cached pages for synchronous writeback.
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

    /// Selects cached pages at or after the supplied byte offset.
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

    /// Selects pages intersecting the half-open byte range.
    ///
    /// # Errors
    /// Returns `InvalidInput` if `range_start + len` overflows.
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

    /// Sets the requested writeback synchronization mode.
    pub const fn with_sync_mode(mut self, sync_mode: WritebackSyncMode) -> Self {
        self.sync_mode = sync_mode;
        self
    }

    /// Sets the maximum number of pages to write.
    pub const fn with_nr_to_write(mut self, nr_to_write: usize) -> Self {
        self.nr_to_write = nr_to_write;
        self
    }

    /// Returns the inclusive byte offset at which the range starts.
    pub const fn range_start(self) -> u64 {
        self.range_start
    }

    /// Returns the exclusive byte offset at which the range ends.
    pub const fn range_end(self) -> u64 {
        self.range_end
    }

    /// Returns the requested synchronization mode.
    pub const fn sync_mode(self) -> WritebackSyncMode {
        self.sync_mode
    }

    /// Returns the remaining page budget.
    pub const fn nr_to_write(self) -> usize {
        self.nr_to_write
    }

    /// Returns the number of pages skipped so far.
    pub const fn pages_skipped(self) -> usize {
        self.pages_skipped
    }

    /// Returns whether only data-integrity metadata is required.
    pub const fn is_data_only(self) -> bool {
        self.data_only
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
        self.i_pages.writeback_until(
            self.visible_len()?,
            control.range_start()..control.range_end(),
            &mut control.nr_to_write,
            &mut control.pages_skipped,
            &mut write_folio_fn,
        )
    }

    /// Transfers dirty snapshots to a filesystem submission owner.
    ///
    /// Budget counts submitted folios, not completed folios. The filesystem must
    /// bound outstanding batches, drain before returning from synchronous
    /// writepages, and retain inode/mapping protection through completion. This
    /// helper does not record deferred errors or make a nonblocking pass safe.
    /// Completion and batch destruction require sleepable task context.
    ///
    /// # Errors
    /// Returns scan/submission errors; the owner must still drain earlier I/O.
    pub fn submit_cached_ranges<'a>(
        &'a self,
        control: &mut WritebackControl,
        max_bytes: usize,
        mut submit: impl FnMut(WritebackBatch<'a>) -> VfsResult<()>,
    ) -> VfsResult<()> {
        self.i_pages.submit_cache_pages(
            self.visible_len()?,
            control.range_start()..control.range_end(),
            &mut control.nr_to_write,
            &mut control.pages_skipped,
            max_bytes,
            &mut submit,
        )
    }

    /// Writes cached dirty ranges through the supplied backing-store writer.
    ///
    /// The writer returns `Ok(completed_prefix_bytes)` on success, or
    /// `Err((completed_prefix_bytes, error))` on failure. A failure before any
    /// progress uses a zero prefix. Invalid prefix lengths return `InvalidInput`.
    ///
    /// The writer reports a completed byte prefix even when a later
    /// filesystem transaction fails. Complete folios in that prefix finish
    /// writeback normally; only the boundary folio and suffix are redirtied.
    pub fn writeback_cached_ranges(
        &self,
        control: &mut WritebackControl,
        max_bytes: usize,
        mut write_range_fn: impl FnMut(u64, &[u8]) -> WritebackRangeResult,
    ) -> VfsResult<()> {
        self.i_pages.write_cache_pages(
            self.visible_len()?,
            control.range_start()..control.range_end(),
            &mut control.nr_to_write,
            &mut control.pages_skipped,
            max_bytes,
            &mut write_range_fn,
        )
    }
}
