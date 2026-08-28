// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Single-group inode bitmap allocation primitives.

use super::bitmap::{clear_bitmap_bit, is_bitmap_bit_set, set_bitmap_bit};
use crate::{BlockGroupNumber, CorruptKind, Ext4Error, Ext4Result, InodeNumber};

/// A validated contiguous inode group range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InodeGroupRange {
    group: BlockGroupNumber,
    first_inode: InodeNumber,
    inode_count: u32,
}

impl InodeGroupRange {
    pub(crate) fn new(
        group: BlockGroupNumber,
        first_inode: InodeNumber,
        inode_count: u32,
    ) -> Ext4Result<Self> {
        if inode_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
        }
        Ok(Self {
            group,
            first_inode,
            inode_count,
        })
    }

    pub(crate) const fn group(self) -> BlockGroupNumber {
        self.group
    }

    pub(crate) const fn inode_count(self) -> u32 {
        self.inode_count
    }

    fn contains(self, inode: InodeNumber) -> bool {
        let offset = inode.get().wrapping_sub(self.first_inode.get());
        offset < self.inode_count
    }

    fn inode_at(self, bit_index: u32) -> Ext4Result<InodeNumber> {
        let inode = self
            .first_inode
            .get()
            .checked_add(bit_index)
            .ok_or(Ext4Error::Overflow)?;
        Ok(InodeNumber::new(inode))
    }

    fn bit_index(self, inode: InodeNumber) -> Ext4Result<u32> {
        if !self.contains(inode) {
            return Err(Ext4Error::OutOfBounds);
        }
        inode
            .get()
            .checked_sub(self.first_inode.get())
            .ok_or(Ext4Error::Overflow)
    }
}

/// One inode selected from an inode bitmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InodeAllocation {
    group: BlockGroupNumber,
    inode: InodeNumber,
    bitmap_bit: u32,
}

impl InodeAllocation {
    fn new(range: InodeGroupRange, inode: InodeNumber, bitmap_bit: u32) -> Self {
        Self {
            group: range.group(),
            inode,
            bitmap_bit,
        }
    }

    #[allow(dead_code)]
    pub(crate) const fn group(self) -> BlockGroupNumber {
        self.group
    }

    #[allow(dead_code)]
    pub(crate) const fn inode(self) -> InodeNumber {
        self.inode
    }

    pub(crate) const fn bitmap_bit(self) -> u32 {
        self.bitmap_bit
    }
}

/// Allocate a free inode from the inode bitmap.
/// @goal: Expected inode number, preferably start searching from here.
pub(crate) fn allocate_inode_from_bitmap(
    bitmap: &mut [u8],
    range: InodeGroupRange,
    goal: Option<InodeNumber>,
    mut is_protected: impl FnMut(InodeNumber) -> bool,
) -> Ext4Result<InodeAllocation> {
    // Ensure the bitmap is large enough to hold all inodes in the range.
    validate_inode_bitmap_capacity(bitmap, range)?;
    let start = match goal {
        Some(goal) if range.contains(goal) => range.bit_index(goal)?,
        Some(_) | None => 0,
    };

    if let Some(allocation) = find_free_inode_linear(bitmap, range, start, &mut is_protected)? {
        debug_assert_eq!(
            is_bitmap_bit_set(bitmap, allocation.bitmap_bit()),
            Ok(false),
            "marking an in-use inode bitmap bit as allocated"
        );
        set_bitmap_bit(bitmap, allocation.bitmap_bit())?;
        return Ok(allocation);
    }
    if start != 0
        && let Some(allocation) = find_free_inode_linear(bitmap, range, 0, &mut is_protected)?
    {
        debug_assert_eq!(
            is_bitmap_bit_set(bitmap, allocation.bitmap_bit()),
            Ok(false),
            "marking an in-use inode bitmap bit as allocated"
        );
        set_bitmap_bit(bitmap, allocation.bitmap_bit())?;
        return Ok(allocation);
    }

    Err(Ext4Error::NoSpace)
}

pub(crate) fn release_inode_to_bitmap(
    bitmap: &mut [u8],
    range: InodeGroupRange,
    inode: InodeNumber,
    mut is_protected: impl FnMut(InodeNumber) -> bool,
) -> Ext4Result<InodeAllocation> {
    validate_inode_bitmap_capacity(bitmap, range)?;
    if is_protected(inode) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
    }

    let bitmap_bit = range.bit_index(inode)?;
    if !is_bitmap_bit_set(bitmap, bitmap_bit)? {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap));
    }
    clear_bitmap_bit(bitmap, bitmap_bit)?;
    Ok(InodeAllocation::new(range, inode, bitmap_bit))
}

fn find_free_inode_linear(
    bitmap: &[u8],
    range: InodeGroupRange,
    start: u32,
    is_protected: &mut impl FnMut(InodeNumber) -> bool,
) -> Ext4Result<Option<InodeAllocation>> {
    for bit_index in start..range.inode_count() {
        if is_bitmap_bit_set(bitmap, bit_index)? {
            continue;
        }
        let inode = range.inode_at(bit_index)?;
        if is_protected(inode) {
            continue;
        }
        return Ok(Some(InodeAllocation::new(range, inode, bit_index)));
    }
    Ok(None)
}

fn validate_inode_bitmap_capacity(bitmap: &[u8], range: InodeGroupRange) -> Ext4Result<()> {
    let bits = bitmap.len().checked_mul(8).ok_or(Ext4Error::Overflow)?;
    if usize::try_from(range.inode_count()).map_err(|_| Ext4Error::Overflow)? > bits {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_allocation_skips_reserved_inodes() {
        let range =
            InodeGroupRange::new(BlockGroupNumber::new(0), InodeNumber::new(1), 16).unwrap();
        let mut bitmap = [0xff, 0x03];

        let allocation =
            allocate_inode_from_bitmap(&mut bitmap, range, None, |inode| inode.get() < 11).unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(0));
        assert_eq!(allocation.inode(), InodeNumber::new(11));
        assert_eq!(allocation.bitmap_bit(), 10);
        assert_eq!(bitmap, [0xff, 0x07]);
    }

    #[test]
    fn inode_release_rejects_reserved_and_duplicate_release() {
        let range =
            InodeGroupRange::new(BlockGroupNumber::new(0), InodeNumber::new(1), 16).unwrap();
        let mut bitmap = [0xff, 0x07];

        assert_eq!(
            release_inode_to_bitmap(&mut bitmap, range, InodeNumber::new(2), |inode| {
                inode.get() < 11
            }),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );

        let released =
            release_inode_to_bitmap(&mut bitmap, range, InodeNumber::new(11), |_| false).unwrap();
        assert_eq!(released.inode(), InodeNumber::new(11));
        assert_eq!(bitmap, [0xff, 0x03]);
        assert_eq!(
            release_inode_to_bitmap(&mut bitmap, range, InodeNumber::new(11), |_| false),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
    }
}
