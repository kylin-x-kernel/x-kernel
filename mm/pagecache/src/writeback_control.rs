// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Policy and attempted-work accounting for one writeback invocation.

use kerrno::{KError, KResult};

/// Writeback request and authoritative progress for one `writepages` operation.
///
/// Every adapter charges prepared pages once when handed to filesystem work,
/// including failures and admission declines. Completion never refunds budget.
/// Filesystem declines are accounted separately from busy cache candidates.
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
    /// Skip existing writeback; stop at budget exhaustion or the first error.
    None,
    /// Wait/revalidate existing writeback and attempt the finite selected set.
    /// After starting, continue despite exhausted budget or immediate errors.
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
    pub fn range(range_start: u64, len: usize) -> KResult<Self> {
        let range_end = range_start
            .checked_add(len as u64)
            .ok_or(KError::InvalidInput)?;
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

    /// Sets the attempted-page budget. ALL continues its finite selection
    /// after exhausting a nonzero initial budget.
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

    /// Accounts filesystem-declined pages before returning from submission.
    /// This does not refund the attempted-work budget.
    pub fn account_skipped(&mut self, pages: usize) {
        self.pages_skipped = self.pages_skipped.saturating_add(pages);
    }

    pub(crate) fn charge(&mut self, pages: usize) {
        self.nr_to_write = self.nr_to_write.saturating_sub(pages);
    }
}
