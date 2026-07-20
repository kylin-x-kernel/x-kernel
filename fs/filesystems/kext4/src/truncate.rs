// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular-file truncate and orphan cleanup.

use alloc::vec;

use crate::{
    BlockMapping, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Inode, Ext4Result, FilesystemBlock,
    LogicalBlock, UnsupportedKind,
    file::RegularWriteMetadata,
    jbd2::{Journal, JournalCredits, TransactionId},
};

const INODE_UPDATE_CREDITS: u32 = 1;
const ORPHAN_HEAD_UPDATE_CREDITS: u32 = 1;

struct DiskSizeCommitted {
    inode: Ext4Inode,
}

struct TailZeroed {
    inode: Ext4Inode,
}

struct OrphanedInode {
    inode: Ext4Inode,
}

struct ExtentsTruncated {
    inode: Ext4Inode,
}

struct OrphanRemoved {
    inode: Ext4Inode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedTruncateOperation {
    Noop,
    Grow,
    Shrink,
}

/// Prepared regular-file truncate state held between VFS page-cache phases.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4PreparedTruncate {
    inode: Ext4Inode,
    old_disk_size: u64,
    new_disk_size: u64,
    operation: PreparedTruncateOperation,
}

impl Ext4PreparedTruncate {
    /// Returns the inode after the prepare phase.
    pub const fn inode(&self) -> &Ext4Inode {
        &self.inode
    }

    /// Returns the ext4 disk size before prepare.
    pub const fn old_disk_size(&self) -> u64 {
        self.old_disk_size
    }

    /// Returns the ext4 disk size committed by prepare.
    pub const fn new_disk_size(&self) -> u64 {
        self.new_disk_size
    }
}

struct OrphanSizeUpdateCreditPlan;

impl OrphanSizeUpdateCreditPlan {
    const fn credit_limit(self) -> u32 {
        INODE_UPDATE_CREDITS.saturating_add(ORPHAN_HEAD_UPDATE_CREDITS)
    }
}

impl JournalCredits {
    const fn for_orphan_size_update() -> Self {
        Self::new(OrphanSizeUpdateCreditPlan.credit_limit())
    }

    fn for_preallocation_discard(
        filesystem: &Ext4Filesystem,
        inode: &Ext4Inode,
    ) -> Ext4Result<Self> {
        let new_blocks = file_block_count(inode.disk_size(), filesystem.layout().block_size())?;
        Ok(Self::new(filesystem.extent_truncate_metadata_credits(
            inode,
            LogicalBlock::new(new_blocks),
        )?))
    }

    fn for_regular_truncate(
        filesystem: &Ext4Filesystem,
        inode: &Ext4Inode,
        new_size: u64,
    ) -> Ext4Result<Self> {
        let new_blocks = file_block_count(new_size, filesystem.layout().block_size())?;
        let extent_credits =
            filesystem.extent_truncate_metadata_credits(inode, LogicalBlock::new(new_blocks))?;
        let credits = extent_credits
            .checked_add(ORPHAN_HEAD_UPDATE_CREDITS)
            .and_then(|credits| credits.checked_add(INODE_UPDATE_CREDITS))
            .ok_or(Ext4Error::Overflow)?;
        Ok(Self::new(credits))
    }
}

impl Ext4Filesystem {
    /// Prepares a regular-file disk-size change using Linux-style ordering.
    ///
    /// Grow zeroes any mapped old EOF tail before committing a larger
    /// `i_disksize`. Shrink first links the inode on the legacy orphan list
    /// and commits the smaller `i_disksize`; the caller must resize the
    /// generic page cache and then call [`Self::finish_regular_inode_truncate`]
    /// so the extents are truncated and the orphan entry is removed.
    pub fn prepare_regular_inode_truncate(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        timestamp: crate::Ext4Timestamp,
    ) -> Ext4Result<Ext4PreparedTruncate> {
        self.ensure_regular_file_mutation_supported(inode)?;
        let old_disk_size = inode.disk_size();
        if new_size == old_disk_size {
            return Ok(Ext4PreparedTruncate {
                inode: inode.clone(),
                old_disk_size,
                new_disk_size: new_size,
                operation: PreparedTruncateOperation::Noop,
            });
        }
        if new_size > old_disk_size {
            self.zero_regular_inode_tail_data(inode, old_disk_size)?;
            let inode = self.commit_regular_inode_write_metadata(
                inode,
                new_size,
                RegularWriteMetadata::Full { timestamp },
            )?;
            return Ok(Ext4PreparedTruncate {
                inode,
                old_disk_size,
                new_disk_size: new_size,
                operation: PreparedTruncateOperation::Grow,
            });
        }

        let committed = self.commit_orphan_size_update(inode, new_size, timestamp)?;
        Ok(Ext4PreparedTruncate {
            inode: committed.inode,
            old_disk_size,
            new_disk_size: new_size,
            operation: PreparedTruncateOperation::Shrink,
        })
    }

    /// Finishes a prepared regular-file disk-size change after page-cache work.
    pub fn finish_regular_inode_truncate(
        &mut self,
        prepared: Ext4PreparedTruncate,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_regular_file_mutation_supported(&prepared.inode)?;
        match prepared.operation {
            PreparedTruncateOperation::Noop | PreparedTruncateOperation::Grow => Ok(prepared.inode),
            PreparedTruncateOperation::Shrink => {
                self.truncate_regular_inode_shrink_committed(prepared.inode, prepared.new_disk_size)
            }
        }
    }

    /// Changes a regular file's visible length using Linux-style orphan protection.
    pub fn truncate_regular_inode(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        timestamp: crate::Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        let prepared = self.prepare_regular_inode_truncate(inode, new_size, timestamp)?;
        self.finish_regular_inode_truncate(prepared)
    }

    /// Discards unwritten preallocation beyond a regular file's `i_disksize`.
    pub fn discard_regular_inode_preallocations(
        &mut self,
        inode: &Ext4Inode,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_regular_file_mutation_supported(inode)?;
        let disk_blocks = file_block_count(inode.disk_size(), self.layout().block_size())?;
        if !self.has_extent_mapping_from(inode, LogicalBlock::new(disk_blocks))? {
            return Ok(inode.clone());
        }

        let credits = JournalCredits::for_preallocation_discard(self, inode)?;
        let journal = self.metadata_journal()?;
        let mut handle = journal.begin(credits)?;
        let transaction = handle.id();
        let updated = match self.truncate_inode_mappings_to(
            TailZeroed {
                inode: inode.clone(),
            },
            inode.disk_size(),
            &mut handle,
        ) {
            Ok(truncated) => truncated.inode,
            Err(error) => {
                drop(handle);
                return Err(self.abort_truncate_transaction(&journal, error));
            }
        };
        if let Err(error) = handle.mark_inode_sync(updated.number()) {
            drop(handle);
            return Err(self.abort_truncate_transaction(&journal, error));
        }
        drop(handle);
        self.commit_truncate_transaction(journal, transaction)?;
        Ok(updated)
    }

    pub(crate) fn cleanup_regular_file_orphan_from_head(
        &mut self,
        inode: &Ext4Inode,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<Ext4Inode> {
        if self.orphan_head() != Some(inode.number()) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.ensure_regular_file_mutation_supported(inode)?;
        if inode.links_count() == 0 {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::UnlinkedOrphanCleanup,
            ));
        }

        let target_size = inode.disk_size();
        let tail_zeroed = self.zero_committed_inode_tail(
            DiskSizeCommitted {
                inode: inode.clone(),
            },
            target_size,
        )?;

        let credits = JournalCredits::for_regular_truncate(self, inode, target_size)?;
        let journal = self.metadata_journal()?;
        let mut handle = journal.begin(credits)?;
        let transaction = handle.id();
        let updated_inode = match self.cleanup_orphaned_inode_from_head_metadata(
            tail_zeroed,
            target_size,
            &mut handle,
        ) {
            Ok(updated_inode) => updated_inode,
            Err(error) => {
                drop(handle);
                return Err(self.abort_truncate_transaction(&journal, error));
            }
        };
        if let Err(error) = handle.mark_inode_sync(inode.number()) {
            drop(handle);
            return Err(self.abort_truncate_transaction(&journal, error));
        }
        drop(handle);
        self.commit_truncate_transaction_with_policy(journal, transaction, recovery_flag_policy)?;
        Ok(updated_inode)
    }

    fn truncate_regular_inode_shrink_committed(
        &mut self,
        inode: Ext4Inode,
        new_size: u64,
    ) -> Ext4Result<Ext4Inode> {
        let credits = JournalCredits::for_regular_truncate(self, &inode, new_size)?;
        let tail_zeroed = self.zero_committed_inode_tail(DiskSizeCommitted { inode }, new_size)?;

        let journal = self.metadata_journal()?;
        let mut handle = journal.begin(credits)?;
        let transaction = handle.id();
        let updated_inode =
            match self.finish_regular_inode_shrink_metadata(tail_zeroed, new_size, &mut handle) {
                Ok(updated_inode) => updated_inode,
                Err(error) => {
                    drop(handle);
                    return Err(self.abort_truncate_transaction(&journal, error));
                }
            };
        if let Err(error) = handle.mark_inode_sync(updated_inode.number()) {
            drop(handle);
            return Err(self.abort_truncate_transaction(&journal, error));
        }
        drop(handle);

        self.commit_truncate_transaction(journal, transaction)?;
        Ok(updated_inode)
    }

    fn commit_orphan_size_update(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        timestamp: crate::Ext4Timestamp,
    ) -> Ext4Result<DiskSizeCommitted> {
        let journal = self.metadata_journal()?;
        let mut handle = journal.begin(JournalCredits::for_orphan_size_update())?;
        let transaction = handle.id();
        let updated_inode =
            match self
                .add_truncate_orphan(inode, &mut handle)
                .and_then(|orphaned| {
                    self.commit_orphaned_disk_size(
                        orphaned,
                        new_size,
                        RegularWriteMetadata::Full { timestamp },
                        &mut handle,
                    )
                }) {
                Ok(committed) => committed.inode,
                Err(error) => {
                    drop(handle);
                    return Err(self.abort_truncate_transaction(&journal, error));
                }
            };
        if let Err(error) = handle.mark_inode_sync(inode.number()) {
            drop(handle);
            return Err(self.abort_truncate_transaction(&journal, error));
        }
        drop(handle);

        self.commit_truncate_transaction(journal, transaction)?;
        Ok(DiskSizeCommitted {
            inode: updated_inode,
        })
    }

    fn add_truncate_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<OrphanedInode> {
        Ok(OrphanedInode {
            inode: self.add_orphan(inode, handle)?,
        })
    }

    fn commit_orphaned_disk_size(
        &self,
        orphaned: OrphanedInode,
        new_size: u64,
        metadata: RegularWriteMetadata,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<DiskSizeCommitted> {
        Ok(DiskSizeCommitted {
            inode: self.update_regular_inode_size_metadata(
                &orphaned.inode,
                new_size,
                metadata,
                handle,
            )?,
        })
    }

    fn zero_committed_inode_tail(
        &self,
        committed: DiskSizeCommitted,
        size: u64,
    ) -> Ext4Result<TailZeroed> {
        self.zero_regular_inode_tail_data(&committed.inode, size)?;
        Ok(TailZeroed {
            inode: committed.inode,
        })
    }

    fn finish_regular_inode_shrink_metadata(
        &mut self,
        tail_zeroed: TailZeroed,
        new_size: u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let truncated = self.truncate_inode_mappings_to(tail_zeroed, new_size, handle)?;
        Ok(self.remove_truncated_orphan(truncated, handle)?.inode)
    }

    fn cleanup_orphaned_inode_from_head_metadata(
        &mut self,
        tail_zeroed: TailZeroed,
        target_size: u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let truncated = self.truncate_inode_mappings_to(tail_zeroed, target_size, handle)?;
        Ok(self.remove_truncated_orphan(truncated, handle)?.inode)
    }

    fn truncate_inode_mappings_to(
        &mut self,
        tail_zeroed: TailZeroed,
        size: u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<ExtentsTruncated> {
        let blocks = file_block_count(size, self.layout().block_size())?;
        Ok(ExtentsTruncated {
            inode: self.truncate_extent_mappings(
                &tail_zeroed.inode,
                LogicalBlock::new(blocks),
                handle,
            )?,
        })
    }

    fn remove_truncated_orphan(
        &mut self,
        truncated: ExtentsTruncated,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<OrphanRemoved> {
        Ok(OrphanRemoved {
            inode: self.remove_orphan(&truncated.inode, handle)?,
        })
    }

    fn zero_regular_inode_tail_data(&self, inode: &Ext4Inode, new_size: u64) -> Ext4Result<()> {
        let block_size = u64::from(self.layout().block_size());
        let tail = new_size % block_size;
        if new_size == 0 || tail == 0 {
            return Ok(());
        }

        let logical = LogicalBlock::new(new_size / block_size);
        let in_block = usize::try_from(tail).map_err(|_| Ext4Error::Overflow)?;
        let block_size_usize =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        match self.map_blocks(inode, logical)? {
            BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => Ok(()),
            BlockMapping::Mapped { physical, len } => {
                if physical.get() == 0 || len.get() == 0 {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                }
                let mut bytes = vec![0; block_size_usize];
                self.read_blocks(FilesystemBlock::new(physical.get()), 1, &mut bytes)?;
                bytes
                    .get_mut(in_block..)
                    .ok_or(Ext4Error::OutOfBounds)?
                    .fill(0);
                // R6 has a synchronous ordered-data baseline: data reaches
                // storage before the later metadata transaction can commit.
                // Full Linux data=ordered completion tracking belongs in the
                // later page I/O/JBD2 integration.
                self.write_contiguous_blocks(FilesystemBlock::new(physical.get()), 1, &bytes)?;
                self.flush_device()
            }
        }
    }

    fn commit_truncate_transaction(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        self.commit_truncate_transaction_with_policy(
            journal,
            transaction,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    fn commit_truncate_transaction_with_policy(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.commit_metadata_transaction_with_policy(journal, transaction, recovery_flag_policy)
    }

    fn abort_truncate_transaction(&mut self, journal: &Journal, error: Ext4Error) -> Ext4Error {
        if let Some(undo) = journal.abort(error)
            && let Err(rollback_error) = self.rollback_metadata_undo(&undo)
        {
            return rollback_error;
        }
        error
    }
}

fn file_block_count(size: u64, block_size: u32) -> Ext4Result<u64> {
    if size == 0 {
        return Ok(0);
    }
    let block_size = u64::from(block_size);
    size.checked_add(block_size - 1)
        .ok_or(Ext4Error::Overflow)?
        .checked_div(block_size)
        .ok_or(Ext4Error::Overflow)
}
