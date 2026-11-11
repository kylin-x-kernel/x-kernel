// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Direct quota-file reads and journal-borrowing metadata writes.

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

// A read context borrows only the filesystem; writes require the existing
// journal handle. Quota I/O must neither reacquire the mount lock
// nor start an independent transaction inside a format operation. The writer
// admits credits before entering quota state and completes/aborts this handle
// after the whole format operation, including any tree and free-list updates.
// Physical admission must cover payload and extent allocations and include
// inode_extra_isize_allocation_blocks whenever inode metadata may be dirtied.
// A handle extension supplies credits, not admission for those physical blocks.
pub(crate) struct Ext4QuotaIo<'fs, 'handle, 'journal> {
    pub(crate) filesystem: &'fs Ext4SbInfo,
    pub(crate) handle: Option<&'handle mut JournalHandle<'journal>>,
}

impl QuotaIo for Ext4QuotaIo<'_, '_, '_> {
    type File = Arc<VfsInode>;

    fn read(&self, file: &Self::File, offset: u64, output: &mut [u8]) -> KResult<usize> {
        self.filesystem
            .read_quota_file(file.private()?, offset, output)
            .map_err(|error| {
                into_vfs_err(
                    self.filesystem.fail_metadata_mutation(
                        self.handle
                            .as_ref()
                            .is_some_and(|handle| handle.has_updates()),
                        error,
                    ),
                )
            })
    }

    fn write(&mut self, file: &Self::File, offset: u64, input: &[u8]) -> KResult<usize> {
        let block_size = u64::from(self.filesystem.layout().block_size());
        if input.len() as u64 > block_size - offset % block_size {
            return Err(LinuxError::EIO.into());
        }
        let handle = self.handle.as_deref_mut().ok_or(LinuxError::EIO)?;
        let result = self
            .filesystem
            .write_quota_file(file.private()?, offset, input, handle);
        result.map_err(|error| {
            let error = self
                .filesystem
                .fail_metadata_mutation(handle.has_updates(), error);
            into_vfs_err(error)
        })
    }

    fn file_len(&self, file: &Self::File) -> KResult<u64> {
        Ok(file.private::<Ext4Inode>()?.size())
    }
}

fn is_valid_quota_inode(kind: QuotaType, number: u32) -> bool {
    match kind {
        QuotaType::User => number == crate::disk::inode::EXT4_USR_QUOTA_INO,
        QuotaType::Group => number == crate::disk::inode::EXT4_GRP_QUOTA_INO,
        QuotaType::Project => number >= crate::disk::inode::EXT4_GOOD_OLD_FIRST_INO,
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

    pub(crate) fn sync_quota_item(
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
                handle: Some(&mut handle),
            },
        );
        // Native I/O errors are classified before conversion at the adapter.
        // A format error after publication still invalidates the operation,
        // while an error without updates leaves the transaction usable.
        let mutation = match result {
            Err(error) if LinuxError::from(error) == LinuxError::EUCLEAN => {
                Err(Ext4Error::Corrupt(crate::CorruptKind::InvalidQuota))
            }
            Err(_) if handle.has_updates() => Err(Ext4Error::InvalidJournalTransaction),
            _ => Ok(()),
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
        &self,
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
        if allocations != 0 || mutations != 0 || is_growing {
            let credits = self
                .extent_writeback_metadata_credits(inode, allocations, mutations)?
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?;
            ensure_metadata_credits(handle, credits)?;
        }
        // Existing payload access charges only the first ownership transfer
        // into this transaction; later overwrites reuse that journal target.
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

pub(crate) fn quota_error(error: kerrno::KError) -> Ext4Error {
    match LinuxError::from(error) {
        LinuxError::EUCLEAN => Ext4Error::Corrupt(crate::CorruptKind::InvalidQuota),
        error => Ext4Error::Quota(error),
    }
}

fn owner_ids(uid: u32, gid: u32, project_id: u32) -> Ext4Result<[kquota::QuotaId; 3]> {
    Ok([
        kquota::QuotaId::new(QuotaType::User, uid).map_err(quota_error)?,
        kquota::QuotaId::new(QuotaType::Group, gid).map_err(quota_error)?,
        kquota::QuotaId::new(QuotaType::Project, project_id).map_err(quota_error)?,
    ])
}

impl Ext4SbInfo {
    pub(crate) fn ensure_quota_feature_supported(&self) -> Ext4Result<()> {
        self.superblock().features().validate_writable()
    }

    pub(crate) fn validate_internal_quota_inodes(&self) -> Ext4Result<()> {
        for kind in QuotaType::ALL {
            let number = self.superblock().quota_inode(kind);
            if number != 0 && !is_valid_quota_inode(kind, number) {
                return Err(Ext4Error::Quota(LinuxError::EUCLEAN));
            }
        }
        Ok(())
    }

    pub(crate) fn transfer_inode_quotas(
        &self,
        inode: &Ext4Inode,
        uid: u32,
        gid: u32,
        project_id: u32,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Option<[Option<kquota::DquotRef>; 3]>> {
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return Ok(None);
        }
        let stat = inode.stat();
        let allocated = stat
            .blocks
            .checked_mul(512)
            .and_then(|bytes| i64::try_from(bytes).ok())
            .ok_or(Ext4Error::Overflow)?;
        let reserved = self.quota_blocks_bytes(inode.reserved_data_blocks())?;
        let super_block = self
            .vfs_superblock
            .upgrade()
            .ok_or(Ext4Error::InvalidInodeState)?;
        let can_bypass_limits = crate::vfs::can_bypass_quota_limits();
        let mut quota = super_block.quota_info();
        let from = owner_ids(stat.uid, stat.gid, inode.project_id())?;
        let to = owner_ids(uid, gid, project_id)?;
        let mut io = Ext4QuotaIo {
            filesystem: self,
            handle: Some(handle),
        };
        quota
            .transfer(
                &mut io,
                (&from, &to),
                allocated,
                reserved,
                ktime::realtime().unix_seconds(),
                can_bypass_limits,
            )
            .map_err(quota_error)?;
        let mut records = [None, None, None];
        for id in to {
            records[id.kind() as usize] = quota.cached_ref(id).map_err(quota_error)?;
        }
        for id in from.into_iter().chain(to) {
            if quota.info(id.kind()).is_some_and(kquota::Dqinfo::is_active) {
                quota.sync_dquot(&mut io, id).map_err(quota_error)?;
            }
        }
        Ok(Some(records))
    }

    fn has_quota_accounting(&self) -> bool {
        self.has_quota_feature() || self.quota_mount_options != 0
    }

    pub(crate) fn has_quota_feature(&self) -> bool {
        self.superblock()
            .features()
            .read_only_compat()
            .contains(crate::disk::features::ReadOnlyCompatFeatures::QUOTA)
    }

    pub(crate) fn is_quota_inode(&self, number: crate::InodeNumber) -> bool {
        self.has_quota_feature()
            && QuotaType::ALL.into_iter().any(|kind| {
                self.superblock().quota_inode(kind) == number.get()
                    && is_valid_quota_inode(kind, number.get())
            })
    }

    /// Includes quota records for up to four inodes changed by rename.
    pub(crate) fn quota_metadata_credits(&self, credits: JournalCredits) -> JournalCredits {
        JournalCredits::new(
            credits
                .get()
                .saturating_add(if self.has_quota_accounting() {
                    if self.superblock().features().has_project() {
                        12
                    } else {
                        8
                    }
                } else {
                    0
                }),
        )
    }

    fn quota_blocks_bytes(&self, blocks: u64) -> Ext4Result<i64> {
        blocks
            .checked_mul(u64::from(self.layout().block_size()))
            .and_then(|bytes| i64::try_from(bytes).ok())
            .ok_or(Ext4Error::Overflow)
    }

    fn inode_block_sectors(&self, blocks: u64) -> i128 {
        i128::from(blocks) * i128::from(self.layout().block_size() / 512)
    }

    /// Initializes each owner record before entering inode or allocator locks.
    pub(crate) fn initialize_owner_quotas(
        &mut self,
        uid: u32,
        gid: u32,
        project_id: u32,
    ) -> Ext4Result<()> {
        self.initialize_owner_quotas_with_policy(
            uid,
            gid,
            project_id,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    fn initialize_owner_quotas_with_policy(
        &mut self,
        uid: u32,
        gid: u32,
        project_id: u32,
        policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        if !self.has_quota_accounting() {
            return Ok(());
        }
        let super_block = self
            .vfs_superblock
            .upgrade()
            .ok_or(Ext4Error::InvalidInodeState)?;
        for id in owner_ids(uid, gid, project_id)? {
            let (file, record_credits) = {
                let mut quota = super_block.quota_info();
                let Some(info) = quota.info(id.kind()) else {
                    continue;
                };
                if !info.is_active() || quota.is_cached_active(id) {
                    continue;
                }
                quota
                    .read_record(
                        &mut Ext4QuotaIo {
                            filesystem: self,
                            handle: None,
                        },
                        id,
                    )
                    .map_err(|error| self.fail_metadata_mutation(false, quota_error(error)))?;
                if quota.is_cached_active(id) {
                    continue;
                }
                let info = quota.info(id.kind()).ok_or(Ext4Error::InvalidInodeState)?;
                (info.file().clone(), info.record_write_blocks_bound())
            };
            let inode = file.private::<Ext4Inode>().map_err(quota_error)?;
            // Every touched quota block may occupy a different filesystem
            // block, including sparse free-list slots below EOF.
            let new_blocks = record_credits;
            let metadata_blocks =
                u64::from(new_blocks) * (u64::from(crate::disk::extent::EXTENT_MAX_DEPTH) + 1);
            let credits = JournalCredits::new(
                self.extent_writeback_metadata_credits(inode, new_blocks, new_blocks)?
                    .checked_add(record_credits)
                    .ok_or(Ext4Error::Overflow)?,
            );
            let journal = self.metadata_journal_for_allocation(
                credits,
                policy,
                u64::from(new_blocks)
                    + metadata_blocks
                    + self.inode_extra_isize_allocation_blocks(inode),
            )?;
            let mut handle = journal.begin(credits)?;
            let result = super_block
                .quota_info()
                .get(
                    &mut Ext4QuotaIo {
                        filesystem: self,
                        handle: Some(&mut handle),
                    },
                    id,
                )
                .map(|_| ())
                .map_err(quota_error);
            self.complete_metadata_mutation_with_policy(handle, result, policy)?;
        }
        Ok(())
    }

    pub(crate) fn initialize_inode_quotas(&mut self, inode: &Ext4Inode) -> Ext4Result<()> {
        self.initialize_inode_quotas_with_policy(
            inode,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn initialize_inode_quotas_with_policy(
        &mut self,
        inode: &Ext4Inode,
        policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return Ok(());
        }
        let stat = inode.stat();
        self.initialize_owner_quotas_with_policy(stat.uid, stat.gid, inode.project_id(), policy)?;
        self.bind_inode_quotas(inode)
    }

    /// Binds records already acquired before entering a metadata transaction.
    pub(crate) fn bind_inode_quotas(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return Ok(());
        }
        let super_block = self
            .vfs_superblock
            .upgrade()
            .ok_or(Ext4Error::InvalidInodeState)?;
        inode.update_quota_records(|uid, gid, project_id, reserved, records| {
            if inode.is_noquota() {
                return Ok(());
            }
            let ids = owner_ids(uid, gid, project_id)?;
            let bytes = self.quota_blocks_bytes(reserved)?;
            let mut quota = super_block.quota_info();
            quota.bind_cached(&ids, bytes, records).map_err(quota_error)
        })
    }

    fn with_owner_quota(
        &self,
        uid: u32,
        gid: u32,
        project_id: u32,
        update: impl FnOnce(&mut QuotaInfo<Arc<VfsInode>>, &[kquota::QuotaId]) -> KResult<()>,
    ) -> Ext4Result<()> {
        if !self.has_quota_accounting() {
            return Ok(());
        }
        let super_block = self
            .vfs_superblock
            .upgrade()
            .ok_or(Ext4Error::InvalidInodeState)?;
        let mut quota = super_block.quota_info();
        let ids = owner_ids(uid, gid, project_id)?;
        for &id in &ids {
            if quota.info(id.kind()).is_some_and(kquota::Dqinfo::is_active)
                && !quota.is_cached_active(id)
            {
                return Err(Ext4Error::InvalidInodeState);
            }
        }
        update(&mut quota, &ids).map_err(quota_error)
    }

    fn with_inode_quota(
        &self,
        records: &[Option<kquota::DquotRef>; 3],
        update: impl FnOnce(
            &mut QuotaInfo<Arc<VfsInode>>,
            &[Option<kquota::DquotRef>; 3],
        ) -> KResult<()>,
    ) -> Ext4Result<()> {
        if !self.has_quota_accounting() || records.iter().all(Option::is_none) {
            return Ok(());
        }
        let super_block = self
            .vfs_superblock
            .upgrade()
            .ok_or(Ext4Error::InvalidInodeState)?;
        update(&mut super_block.quota_info(), records).map_err(quota_error)
    }

    pub(crate) fn reserve_quota_space(
        &self,
        records: &[Option<kquota::DquotRef>; 3],
        blocks: u64,
    ) -> Ext4Result<()> {
        if blocks == 0 {
            return Ok(());
        }
        let bytes = self.quota_blocks_bytes(blocks)?;
        let mut flags = kquota::ChargeFlags::WARN;
        flags.set(
            kquota::ChargeFlags::BYPASS_LIMITS,
            crate::vfs::can_bypass_quota_limits(),
        );
        self.with_inode_quota(records, |quota, records| {
            quota.reserve_space_refs(records, bytes, ktime::realtime().unix_seconds(), flags)
        })
    }

    pub(crate) fn release_quota_reservation(
        &self,
        records: &[Option<kquota::DquotRef>; 3],
        blocks: u64,
    ) -> Ext4Result<()> {
        if blocks == 0 {
            return Ok(());
        }
        let bytes = self.quota_blocks_bytes(blocks)?;
        self.with_inode_quota(records, |quota, records| {
            quota.release_space_refs(records, bytes)
        })
    }

    pub(crate) fn charge_quota_space(
        &self,
        inode: &Ext4Inode,
        blocks: u64,
        _handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if blocks == 0 {
            return Ok(());
        }
        let sectors = self.inode_block_sectors(blocks);
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return inode.update_allocated_blocks(sectors, |_| Ok(()));
        }
        let bytes = self.quota_blocks_bytes(blocks)?;
        let mut flags = kquota::ChargeFlags::WARN;
        flags.set(
            kquota::ChargeFlags::BYPASS_LIMITS,
            crate::vfs::can_bypass_quota_limits(),
        );
        inode.update_allocated_blocks(sectors, |records| {
            self.with_inode_quota(records, |quota, records| {
                quota.alloc_space_refs(records, bytes, ktime::realtime().unix_seconds(), flags)
            })
        })
    }

    pub(crate) fn validate_free_quota_space(
        &self,
        inode: &Ext4Inode,
        blocks: u64,
    ) -> Ext4Result<()> {
        if blocks == 0 {
            return Ok(());
        }
        let sectors = self.inode_block_sectors(blocks);
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return inode.validate_allocated_blocks(-sectors, |_| Ok(()));
        }
        let bytes = self.quota_blocks_bytes(blocks)?;
        inode.validate_allocated_blocks(-sectors, |records| {
            self.with_inode_quota(records, |quota, records| {
                quota.validate_free_space_refs(records, bytes)
            })
        })
    }

    pub(crate) fn free_quota_space(
        &self,
        inode: &Ext4Inode,
        blocks: u64,
        _handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if blocks == 0 {
            return Ok(());
        }
        let sectors = self.inode_block_sectors(blocks);
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return inode.update_allocated_blocks(-sectors, |_| Ok(()));
        }
        let bytes = self.quota_blocks_bytes(blocks)?;
        inode.update_allocated_blocks(-sectors, |records| {
            self.with_inode_quota(records, |quota, records| {
                quota.free_space_refs(records, bytes)
            })
        })
    }

    pub(crate) fn claim_quota_reservation(
        &self,
        inode: &Ext4Inode,
        records: &[Option<kquota::DquotRef>; 3],
        blocks: u64,
    ) -> Ext4Result<()> {
        if blocks == 0 {
            return Ok(());
        }
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return Ok(());
        }
        let bytes = self.quota_blocks_bytes(blocks)?;
        self.with_inode_quota(records, |quota, records| {
            quota.claim_space_refs(records, bytes)
        })
    }

    pub(crate) fn charge_quota_inode(
        &self,
        uid: u32,
        gid: u32,
        project_id: u32,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let can_bypass_limits = crate::vfs::can_bypass_quota_limits();
        self.with_owner_quota(uid, gid, project_id, |quota, ids| {
            quota.alloc_inode(
                &mut Ext4QuotaIo {
                    filesystem: self,
                    handle: Some(handle),
                },
                ids,
                ktime::realtime().unix_seconds(),
                can_bypass_limits,
            )
        })
    }

    pub(crate) fn free_quota_inode(
        &self,
        uid: u32,
        gid: u32,
        project_id: u32,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.with_owner_quota(uid, gid, project_id, |quota, ids| {
            quota.free_inode(
                &mut Ext4QuotaIo {
                    filesystem: self,
                    handle: Some(handle),
                },
                ids,
            )
        })
    }

    pub(crate) fn free_inode_quota(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        if self.is_quota_inode(inode.number()) || inode.is_noquota() {
            return Ok(());
        }
        inode.with_quota_records(|records| {
            self.with_inode_quota(records, |quota, records| quota.free_inode_refs(records))
        })
    }

    pub(crate) fn sync_owner_quotas(
        &self,
        uid: u32,
        gid: u32,
        project_id: u32,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.with_owner_quota(uid, gid, project_id, |quota, ids| {
            for &id in ids {
                if quota.info(id.kind()).is_some() {
                    quota.sync_dquot(
                        &mut Ext4QuotaIo {
                            filesystem: self,
                            handle: Some(handle),
                        },
                        id,
                    )?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn sync_inode_quotas(
        &self,
        inode: &Ext4Inode,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if !self.has_quota_accounting() || self.is_quota_inode(inode.number()) || inode.is_noquota()
        {
            return Ok(());
        }
        inode.with_quota_records(|records| {
            self.with_inode_quota(records, |quota, records| {
                let mut io = Ext4QuotaIo {
                    filesystem: self,
                    handle: Some(handle),
                };
                for record in records.iter().flatten() {
                    let id = record.lock().id();
                    if quota.info(id.kind()).is_some_and(kquota::Dqinfo::is_active) {
                        quota.sync_dquot(&mut io, id)?;
                    }
                }
                Ok(())
            })
        })
    }
}

impl Ext4SbInfo {
    pub(crate) fn enable_internal_quota(
        &mut self,
        super_block: &SuperBlock,
        kind: QuotaType,
        file: &Arc<VfsInode>,
    ) -> Ext4Result<()> {
        self.ensure_writable()?;
        // Loading the header is read-only and must not set recovery flags.
        let result = super_block
            .quota_info()
            .enable(
                &mut Ext4QuotaIo {
                    filesystem: self,
                    handle: None,
                },
                kind,
                file,
                kquota::QuotaFormat::VfsV1,
                kquota::QuotaState::USAGE_ENABLED
                    | if self.quota_mount_options & (1 << kind as u8) != 0 {
                        kquota::QuotaState::LIMITS_ENABLED
                    } else {
                        kquota::QuotaState::empty()
                    },
            )
            .map_err(quota_error);
        result.map_err(|error| self.fail_metadata_mutation(false, error))
    }

    pub(crate) fn release_internal_quota_records(
        &mut self,
        super_block: &SuperBlock,
        kind: QuotaType,
    ) -> Ext4Result<()> {
        let mut after = None;
        loop {
            let next = super_block.quota_info().next_cached_id(after);
            let Some(id) = next else {
                break;
            };
            after = Some(id);
            if id.kind() != kind {
                continue;
            }
            let credits = JournalCredits::new(
                super_block
                    .quota_info()
                    .info(kind)
                    .ok_or(Ext4Error::InvalidInodeState)?
                    .record_write_blocks_bound(),
            );
            let journal = self.metadata_journal_without_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )?;
            let mut handle = journal.begin(credits)?;
            let result = super_block
                .quota_info()
                .release(
                    &mut Ext4QuotaIo {
                        filesystem: self,
                        handle: Some(&mut handle),
                    },
                    id,
                )
                .map_err(quota_error);
            self.complete_metadata_mutation(handle, result)?;
        }
        Ok(())
    }

    pub(crate) fn mark_quotas_suspended(&self, super_block: &SuperBlock) -> Ext4Result<()> {
        let mut quota = super_block.quota_info();
        for kind in QuotaType::ALL {
            if quota.info(kind).is_some() {
                quota.check_no_reservations(kind).map_err(quota_error)?;
            }
        }
        for kind in QuotaType::ALL {
            if quota.info(kind).is_some() {
                quota.suspend_cached(kind).map_err(quota_error)?;
            }
        }
        Ok(())
    }

    pub(crate) fn suspend_internal_quotas(&mut self, super_block: &SuperBlock) -> Ext4Result<()> {
        {
            let quota = super_block.quota_info();
            for kind in QuotaType::ALL {
                if quota.info(kind).is_some() {
                    quota.check_no_reservations(kind).map_err(quota_error)?;
                }
            }
        }
        self.sync_quotas(super_block).map_err(quota_error)?;
        for kind in QuotaType::ALL {
            // Deactivated classes may still have records awaiting release.
            let needs_release = super_block
                .quota_info()
                .info(kind)
                .is_some_and(|info| !info.state().contains(kquota::QuotaState::SUSPENDED));
            if needs_release {
                self.release_internal_quota_records(super_block, kind)?;
            }
        }
        self.mark_quotas_suspended(super_block)
    }

    pub(crate) fn resume_internal_quotas(&self, super_block: &SuperBlock) -> Ext4Result<()> {
        let mut quota = super_block.quota_info();
        for kind in QuotaType::ALL {
            if quota
                .info(kind)
                .is_some_and(|info| info.state().contains(kquota::QuotaState::SUSPENDED))
            {
                quota.resume(kind).map_err(quota_error)?;
            }
        }
        Ok(())
    }

    /// Returns writeback status and the file for release outside mount protection.
    /// Aborted mutations leave recovery to the journal while releasing ownership.
    fn disable_internal_quota(
        &mut self,
        super_block: &SuperBlock,
        kind: QuotaType,
    ) -> (Ext4Result<()>, Option<Arc<VfsInode>>) {
        let state = super_block.quota_info().info(kind).map(|info| info.state());
        let Some(state) = state else {
            return (Ok(()), None);
        };
        let result = (|| {
            self.ensure_writable()?;
            super_block
                .quota_info()
                .check_no_reservations(kind)
                .map_err(quota_error)?;
            if !state.contains(kquota::QuotaState::SUSPENDED) {
                self.release_internal_quota_records(super_block, kind)?;
            }
            // Record release may edit the tree, free lists and header. These
            // metadata writes must reach home blocks before ordinary file I/O.
            self.sync_filesystem()?;
            super_block.quota_info().unload(kind).map_err(quota_error)
        })();
        match result {
            Ok(file) => (Ok(()), Some(file)),
            Err(error) => {
                let file = if self
                    .journal
                    .as_ref()
                    .is_some_and(|journal| journal.is_aborted())
                {
                    super_block.quota_info().invalidate(kind)
                } else {
                    None
                };
                (Err(error), file)
            }
        }
    }
}

impl crate::vfs::Ext4Mount {
    pub(crate) fn suspend_internal_quotas(&self, super_block: &SuperBlock) -> Ext4Result<()> {
        let inodes = super_block.live_inodes();
        // An inode already in final eviction cannot be pinned by the snapshot.
        super_block.wait_for_inode_evictions();
        let result = {
            let mut state = self.write(super_block);
            (|| {
                let quota = super_block.quota_info();
                for kind in QuotaType::ALL {
                    if quota.info(kind).is_some() {
                        quota.check_no_reservations(kind).map_err(quota_error)?;
                    }
                }
                drop(quota);
                for inode in &inodes {
                    inode
                        .private::<Ext4Inode>()
                        .map_err(quota_error)?
                        .drop_quota_records(None);
                }
                state.suspend_internal_quotas(super_block)
            })()
        };
        drop(inodes);
        result
    }

    /// Disables one class and releases its backing inode after mount protection.
    pub(crate) fn disable_internal_quota(
        &self,
        super_block: &SuperBlock,
        kind: QuotaType,
    ) -> Ext4Result<()> {
        let inodes = super_block.live_inodes();
        let deactivated: Ext4Result<bool> = {
            let _state = self.write(super_block);
            (|| {
                let mut quota = super_block.quota_info();
                if quota.info(kind).is_none() {
                    return Ok(false);
                }
                quota.deactivate(kind).map_err(quota_error)?;
                drop(quota);
                for inode in &inodes {
                    inode
                        .private::<Ext4Inode>()
                        .map_err(quota_error)?
                        .drop_quota_records(Some(kind));
                }
                Ok(true)
            })()
        };
        drop(inodes);
        if !deactivated? {
            return Ok(());
        }
        // An inode whose last reference disappeared before the snapshot still
        // owns its quota records until eviction finishes.
        super_block.wait_for_inode_evictions();
        let (result, file) = self
            .write(super_block)
            .disable_internal_quota(super_block, kind);
        let cleanup = (|| {
            if let Some(file) = &file {
                let _data_guard = file.lock_data();
                file.invalidate_cached_pages()?;
                let inode = file.private::<Ext4Inode>()?;
                if !self.read().has_quota_feature() {
                    if !super_block.is_readonly() {
                        let mut state = self.write(super_block);
                        state
                            .set_quota_file_protection(inode, false)
                            .map_err(into_vfs_err)?;
                        state.sync_filesystem().map_err(into_vfs_err)?;
                    }
                    inode.set_noquota(false);
                    if file.write_count() != 0 {
                        self.write(super_block)
                            .initialize_inode_quotas(inode)
                            .map_err(into_vfs_err)?;
                    }
                }
            }
            Ok::<(), kerrno::KError>(())
        })()
        .map_err(quota_error);
        drop(file);
        result.and(cleanup)
    }
}

impl Ext4SbInfo {
    /// Reads quota state without journal or physical-allocation admission.
    pub(crate) fn read_quota_record<T>(
        &self,
        super_block: &SuperBlock,
        read: impl FnOnce(&QuotaInfo<Arc<VfsInode>>, &mut Ext4QuotaIo<'_, '_, '_>) -> KResult<T>,
    ) -> KResult<T> {
        read(
            &super_block.quota_info(),
            &mut Ext4QuotaIo {
                filesystem: self,
                handle: None,
            },
        )
        .map_err(|error| into_vfs_err(self.fail_metadata_mutation(false, quota_error(error))))
    }

    /// Loads existing records before admitting storage for a missing record.
    pub(crate) fn quota_record_operation<T>(
        &mut self,
        super_block: &SuperBlock,
        id: kquota::QuotaId,
        operation: impl FnOnce(
            &mut QuotaInfo<Arc<VfsInode>>,
            &mut Ext4QuotaIo<'_, '_, '_>,
        ) -> KResult<T>,
    ) -> KResult<T> {
        let (file, blocks) = {
            let mut quota = super_block.quota_info();
            quota
                .read_record(
                    &mut Ext4QuotaIo {
                        filesystem: self,
                        handle: None,
                    },
                    id,
                )
                .map_err(|error| {
                    into_vfs_err(self.fail_metadata_mutation(false, quota_error(error)))
                })?;
            let info = quota.info(id.kind()).ok_or(LinuxError::ESRCH)?;
            (
                info.file().clone(),
                if quota.is_cached_active(id) {
                    0
                } else {
                    info.record_write_blocks_bound()
                },
            )
        };
        let inode = file.private::<Ext4Inode>()?;
        let metadata_blocks =
            u64::from(blocks) * (u64::from(crate::disk::extent::EXTENT_MAX_DEPTH) + 1);
        let credits = JournalCredits::new(if blocks == 0 {
            1
        } else {
            self.extent_writeback_metadata_credits(inode, blocks, blocks)
                .map_err(into_vfs_err)?
                .checked_add(blocks)
                .ok_or(LinuxError::EOVERFLOW)?
        });
        let journal = if blocks == 0 {
            self.metadata_journal_without_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
        } else {
            self.metadata_journal_for_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                u64::from(blocks)
                    + metadata_blocks
                    + self.inode_extra_isize_allocation_blocks(inode),
            )
        }
        .map_err(into_vfs_err)?;
        let mut handle = journal.begin(credits).map_err(into_vfs_err)?;
        let result = operation(
            &mut super_block.quota_info(),
            &mut Ext4QuotaIo {
                filesystem: self,
                handle: Some(&mut handle),
            },
        );
        match result {
            Ok(value) => {
                self.complete_metadata_mutation(handle, Ok(()))
                    .map_err(into_vfs_err)?;
                Ok(value)
            }
            Err(error) => {
                self.complete_metadata_mutation(handle, Err::<(), _>(quota_error(error)))
                    .map_err(into_vfs_err)?;
                Err(error)
            }
        }
    }

    pub(crate) fn set_quota_file_protection(
        &mut self,
        inode: &Ext4Inode,
        is_enabled: bool,
    ) -> Ext4Result<()> {
        let credits = JournalCredits::new(2);
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        let protected = crate::disk::inode::EXT4_IMMUTABLE_FL | crate::disk::inode::EXT4_NOATIME_FL;
        let flags = if is_enabled {
            inode.flags() | protected
        } else {
            inode.flags() & !protected
        };
        let result = self.update_inode_flags_timestamps_metadata(
            inode,
            flags,
            crate::Ext4Timestamp::new(ktime::realtime().unix_seconds(), 0),
            &mut handle,
        );
        self.complete_metadata_mutation(handle, result)
    }
}
