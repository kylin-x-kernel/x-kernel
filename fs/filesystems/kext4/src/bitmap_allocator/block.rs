// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Single-group block bitmap allocation primitives.

use super::bitmap::{clear_bitmap_bit, is_bitmap_bit_set, set_bitmap_bit};
use crate::{
    BlockCount, BlockGroupNumber, CorruptKind, Ext4Error, Ext4Result, FilesystemBlock,
    PhysicalBlock,
};

/// A validated contiguous block group range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockGroupRange {
    group: BlockGroupNumber,
    // First block number of this group.
    first_block: FilesystemBlock,
    // The total number of blocks in this group.
    block_count: u32,
}

impl BlockGroupRange {
    pub(crate) fn new(
        group: BlockGroupNumber,
        first_block: FilesystemBlock,
        block_count: u32,
    ) -> Ext4Result<Self> {
        if block_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        Ok(Self {
            group,
            first_block,
            block_count,
        })
    }

    pub(crate) const fn group(self) -> BlockGroupNumber {
        self.group
    }

    pub(crate) const fn block_count(self) -> u32 {
        self.block_count
    }

    pub(crate) fn contains(self, block: FilesystemBlock) -> bool {
        let offset = block.get().wrapping_sub(self.first_block.get());
        offset < u64::from(self.block_count)
    }

    pub(crate) fn block_at(self, bit_index: u32) -> Ext4Result<FilesystemBlock> {
        let block = self
            .first_block
            .get()
            .checked_add(u64::from(bit_index))
            .ok_or(Ext4Error::Overflow)?;
        Ok(FilesystemBlock::new(block))
    }

    pub(crate) fn bit_index(self, block: FilesystemBlock) -> Ext4Result<u32> {
        if !self.contains(block) {
            return Err(Ext4Error::OutOfBounds);
        }
        u32::try_from(block.get() - self.first_block.get()).map_err(|_| Ext4Error::Overflow)
    }
}

/// One block selected from a block bitmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockAllocation {
    group: BlockGroupNumber,
    block: PhysicalBlock,
    bitmap_bit: u32,
}

impl BlockAllocation {
    fn new(range: BlockGroupRange, block: FilesystemBlock, bitmap_bit: u32) -> Self {
        Self {
            group: range.group(),
            block: PhysicalBlock::new(block.get()),
            bitmap_bit,
        }
    }

    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) const fn group(self) -> BlockGroupNumber {
        self.group
    }

    #[allow(dead_code)]
    pub(crate) const fn block(self) -> PhysicalBlock {
        self.block
    }

    pub(crate) const fn bitmap_bit(self) -> u32 {
        self.bitmap_bit
    }
}

/// One contiguous physical run selected from a block bitmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockRunAllocation {
    // Which group this contiguous space belongs to.
    group: BlockGroupNumber,
    // The physical block number converted from FilesystemBlock.
    first_block: PhysicalBlock,
    // The number of contiguous blocks in a region within the group.
    block_count: BlockCount,
    // Starting position of this range within the group, expressed as a bitmap bit index.
    first_bitmap_bit: u32,
}

impl BlockRunAllocation {
    pub(crate) fn new(
        range: BlockGroupRange,
        first_block: FilesystemBlock,
        block_count: BlockCount,
        first_bitmap_bit: u32,
    ) -> Self {
        debug_assert!(
            first_bitmap_bit
                .checked_add(block_count.get())
                .is_some_and(|end_bit| end_bit <= range.block_count()),
            "block run exceeds group: {first_bitmap_bit}, {block_count}, {}",
            range.block_count()
        );
        Self {
            group: range.group(),
            first_block: PhysicalBlock::new(first_block.get()),
            block_count,
            first_bitmap_bit,
        }
    }

    #[cfg(test)]
    pub(crate) const fn group(self) -> BlockGroupNumber {
        self.group
    }

    pub(crate) const fn first_block(self) -> PhysicalBlock {
        self.first_block
    }

    pub(crate) const fn block_count(self) -> BlockCount {
        self.block_count
    }

    pub(crate) const fn first_bitmap_bit(self) -> u32 {
        self.first_bitmap_bit
    }

    pub(crate) fn first_block_allocation(self) -> BlockAllocation {
        BlockAllocation {
            group: self.group,
            block: self.first_block,
            bitmap_bit: self.first_bitmap_bit,
        }
    }
}

pub(crate) fn allocate_block_run_from_bitmap(
    bitmap: &mut [u8],
    range: BlockGroupRange,
    goal: Option<FilesystemBlock>,
    min_len: BlockCount,
    expected_len: BlockCount,
    mut is_protected: impl FnMut(FilesystemBlock) -> bool,
) -> Ext4Result<BlockRunAllocation> {
    validate_bitmap_capacity(bitmap, range)?;
    validate_run_lengths(min_len, expected_len, range)?;
    let start = match goal {
        Some(goal) if range.contains(goal) => range.bit_index(goal)?,
        Some(_) | None => 0,
    };
    let min_len = min_len.get();
    let expected_len = expected_len.get().min(range.block_count());

    if let Some(allocation) = find_free_run_linear(
        bitmap,
        range,
        start,
        min_len,
        expected_len,
        &mut is_protected,
    )? {
        set_bitmap_run(
            bitmap,
            allocation.first_bitmap_bit(),
            allocation.block_count(),
        )?;
        return Ok(allocation);
    }
    if start != 0
        && let Some(allocation) =
            find_free_run_linear(bitmap, range, 0, min_len, expected_len, &mut is_protected)?
    {
        set_bitmap_run(
            bitmap,
            allocation.first_bitmap_bit(),
            allocation.block_count(),
        )?;
        return Ok(allocation);
    }

    Err(Ext4Error::NoSpace)
}

/// @block: the block to be released.
/// return: block information returned after release.
pub(crate) fn release_block_to_bitmap(
    bitmap: &mut [u8],
    range: BlockGroupRange,
    block: FilesystemBlock,
    mut is_protected: impl FnMut(FilesystemBlock) -> bool,
) -> Ext4Result<BlockAllocation> {
    validate_bitmap_capacity(bitmap, range)?;
    if is_protected(block) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
    }

    let bitmap_bit = range.bit_index(block)?;
    if !is_bitmap_bit_set(bitmap, bitmap_bit)? {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
    }
    clear_bitmap_bit(bitmap, bitmap_bit)?;
    Ok(BlockAllocation::new(range, block, bitmap_bit))
}

fn find_free_run_linear(
    bitmap: &[u8],
    range: BlockGroupRange,
    start: u32,
    min_len: u32,
    expected_len: u32,
    is_protected: &mut impl FnMut(FilesystemBlock) -> bool,
) -> Ext4Result<Option<BlockRunAllocation>> {
    let mut bit_index = start;
    while bit_index < range.block_count() {
        if is_bitmap_bit_set(bitmap, bit_index)? {
            bit_index += 1;
            continue;
        }
        let block = range.block_at(bit_index)?;
        if is_protected(block) {
            bit_index += 1;
            continue;
        }

        let run_len = free_run_len(bitmap, range, bit_index, expected_len, is_protected)?;
        if run_len >= min_len {
            return Ok(Some(BlockRunAllocation::new(
                range,
                block,
                BlockCount::new(run_len),
                bit_index,
            )));
        }
        bit_index = bit_index
            .checked_add(run_len.max(1))
            .ok_or(Ext4Error::Overflow)?;
    }
    Ok(None)
}

fn free_run_len(
    bitmap: &[u8],
    range: BlockGroupRange,
    start: u32,
    expected_len: u32,
    is_protected: &mut impl FnMut(FilesystemBlock) -> bool,
) -> Ext4Result<u32> {
    let end = start
        .checked_add(expected_len)
        .ok_or(Ext4Error::Overflow)?
        .min(range.block_count());
    let mut len = 0u32;
    for bit_index in start..end {
        if is_bitmap_bit_set(bitmap, bit_index)? {
            break;
        }
        let block = range.block_at(bit_index)?;
        if is_protected(block) {
            break;
        }
        len = len.checked_add(1).ok_or(Ext4Error::Overflow)?;
    }
    Ok(len)
}

fn set_bitmap_run(bitmap: &mut [u8], first_bit: u32, block_count: BlockCount) -> Ext4Result<()> {
    let end = first_bit
        .checked_add(block_count.get())
        .ok_or(Ext4Error::Overflow)?;
    for bit in first_bit..end {
        set_bitmap_bit(bitmap, bit)?;
    }
    Ok(())
}

fn validate_run_lengths(
    min_len: BlockCount,
    expected_len: BlockCount,
    range: BlockGroupRange,
) -> Ext4Result<()> {
    if min_len.get() == 0 || expected_len.get() == 0 || min_len > expected_len {
        return Err(Ext4Error::OutOfBounds);
    }
    if min_len.get() > range.block_count() {
        return Err(Ext4Error::NoSpace);
    }
    Ok(())
}

fn validate_bitmap_capacity(bitmap: &[u8], range: BlockGroupRange) -> Ext4Result<()> {
    let bits = bitmap.len().checked_mul(8).ok_or(Ext4Error::Overflow)?;
    if usize::try_from(range.block_count()).map_err(|_| Ext4Error::Overflow)? > bits {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_uses_goal_when_available() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(2), FilesystemBlock::new(100), 32).unwrap();
        let mut bitmap = [0xff, 0x0f, 0, 0];

        let allocation = allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            Some(FilesystemBlock::new(117)),
            BlockCount::new(1),
            BlockCount::new(1),
            |_| false,
        )
        .unwrap()
        .first_block_allocation();

        assert_eq!(allocation.group(), BlockGroupNumber::new(2));
        assert_eq!(allocation.block(), PhysicalBlock::new(117));
        assert_eq!(allocation.bitmap_bit(), 17);
        assert_eq!(bitmap[2] & 0b0000_0010, 0b0000_0010);
    }

    #[test]
    fn allocation_skips_protected_blocks_and_wraps() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 8).unwrap();
        let mut bitmap = [0b1100_1111];

        let allocation = allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            Some(FilesystemBlock::new(15)),
            BlockCount::new(1),
            BlockCount::new(1),
            |block| block == FilesystemBlock::new(14),
        )
        .unwrap()
        .first_block_allocation();

        assert_eq!(allocation.block(), PhysicalBlock::new(15));
        assert_eq!(bitmap, [0b1110_1111]);
    }

    #[test]
    fn allocation_reports_no_space_without_mutating_bitmap() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 4).unwrap();
        let mut bitmap = [0b0000_1110];
        let before = bitmap;

        assert_eq!(
            allocate_block_run_from_bitmap(
                &mut bitmap,
                range,
                None,
                BlockCount::new(1),
                BlockCount::new(1),
                |block| { block == FilesystemBlock::new(10) },
            ),
            Err(Ext4Error::NoSpace)
        );
        assert_eq!(bitmap, before);
    }

    #[test]
    fn run_allocation_uses_goal_and_marks_contiguous_bits() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
        let mut bitmap = [0b1111_1111, 0];

        let allocation = allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            Some(FilesystemBlock::new(18)),
            BlockCount::new(4),
            BlockCount::new(4),
            |_| false,
        )
        .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(0));
        assert_eq!(allocation.first_block(), PhysicalBlock::new(18));
        assert_eq!(allocation.block_count(), BlockCount::new(4));
        assert_eq!(allocation.first_bitmap_bit(), 8);
        assert_eq!(bitmap, [0xff, 0b0000_1111]);
    }

    #[test]
    fn run_allocation_can_return_partial_run_above_minimum() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
        let mut bitmap = [0b1111_1111, 0b1111_1100];

        let allocation = allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            Some(FilesystemBlock::new(18)),
            BlockCount::new(2),
            BlockCount::new(4),
            |_| false,
        )
        .unwrap();

        assert_eq!(allocation.first_block(), PhysicalBlock::new(18));
        assert_eq!(allocation.block_count(), BlockCount::new(2));
        assert_eq!(bitmap, [0xff, 0xff]);
    }

    #[test]
    fn run_allocation_rejects_runs_shorter_than_minimum_without_mutating_bitmap() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
        let mut bitmap = [0b1111_1111, 0b1111_1100];
        let before = bitmap;

        assert_eq!(
            allocate_block_run_from_bitmap(
                &mut bitmap,
                range,
                Some(FilesystemBlock::new(18)),
                BlockCount::new(3),
                BlockCount::new(4),
                |_| false,
            ),
            Err(Ext4Error::NoSpace)
        );
        assert_eq!(bitmap, before);
    }

    #[test]
    fn run_allocation_stops_at_protected_blocks() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
        let mut bitmap = [0b0011_1111, 0xff];

        let allocation = allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            Some(FilesystemBlock::new(16)),
            BlockCount::new(2),
            BlockCount::new(4),
            |block| block == FilesystemBlock::new(18),
        )
        .unwrap();

        assert_eq!(allocation.first_block(), PhysicalBlock::new(16));
        assert_eq!(allocation.block_count(), BlockCount::new(2));
        assert_eq!(bitmap, [0xff, 0xff]);
    }

    #[test]
    fn release_clears_allocated_block_and_rejects_duplicate_release() {
        let range =
            BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(50), 8).unwrap();
        let mut bitmap = [0b0000_0100];

        let released =
            release_block_to_bitmap(&mut bitmap, range, FilesystemBlock::new(52), |_| false)
                .unwrap();

        assert_eq!(released.block(), PhysicalBlock::new(52));
        assert_eq!(bitmap, [0]);
        assert_eq!(
            release_block_to_bitmap(&mut bitmap, range, FilesystemBlock::new(52), |_| false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
    }
}
