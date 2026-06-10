// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Result, disk::codec};

/// Decoded block group metadata addresses and counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockGroupDescriptor {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    free_blocks_count: u32,
    free_inodes_count: u32,
    used_directories_count: u32,
    flags: u16,
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
            flags: codec::le_u16(input, 18)?,
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

    /// Returns the stored descriptor checksum.
    pub const fn checksum(&self) -> u16 {
        self.checksum
    }
}
