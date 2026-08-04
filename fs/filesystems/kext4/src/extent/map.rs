// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{
    BlockMapping,
    checksum::verify_extent_block_checksum,
    legacy::legacy_max_file_size,
    validate::{decode_header, find_index, map_leaf, min_lblk, validate_extent_entries},
};
use crate::{
    CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result, FilesystemBlock, LogicalBlock,
    inode::Ext4Inode,
};

impl Ext4Filesystem {
    /// Returns the Linux-compatible maximum byte size for this inode format.
    pub fn max_file_size(&self, inode: &Ext4Inode) -> Ext4Result<u64> {
        if inode.has_extents() {
            return self.extent_max_file_size();
        }
        self.legacy_max_file_size()
    }

    /// Returns the superblock-wide maximum size used by extent-format inodes.
    pub fn extent_max_file_size(&self) -> Ext4Result<u64> {
        extent_max_file_size(
            self.layout().block_size(),
            self.superblock().features().has_huge_file(),
        )
    }

    /// Returns the superblock-wide maximum size for legacy block-map inodes.
    pub fn legacy_max_file_size(&self) -> Ext4Result<u64> {
        legacy_max_file_size(
            self.layout().block_size(),
            self.superblock().features().has_huge_file(),
        )
    }

    /// Maps a logical inode block without allocating or modifying metadata.
    pub fn map_blocks(&self, inode: &Ext4Inode, logical: LogicalBlock) -> Ext4Result<BlockMapping> {
        if inode.has_extents() {
            if !self.superblock().features().has_extents() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
            }
            let logical = u32::try_from(logical.get()).map_err(|_| Ext4Error::Overflow)?;
            return self.map_extent_node(inode, inode.extent_bytes(), logical, None, None, None);
        }
        self.map_legacy_blocks(inode, logical)
    }

    fn map_extent_node(
        &self,
        inode: &Ext4Inode,
        bytes: &[u8],
        logical: u32,
        expected_depth: Option<u16>,
        expected_lblk: Option<u32>,
        upper_lblk: Option<u32>,
    ) -> Ext4Result<BlockMapping> {
        let header = decode_header(bytes)?;
        if expected_depth.is_some_and(|depth| depth != header.depth()) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        validate_extent_entries(bytes, header, expected_lblk, upper_lblk, |block, count| {
            self.is_inode_physical_block_valid(inode.number(), block, count)
        })?;
        if header.depth() == 0 {
            return map_leaf(bytes, header, logical, upper_lblk);
        }

        let selected = find_index(bytes, header, logical)?;
        let child_upper_lblk = min_lblk(upper_lblk, selected.next_lblk);
        let block = self.read_metadata_block(FilesystemBlock::new(selected.index.leaf().get()))?;
        let child_depth = header
            .depth()
            .checked_sub(1)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        let bytes = block.as_ref();
        if self.superblock().features().has_metadata_checksum() {
            verify_extent_block_checksum(self, inode, selected.index.leaf(), bytes)?;
        }
        self.map_extent_node(
            inode,
            bytes,
            logical,
            Some(child_depth),
            Some(selected.index.block()),
            child_upper_lblk,
        )
    }
}

pub(super) fn extent_max_file_size(block_size: u32, has_huge_file: bool) -> Ext4Result<u64> {
    let block_bits = ext4_block_bits(block_size)?;
    let upper_limit = if has_huge_file {
        i64::MAX as u64
    } else {
        ((1_u64 << 32) - 1)
            .checked_shr(block_bits.checked_sub(9).ok_or(Ext4Error::Overflow)?)
            .and_then(|blocks| blocks.checked_shl(block_bits))
            .ok_or(Ext4Error::Overflow)?
    };
    ((1_u64 << 32) - 1)
        .checked_shl(block_bits)
        .map(|max_bytes| max_bytes.min(upper_limit))
        .ok_or(Ext4Error::Overflow)
}

pub(super) fn ext4_block_bits(block_size: u32) -> Ext4Result<u32> {
    if block_size < 512 || !block_size.is_power_of_two() {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockSize));
    }
    Ok(block_size.trailing_zeros())
}
