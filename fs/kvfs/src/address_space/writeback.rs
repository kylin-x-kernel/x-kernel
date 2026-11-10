// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Writeback control and filesystem dispatch.

use core::ops::Range;

use pagecache::{
    OwnedWritebackCompletion, PageIndex, WritebackBatch, WritebackCompletion, WritebackRangeResult,
};

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
}

/// Whether a writeback pass is opportunistic or data-integrity oriented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritebackSyncMode {
    /// Start writeback without requiring all matching folios to be written.
    None,
    /// Submit matching dirty folios for a data-integrity operation.
    All,
}

impl WritebackControl {
    /// Selects all cached pages for data-integrity submission.
    pub const fn all() -> Self {
        Self {
            range_start: 0,
            range_end: u64::MAX,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
        }
    }

    /// Selects cached pages at or after the supplied byte offset.
    pub const fn from(range_start: u64) -> Self {
        Self {
            range_start,
            range_end: u64::MAX,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
        }
    }

    /// Selects pages intersecting the half-open byte range.
    ///
    /// # Errors
    /// Returns `InvalidInput` if `range_start + len` overflows.
    pub fn range(range_start: u64, len: usize) -> VfsResult<Self> {
        let range_end = range_start
            .checked_add(len as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(Self {
            range_start,
            range_end,
            sync_mode: WritebackSyncMode::All,
            nr_to_write: usize::MAX,
            pages_skipped: 0,
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
}

impl AddressSpace {
    /// Retains this address space until asynchronous folio completion finishes.
    /// The embedded page index keeps the same owner throughout writeback.
    ///
    /// # Errors
    /// Returns `InvalidInput` if the inode is gone or the batch belongs elsewhere.
    pub fn retain_writeback_completion(
        &self,
        completion: WritebackCompletion<'_>,
    ) -> VfsResult<OwnedWritebackCompletion<Self>> {
        let owner = self.inode().ok_or(VfsError::InvalidInput)?.address_space();
        completion.into_owned(owner, |mapping| &mapping.i_pages)
    }

    /// Submits dirty pages belonging to this address space.
    /// Return alone does not imply data completion or filesystem persistence.
    pub fn writepages(&self) -> VfsResult<()> {
        let mut control = WritebackControl::all();
        self.writepages_control(&mut control)
    }

    /// Submits dirty pages intersecting `[start, start + len)`.
    pub fn writepages_range(&self, start: u64, len: usize) -> VfsResult<()> {
        let mut control = WritebackControl::range(start, len)?;
        self.writepages_control(&mut control)
    }

    /// Submits dirty pages from `start` through EOF.
    pub fn writepages_from(&self, start: u64) -> VfsResult<()> {
        let mut control = WritebackControl::from(start);
        self.writepages_control(&mut control)
    }

    /// Submits dirty pages according to an explicit control object.
    /// Requires sleepable task context; no current process is needed.
    /// Zero budget and empty ranges bypass filesystem dispatch.
    /// Inline completion is allowed but not required.
    /// The invalidate guard covers this call only. An asynchronous filesystem
    /// must independently protect mappings through completion and drain them
    /// before truncate or release.
    pub fn writepages_control(&self, control: &mut WritebackControl) -> VfsResult<()> {
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        if control.nr_to_write() == 0 || control.range_start() == control.range_end() {
            return Ok(());
        }
        let _invalidate_guard = self.invalidate_lock.read();
        self.ops.writepages(self, control)
    }

    /// Submits and waits for data intersecting a half-open byte range.
    ///
    /// Captures a finite resident-page endpoint before submission, including
    /// clean pages with earlier writeback. Both submission and waiting use that
    /// endpoint, so later appends cannot extend this operation. Earlier I/O is
    /// still waited after a submission failure. This reports errors recorded
    /// during the operation without advancing any open-file error cursor.
    /// Filesystem metadata, journal commits and device flushes belong to fsync.
    ///
    /// Requires sleepable task context without spinlocks or IRQ/preemption
    /// guards; no current process is required. No index or invalidate lock is
    /// retained while waiting for completion.
    ///
    /// # Errors
    /// Returns `InvalidInput` for a backwards range or nonsleepable context,
    /// otherwise the first submission, wait or newly recorded writeback error.
    pub fn write_and_wait_range(&self, range: Range<u64>) -> VfsResult<()> {
        self.write_and_wait_range_with(range, |control| self.writepages_control(control))
    }

    /// Shares data completion with callers already holding invalidate exclusion.
    ///
    /// The caller supplies submission admission only; target capture, waiting and
    /// error precedence remain one algorithm. Completion must not require the
    /// caller's retained locks.
    pub(super) fn write_and_wait_range_with(
        &self,
        range: Range<u64>,
        submit: impl FnOnce(&mut WritebackControl) -> VfsResult<()>,
    ) -> VfsResult<()> {
        if range.start > range.end {
            return Err(VfsError::InvalidInput);
        }
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        let generation = self.writeback_errors.current_generation();
        let target = self.i_pages.capture_writeback_range(range)?;
        let submitted = if target.is_empty() {
            Ok(())
        } else {
            let mut control = WritebackControl {
                range_start: target.start,
                range_end: target.end,
                ..WritebackControl::all()
            };
            submit(&mut control)
        };
        let waited = self.i_pages.wait_on_writeback_range(target);
        let recorded = self.writeback_errors.check(generation);
        submitted.and(waited).and(recorded)
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
    /// bound outstanding batches, retain inode/mapping protection through
    /// completion, and drain before releasing backing mappings. This
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
