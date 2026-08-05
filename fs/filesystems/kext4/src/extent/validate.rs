// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{BlockMapping, BlockMappingFlags};
use crate::{
    BlockCount, CorruptKind, Ext4Error, Ext4Result, PhysicalBlock, disk::extent as disk_extent,
};

pub(super) fn decode_header(bytes: &[u8]) -> Ext4Result<disk_extent::ExtentHeader> {
    if !disk_extent::ExtentHeader::has_magic(bytes)? {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let header = disk_extent::ExtentHeader::decode(bytes)?;
    if !header.validate() {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    if header.max() == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    if header.entries() == 0 && header.depth() > 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let capacity_bytes = usize::from(header.max())
        .checked_mul(disk_extent::EXTENT_ENTRY_SIZE)
        .and_then(|bytes| bytes.checked_add(disk_extent::EXTENT_HEADER_SIZE))
        .ok_or(Ext4Error::Overflow)?;
    if capacity_bytes > bytes.len() {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    Ok(header)
}

pub(super) fn validate_extent_entries(
    bytes: &[u8],
    header: disk_extent::ExtentHeader,
    expected_lblk: Option<u32>,
    upper_lblk: Option<u32>,
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<()> {
    let entry_count = usize::from(header.entries());
    if entry_count == 0 {
        return Ok(());
    }

    if header.depth() == 0 {
        let mut cur: u32 = 0;
        for i in 0..entry_count {
            let extent = decode_leaf(bytes, i)?;
            let lblock = extent.block();
            let len = u32::from(extent.actual_len());

            if i == 0 && expected_lblk.is_some_and(|expected| lblock != expected) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }

            let end = lblock.checked_add(len).ok_or(Ext4Error::Overflow)?;
            if len == 0 || end <= lblock {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            if upper_lblk.is_some_and(|limit| lblock >= limit || end > limit) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }

            if lblock < cur {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            cur = end;

            if !is_valid_physical_block(extent.start().get(), u64::from(len)) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
        }
    } else {
        let mut cur: u32 = 0;
        for i in 0..entry_count {
            let index = decode_index(bytes, i)?;
            let lblock = index.block();

            if i == 0 && expected_lblk.is_some_and(|expected| lblock != expected) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }

            if lblock < cur {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            if upper_lblk.is_some_and(|limit| lblock >= limit) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            cur = lblock.checked_add(1).ok_or(Ext4Error::Overflow)?;

            if index.leaf().get() == 0 {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }

            if !is_valid_physical_block(index.leaf().get(), 1) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SelectedExtentIndex {
    pub(super) entry: usize,
    pub(super) index: disk_extent::ExtentIndex,
    pub(super) next_lblk: Option<u32>,
}

pub(super) fn find_index(
    bytes: &[u8],
    header: disk_extent::ExtentHeader,
    logical: u32,
) -> Ext4Result<SelectedExtentIndex> {
    if header.entries() == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }

    let mut left = 0usize;
    let mut right = usize::from(header.entries());
    while left < right {
        let middle = left + (right - left) / 2;
        let index = decode_index(bytes, middle)?;
        if index.block() <= logical {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    let selected = left.saturating_sub(1);
    let index = decode_index(bytes, selected)?;
    if index.leaf().get() == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let next_lblk = if selected + 1 < usize::from(header.entries()) {
        Some(decode_index(bytes, selected + 1)?.block())
    } else {
        None
    };
    Ok(SelectedExtentIndex {
        entry: selected,
        index,
        next_lblk,
    })
}

pub(super) fn map_leaf(
    bytes: &[u8],
    header: disk_extent::ExtentHeader,
    logical: u32,
    upper_lblk: Option<u32>,
) -> Ext4Result<BlockMapping> {
    if header.entries() == 0 {
        let len = hole_len_to(logical, upper_lblk)?;
        return Ok(BlockMapping::Hole {
            len: BlockCount::new(len),
            flags: BlockMappingFlags::empty(),
        });
    }

    let mut left = 0usize;
    let mut right = usize::from(header.entries());
    while left < right {
        let middle = left + (right - left) / 2;
        let extent = decode_leaf(bytes, middle)?;
        if extent.block() <= logical {
            left = middle + 1;
        } else {
            right = middle;
        }
    }

    if let Some(selected) = left.checked_sub(1) {
        let extent = decode_leaf(bytes, selected)?;
        let len = u32::from(extent.actual_len());
        if len == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        let end = extent.block().checked_add(len).ok_or(Ext4Error::Overflow)?;
        if logical < end {
            let offset = logical
                .checked_sub(extent.block())
                .ok_or(Ext4Error::Overflow)?;
            let mapped_len = len.checked_sub(offset).ok_or(Ext4Error::Overflow)?;
            let physical = extent
                .start()
                .get()
                .checked_add(u64::from(offset))
                .ok_or(Ext4Error::Overflow)?;
            if extent.is_unwritten() {
                return Ok(BlockMapping::Unwritten {
                    physical: PhysicalBlock::new(physical),
                    len: BlockCount::new(mapped_len),
                    flags: BlockMappingFlags::empty(),
                });
            }
            return Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(physical),
                len: BlockCount::new(mapped_len),
                flags: BlockMappingFlags::empty(),
            });
        }
    }

    let next = if left < usize::from(header.entries()) {
        decode_leaf(bytes, left)?.block().saturating_sub(logical)
    } else {
        hole_len_to(logical, upper_lblk)?
    };
    Ok(BlockMapping::Hole {
        len: BlockCount::new(next.max(1)),
        flags: BlockMappingFlags::empty(),
    })
}

pub(super) fn min_lblk(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn hole_len_to(logical: u32, upper_lblk: Option<u32>) -> Ext4Result<u32> {
    match upper_lblk {
        Some(limit) if limit <= logical => Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent)),
        Some(limit) => Ok(limit - logical),
        None => Ok(u32::MAX),
    }
}

pub(super) fn decode_index(bytes: &[u8], entry: usize) -> Ext4Result<disk_extent::ExtentIndex> {
    let offset = entry_offset(entry)?;
    let end = offset
        .checked_add(disk_extent::EXTENT_ENTRY_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    disk_extent::ExtentIndex::decode(bytes.get(offset..end).ok_or(Ext4Error::OutOfBounds)?)
}

pub(super) fn decode_leaf(bytes: &[u8], entry: usize) -> Ext4Result<disk_extent::ExtentLeaf> {
    let offset = entry_offset(entry)?;
    let end = offset
        .checked_add(disk_extent::EXTENT_ENTRY_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    disk_extent::ExtentLeaf::decode(bytes.get(offset..end).ok_or(Ext4Error::OutOfBounds)?)
}

pub(super) fn entry_offset(entry: usize) -> Ext4Result<usize> {
    entry
        .checked_mul(disk_extent::EXTENT_ENTRY_SIZE)
        .and_then(|offset| offset.checked_add(disk_extent::EXTENT_HEADER_SIZE))
        .ok_or(Ext4Error::Overflow)
}
