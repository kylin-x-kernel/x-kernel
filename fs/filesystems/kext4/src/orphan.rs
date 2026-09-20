// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 orphan-file and legacy orphan-list algorithms.

use alloc::vec::Vec;

use crate::{
    BlockMapping, ChecksumTarget, CorruptKind, Ext4DiskSuperblock, Ext4Error, Ext4Inode,
    Ext4Result, Ext4SbInfo, FilesystemBlock, InodeKind, InodeNumber, LogicalBlock, UnsupportedKind,
    buffer::Ext4MetadataIo,
    disk::{checksum, codec, superblock},
    inode::{OrphanLocation, inode_checksum_seed},
    jbd2::{JournalCredits, JournalHandle},
    journal::RecoveryFlagPolicy,
    superblock::{lock, metadata_access_bytes, replace_metadata_access_bytes},
};

const EXT4_MAX_ORPHAN_FILE_BLOCKS: u64 = 512;
const EXT4_ORPHAN_BLOCK_MAGIC: u32 = 0x0b10_ca04;
const EXT4_ORPHAN_BLOCK_TAIL_SIZE: usize = 8;
const ORPHAN_HEAD_UPDATE_CREDITS: JournalCredits = JournalCredits::new(1);

/// Mount-lifetime state corresponding to Linux `ext4_orphan_info`.
pub(crate) struct OrphanInfo {
    checksum_seed: u32,
    blocks: Vec<OrphanBlock>,
}

/// Cached identity and free-entry count for one orphan-file block.
struct OrphanBlock {
    physical: FilesystemBlock,
    free_entries: usize,
}

impl OrphanInfo {
    fn is_empty(&self, entries_per_block: usize) -> bool {
        self.blocks
            .iter()
            .all(|block| block.free_entries == entries_per_block)
    }

    /// Allocates a slot under the caller's exclusive mount access. Unlike
    /// Linux's concurrent allocator, this needs neither a CPU hash nor CAS.
    fn add(
        &mut self,
        inode: &Ext4Inode,
        metadata_io: &Ext4MetadataIo,
        handle: &mut JournalHandle<'_>,
        superblock: &Ext4DiskSuperblock,
    ) -> Ext4Result<()> {
        let entries_per_block =
            (superblock.block_size() as usize - EXT4_ORPHAN_BLOCK_TAIL_SIZE) / 4;
        let has_checksum = superblock.features().has_metadata_checksum();
        let (block_index, block) = self
            .blocks
            .iter_mut()
            .enumerate()
            .find(|(_, block)| block.free_entries != 0)
            .ok_or(Ext4Error::NoSpace)?;
        let access = metadata_io.write_access(block.physical, handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        let offset = (0..entries_per_block)
            .find(|offset| bytes[*offset * 4..*offset * 4 + 4] == [0; 4])
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?;
        // Mount validation bounds the file to 512 blocks, so its slot index
        // fits u32 for every supported ext4 block size.
        let index = u32::try_from(block_index * entries_per_block + offset)
            .map_err(|_| Ext4Error::Overflow)?;
        let location = OrphanLocation::File { index };
        bytes[offset * 4..offset * 4 + 4].copy_from_slice(&inode.number().get().to_le_bytes());
        update_orphan_block_checksum(block.physical, self.checksum_seed, &mut bytes, has_checksum)?;
        inode.bind_orphan(location)?;
        if let Err(error) = replace_metadata_access_bytes(&access, bytes) {
            inode.clear_orphan(location);
            return Err(error);
        }
        block.free_entries -= 1;
        Ok(())
    }

    fn remove(
        &mut self,
        inode: &Ext4Inode,
        index: u32,
        metadata_io: &Ext4MetadataIo,
        handle: &mut JournalHandle<'_>,
        superblock: &Ext4DiskSuperblock,
    ) -> Ext4Result<()> {
        let entries_per_block =
            (superblock.block_size() as usize - EXT4_ORPHAN_BLOCK_TAIL_SIZE) / 4;
        let has_checksum = superblock.features().has_metadata_checksum();
        let entry = index as usize;
        let block = self
            .blocks
            .get_mut(entry / entries_per_block)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?;
        let offset = entry % entries_per_block;
        let access = metadata_io.write_access(block.physical, handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        if codec::le_u32(&bytes, offset * 4)? != inode.number().get()
            || block.free_entries >= entries_per_block
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }
        bytes[offset * 4..offset * 4 + 4].fill(0);
        update_orphan_block_checksum(block.physical, self.checksum_seed, &mut bytes, has_checksum)?;
        replace_metadata_access_bytes(&access, bytes)?;
        block.free_entries += 1;
        inode.clear_orphan(OrphanLocation::File { index });
        Ok(())
    }
}

impl Ext4SbInfo {
    /// Initializes orphan-file state after journal replay has established the
    /// authoritative home-block contents.
    pub(crate) fn initialize_orphan_info(&mut self) -> Ext4Result<()> {
        self.orphan_info = self.load_orphan_info()?;
        Ok(())
    }

    fn load_orphan_info(&self) -> Ext4Result<Option<OrphanInfo>> {
        let features = self.superblock().features();
        if !features.has_orphan_file() {
            if self.has_orphan_present() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
            }
            return Ok(None);
        }

        let number = self
            .superblock()
            .orphan_file_inode()
            .map(|inode| InodeNumber::new(inode.get()))
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?;
        let inode = self.special_iget(number)?;
        if inode.kind() != InodeKind::RegularFile {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }

        let block_size = u64::from(self.layout().block_size());
        if inode.size() % block_size != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }
        let block_count = inode.size() / block_size;
        if block_count > EXT4_MAX_ORPHAN_FILE_BLOCKS {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }

        let checksum_seed = inode_checksum_seed(
            self.superblock().checksum_seed(),
            inode.number(),
            inode.generation(),
        );
        let entries_per_block = self.orphan_entries_per_block();
        let mut blocks =
            Vec::with_capacity(usize::try_from(block_count).map_err(|_| Ext4Error::Overflow)?);
        for logical in 0..block_count {
            let physical = match self.map_blocks(&inode, LogicalBlock::new(logical))? {
                BlockMapping::Mapped { physical, len, .. }
                    if physical.get() != 0 && len.get() != 0 =>
                {
                    FilesystemBlock::new(physical.get())
                }
                BlockMapping::Mapped { .. }
                | BlockMapping::Hole { .. }
                | BlockMapping::Unwritten { .. } => {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
                }
            };
            let block = self.read_metadata_block(physical)?;
            self.verify_orphan_block(inode.number(), physical, checksum_seed, block.as_ref())?;

            let mut free_entries = 0usize;
            for offset in 0..entries_per_block {
                if codec::le_u32(block.as_ref(), offset * 4)? == 0 {
                    free_entries = free_entries.checked_add(1).ok_or(Ext4Error::Overflow)?;
                }
            }
            blocks.push(OrphanBlock {
                physical,
                free_entries,
            });
        }

        Ok(Some(OrphanInfo {
            checksum_seed,
            blocks,
        }))
    }

    fn verify_orphan_block(
        &self,
        inode: InodeNumber,
        physical: FilesystemBlock,
        checksum_seed: u32,
        bytes: &[u8],
    ) -> Ext4Result<()> {
        let entries_bytes = self.orphan_entries_bytes();
        if codec::le_u32(bytes, entries_bytes)? != EXT4_ORPHAN_BLOCK_MAGIC {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }
        let entries = bytes.get(..entries_bytes).ok_or(Ext4Error::OutOfBounds)?;
        let expected = orphan_block_checksum(checksum_seed, physical, entries);
        let actual = codec::le_u32(bytes, entries_bytes + 4)?;
        if expected != actual {
            return Err(Ext4Error::ChecksumMismatch {
                target: ChecksumTarget::OrphanFileBlock {
                    inode: inode.get(),
                    block: physical.get(),
                },
                expected,
                actual,
            });
        }
        Ok(())
    }

    fn orphan_entries_bytes(&self) -> usize {
        let block_size = self.device.block_size();
        debug_assert!(block_size >= EXT4_ORPHAN_BLOCK_TAIL_SIZE);
        block_size - EXT4_ORPHAN_BLOCK_TAIL_SIZE
    }

    fn orphan_entries_per_block(&self) -> usize {
        let bytes = self.orphan_entries_bytes();
        debug_assert!(bytes.is_multiple_of(4));
        bytes / 4
    }

    pub(crate) fn orphan_head(&self) -> Option<InodeNumber> {
        match lock(&self.allocator).last_orphan {
            0 => None,
            head => Some(InodeNumber::new(head)),
        }
    }

    /// Returns whether the initialized orphan file has no live entries.
    ///
    /// Like Linux `ext4_orphan_file_empty()`, a filesystem without the
    /// orphan-file feature is empty by definition. When the feature is
    /// present, the mount-lifetime orphan info must already have been loaded
    /// after journal replay; treating an uninitialized file as empty could
    /// clear recovery evidence before its entries are inspected.
    pub(crate) fn orphan_file_empty(&self) -> Ext4Result<bool> {
        if !self.superblock().features().has_orphan_file() {
            return Ok(true);
        }
        let entries_per_block = self.orphan_entries_per_block();
        let info = self
            .orphan_info
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?;
        Ok(info.is_empty(entries_per_block))
    }

    pub(crate) fn add_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.ensure_regular_file_orphan_inode_supported(inode)?;
        self.add_orphan_with_fallback(inode, handle)
    }

    pub(crate) fn add_namespace_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.validate_orphan_number(inode.number())?;
        self.add_orphan_with_fallback(inode, handle)
    }

    fn add_orphan_with_fallback(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if inode.orphan_location().is_some() {
            return Ok(());
        }
        if self.superblock().features().has_orphan_file() {
            let info = self
                .orphan_info
                .as_mut()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?;
            match info.add(inode, &self.metadata_io, handle, &self.superblock) {
                Ok(()) => return Ok(()),
                Err(Ext4Error::NoSpace) => {}
                Err(error) => return Err(error),
            }
        }
        self.add_orphan_to_legacy_list(inode, handle)
    }

    fn add_orphan_to_legacy_list(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if inode.orphan_location().is_some() {
            return Ok(());
        }
        let location = OrphanLocation::LegacyList;
        inode.bind_orphan(location)?;
        let head = self.orphan_head();
        let result = self
            .update_inode_orphan_next(inode.number(), head, handle)
            .and_then(|()| self.set_orphan_head(Some(inode.number()), handle));
        if let Err(error) = result {
            inode.clear_orphan(location);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn remove_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        match inode.orphan_location() {
            Some(OrphanLocation::File { index }) => self
                .orphan_info
                .as_mut()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?
                .remove(inode, index, &self.metadata_io, handle, &self.superblock),
            Some(OrphanLocation::LegacyList) => self.remove_orphan_from_legacy_list(inode, handle),
            None => Ok(()),
        }
    }

    #[cfg(unittest)]
    pub(crate) fn cleanup_orphans(&mut self) -> Ext4Result<usize> {
        self.cleanup_orphans_with_policy(RecoveryFlagPolicy::ClearAfterCheckpoint)
    }

    pub(crate) fn cleanup_orphans_preserving_recovery(&mut self) -> Ext4Result<usize> {
        self.cleanup_orphans_with_policy(RecoveryFlagPolicy::PreserveDuringRecovery)
    }

    fn cleanup_orphans_with_policy(
        &mut self,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<usize> {
        let has_orphan_file = self.superblock().features().has_orphan_file();
        if has_orphan_file != self.orphan_info.is_some()
            || (self.has_orphan_present() && !has_orphan_file)
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }
        if (self.orphan_head().is_some() || !self.orphan_file_empty()?) && self.journal.is_none() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite));
        }

        let mut cleaned = 0usize;
        let mut legacy_steps = 0u32;
        while let Some(head) = self.orphan_head() {
            self.advance_orphan_walk(&mut legacy_steps)?;
            match self.recovery_orphan(head, OrphanLocation::LegacyList)? {
                Some(inode) => self.cleanup_tracked_orphan(&inode, recovery_flag_policy)?,
                None => self.discard_invalid_orphan_chain(recovery_flag_policy)?,
            }
            cleaned = cleaned.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }

        let entries_per_block = self.orphan_entries_per_block();
        let orphan_block_count = self
            .orphan_info
            .as_ref()
            .map_or(0, |info| info.blocks.len());
        for block_index in 0..orphan_block_count {
            let physical = self
                .orphan_info
                .as_ref()
                .and_then(|info| info.blocks.get(block_index))
                .map(|block| block.physical)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))?;
            // Keep one immutable snapshot so clearing each slot never forces a
            // full orphan-file rescan during checkpoint-driven recovery.
            let block = self.read_metadata_block(physical)?;
            for offset in 0..entries_per_block {
                let number = codec::le_u32(block.as_ref(), offset * 4)?;
                if number == 0 {
                    continue;
                }
                let entry = block_index
                    .checked_mul(entries_per_block)
                    .and_then(|entry| entry.checked_add(offset))
                    .ok_or(Ext4Error::Overflow)?;
                let location = OrphanLocation::File {
                    index: u32::try_from(entry).map_err(|_| Ext4Error::Overflow)?,
                };
                let Some(inode) = self.recovery_orphan(InodeNumber::new(number), location)? else {
                    // Slots are independent: a bad entry must not prevent
                    // recovery of subsequent inodes (Linux orphan cleanup).
                    continue;
                };
                self.cleanup_tracked_orphan(&inode, recovery_flag_policy)?;
                cleaned = cleaned.checked_add(1).ok_or(Ext4Error::Overflow)?;
            }
        }
        // Bad slots remain on disk. Until filesystem error-state / read-only
        // mount handling exists, report them after salvaging the valid slots.
        if !self.orphan_file_empty()? {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile));
        }
        Ok(cleaned)
    }

    fn recovery_orphan(
        &self,
        number: InodeNumber,
        location: OrphanLocation,
    ) -> Ext4Result<Option<Ext4Inode>> {
        let Some((inode, next)) = self.orphan_iget_with_next(number)? else {
            return Ok(None);
        };
        if matches!(location, OrphanLocation::LegacyList)
            && next.is_some_and(|next| !self.is_valid_orphan_number(next))
        {
            return Ok(None);
        }
        inode.bind_orphan(location)?;
        Ok(Some(inode))
    }

    fn cleanup_tracked_orphan(
        &mut self,
        inode: &Ext4Inode,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        if inode.links_count() == 0 {
            self.cleanup_unlinked_orphan(inode, recovery_flag_policy)
        } else {
            self.cleanup_regular_file_orphan(inode, recovery_flag_policy)
        }
    }

    fn discard_invalid_orphan_chain(
        &mut self,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let journal =
            self.metadata_journal_for_mutation(ORPHAN_HEAD_UPDATE_CREDITS, recovery_flag_policy)?;
        let mut handle = journal.begin(ORPHAN_HEAD_UPDATE_CREDITS)?;
        let result = self.set_orphan_head(None, &mut handle);
        self.complete_metadata_mutation_with_policy(handle, result, recovery_flag_policy)
    }

    fn remove_orphan_from_legacy_list(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let mut previous = None;
        let mut current = self.orphan_head();
        let mut steps = 0u32;
        while let Some(number) = current {
            self.advance_orphan_walk(&mut steps)?;
            self.validate_orphan_number(number)?;

            let next = self.orphan_next(number)?;
            if number == inode.number() {
                match previous {
                    Some(previous) => self.update_inode_orphan_next(previous, next, handle)?,
                    None => self.set_orphan_head(next, handle)?,
                }
                self.update_inode_orphan_next(inode.number(), None, handle)?;
                inode.clear_orphan(OrphanLocation::LegacyList);
                return Ok(());
            }
            previous = Some(number);
            current = next;
        }
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    }

    pub(crate) fn set_orphan_head(
        &mut self,
        head: Option<InodeNumber>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let head = head.map_or(0, |inode| inode.get());
        if head != 0 {
            self.validate_orphan_number(InodeNumber::new(head))?;
        }

        let (block, offset, len) = self.primary_superblock_location()?;
        let access = self.metadata_io.write_access(block, handle)?;
        let mut block_bytes = metadata_access_bytes(&access)?;
        let superblock_bytes = block_bytes
            .get_mut(offset..offset + len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let updated = superblock::set_last_orphan(superblock_bytes, head)?;
        replace_metadata_access_bytes(&access, block_bytes)?;
        lock(&self.allocator).last_orphan = updated.last_orphan();
        Ok(())
    }

    fn ensure_regular_file_orphan_inode_supported(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        self.validate_orphan_number(inode.number())?;
        self.ensure_regular_file_mutation_supported(inode)
    }

    pub(crate) fn validate_orphan_number(&self, inode: InodeNumber) -> Ext4Result<()> {
        if !self.is_valid_orphan_number(inode) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        Ok(())
    }

    pub(crate) fn is_valid_orphan_number(&self, inode: InodeNumber) -> bool {
        inode.get() != 0
            && inode.get() <= self.superblock().inodes_count()
            && inode.get() != crate::disk::inode::EXT4_ROOT_INO
            && !self.is_special_inode_number(inode)
    }

    pub(crate) fn valid_orphan_next(
        &self,
        next: Option<InodeNumber>,
    ) -> Ext4Result<Option<InodeNumber>> {
        if let Some(next) = next {
            self.validate_orphan_number(next)?;
        }
        Ok(next)
    }

    fn orphan_next(&self, inode: InodeNumber) -> Ext4Result<Option<InodeNumber>> {
        let next = match self.raw_inode(inode)?.dtime() {
            0 => None,
            next => Some(InodeNumber::new(next)),
        };
        self.valid_orphan_next(next)
    }

    fn advance_orphan_walk(&self, steps: &mut u32) -> Ext4Result<()> {
        *steps = steps.checked_add(1).ok_or(Ext4Error::Overflow)?;
        if *steps > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        Ok(())
    }
}

fn update_orphan_block_checksum(
    physical: FilesystemBlock,
    checksum_seed: u32,
    bytes: &mut [u8],
    has_checksum: bool,
) -> Ext4Result<()> {
    if !has_checksum {
        return Ok(());
    }
    let entries_bytes = bytes
        .len()
        .checked_sub(EXT4_ORPHAN_BLOCK_TAIL_SIZE)
        .ok_or(Ext4Error::OutOfBounds)?;
    let checksum = orphan_block_checksum(checksum_seed, physical, &bytes[..entries_bytes]);
    bytes[entries_bytes + 4..].copy_from_slice(&checksum.to_le_bytes());
    Ok(())
}

fn orphan_block_checksum(checksum_seed: u32, physical: FilesystemBlock, entries: &[u8]) -> u32 {
    let checksum = checksum::crc32c(checksum_seed, &physical.get().to_le_bytes());
    checksum::crc32c(checksum, entries)
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    use crate::superblock::test_fixtures::{TEST_FREE_BLOCKS, journal_allocator_test_filesystem};

    #[def_test]
    fn recovery_rejects_reserved_out_of_range_and_unallocated_heads() {
        let (filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let first = filesystem.superblock().first_inode();
        let count = filesystem.superblock().inodes_count();
        for number in (1..first).chain(core::iter::once(count.checked_add(1).unwrap())) {
            assert!(
                filesystem
                    .recovery_orphan(InodeNumber::new(number), OrphanLocation::LegacyList)
                    .unwrap()
                    .is_none(),
                "invalid orphan number {number} must not be loaded"
            );
        }
        let unallocated = (first..=count)
            .map(InodeNumber::new)
            .find(|inode| !filesystem.is_inode_allocated(*inode).unwrap())
            .expect("fixture must contain an unallocated inode");
        assert!(
            filesystem
                .recovery_orphan(unallocated, OrphanLocation::LegacyList)
                .unwrap()
                .is_none(),
            "an unallocated inode must not be loaded as an orphan"
        );
    }
}

#[cfg(unittest)]
mod component_tests;

#[cfg(unittest)]
mod file_tests;
