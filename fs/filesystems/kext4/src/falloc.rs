// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular-file fallocate preallocation.

use kvfs::SuperBlock;

use crate::{
    BlockCount, BlockMapping, CorruptKind, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo,
    LogicalBlock,
    disk::extent::EXT_UNWRITTEN_MAX_LEN,
    extent::ExtentMappingState,
    file::RegularWriteMetadata,
    jbd2::JournalCredits,
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    truncate::file_block_count,
    vfs::Ext4Mount,
};

// One batch journals an allocator run plus the insert and inode-publish
// mutations the ordered-writeback credit model charges for a hole allocation.
const FALLOCATE_CREDIT_ALLOCATION_RUNS: u32 = 1;
const FALLOCATE_CREDIT_EXTENT_MUTATIONS: u32 = 2;

impl Ext4Mount {
    /// Preallocates unwritten extents for `[offset, offset + len)`, skipping
    /// already-mapped ranges so repeated calls are idempotent.
    ///
    /// - `super_block`: mount that owns this core state.
    /// - `inode`: regular file whose extent tree is updated.
    /// - `offset`: range start in bytes.
    /// - `len`: range length in bytes.
    /// - `keep_size`: leaves visible size and `i_disksize` untouched so the
    ///   reservation survives the final writable close.
    /// - `timestamp`: caller-supplied timestamp for the ctime/mtime refresh.
    /// - `committed_end`: out-param, the end offset in bytes the caller must
    ///   publish as the visible size: `end` on success, the successfully
    ///   allocated prefix on a partial failure, or `0` when nothing should be
    ///   published (KEEP_SIZE or a first-batch failure).
    ///
    /// Returns: `()` once every hole batch is committed and, unless
    /// `keep_size` is set, the size update is committed after the extents are
    /// durable (Linux `ext4_alloc_file_blocks` ordering).
    ///
    /// Errors: `NoSpace` from a batch propagates after advancing `i_disksize`
    /// to the successfully allocated prefix (capped at `offset + len`, never
    /// shrinking) and refreshing ctime/mtime — earlier batches stay committed
    /// as unwritten preallocation beyond EOF and `committed_end` reports that
    /// prefix; a first-batch failure leaves the size untouched. Other errors
    /// behave the same.
    ///
    /// Locking: the caller holds the inode data and address-space invalidate
    /// locks, serializing this inode's mutations; the mount write lock is taken
    /// only for one batch at a time plus the size commits, not across the whole
    /// sweep. Commits journal transactions and may sleep.
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: clippy::too_many_arguments: fallocate passes the full \
                  request context; a params struct would obscure the call sites"
    )]
    pub(crate) fn fallocate_regular_inode_preallocate(
        &self,
        super_block: &SuperBlock,
        inode: &Ext4Inode,
        offset: u64,
        len: u64,
        keep_size: bool,
        timestamp: crate::Ext4Timestamp,
        committed_end: &mut u64,
    ) -> Ext4Result<()> {
        let end = offset.checked_add(len).ok_or(Ext4Error::Overflow)?;
        let (block_size, start_lblk, end_lblk, old_size, extend_size) = {
            let state = self.read();
            state.ensure_regular_file_mutation_supported(inode)?;
            let block_size = state.layout().block_size();
            let start_lblk = offset / u64::from(block_size);
            let end_lblk = file_block_count(end, block_size)?;
            let old_size = inode.size().max(inode.disk_size());
            let extend_size = !keep_size && end > old_size;
            (block_size, start_lblk, end_lblk, old_size, extend_size)
        };

        if extend_size {
            // Zero the old EOF tail before the extension exposes it; hole and
            // unwritten tails already read as zeroes.
            self.write(super_block)
                .zero_regular_inode_tail_data(inode, old_size)?;
        }

        let max_batch = u64::from(u32::from(EXT_UNWRITTEN_MAX_LEN));
        let mut logical = start_lblk;
        let mut error = None;
        while error.is_none() && logical < end_lblk {
            let mapping = match self.read().map_blocks(inode, LogicalBlock::new(logical)) {
                Ok(mapping) => mapping,
                Err(err) => {
                    error = Some(err);
                    break;
                }
            };
            match mapping {
                BlockMapping::Mapped { len, .. } | BlockMapping::Unwritten { len, .. } => {
                    match logical.checked_add(u64::from(len.get())) {
                        Some(next) => logical = next,
                        None => error = Some(Ext4Error::Overflow),
                    }
                }
                BlockMapping::Hole { len, .. } => {
                    // `logical < end_lblk` holds here, so the subtraction stays in range.
                    let want = u64::from(len.get()).min(end_lblk - logical).min(max_batch);
                    let Ok(want) = u32::try_from(want) else {
                        error = Some(Ext4Error::Overflow);
                        break;
                    };
                    let allocated = match self.write(super_block).fallocate_hole_batch(
                        inode,
                        LogicalBlock::new(logical),
                        BlockCount::new(want),
                    ) {
                        Ok(allocated) => allocated,
                        Err(err) => {
                            error = Some(err);
                            break;
                        }
                    };
                    if allocated == 0 {
                        error = Some(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                        break;
                    }
                    match logical.checked_add(u64::from(allocated)) {
                        Some(next) => logical = next,
                        None => error = Some(Ext4Error::Overflow),
                    }
                }
            }
        }

        match error {
            None => {
                if extend_size {
                    self.write(super_block)
                        .commit_regular_inode_write_metadata(
                            inode,
                            end,
                            RegularWriteMetadata::Full { timestamp },
                        )?;
                } else {
                    // KEEP_SIZE still refreshes ctime/mtime (Linux file_modified()) but
                    // must not advance `i_disksize` past the committed visible state.
                    self.write(super_block)
                        .commit_regular_inode_write_metadata(
                            inode,
                            inode.disk_size(),
                            RegularWriteMetadata::Full { timestamp },
                        )?;
                }
                *committed_end = if keep_size { 0 } else { end };
                Ok(())
            }
            Some(error) => {
                // Settle the committed prefix and refresh timestamps before
                // returning the error (Linux `min(epos, new_size)`); nothing
                // processed leaves the size untouched (`if (!epos) return ret`).
                let has_progress = !keep_size && logical > start_lblk;
                let settled_end = logical.saturating_mul(u64::from(block_size)).min(end);
                if has_progress && settled_end > old_size {
                    self.write(super_block)
                        .commit_regular_inode_write_metadata(
                            inode,
                            settled_end,
                            RegularWriteMetadata::Full { timestamp },
                        )?;
                } else {
                    self.write(super_block)
                        .commit_regular_inode_write_metadata(
                            inode,
                            inode.disk_size(),
                            RegularWriteMetadata::Full { timestamp },
                        )?;
                }
                *committed_end = if has_progress { settled_end } else { 0 };
                Err(error)
            }
        }
    }
}

impl Ext4SbInfo {
    /// Allocates and inserts one unwritten-extent batch in a single journal
    /// transaction.
    ///
    /// - `inode`: regular file receiving the mapping.
    /// - `logical`: first logical block of the batch.
    /// - `expected`: requested block count in blocks; `ALLOW_PARTIAL` may
    ///   serve fewer.
    ///
    /// Returns: the number of blocks actually allocated and mapped; may be
    /// below `expected`.
    ///
    /// Errors: `NoSpace` when the allocator cannot serve the request; journal
    /// errors propagate unchanged.
    ///
    /// Locking: same context as the caller (ext4 write lock held); opens and
    /// completes its own journal transaction, so it may sleep.
    fn fallocate_hole_batch(
        &mut self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
        expected: BlockCount,
    ) -> Ext4Result<u32> {
        let credits = JournalCredits::new(self.extent_writeback_metadata_credits(
            inode,
            FALLOCATE_CREDIT_ALLOCATION_RUNS,
            FALLOCATE_CREDIT_EXTENT_MUTATIONS,
        )?);
        let credits = self.quota_metadata_credits(credits);
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let goal = self.allocation_goal_after_previous_extent(inode, logical)?;
        let request = Ext4AllocationRequest::new(
            inode,
            logical,
            goal,
            expected,
            BlockCount::new(1),
            Ext4AllocationFlags::ALLOW_PARTIAL,
        )?;
        let result = self
            .allocate_blocks_for_write(request, &mut handle)
            .and_then(|allocation| {
                self.insert_extent_mapping(
                    inode,
                    logical,
                    allocation.physical_start(),
                    allocation.block_count(),
                    ExtentMappingState::Unwritten,
                    &mut handle,
                )
                .map(|()| allocation.block_count().get())
            });
        self.complete_metadata_mutation(handle, result)
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(component_tests, "../ut-tests/falloc/component_tests.rs");
