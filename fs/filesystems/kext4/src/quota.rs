// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Direct quota-file metadata I/O within the caller's journal transaction.

use alloc::{sync::Arc, vec};

use kerrno::{KResult, LinuxError};
use kquota::{QuotaInfo, QuotaIo, QuotaType};
use kvfs::{SuperBlock, VfsInode};

use crate::{
    BlockCount, BlockMapping, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, FilesystemBlock,
    LogicalBlock,
    extent::ExtentMappingState,
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalHandle},
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    superblock::{ensure_metadata_credits, metadata_access_bytes, replace_metadata_access_bytes},
    vfs::into_vfs_err,
};

// Borrow both existing owners: quota I/O must neither reacquire the mount lock
// nor start an independent transaction inside a format operation. The caller
// admits credits before entering quota state and completes/aborts this handle
// after the whole format operation, including any tree and free-list updates.
// Physical admission must cover payload and extent allocations and include
// inode_extra_isize_allocation_blocks whenever inode metadata may be dirtied.
// A handle extension supplies credits, not admission for those physical blocks.
pub(crate) struct Ext4QuotaIo<'fs, 'handle, 'journal> {
    pub(crate) filesystem: &'fs mut Ext4SbInfo,
    pub(crate) handle: &'handle mut JournalHandle<'journal>,
}

impl QuotaIo for Ext4QuotaIo<'_, '_, '_> {
    type File = Arc<VfsInode>;

    fn read(&self, file: &Self::File, offset: u64, output: &mut [u8]) -> KResult<usize> {
        self.filesystem
            .read_quota_file(file.private()?, offset, output)
            .map_err(into_vfs_err)
    }

    fn write(&mut self, file: &Self::File, offset: u64, input: &[u8]) -> KResult<usize> {
        let block_size = u64::from(self.filesystem.layout().block_size());
        if input.len() as u64 > block_size - offset % block_size {
            return Err(LinuxError::EIO.into());
        }
        let result = self
            .filesystem
            .write_quota_file(file.private()?, offset, input, self.handle);
        result.map_err(|error| {
            let error = self
                .filesystem
                .fail_metadata_mutation(self.handle.has_updates(), error);
            into_vfs_err(error)
        })
    }

    fn file_len(&self, file: &Self::File) -> KResult<u64> {
        Ok(file.private::<Ext4Inode>()?.size())
    }
}

impl Ext4SbInfo {
    /// Writes each dirty quota record in a bounded transaction, then file info.
    /// Mount write exclusion precedes the sleepable superblock quota lock.
    pub(crate) fn sync_quotas(&mut self, super_block: &SuperBlock) -> KResult<()> {
        let mut quota = super_block.quota_info();
        let mut result = Ok(());
        let mut after = None;
        while let Some(id) = quota.next_dirty_id(after) {
            let write_result =
                self.sync_quota_item(&mut quota, |quota, io| quota.sync_dquot(io, id));
            result = result.and(write_result);
            after = Some(id);
        }
        for kind in QuotaType::ALL {
            if quota.info(kind).is_some_and(|info| info.is_dirty()) {
                let write_result =
                    self.sync_quota_item(&mut quota, |quota, io| quota.sync_info(io, kind));
                result = result.and(write_result);
            }
        }
        result
    }

    fn sync_quota_item(
        &mut self,
        quota: &mut QuotaInfo<Arc<VfsInode>>,
        write: impl FnOnce(&mut QuotaInfo<Arc<VfsInode>>, &mut Ext4QuotaIo<'_, '_, '_>) -> KResult<()>,
    ) -> KResult<()> {
        // Active records and file information overwrite existing payloads.
        // Inactive records report an initialization error without doing I/O;
        // their structural allocation remains the acquisition path's job.
        let credits = JournalCredits::new(1);
        let journal = self
            .metadata_journal_without_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .map_err(into_vfs_err)?;
        let mut handle = journal.begin(credits).map_err(into_vfs_err)?;
        let result = write(
            quota,
            &mut Ext4QuotaIo {
                filesystem: self,
                handle: &mut handle,
            },
        );
        // Native I/O errors are classified before conversion at the adapter.
        // A format error after publication still invalidates the operation,
        // while an error without updates leaves the transaction usable.
        let mutation = if result.is_err() && handle.has_updates() {
            Err(Ext4Error::InvalidJournalTransaction)
        } else {
            Ok(())
        };
        let completion = self
            .complete_metadata_mutation(handle, mutation)
            .map_err(into_vfs_err);
        result.and(completion)
    }

    fn read_quota_file(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        self.ensure_regular_file_mutation_supported(inode)?;
        let total = output
            .len()
            .min(inode.size().saturating_sub(offset) as usize);
        let block_size = u64::from(self.layout().block_size());
        let mut copied = 0;
        while copied < total {
            let position = offset + copied as u64;
            let in_block = (position % block_size) as usize;
            let count = (total - copied).min(block_size as usize - in_block);
            let destination = &mut output[copied..copied + count];
            match self.map_blocks(inode, LogicalBlock::new(position / block_size))? {
                BlockMapping::Mapped { physical, .. } => {
                    let buffer = self.read_metadata_block(FilesystemBlock::new(physical.get()))?;
                    destination.copy_from_slice(&buffer.as_ref()[in_block..in_block + count]);
                }
                BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => destination.fill(0),
            }
            copied += count;
        }
        Ok(copied)
    }

    fn write_quota_file(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        input: &[u8],
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<usize> {
        self.ensure_regular_file_mutation_supported(inode)?;
        if input.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(Ext4Error::Overflow)?;
        let block_size = u64::from(self.layout().block_size());
        let logical = LogicalBlock::new(offset / block_size);
        let mapping = self.map_blocks(inode, logical)?;
        let (allocations, mutations) = match mapping {
            BlockMapping::Mapped { .. } => (0, 0),
            BlockMapping::Unwritten { .. } => (0, 1),
            BlockMapping::Hole { .. } => (1, 1),
        };
        let is_growing = end > inode.size();
        let credits = if allocations == 0 && mutations == 0 && !is_growing {
            1
        } else {
            self.extent_writeback_metadata_credits(inode, allocations, mutations)?
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?
        };
        ensure_metadata_credits(handle, credits)?;
        let (physical, is_new) = match mapping {
            BlockMapping::Mapped { physical, .. } => (physical, false),
            BlockMapping::Unwritten { physical, .. } => {
                self.convert_unwritten_extent_range(inode, logical, BlockCount::new(1), handle)?;
                (physical, true)
            }
            BlockMapping::Hole { .. } => {
                self.extent_insert_metadata_block_bound(inode, logical, BlockCount::new(1))?;
                let request = Ext4AllocationRequest::new(
                    inode,
                    logical,
                    None,
                    BlockCount::new(1),
                    BlockCount::new(1),
                    Ext4AllocationFlags::EXACT,
                )?;
                let allocation = self.allocate_blocks_for_write(request, handle)?;
                let physical = allocation.first_block_allocation().block();
                self.insert_extent_mapping(
                    inode,
                    logical,
                    physical,
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    handle,
                )?;
                (physical, true)
            }
        };
        let block = FilesystemBlock::new(physical.get());
        let access = if is_new {
            self.metadata_io.create_access(block, handle)?
        } else {
            self.metadata_io.write_access(block, handle)?
        };
        let mut bytes = if is_new {
            vec![0; block_size as usize]
        } else {
            metadata_access_bytes(&access)?
        };
        let start = (offset % block_size) as usize;
        bytes[start..start + input.len()].copy_from_slice(input);
        replace_metadata_access_bytes(&access, bytes)?;
        if is_growing {
            self.update_regular_inode_write_metadata(
                inode,
                end,
                RegularWriteMetadata::SizeOnly,
                handle,
            )?;
            inode.set_size(end);
        }
        Ok(input.len())
    }
}
