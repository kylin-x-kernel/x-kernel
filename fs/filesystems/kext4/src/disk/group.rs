// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    CorruptKind, Ext4Error, Ext4Result,
    disk::{checksum, codec},
};

const FREE_BLOCKS_COUNT_LO_OFFSET: usize = 12;
const FREE_BLOCKS_COUNT_HI_OFFSET: usize = 44;
const FREE_INODES_COUNT_LO_OFFSET: usize = 14;
const FREE_INODES_COUNT_HI_OFFSET: usize = 46;
const USED_DIRECTORIES_COUNT_LO_OFFSET: usize = 16;
const USED_DIRECTORIES_COUNT_HI_OFFSET: usize = 48;
const FLAGS_OFFSET: usize = 18;
const BLOCK_BITMAP_CHECKSUM_LO_OFFSET: usize = 24;
const INODE_BITMAP_CHECKSUM_LO_OFFSET: usize = 26;
const ITABLE_UNUSED_LO_OFFSET: usize = 28;
const CHECKSUM_OFFSET: usize = 30;
const ITABLE_UNUSED_HI_OFFSET: usize = 50;
const BLOCK_BITMAP_CHECKSUM_HI_OFFSET: usize = 56;
const INODE_BITMAP_CHECKSUM_HI_OFFSET: usize = 58;

const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;

/// Decoded on-disk block group descriptor, mirroring the raw 64-byte layout.
///
/// This is the ext4 `struct ext4_group_desc` analogue: it only decodes and
/// exposes the disk fields and is not held in mount state. The resident
/// management representations are derived from it — [`GroupGeometry`] for the
/// frozen addresses and [`GroupMutableState`] for the allocator-mutable
/// counters and flags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockGroupDescriptor {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    free_blocks_count: u32,
    free_inodes_count: u32,
    used_directories_count: u32,
    flags: u16,
    block_bitmap_checksum: u32,
    inode_bitmap_checksum: u32,
    itable_unused_count: u32,
    checksum: u16,
}

impl BlockGroupDescriptor {
    pub(crate) fn decode(input: &[u8], has_64bit: bool) -> Ext4Result<Self> {
        let high = |offset| -> Ext4Result<u64> {
            if has_64bit {
                Ok(u64::from(codec::le_u32(input, offset)?) << 32)
            } else {
                Ok(0)
            }
        };
        let high_count = |offset| -> Ext4Result<u32> {
            if has_64bit {
                Ok(u32::from(codec::le_u16(input, offset)?) << 16)
            } else {
                Ok(0)
            }
        };

        Ok(Self {
            block_bitmap: u64::from(codec::le_u32(input, 0)?) | high(32)?,
            inode_bitmap: u64::from(codec::le_u32(input, 4)?) | high(36)?,
            inode_table: u64::from(codec::le_u32(input, 8)?) | high(40)?,
            free_blocks_count: u32::from(codec::le_u16(input, 12)?) | high_count(44)?,
            free_inodes_count: u32::from(codec::le_u16(input, 14)?) | high_count(46)?,
            used_directories_count: u32::from(codec::le_u16(input, 16)?) | high_count(48)?,
            flags: codec::le_u16(input, FLAGS_OFFSET)?,
            block_bitmap_checksum: u32::from(codec::le_u16(
                input,
                BLOCK_BITMAP_CHECKSUM_LO_OFFSET,
            )?) | high_count(BLOCK_BITMAP_CHECKSUM_HI_OFFSET)?,
            inode_bitmap_checksum: u32::from(codec::le_u16(
                input,
                INODE_BITMAP_CHECKSUM_LO_OFFSET,
            )?) | high_count(INODE_BITMAP_CHECKSUM_HI_OFFSET)?,
            itable_unused_count: u32::from(codec::le_u16(input, ITABLE_UNUSED_LO_OFFSET)?)
                | high_count(ITABLE_UNUSED_HI_OFFSET)?,
            checksum: codec::le_u16(input, 30)?,
        })
    }

    /// Returns the block containing this group's block bitmap.
    pub const fn block_bitmap(&self) -> u64 {
        self.block_bitmap
    }

    /// Returns the block containing this group's inode bitmap.
    pub const fn inode_bitmap(&self) -> u64 {
        self.inode_bitmap
    }

    /// Returns the first block of this group's inode table.
    pub const fn inode_table(&self) -> u64 {
        self.inode_table
    }

    /// Returns the group's free block count.
    pub const fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count
    }

    /// Returns the group's free inode count.
    pub const fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// Returns the group's used directory count.
    pub const fn used_directories_count(&self) -> u32 {
        self.used_directories_count
    }

    /// Returns the block group flags.
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// Returns the stored block bitmap checksum.
    pub const fn block_bitmap_checksum(&self) -> u32 {
        self.block_bitmap_checksum
    }

    /// Returns the stored inode bitmap checksum.
    pub const fn inode_bitmap_checksum(&self) -> u32 {
        self.inode_bitmap_checksum
    }

    /// Returns how many inode table entries at the end of this group are
    /// unused.
    pub const fn itable_unused_count(&self) -> u32 {
        self.itable_unused_count
    }

    /// Returns the stored descriptor checksum.
    pub const fn checksum(&self) -> u16 {
        self.checksum
    }

    /// Partitions the descriptor into its frozen-address part and its
    /// allocator-mutable part.
    ///
    /// This is the single place that assigns every descriptor field to one
    /// resident home, so the ownership split between [`GroupGeometry`] and
    /// [`GroupMutableState`] is enumerated exactly once and cannot drift.
    pub(crate) fn split(&self) -> (GroupGeometry, GroupMutableState) {
        (
            GroupGeometry {
                block_bitmap: self.block_bitmap,
                inode_bitmap: self.inode_bitmap,
                inode_table: self.inode_table,
            },
            GroupMutableState {
                free_blocks_count: self.free_blocks_count,
                free_inodes_count: self.free_inodes_count,
                used_directories_count: self.used_directories_count,
                flags: self.flags,
                block_bitmap_checksum: self.block_bitmap_checksum,
                inode_bitmap_checksum: self.inode_bitmap_checksum,
            },
        )
    }
}

/// Per-group block addresses frozen at mount time.
///
/// This table is the mount's single read source for the block/inode bitmap
/// and inode-table addresses. Linux keeps these address fields in
/// `sbi->s_group_desc`'s page-cached descriptors and reads them without any
/// lock (`ext4_get_inode_loc` → `ext4_get_group_desc(..., NULL)` →
/// `ext4_inode_table`); KExt4 has no descriptor page cache, so it stores the
/// addresses once in an immutable table and lets the inode-table hot path
/// read them without taking the allocator lock. The allocator's mutable
/// state ([`GroupMutableState`]) deliberately carries no address fields, so
/// the two states cannot diverge field by field. Journal replay may rewrite
/// descriptor blocks, so [`crate::superblock::Ext4SbInfo::reload_mutable_metadata_state`]
/// re-validates replayed descriptors and rejects any address change instead
/// of silently splitting the frozen table from the reloaded descriptors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GroupGeometry {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
}

impl GroupGeometry {
    pub(crate) fn from_descriptor(descriptor: &BlockGroupDescriptor) -> Self {
        descriptor.split().0
    }

    /// Returns the block containing this group's block bitmap.
    pub(crate) const fn block_bitmap(&self) -> u64 {
        self.block_bitmap
    }

    /// Returns the block containing this group's inode bitmap.
    pub(crate) const fn inode_bitmap(&self) -> u64 {
        self.inode_bitmap
    }

    /// Returns the first block of this group's inode table.
    pub(crate) const fn inode_table(&self) -> u64 {
        self.inode_table
    }
}

/// Per-group mutable descriptor state held under the allocator lock.
///
/// Mirrors only the group-descriptor fields that allocation and release
/// transactions update: counters, flags, and bitmap checksums. The
/// block/inode bitmap and inode-table addresses are deliberately absent so
/// the frozen [`GroupGeometry`] table stays the mount's single read source
/// for addresses; fields written only through raw descriptor bytes and
/// re-validated on every decode (descriptor checksum, `itable_unused`) stay
/// on the disk path and are not mirrored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GroupMutableState {
    free_blocks_count: u32,
    free_inodes_count: u32,
    used_directories_count: u32,
    flags: u16,
    block_bitmap_checksum: u32,
    inode_bitmap_checksum: u32,
}

impl GroupMutableState {
    pub(crate) fn from_descriptor(descriptor: &BlockGroupDescriptor) -> Self {
        descriptor.split().1
    }

    /// Returns the group's free block count.
    pub(crate) const fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count
    }

    /// Returns the group's free inode count.
    pub(crate) const fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// Returns the group's used directory count.
    pub(crate) const fn used_directories_count(&self) -> u32 {
        self.used_directories_count
    }

    /// Returns the block group flags.
    #[cfg(test)]
    pub(crate) const fn flags(&self) -> u16 {
        self.flags
    }

    /// Returns the stored block bitmap checksum.
    pub(crate) const fn block_bitmap_checksum(&self) -> u32 {
        self.block_bitmap_checksum
    }

    /// Returns the stored inode bitmap checksum.
    pub(crate) const fn inode_bitmap_checksum(&self) -> u32 {
        self.inode_bitmap_checksum
    }

    /// Returns whether the block bitmap still needs ext4 lazy initialization.
    pub(crate) const fn has_uninit_block_bitmap(&self) -> bool {
        self.flags & EXT4_BG_BLOCK_UNINIT != 0
    }

    /// Returns whether the inode bitmap still needs ext4 lazy initialization.
    pub(crate) const fn has_uninit_inode_bitmap(&self) -> bool {
        self.flags & EXT4_BG_INODE_UNINIT != 0
    }
}

pub(crate) fn update_group_block_bitmap_metadata(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    bitmap_checksum_bytes: usize,
    bitmap: &[u8],
) -> Ext4Result<BlockGroupDescriptor> {
    clear_group_flags(input, EXT4_BG_BLOCK_UNINIT)?;
    update_bitmap_checksum(
        input,
        BLOCK_BITMAP_CHECKSUM_LO_OFFSET,
        BLOCK_BITMAP_CHECKSUM_HI_OFFSET,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        bitmap_checksum_bytes,
        bitmap,
    )?;
    update_group_descriptor_checksum(input, group, checksum_seed, has_metadata_checksum)?;
    BlockGroupDescriptor::decode(input, has_64bit)
}

pub(crate) fn update_group_inode_bitmap_metadata(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    bitmap_checksum_bytes: usize,
    bitmap: &[u8],
) -> Ext4Result<BlockGroupDescriptor> {
    clear_group_flags(input, EXT4_BG_INODE_UNINIT)?;
    update_bitmap_checksum(
        input,
        INODE_BITMAP_CHECKSUM_LO_OFFSET,
        INODE_BITMAP_CHECKSUM_HI_OFFSET,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        bitmap_checksum_bytes,
        bitmap,
    )?;
    update_group_descriptor_checksum(input, group, checksum_seed, has_metadata_checksum)?;
    BlockGroupDescriptor::decode(input, has_64bit)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn update_group_inode_allocation_metadata(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    bitmap_checksum_bytes: usize,
    bitmap: &[u8],
    allocated_bitmap_bit: u32,
    inodes_in_group: u32,
    had_uninit_inode_bitmap: bool,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = update_group_inode_bitmap_metadata(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        bitmap_checksum_bytes,
        bitmap,
    )?;
    let previous_unused = if had_uninit_inode_bitmap {
        inodes_in_group
    } else {
        descriptor.itable_unused_count()
    };
    let itable_unused_count =
        updated_itable_unused_count(previous_unused, allocated_bitmap_bit, inodes_in_group)?;
    set_group_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        ITABLE_UNUSED_LO_OFFSET,
        ITABLE_UNUSED_HI_OFFSET,
        itable_unused_count,
    )
}

pub(crate) fn decrement_group_free_blocks_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    blocks: u32,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = BlockGroupDescriptor::decode(input, has_64bit)?;
    let free_blocks_count = descriptor
        .free_blocks_count()
        .checked_sub(blocks)
        .ok_or(Ext4Error::NoSpace)?;
    set_group_free_blocks_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        free_blocks_count,
    )
}

pub(crate) fn increment_group_free_blocks_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    blocks: u32,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = BlockGroupDescriptor::decode(input, has_64bit)?;
    let free_blocks_count = descriptor
        .free_blocks_count()
        .checked_add(blocks)
        .ok_or(Ext4Error::Overflow)?;
    set_group_free_blocks_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        free_blocks_count,
    )
}

pub(crate) fn decrement_group_free_inodes_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    inodes: u32,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = BlockGroupDescriptor::decode(input, has_64bit)?;
    let free_inodes_count = descriptor
        .free_inodes_count()
        .checked_sub(inodes)
        .ok_or(Ext4Error::NoSpace)?;
    set_group_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        FREE_INODES_COUNT_LO_OFFSET,
        FREE_INODES_COUNT_HI_OFFSET,
        free_inodes_count,
    )
}

pub(crate) fn increment_group_free_inodes_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    inodes: u32,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = BlockGroupDescriptor::decode(input, has_64bit)?;
    let free_inodes_count = descriptor
        .free_inodes_count()
        .checked_add(inodes)
        .ok_or(Ext4Error::Overflow)?;
    set_group_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        FREE_INODES_COUNT_LO_OFFSET,
        FREE_INODES_COUNT_HI_OFFSET,
        free_inodes_count,
    )
}

pub(crate) fn increment_group_used_directories_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = BlockGroupDescriptor::decode(input, has_64bit)?;
    let used_directories_count = descriptor
        .used_directories_count()
        .checked_add(1)
        .ok_or(Ext4Error::Overflow)?;
    set_group_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        USED_DIRECTORIES_COUNT_LO_OFFSET,
        USED_DIRECTORIES_COUNT_HI_OFFSET,
        used_directories_count,
    )
}

pub(crate) fn decrement_group_used_directories_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
) -> Ext4Result<BlockGroupDescriptor> {
    let descriptor = BlockGroupDescriptor::decode(input, has_64bit)?;
    let used_directories_count = descriptor
        .used_directories_count()
        .checked_sub(1)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))?;
    set_group_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        USED_DIRECTORIES_COUNT_LO_OFFSET,
        USED_DIRECTORIES_COUNT_HI_OFFSET,
        used_directories_count,
    )
}

pub(crate) fn set_group_free_blocks_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    free_blocks_count: u32,
) -> Ext4Result<BlockGroupDescriptor> {
    set_group_count(
        input,
        group,
        checksum_seed,
        has_64bit,
        has_metadata_checksum,
        FREE_BLOCKS_COUNT_LO_OFFSET,
        FREE_BLOCKS_COUNT_HI_OFFSET,
        free_blocks_count,
    )
}

#[allow(clippy::too_many_arguments)]
fn set_group_count(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    low_offset: usize,
    high_offset: usize,
    value: u32,
) -> Ext4Result<BlockGroupDescriptor> {
    if !has_64bit && value > u32::from(u16::MAX) {
        return Err(Ext4Error::Overflow);
    }
    let low = (value & u32::from(u16::MAX)) as u16;
    put_u16_at(input, low_offset, low)?;
    if has_64bit {
        let high = (value >> 16) as u16;
        put_u16_at(input, high_offset, high)?;
    }
    update_group_descriptor_checksum(input, group, checksum_seed, has_metadata_checksum)?;
    BlockGroupDescriptor::decode(input, has_64bit)
}

fn clear_group_flags(input: &mut [u8], flags: u16) -> Ext4Result<()> {
    let current = codec::le_u16(input, FLAGS_OFFSET)?;
    let updated = current & !flags;
    put_u16_at(input, FLAGS_OFFSET, updated)
}

#[allow(clippy::too_many_arguments)]
fn update_bitmap_checksum(
    input: &mut [u8],
    low_offset: usize,
    high_offset: usize,
    checksum_seed: u32,
    has_64bit: bool,
    has_metadata_checksum: bool,
    bitmap_checksum_bytes: usize,
    bitmap: &[u8],
) -> Ext4Result<()> {
    if !has_metadata_checksum {
        return Ok(());
    }

    let checksum_input = bitmap
        .get(..bitmap_checksum_bytes)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let checksum = checksum::bitmap_checksum(checksum_input, checksum_seed);
    let low = checksum as u16;
    put_u16_at(input, low_offset, low)?;
    if has_64bit {
        let high = (checksum >> 16) as u16;
        put_u16_at(input, high_offset, high)?;
    }
    Ok(())
}

fn updated_itable_unused_count(
    current: u32,
    allocated_bitmap_bit: u32,
    inodes_in_group: u32,
) -> Ext4Result<u32> {
    if current > inodes_in_group || allocated_bitmap_bit >= inodes_in_group {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
    }
    let first_unused_tail_inode = inodes_in_group
        .checked_sub(current)
        .ok_or(Ext4Error::Overflow)?;
    if allocated_bitmap_bit < first_unused_tail_inode {
        return Ok(current);
    }
    inodes_in_group
        .checked_sub(allocated_bitmap_bit)
        .and_then(|remaining| remaining.checked_sub(1))
        .ok_or(Ext4Error::Overflow)
}

fn update_group_descriptor_checksum(
    input: &mut [u8],
    group: u32,
    checksum_seed: u32,
    has_metadata_checksum: bool,
) -> Ext4Result<()> {
    if has_metadata_checksum {
        let checksum = checksum::group_descriptor_checksum(input, group, checksum_seed)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        put_u16_at(input, CHECKSUM_OFFSET, checksum)?;
    }
    Ok(())
}

fn put_u16_at(input: &mut [u8], offset: usize, value: u16) -> Ext4Result<()> {
    let end = offset.checked_add(2).ok_or(Ext4Error::Overflow)?;
    let dst = input
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    dst.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BlockGroupDescriptor, EXT4_BG_BLOCK_UNINIT, EXT4_BG_INODE_UNINIT,
        decrement_group_free_blocks_count, increment_group_free_blocks_count,
        update_group_block_bitmap_metadata, update_group_inode_allocation_metadata,
        update_group_inode_bitmap_metadata,
    };
    use crate::disk::checksum;

    #[test]
    fn decrements_64bit_group_free_blocks_and_updates_checksum() {
        let seed = 0x1234_5678;
        let group = 7;
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 12, 0x0001);
        put_u16(&mut bytes, 44, 0x0001);
        update_group_checksum(&mut bytes, group, seed);

        let descriptor =
            decrement_group_free_blocks_count(&mut bytes, group, seed, true, true, 2).unwrap();

        assert_eq!(descriptor.free_blocks_count(), 0xffff);
        assert_eq!(
            BlockGroupDescriptor::decode(&bytes, true).unwrap(),
            descriptor
        );
        assert_eq!(
            descriptor.checksum(),
            checksum::group_descriptor_checksum(&bytes, group, seed).unwrap()
        );
    }

    #[test]
    fn increment_rejects_counter_overflow() {
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 12, 0xffff);
        put_u16(&mut bytes, 44, 0xffff);

        assert_eq!(
            increment_group_free_blocks_count(&mut bytes, 0, 0, true, false, 1),
            Err(crate::Ext4Error::Overflow)
        );
    }

    #[test]
    fn non_64bit_counter_rejects_truncated_high_bits() {
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 12, 0xffff);

        assert_eq!(
            increment_group_free_blocks_count(&mut bytes, 0, 0, false, false, 1),
            Err(crate::Ext4Error::Overflow)
        );
    }

    #[test]
    fn decodes_bitmap_checksums_and_itable_unused() {
        let bytes = descriptor_with_bitmap_metadata();

        let descriptor = BlockGroupDescriptor::decode(&bytes, true).unwrap();

        assert_eq!(descriptor.block_bitmap_checksum(), 0x1234_5678);
        assert_eq!(descriptor.inode_bitmap_checksum(), 0x9abc_def0);
        assert_eq!(descriptor.itable_unused_count(), 0x0002_0003);
    }

    #[test]
    fn updates_block_bitmap_checksum_and_clears_uninit_flag() {
        let seed = 0x1122_3344;
        let group = 2;
        let bitmap = [0b1010_0101; 4096];
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 18, EXT4_BG_BLOCK_UNINIT);

        let descriptor = update_group_block_bitmap_metadata(
            &mut bytes,
            group,
            seed,
            true,
            true,
            bitmap.len(),
            &bitmap,
        )
        .unwrap();

        assert_eq!(descriptor.flags() & EXT4_BG_BLOCK_UNINIT, 0);
        assert_eq!(
            descriptor.block_bitmap_checksum(),
            checksum::bitmap_checksum(&bitmap, seed)
        );
        assert_eq!(
            descriptor.checksum(),
            checksum::group_descriptor_checksum(&bytes, group, seed).unwrap()
        );
    }

    #[test]
    fn updates_inode_bitmap_checksum_and_clears_uninit_flag() {
        let seed = 0x5566_7788;
        let group = 3;
        let bitmap = [0b0101_1010; 4096];
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 18, EXT4_BG_INODE_UNINIT);

        let descriptor = update_group_inode_bitmap_metadata(
            &mut bytes,
            group,
            seed,
            true,
            true,
            bitmap.len(),
            &bitmap,
        )
        .unwrap();

        assert_eq!(descriptor.flags() & EXT4_BG_INODE_UNINIT, 0);
        assert_eq!(
            descriptor.inode_bitmap_checksum(),
            checksum::bitmap_checksum(&bitmap, seed)
        );
        assert_eq!(
            descriptor.checksum(),
            checksum::group_descriptor_checksum(&bytes, group, seed).unwrap()
        );
    }

    #[test]
    fn inode_allocation_reduces_itable_unused_tail() {
        let seed = 0x99aa_bbcc;
        let group = 4;
        let bitmap = [0; 4096];
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 18, EXT4_BG_INODE_UNINIT);
        put_u16(&mut bytes, 28, 32);

        let descriptor = update_group_inode_allocation_metadata(
            &mut bytes,
            group,
            seed,
            true,
            true,
            bitmap.len(),
            &bitmap,
            10,
            32,
            true,
        )
        .unwrap();

        assert_eq!(descriptor.itable_unused_count(), 21);
        assert_eq!(
            descriptor.inode_bitmap_checksum(),
            checksum::bitmap_checksum(&bitmap, seed)
        );
    }

    #[test]
    fn inode_allocation_treats_uninit_inode_bitmap_as_fully_unused() {
        let seed = 0x99aa_bbcc;
        let group = 4;
        let bitmap = [0; 4096];
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 18, EXT4_BG_INODE_UNINIT);
        put_u16(&mut bytes, 28, 3);

        let descriptor = update_group_inode_allocation_metadata(
            &mut bytes,
            group,
            seed,
            true,
            true,
            bitmap.len(),
            &bitmap,
            10,
            32,
            true,
        )
        .unwrap();

        assert_eq!(descriptor.itable_unused_count(), 21);
    }

    #[test]
    fn checked_u16_write_reports_truncated_descriptor() {
        let mut bytes = [0; 1];

        assert_eq!(
            super::put_u16_at(&mut bytes, 0, 1),
            Err(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))
        );
    }

    #[test]
    fn checked_u16_write_reports_offset_overflow() {
        let mut bytes = [0; 8];

        assert_eq!(
            super::put_u16_at(&mut bytes, usize::MAX, 1),
            Err(crate::Ext4Error::Overflow)
        );
    }

    fn valid_group_descriptor() -> [u8; 64] {
        let mut bytes = [0; 64];
        put_u32(&mut bytes, 0, 8);
        put_u32(&mut bytes, 4, 9);
        put_u32(&mut bytes, 8, 10);
        bytes
    }

    fn descriptor_with_bitmap_metadata() -> [u8; 64] {
        let mut bytes = valid_group_descriptor();
        put_u16(&mut bytes, 24, 0x5678);
        put_u16(&mut bytes, 26, 0xdef0);
        put_u16(&mut bytes, 28, 0x0003);
        put_u16(&mut bytes, 50, 0x0002);
        put_u16(&mut bytes, 56, 0x1234);
        put_u16(&mut bytes, 58, 0x9abc);
        bytes
    }

    fn update_group_checksum(bytes: &mut [u8], group: u32, seed: u32) {
        let checksum = checksum::group_descriptor_checksum(bytes, group, seed).unwrap();
        put_u16(bytes, 30, checksum);
    }

    fn put_u16(output: &mut [u8], offset: usize, value: u16) {
        output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}
