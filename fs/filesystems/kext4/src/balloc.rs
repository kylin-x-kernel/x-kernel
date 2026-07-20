// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block allocator entry points for the mounted ext4 filesystem.

use crate::{
    bitmap_allocator::{self, BlockAllocation, BlockGroupRange, BlockRunAllocation},
    disk::{
        BlockGroupDescriptor, checksum, decrement_group_free_blocks_count,
        increment_group_free_blocks_count, set_group_free_blocks_count, superblock,
        update_group_block_bitmap_metadata,
    },
    error::{CorruptKind, Ext4Error, Ext4Result},
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    superblock::{
        Ext4Filesystem, bitmap_bit_capacity, count_clear_ext4_bitmap_bits, ensure_metadata_credits,
        ext4_bitmap_checksum_matches, ext4_mark_bitmap_end, metadata_access_bytes,
        replace_metadata_access_bytes, set_ext4_bitmap_bit, validate_ext4_bitmap_range_set,
    },
    types::{BlockCount, BlockGroupNumber, FilesystemBlock, InodeNumber, PhysicalBlock},
};

const BLOCK_ALLOCATOR_METADATA_CREDITS: u32 = 3;
const METADATA_BLOCK_RELEASE_CREDITS: u32 = BLOCK_ALLOCATOR_METADATA_CREDITS + 1;

impl Ext4Filesystem {
    #[allow(dead_code)]
    pub(crate) fn allocate_block(
        &mut self,
        goal: Option<FilesystemBlock>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        let request = Ext4AllocationRequest::for_metadata(
            goal,
            BlockCount::new(1),
            BlockCount::new(1),
            Ext4AllocationFlags::EXACT,
        )?;
        self.allocate_blocks_for_write(request, handle)
            .map(|allocation| allocation.first_block_allocation())
    }

    #[allow(dead_code)]
    pub(crate) fn allocate_block_in_group(
        &mut self,
        group: BlockGroupNumber,
        goal: Option<FilesystemBlock>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        let allocation = self.allocate_block_run_in_group(
            group,
            goal,
            BlockCount::new(1),
            BlockCount::new(1),
            handle,
        )?;
        Ok(allocation.first_block_allocation())
    }

    pub(crate) fn allocate_block_run_in_group(
        &mut self,
        group: BlockGroupNumber,
        goal: Option<FilesystemBlock>,
        min_len: BlockCount,
        expected_len: BlockCount,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockRunAllocation> {
        if min_len.get() == 0 || expected_len.get() == 0 || min_len > expected_len {
            return Err(Ext4Error::OutOfBounds);
        }
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        let descriptor = self
            .groups
            .get(group_index)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        if descriptor.free_blocks_count() == 0 && !descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::NoSpace);
        }
        if group.get() == 0 && descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        if self.superblock.free_blocks_count() == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        ensure_metadata_credits(handle, BLOCK_ALLOCATOR_METADATA_CREDITS)?;

        let range = self.block_group_range(group)?;
        let block_bitmap_checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let bitmap_block = FilesystemBlock::new(descriptor.block_bitmap());
        let (descriptor_block, descriptor_offset, descriptor_len) =
            self.group_descriptor_location(group)?;
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;

        let had_uninit_block_bitmap = descriptor.has_uninit_block_bitmap();
        if !had_uninit_block_bitmap {
            let bitmap = self.read_metadata_block(bitmap_block)?;
            self.verify_block_bitmap_checksum_for_group(&descriptor, bitmap.as_ref())?;
            self.validate_block_bitmap_for_group(range, bitmap.as_ref())?;
        }

        let bitmap_access = self.metadata_io.undo_access(bitmap_block, handle)?;
        let descriptor_access = self.metadata_io.undo_access(descriptor_block, handle)?;
        let superblock_access = self.metadata_io.undo_access(superblock_block, handle)?;

        let mut bitmap_bytes = metadata_access_bytes(&bitmap_access)?;
        if had_uninit_block_bitmap {
            self.prepare_block_bitmap_for_group(range, &mut bitmap_bytes, true)?;
        }
        let prepared_free_blocks = if had_uninit_block_bitmap {
            Some(count_clear_ext4_bitmap_bits(
                &bitmap_bytes,
                range.block_count(),
            )?)
        } else {
            None
        };
        let suggested_goal = self
            .ensure_block_group_free_cache(group, range, &bitmap_bytes)?
            .suggest_goal(range, goal, min_len, expected_len)?;
        let allocation = match bitmap_allocator::allocate_block_run_from_bitmap(
            &mut bitmap_bytes,
            range,
            suggested_goal.or(goal),
            min_len,
            expected_len,
            |block| self.is_system_zone_block(block),
        ) {
            Ok(allocation) => allocation,
            Err(Ext4Error::NoSpace) if min_len.get() == 1 => {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
            }
            Err(Ext4Error::NoSpace) => return Err(Ext4Error::NoSpace),
            Err(error) => return Err(error),
        };

        let mut descriptor_bytes = metadata_access_bytes(&descriptor_access)?;
        let descriptor_slice = descriptor_bytes
            .get_mut(descriptor_offset..descriptor_offset + descriptor_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        if let Some(free_blocks) = prepared_free_blocks {
            let _ = set_group_free_blocks_count(
                descriptor_slice,
                group.get(),
                self.superblock.checksum_seed(),
                self.superblock.features().has_64bit(),
                self.superblock.features().has_metadata_checksum(),
                free_blocks,
            )?;
        }
        let _ = decrement_group_free_blocks_count(
            descriptor_slice,
            group.get(),
            self.superblock.checksum_seed(),
            self.superblock.features().has_64bit(),
            self.superblock.features().has_metadata_checksum(),
            allocation.block_count().get(),
        )?;
        let updated_descriptor = update_group_block_bitmap_metadata(
            descriptor_slice,
            group.get(),
            self.superblock.checksum_seed(),
            self.superblock.features().has_64bit(),
            self.superblock.features().has_metadata_checksum(),
            block_bitmap_checksum_bytes,
            &bitmap_bytes,
        )?;

        let mut superblock_bytes = metadata_access_bytes(&superblock_access)?;
        let superblock_slice = superblock_bytes
            .get_mut(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        if let Some(free_blocks) = prepared_free_blocks {
            let previous = descriptor.free_blocks_count();
            if free_blocks > previous {
                let delta = free_blocks
                    .checked_sub(previous)
                    .ok_or(Ext4Error::Overflow)?;
                let _ = superblock::increment_free_blocks_count(superblock_slice, delta)?;
            } else if previous > free_blocks {
                let delta = previous
                    .checked_sub(free_blocks)
                    .ok_or(Ext4Error::Overflow)?;
                let _ = superblock::decrement_free_blocks_count(superblock_slice, delta)?;
            }
        }
        let updated_superblock = superblock::decrement_free_blocks_count(
            superblock_slice,
            allocation.block_count().get(),
        )?;

        replace_metadata_access_bytes(&bitmap_access, bitmap_bytes)?;
        replace_metadata_access_bytes(&descriptor_access, descriptor_bytes)?;
        replace_metadata_access_bytes(&superblock_access, superblock_bytes)?;
        self.block_free_extent_caches
            .get_mut(group_index)
            .and_then(Option::as_mut)
            .ok_or(Ext4Error::OutOfBounds)?
            .mark_allocated(allocation.first_bitmap_bit(), allocation.block_count())?;
        self.groups[group_index] = updated_descriptor;
        self.superblock = updated_superblock;
        Ok(allocation)
    }

    #[allow(dead_code)]
    pub(crate) fn release_allocated_block(
        &mut self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, handle, false, false, None)
    }

    #[allow(dead_code)]
    pub(crate) fn release_allocated_metadata_block(
        &mut self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, handle, true, true, None)
    }

    pub(crate) fn release_allocated_metadata_block_without_revoke(
        &mut self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, handle, false, true, None)
    }

    #[allow(dead_code)]
    pub(crate) fn release_inode_metadata_block(
        &mut self,
        inode: InodeNumber,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, handle, true, true, Some(inode))
    }

    pub(crate) fn release_inode_metadata_block_without_revoke(
        &mut self,
        inode: InodeNumber,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, handle, false, true, Some(inode))
    }

    fn release_allocated_block_inner(
        &mut self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
        revoke_metadata: bool,
        forget_metadata: bool,
        metadata_owner: Option<InodeNumber>,
    ) -> Ext4Result<BlockAllocation> {
        let block = FilesystemBlock::new(block.get());
        let is_owned_metadata_zone =
            metadata_owner.is_some_and(|owner| self.is_inode_owned_system_zone_block(block, owner));
        if self.is_system_zone_block(block) && !is_owned_metadata_zone {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        let required_credits = if revoke_metadata {
            METADATA_BLOCK_RELEASE_CREDITS
        } else {
            BLOCK_ALLOCATOR_METADATA_CREDITS
        };
        ensure_metadata_credits(handle, required_credits)?;
        let group = self.block_group_for_block(block)?;
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        let descriptor = self
            .groups
            .get(group_index)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        if descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        let range = self.block_group_range(group)?;
        let block_bitmap_checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let bitmap_block = FilesystemBlock::new(descriptor.block_bitmap());
        let (descriptor_block, descriptor_offset, descriptor_len) =
            self.group_descriptor_location(group)?;
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;

        let bitmap = self.read_metadata_block(bitmap_block)?;
        self.verify_block_bitmap_checksum_for_group(&descriptor, bitmap.as_ref())?;
        self.validate_block_bitmap_for_group(range, bitmap.as_ref())?;

        let bitmap_access = self.metadata_io.undo_access(bitmap_block, handle)?;
        let descriptor_access = self.metadata_io.undo_access(descriptor_block, handle)?;
        let superblock_access = self.metadata_io.undo_access(superblock_block, handle)?;

        let mut bitmap_bytes = metadata_access_bytes(&bitmap_access)?;
        self.ensure_block_group_free_cache(group, range, &bitmap_bytes)?;
        let released = bitmap_allocator::release_block_to_bitmap(
            &mut bitmap_bytes,
            range,
            block,
            |candidate| self.is_system_zone_block(candidate) && candidate != block,
        )?;

        let mut descriptor_bytes = metadata_access_bytes(&descriptor_access)?;
        let descriptor_slice = descriptor_bytes
            .get_mut(descriptor_offset..descriptor_offset + descriptor_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let _ = increment_group_free_blocks_count(
            descriptor_slice,
            group.get(),
            self.superblock.checksum_seed(),
            self.superblock.features().has_64bit(),
            self.superblock.features().has_metadata_checksum(),
            1,
        )?;
        let updated_descriptor = update_group_block_bitmap_metadata(
            descriptor_slice,
            group.get(),
            self.superblock.checksum_seed(),
            self.superblock.features().has_64bit(),
            self.superblock.features().has_metadata_checksum(),
            block_bitmap_checksum_bytes,
            &bitmap_bytes,
        )?;
        if updated_descriptor.free_blocks_count() > range.block_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }

        let mut superblock_bytes = metadata_access_bytes(&superblock_access)?;
        let superblock_slice = superblock_bytes
            .get_mut(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let updated_superblock = superblock::increment_free_blocks_count(superblock_slice, 1)?;

        if revoke_metadata {
            self.metadata_io.forget_metadata_block(block, handle)?;
        } else if forget_metadata {
            self.metadata_io
                .forget_metadata_block_without_revoke(block, handle)?;
        }
        replace_metadata_access_bytes(&bitmap_access, bitmap_bytes)?;
        replace_metadata_access_bytes(&descriptor_access, descriptor_bytes)?;
        replace_metadata_access_bytes(&superblock_access, superblock_bytes)?;
        self.block_free_extent_caches
            .get_mut(group_index)
            .and_then(Option::as_mut)
            .ok_or(Ext4Error::OutOfBounds)?
            .mark_free(released.bitmap_bit())?;
        self.groups[group_index] = updated_descriptor;
        self.superblock = updated_superblock;
        if is_owned_metadata_zone {
            let owner =
                metadata_owner.ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))?;
            self.remove_system_zone(block.get(), 1, Some(owner))?;
        }
        Ok(released)
    }

    pub(crate) fn prepare_block_bitmap_for_group(
        &self,
        range: BlockGroupRange,
        bitmap: &mut [u8],
        initialize: bool,
    ) -> Ext4Result<()> {
        if initialize {
            bitmap.fill(0);
            self.mark_system_zone_bits_in_block_bitmap(range, bitmap)?;
            ext4_mark_bitmap_end(range.block_count(), bitmap_bit_capacity(bitmap)?, bitmap)?;
            return Ok(());
        }

        self.validate_block_bitmap_for_group(range, bitmap)
    }

    fn validate_block_bitmap_for_group(
        &self,
        range: BlockGroupRange,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        self.validate_system_zone_bits_in_block_bitmap(range, bitmap)?;
        validate_ext4_bitmap_range_set(
            bitmap,
            range.block_count(),
            bitmap_bit_capacity(bitmap)?,
            CorruptKind::InvalidBlockBitmap,
        )
    }

    fn verify_block_bitmap_checksum_for_group(
        &self,
        descriptor: &BlockGroupDescriptor,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        if !self.superblock.features().has_metadata_checksum() {
            return Ok(());
        }

        let checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let input = bitmap
            .get(..checksum_bytes)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let calculated = checksum::bitmap_checksum(input, self.superblock.checksum_seed());
        if !ext4_bitmap_checksum_matches(
            calculated,
            descriptor.block_bitmap_checksum(),
            self.superblock.features().has_64bit(),
        ) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }

        Ok(())
    }

    fn mark_system_zone_bits_in_block_bitmap(
        &self,
        range: BlockGroupRange,
        bitmap: &mut [u8],
    ) -> Ext4Result<()> {
        for bit in 0..range.block_count() {
            let block = range.block_at(bit)?;
            if self.is_system_zone_block(block) {
                set_ext4_bitmap_bit(bitmap, bit)?;
            }
        }
        Ok(())
    }

    fn validate_system_zone_bits_in_block_bitmap(
        &self,
        range: BlockGroupRange,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        for bit in 0..range.block_count() {
            let block = range.block_at(bit)?;
            if self.is_system_zone_block(block)
                && !crate::superblock::is_ext4_bitmap_bit_set(bitmap, bit)?
            {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
            }
        }
        Ok(())
    }
}
