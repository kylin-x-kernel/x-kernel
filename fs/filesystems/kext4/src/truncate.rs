// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular-file truncate and orphan cleanup.

use alloc::vec;

use crate::{
    BlockMapping, CorruptKind, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, FilesystemBlock,
    LogicalBlock, UnsupportedKind, file::RegularWriteMetadata, jbd2::JournalCredits,
};

const INODE_UPDATE_CREDITS: u32 = 1;
const ORPHAN_HEAD_UPDATE_CREDITS: u32 = 1;

impl JournalCredits {
    const fn for_orphan_size_update() -> Self {
        Self::new(INODE_UPDATE_CREDITS.saturating_add(ORPHAN_HEAD_UPDATE_CREDITS))
    }

    fn for_preallocation_discard(filesystem: &Ext4SbInfo, inode: &Ext4Inode) -> Ext4Result<Self> {
        let new_blocks = file_block_count(inode.disk_size(), filesystem.layout().block_size())?;
        Ok(Self::new(filesystem.extent_truncate_metadata_credits(
            inode,
            LogicalBlock::new(new_blocks),
        )?))
    }

    fn for_regular_truncate(
        filesystem: &Ext4SbInfo,
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

impl Ext4SbInfo {
    /// Prepares a regular-file disk-size change using Linux-style ordering.
    ///
    /// Grow zeroes any mapped old EOF tail before committing a larger
    /// `i_disksize`. Shrink first links the inode on the legacy orphan list
    /// and commits the smaller `i_disksize`; the caller must resize the
    /// generic page cache and then call [`Self::finish_regular_inode_shrink`]
    /// so the extents are truncated and the orphan entry is removed.
    pub fn prepare_regular_inode_truncate(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        timestamp: crate::Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.ensure_regular_file_mutation_supported(inode)?;
        let old_disk_size = inode.disk_size();
        if new_size == old_disk_size {
            return Ok(());
        }
        if new_size > old_disk_size {
            self.zero_regular_inode_tail_data(inode, old_disk_size)?;
            self.commit_regular_inode_write_metadata(
                inode,
                new_size,
                RegularWriteMetadata::Full { timestamp },
            )?;
            return Ok(());
        }

        self.commit_orphan_size_update(inode, new_size, timestamp)
    }

    /// Finishes a prepared shrink after the VFS page cache has been truncated.
    pub fn finish_regular_inode_shrink(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
    ) -> Ext4Result<()> {
        self.ensure_regular_file_mutation_supported(inode)?;
        self.truncate_regular_inode_shrink_committed(inode, new_size)
    }

    /// Changes a regular file's visible length using Linux-style orphan protection.
    #[cfg(test)]
    pub fn truncate_regular_inode(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        timestamp: crate::Ext4Timestamp,
    ) -> Ext4Result<()> {
        let is_visible_shrink = new_size < inode.size();
        let is_disk_shrink = new_size < inode.disk_size();
        self.prepare_regular_inode_truncate(inode, new_size, timestamp)?;
        inode.set_size(new_size);
        if is_visible_shrink {
            let first_unneeded = file_block_count(new_size, self.layout().block_size())?;
            self.truncate_delalloc_range(inode, LogicalBlock::new(first_unneeded))?;
        }
        if is_disk_shrink {
            self.finish_regular_inode_shrink(inode, new_size)?;
        }
        Ok(())
    }

    /// Discards unwritten preallocation beyond a regular file's `i_disksize`.
    ///
    /// Active delayed-allocation reservations make this operation a no-op so
    /// allocation state cannot be discarded underneath dirty page-cache data.
    pub fn discard_regular_inode_preallocations(&mut self, inode: &Ext4Inode) -> Ext4Result<()> {
        self.ensure_regular_file_mutation_supported(inode)?;
        if inode.has_delalloc_reservations() {
            return Ok(());
        }
        let disk_blocks = file_block_count(inode.disk_size(), self.layout().block_size())?;
        if !self.has_extent_mapping_from(inode, LogicalBlock::new(disk_blocks))? {
            return Ok(());
        }

        let credits = JournalCredits::for_preallocation_discard(self, inode)?;
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let blocks = file_block_count(inode.disk_size(), self.layout().block_size())?;
        let result = self.truncate_extent_mappings(inode, LogicalBlock::new(blocks), &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    pub(crate) fn cleanup_regular_file_orphan_from_head(
        &mut self,
        inode: &Ext4Inode,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
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
        self.zero_regular_inode_tail_data(inode, target_size)?;

        let credits = JournalCredits::for_regular_truncate(self, inode, target_size)?;
        let journal = self.metadata_journal_for_mutation(credits, recovery_flag_policy)?;
        let mut handle = journal.begin(credits)?;
        let result =
            self.cleanup_orphaned_inode_from_head_metadata(inode, target_size, &mut handle);
        self.complete_metadata_mutation_with_policy(handle, result, recovery_flag_policy)
    }

    fn truncate_regular_inode_shrink_committed(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
    ) -> Ext4Result<()> {
        let credits = JournalCredits::for_regular_truncate(self, inode, new_size)?;
        self.zero_regular_inode_tail_data(inode, new_size)?;

        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.finish_regular_inode_shrink_metadata(inode, new_size, &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    fn commit_orphan_size_update(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        timestamp: crate::Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.validate_inode_timestamp_update(inode, timestamp)?;
        let credits = JournalCredits::for_orphan_size_update();
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let transaction = handle.id();
        let result = self.add_orphan(inode, &mut handle).and_then(|()| {
            self.update_regular_inode_size_metadata(
                inode,
                new_size,
                RegularWriteMetadata::Full { timestamp },
                &mut handle,
            )
        });
        let has_updates = handle.has_updates();
        match result {
            Ok(()) => {
                drop(handle);
            }
            Err(error) => {
                let error = self.fail_metadata_mutation(has_updates, error);
                drop(handle);
                return Err(error);
            }
        };

        // The orphan entry and reduced on-disk size must be durable before a
        // later transaction is allowed to free the old block mappings.
        if let Err(error) = self.commit_metadata_transaction(transaction) {
            return Err(self.fail_metadata_mutation(has_updates, error));
        }
        Ok(())
    }

    fn finish_regular_inode_shrink_metadata(
        &mut self,
        inode: &Ext4Inode,
        new_size: u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let blocks = file_block_count(new_size, self.layout().block_size())?;
        self.truncate_extent_mappings(inode, LogicalBlock::new(blocks), handle)?;
        self.remove_orphan(inode, handle)
    }

    fn cleanup_orphaned_inode_from_head_metadata(
        &mut self,
        inode: &Ext4Inode,
        target_size: u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let blocks = file_block_count(target_size, self.layout().block_size())?;
        self.truncate_extent_mappings(inode, LogicalBlock::new(blocks), handle)?;
        self.remove_orphan(inode, handle)
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
            BlockMapping::Mapped { physical, len, .. } => {
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
