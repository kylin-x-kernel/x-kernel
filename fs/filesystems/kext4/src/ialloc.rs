// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode allocator entry points for the mounted ext4 filesystem.

use alloc::vec;

use crate::{
    bitmap_allocator::{self, InodeAllocation},
    disk::{
        GroupMutableState, checksum, decrement_group_free_inodes_count,
        decrement_group_used_directories_count, increment_group_free_inodes_count,
        increment_group_used_directories_count, set_group_free_blocks_count, superblock,
        update_group_block_bitmap_metadata, update_group_inode_allocation_metadata,
        update_group_inode_bitmap_metadata,
    },
    error::{CorruptKind, Ext4Error, Ext4Result},
    inode::{InodeInitialization, InodeKind},
    superblock::{
        Ext4SbInfo, bitmap_bit_capacity, count_clear_ext4_bitmap_bits, ensure_metadata_credits,
        ext4_bitmap_checksum_matches, ext4_mark_bitmap_end, is_ext4_bitmap_bit_set, lock,
        replace_metadata_access_bytes, validate_ext4_bitmap_range_set,
    },
    types::{BlockGroupNumber, FilesystemBlock, InodeNumber},
};

const INODE_ALLOCATOR_METADATA_CREDITS: u32 = 4;

impl Ext4SbInfo {
    pub(crate) fn is_inode_allocated(&self, inode: InodeNumber) -> Ext4Result<bool> {
        let group = self.block_group_for_inode(inode)?;
        let geometry = self.group_geometry(group).ok_or(Ext4Error::OutOfBounds)?;
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        let descriptor = lock(&self.allocator)
            .groups
            .get(group_index)
            .copied()
            .ok_or(Ext4Error::OutOfBounds)?;

        // An uninitialized inode bitmap represents an entirely free
        // non-reserved range, so no legacy orphan may point into it.
        if descriptor.has_uninit_inode_bitmap() {
            return Ok(false);
        }

        let range = self.inode_group_range(group)?;
        let bitmap = self.read_metadata_block(FilesystemBlock::new(geometry.inode_bitmap()))?;
        self.verify_inode_bitmap_checksum_for_group(
            descriptor.inode_bitmap_checksum(),
            bitmap.as_ref(),
        )?;
        self.validate_inode_bitmap_for_group(range, bitmap.as_ref())?;

        let bit_index = inode
            .get()
            .checked_sub(1)
            .ok_or(Ext4Error::OutOfBounds)?
            .checked_rem(self.superblock().inodes_per_group())
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))?;
        is_ext4_bitmap_bit_set(bitmap.as_ref(), bit_index)
    }

    #[allow(dead_code)]
    pub(crate) fn allocate_inode(
        &self,
        parent: Option<InodeNumber>,
        initialization: InodeInitialization,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<InodeAllocation> {
        self.allocate_inode_with_name(parent, None, initialization, handle)
    }

    pub(crate) fn allocate_named_inode(
        &self,
        parent: Option<InodeNumber>,
        child_name: &[u8],
        initialization: InodeInitialization,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<InodeAllocation> {
        self.allocate_inode_with_name(parent, Some(child_name), initialization, handle)
    }

    fn allocate_inode_with_name(
        &self,
        parent: Option<InodeNumber>,
        child_name: Option<&[u8]>,
        initialization: InodeInitialization,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<InodeAllocation> {
        if self.free_inodes_count() == 0 {
            return Err(Ext4Error::NoSpace);
        }
        let start = if initialization.kind().is_directory() {
            self.find_group_orlov(parent, child_name)?
        } else {
            self.find_group_other(parent)?
        };
        let mut first_corruption = None;
        for group in self.group_scan_order(start)? {
            let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
            // Peek this group's cheap hint under a short lock, then attempt
            // the allocation in its own critical section: the allocator lock
            // is not reentrant, so the scan cannot hold it across the entry.
            let (free_inodes, uninit_inode_bitmap) = {
                let alloc = lock(&self.allocator);
                let descriptor = alloc
                    .groups
                    .get(group_index)
                    .ok_or(Ext4Error::OutOfBounds)?;
                (
                    descriptor.free_inodes_count(),
                    descriptor.has_uninit_inode_bitmap(),
                )
            };
            if free_inodes == 0 && !uninit_inode_bitmap {
                continue;
            }
            match self.allocate_inode_in_group(group, initialization, handle) {
                Ok(allocation) => return Ok(allocation),
                // A group drained by a concurrent allocator is not
                // ENOSPC for the caller: keep scanning the remaining
                // groups, like the block path does.
                Err(Ext4Error::NoSpace) => continue,
                Err(
                    error @ Ext4Error::Corrupt(
                        CorruptKind::InvalidInodeBitmap | CorruptKind::InvalidBlockBitmap,
                    ),
                ) => {
                    first_corruption.get_or_insert(error);
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(first_corruption.unwrap_or(Ext4Error::NoSpace))
    }

    #[allow(dead_code)]
    pub(crate) fn allocate_inode_in_group(
        &self,
        group: BlockGroupNumber,
        initialization: InodeInitialization,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<InodeAllocation> {
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        // Geometry and block locations come from frozen mount tables and
        // need no lock; resolve them before the allocator lock so the
        // critical section starts with every address already known.
        let range = self.inode_group_range(group)?;
        let block_range = self.block_group_range(group)?;
        let block_bitmap_checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let inode_bitmap_checksum_bytes = self.inode_bitmap_checksum_bytes()?;
        let geometry = self.group_geometry(group).ok_or(Ext4Error::OutOfBounds)?;
        let inode_bitmap_block = FilesystemBlock::new(geometry.inode_bitmap());
        let block_bitmap_block = FilesystemBlock::new(geometry.block_bitmap());
        let inode_table_start = geometry.inode_table();
        let (descriptor_block, descriptor_offset, descriptor_len) =
            self.group_descriptor_location(group)?;
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        // Warm the metadata cache outside the allocator lock so a
        // cold-cache device read does not serialize every allocator behind
        // the mutex (Linux reads the bitmap buffer before
        // `ext4_lock_group`). The authoritative snapshots are re-read
        // inside the lock below. Prefetching the bitmaps of an uninit
        // group costs a harmless cached read each; the block bitmap is
        // only touched in-lock on the uninit path, so prefetching keeps
        // that write_access from cold-reading inside the critical section.
        // The inode-table block cannot be prefetched because its address
        // depends on the inode selected from the bitmap.
        self.prefetch_metadata_blocks(&[
            inode_bitmap_block,
            block_bitmap_block,
            descriptor_block,
            superblock_block,
        ])?;

        // The allocator lock still spans the in-lock read-modify-write so
        // concurrent callers cannot select the same inode from the same
        // bitmap snapshot.
        let mut alloc = lock(&self.allocator);
        let descriptor = alloc
            .groups
            .get(group_index)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        if descriptor.free_inodes_count() == 0 && !descriptor.has_uninit_inode_bitmap() {
            return Err(Ext4Error::NoSpace);
        }
        if group.get() == 0 && descriptor.has_uninit_inode_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
        }
        if group.get() == 0 && descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        if alloc.free_inodes_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
        }
        let required_credits = if descriptor.has_uninit_block_bitmap() {
            INODE_ALLOCATOR_METADATA_CREDITS
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?
        } else {
            INODE_ALLOCATOR_METADATA_CREDITS
        };
        ensure_metadata_credits(handle, required_credits)?;

        let had_uninit_inode_bitmap = descriptor.has_uninit_inode_bitmap();
        let mut prepared_block_bitmap = None;
        let mut prepared_block_free_count = None;
        if descriptor.has_uninit_block_bitmap() {
            let mut block_bitmap_bytes = vec![
                0;
                usize::try_from(self.layout().block_size())
                    .map_err(|_| Ext4Error::Overflow)?
            ];
            self.prepare_block_bitmap_for_group(block_range, &mut block_bitmap_bytes, true)?;
            prepared_block_free_count = Some(count_clear_ext4_bitmap_bits(
                &block_bitmap_bytes,
                block_range.block_count(),
            )?);
            prepared_block_bitmap = Some(block_bitmap_bytes);
        }

        let mut bitmap_bytes = if had_uninit_inode_bitmap {
            vec![0; usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?]
        } else {
            let bitmap = self.read_metadata_block(inode_bitmap_block)?;
            self.verify_inode_bitmap_checksum_for_group(
                descriptor.inode_bitmap_checksum(),
                bitmap.as_ref(),
            )?;
            self.validate_inode_bitmap_for_group(range, bitmap.as_ref())?;
            bitmap.as_ref().to_vec()
        };
        if had_uninit_inode_bitmap {
            self.prepare_inode_bitmap_for_group(range, &mut bitmap_bytes, true)?;
        }
        let allocation = match bitmap_allocator::allocate_inode_from_bitmap(
            &mut bitmap_bytes,
            range,
            None,
            |inode| self.is_reserved_inode(inode),
        ) {
            Ok(allocation) => allocation,
            Err(Ext4Error::NoSpace) => {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
            }
            Err(error) => return Err(error),
        };
        let inode_table_block = self.inode_table_entry_block_in_group(
            allocation.inode(),
            group_index,
            inode_table_start,
        )?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        self.initialize_inode_table_entry_in_group(
            &mut inode_table_bytes,
            allocation.inode(),
            group_index,
            inode_table_start,
            initialization,
        )?;

        let mut descriptor_bytes = self
            .read_metadata_block(descriptor_block)?
            .as_ref()
            .to_vec();
        let descriptor_slice = descriptor_bytes
            .get_mut(descriptor_offset..descriptor_offset + descriptor_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        if let (Some(block_bitmap), Some(free_blocks)) =
            (prepared_block_bitmap.as_ref(), prepared_block_free_count)
        {
            let _ = set_group_free_blocks_count(
                descriptor_slice,
                group.get(),
                self.superblock().checksum_seed(),
                self.superblock().features().has_64bit(),
                self.superblock().features().has_metadata_checksum(),
                free_blocks,
            )?;
            let _ = update_group_block_bitmap_metadata(
                descriptor_slice,
                group.get(),
                self.superblock().checksum_seed(),
                self.superblock().features().has_64bit(),
                self.superblock().features().has_metadata_checksum(),
                block_bitmap_checksum_bytes,
                block_bitmap,
            )?;
        }
        let _ = decrement_group_free_inodes_count(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            1,
        )?;
        let mut updated_descriptor = update_group_inode_allocation_metadata(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            inode_bitmap_checksum_bytes,
            &bitmap_bytes,
            allocation.bitmap_bit(),
            range.inode_count(),
            had_uninit_inode_bitmap,
        )?;
        if initialization.kind().is_directory() {
            updated_descriptor = increment_group_used_directories_count(
                descriptor_slice,
                group.get(),
                self.superblock().checksum_seed(),
                self.superblock().features().has_64bit(),
                self.superblock().features().has_metadata_checksum(),
            )?;
        }

        let mut superblock_bytes = self
            .read_metadata_block(superblock_block)?
            .as_ref()
            .to_vec();
        let superblock_slice = superblock_bytes
            .get_mut(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        if let Some(free_blocks) = prepared_block_free_count {
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
        let updated_superblock = superblock::decrement_free_inodes_count(superblock_slice, 1)?;

        // Publication stays inside the allocator lock: the bitmap bytes are
        // the bit-level source of truth, so releasing the lock before the
        // replacements land would let a concurrent allocator observe the
        // updated counters against a stale bitmap and double-allocate.
        let bitmap_access = self.metadata_io.write_access(inode_bitmap_block, handle)?;
        let block_bitmap_access = if descriptor.has_uninit_block_bitmap() {
            Some(self.metadata_io.write_access(block_bitmap_block, handle)?)
        } else {
            None
        };
        let descriptor_access = self.metadata_io.write_access(descriptor_block, handle)?;
        let superblock_access = self.metadata_io.write_access(superblock_block, handle)?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&bitmap_access, bitmap_bytes)?;
        if let (Some(access), Some(block_bitmap_bytes)) =
            (block_bitmap_access.as_ref(), prepared_block_bitmap)
        {
            replace_metadata_access_bytes(access, block_bitmap_bytes)?;
        }
        replace_metadata_access_bytes(&descriptor_access, descriptor_bytes)?;
        replace_metadata_access_bytes(&superblock_access, superblock_bytes)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        alloc.groups[group_index] = GroupMutableState::from_descriptor(&updated_descriptor);
        alloc.free_blocks_count = updated_superblock.on_disk_free_blocks_count();
        alloc.free_inodes_count = updated_superblock.on_disk_free_inodes_count();
        if initialization.kind().is_directory() {
            alloc.used_directories_count = alloc.used_directories_count.saturating_add(1);
        }
        Ok(allocation)
    }

    #[allow(dead_code)]
    pub(crate) fn release_allocated_inode(
        &self,
        inode: InodeNumber,
        kind: InodeKind,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<InodeAllocation> {
        if self.is_reserved_inode(inode) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
        }
        ensure_metadata_credits(handle, INODE_ALLOCATOR_METADATA_CREDITS)?;
        let group = self.block_group_for_inode(inode)?;
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        // Geometry and block locations come from frozen mount tables and
        // need no lock; the freed inode is known up front, so every block
        // this path touches is addressable before the allocator lock.
        let range = self.inode_group_range(group)?;
        let inode_bitmap_checksum_bytes = self.inode_bitmap_checksum_bytes()?;
        let geometry = self.group_geometry(group).ok_or(Ext4Error::OutOfBounds)?;
        let inode_bitmap_block = FilesystemBlock::new(geometry.inode_bitmap());
        let inode_table_start = geometry.inode_table();
        let (descriptor_block, descriptor_offset, descriptor_len) =
            self.group_descriptor_location(group)?;
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        let inode_table_block =
            self.inode_table_entry_block_in_group(inode, group_index, inode_table_start)?;
        // Warm the metadata cache outside the allocator lock so a
        // cold-cache device read does not serialize every allocator behind
        // the mutex; the authoritative snapshots are re-read inside the
        // lock below.
        self.prefetch_metadata_blocks(&[
            inode_bitmap_block,
            inode_table_block,
            descriptor_block,
            superblock_block,
        ])?;

        // The allocator lock still spans the in-lock read-modify-write so
        // concurrent releases keep the bitmap and counters consistent with
        // one another.
        let mut alloc = lock(&self.allocator);
        let descriptor = alloc
            .groups
            .get(group_index)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        if descriptor.has_uninit_inode_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
        }

        let bitmap = self.read_metadata_block(inode_bitmap_block)?;
        self.verify_inode_bitmap_checksum_for_group(
            descriptor.inode_bitmap_checksum(),
            bitmap.as_ref(),
        )?;
        self.validate_inode_bitmap_for_group(range, bitmap.as_ref())?;

        let mut bitmap_bytes = bitmap.as_ref().to_vec();
        let released =
            bitmap_allocator::release_inode_to_bitmap(&mut bitmap_bytes, range, inode, |inode| {
                self.is_reserved_inode(inode)
            })?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        self.clear_inode_table_entry_in_group(
            &mut inode_table_bytes,
            inode,
            group_index,
            inode_table_start,
        )?;

        let mut descriptor_bytes = self
            .read_metadata_block(descriptor_block)?
            .as_ref()
            .to_vec();
        let descriptor_slice = descriptor_bytes
            .get_mut(descriptor_offset..descriptor_offset + descriptor_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let mut updated_descriptor = increment_group_free_inodes_count(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            1,
        )?;
        if updated_descriptor.free_inodes_count() > range.inode_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
        }
        updated_descriptor = update_group_inode_bitmap_metadata(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            inode_bitmap_checksum_bytes,
            &bitmap_bytes,
        )?;
        if kind.is_directory() {
            updated_descriptor = decrement_group_used_directories_count(
                descriptor_slice,
                group.get(),
                self.superblock().checksum_seed(),
                self.superblock().features().has_64bit(),
                self.superblock().features().has_metadata_checksum(),
            )?;
        }

        let mut superblock_bytes = self
            .read_metadata_block(superblock_block)?
            .as_ref()
            .to_vec();
        let superblock_slice = superblock_bytes
            .get_mut(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let updated_superblock = superblock::increment_free_inodes_count(superblock_slice, 1)?;

        // Publication stays inside the allocator lock: the bitmap bytes are
        // the bit-level source of truth, so releasing the lock before the
        // replacements land would let a concurrent allocator observe the
        // updated counters against a stale bitmap and double-allocate.
        let bitmap_access = self.metadata_io.write_access(inode_bitmap_block, handle)?;
        let descriptor_access = self.metadata_io.write_access(descriptor_block, handle)?;
        let superblock_access = self.metadata_io.write_access(superblock_block, handle)?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&bitmap_access, bitmap_bytes)?;
        replace_metadata_access_bytes(&descriptor_access, descriptor_bytes)?;
        replace_metadata_access_bytes(&superblock_access, superblock_bytes)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        alloc.groups[group_index] = GroupMutableState::from_descriptor(&updated_descriptor);
        alloc.free_blocks_count = updated_superblock.on_disk_free_blocks_count();
        alloc.free_inodes_count = updated_superblock.on_disk_free_inodes_count();
        if kind.is_directory() {
            alloc.used_directories_count = alloc.used_directories_count.saturating_sub(1);
        }
        Ok(released)
    }

    fn prepare_inode_bitmap_for_group(
        &self,
        range: crate::bitmap_allocator::InodeGroupRange,
        bitmap: &mut [u8],
        initialize: bool,
    ) -> Ext4Result<()> {
        if initialize {
            bitmap.fill(0);
            ext4_mark_bitmap_end(range.inode_count(), bitmap_bit_capacity(bitmap)?, bitmap)?;
            return Ok(());
        }
        self.validate_inode_bitmap_for_group(range, bitmap)
    }

    fn validate_inode_bitmap_for_group(
        &self,
        range: crate::bitmap_allocator::InodeGroupRange,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        validate_ext4_bitmap_range_set(
            bitmap,
            range.inode_count(),
            bitmap_bit_capacity(bitmap)?,
            CorruptKind::InvalidInodeBitmap,
        )?;
        if range.group() == BlockGroupNumber::new(0) {
            let reserved_bits = self
                .superblock()
                .first_inode()
                .checked_sub(1)
                .ok_or(Ext4Error::Overflow)?
                .min(range.inode_count());
            validate_ext4_bitmap_range_set(
                bitmap,
                0,
                reserved_bits,
                CorruptKind::InvalidInodeBitmap,
            )?;
        }
        Ok(())
    }

    fn verify_inode_bitmap_checksum_for_group(
        &self,
        expected_checksum: u32,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }

        let checksum_bytes = self.inode_bitmap_checksum_bytes()?;
        let input = bitmap
            .get(..checksum_bytes)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let calculated = checksum::bitmap_checksum(input, self.superblock().checksum_seed());
        if !ext4_bitmap_checksum_matches(
            calculated,
            expected_checksum,
            self.superblock().features().has_64bit(),
        ) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
        }

        Ok(())
    }
}
