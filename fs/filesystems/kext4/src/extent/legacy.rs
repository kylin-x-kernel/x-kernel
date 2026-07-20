// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::BlockMapping;
use crate::{
    BlockCount, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result, FilesystemBlock, LogicalBlock,
    PhysicalBlock, disk::codec, inode::Ext4Inode,
};

const LEGACY_BLOCK_POINTER_SIZE: usize = 4;
const LEGACY_DIRECT_BLOCKS: u64 = 12;
const LEGACY_SINGLE_INDIRECT_INDEX: usize = 12;
const LEGACY_DOUBLE_INDIRECT_INDEX: usize = 13;
const LEGACY_TRIPLE_INDIRECT_INDEX: usize = 14;

impl Ext4Filesystem {
    pub(super) fn map_legacy_blocks(
        &self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
    ) -> Ext4Result<BlockMapping> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let pointers_per_block = legacy_pointers_per_block(block_size)?;
        let logical = logical.get();

        if logical < LEGACY_DIRECT_BLOCKS {
            return self.map_legacy_direct_blocks(inode, logical);
        }

        let single_span = pointers_per_block;
        let double_span = legacy_tree_span(pointers_per_block, 2)?;
        let triple_span = legacy_tree_span(pointers_per_block, 3)?;
        let mut logical = logical
            .checked_sub(LEGACY_DIRECT_BLOCKS)
            .ok_or(Ext4Error::Overflow)?;

        if logical < single_span {
            let root = legacy_inode_block_pointer(inode, LEGACY_SINGLE_INDIRECT_INDEX)?;
            return self.map_legacy_indirect_tree(inode, root, 1, logical, pointers_per_block);
        }
        logical = logical
            .checked_sub(single_span)
            .ok_or(Ext4Error::Overflow)?;

        if logical < double_span {
            let root = legacy_inode_block_pointer(inode, LEGACY_DOUBLE_INDIRECT_INDEX)?;
            return self.map_legacy_indirect_tree(inode, root, 2, logical, pointers_per_block);
        }
        logical = logical
            .checked_sub(double_span)
            .ok_or(Ext4Error::Overflow)?;

        if logical < triple_span {
            let root = legacy_inode_block_pointer(inode, LEGACY_TRIPLE_INDIRECT_INDEX)?;
            return self.map_legacy_indirect_tree(inode, root, 3, logical, pointers_per_block);
        }

        Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
    }

    fn map_legacy_direct_blocks(
        &self,
        inode: &Ext4Inode,
        logical: u64,
    ) -> Ext4Result<BlockMapping> {
        let index = usize::try_from(logical).map_err(|_| Ext4Error::Overflow)?;
        let physical = legacy_inode_block_pointer(inode, index)?;
        if physical == 0 {
            let mut run_len = 1u32;
            let mut next_index = index.checked_add(1).ok_or(Ext4Error::Overflow)?;
            while next_index
                < usize::try_from(LEGACY_DIRECT_BLOCKS).map_err(|_| Ext4Error::Overflow)?
                && legacy_inode_block_pointer(inode, next_index)? == 0
            {
                run_len = run_len.checked_add(1).ok_or(Ext4Error::Overflow)?;
                next_index = next_index.checked_add(1).ok_or(Ext4Error::Overflow)?;
            }
            return Ok(BlockMapping::Hole {
                len: BlockCount::new(run_len),
            });
        }

        let mut run_len = 1u32;
        let mut next_index = index.checked_add(1).ok_or(Ext4Error::Overflow)?;
        while next_index < usize::try_from(LEGACY_DIRECT_BLOCKS).map_err(|_| Ext4Error::Overflow)? {
            let expected = u64::from(physical)
                .checked_add(u64::from(run_len))
                .ok_or(Ext4Error::Overflow)?;
            if u64::from(legacy_inode_block_pointer(inode, next_index)?) != expected {
                break;
            }
            if !self.is_inode_physical_block_valid(
                inode.number(),
                u64::from(physical),
                u64::from(run_len) + 1,
            ) {
                break;
            }
            run_len = run_len.checked_add(1).ok_or(Ext4Error::Overflow)?;
            next_index = next_index.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
        self.legacy_mapped_block(inode, physical, run_len)
    }

    fn map_legacy_indirect_tree(
        &self,
        inode: &Ext4Inode,
        root: u32,
        depth: u8,
        logical: u64,
        pointers_per_block: u64,
    ) -> Ext4Result<BlockMapping> {
        let span = legacy_tree_span(pointers_per_block, depth)?;
        if root == 0 {
            return legacy_hole(logical_span_remaining(span, logical)?);
        }
        self.validate_legacy_pointer_block(inode, root)?;

        if depth == 1 {
            return self.map_legacy_indirect_leaf(inode, root, logical, pointers_per_block);
        }

        let child_span = legacy_tree_span(
            pointers_per_block,
            depth.checked_sub(1).ok_or(Ext4Error::Overflow)?,
        )?;
        let child_index = logical.checked_div(child_span).ok_or(Ext4Error::Overflow)?;
        let child_logical = logical.checked_rem(child_span).ok_or(Ext4Error::Overflow)?;
        let buffer = self.read_metadata_block(FilesystemBlock::new(u64::from(root)))?;
        let child = legacy_pointer_block_entry(buffer.as_ref(), child_index)?;
        if child == 0 {
            return legacy_hole(logical_span_remaining(child_span, child_logical)?);
        }
        self.map_legacy_indirect_tree(
            inode,
            child,
            depth.checked_sub(1).ok_or(Ext4Error::Overflow)?,
            child_logical,
            pointers_per_block,
        )
    }

    fn map_legacy_indirect_leaf(
        &self,
        inode: &Ext4Inode,
        block: u32,
        logical: u64,
        pointers_per_block: u64,
    ) -> Ext4Result<BlockMapping> {
        let buffer = self.read_metadata_block(FilesystemBlock::new(u64::from(block)))?;
        let bytes = buffer.as_ref();
        let physical = legacy_pointer_block_entry(bytes, logical)?;
        if physical == 0 {
            let mut run_len = 1u32;
            let mut next = logical.checked_add(1).ok_or(Ext4Error::Overflow)?;
            while next < pointers_per_block && legacy_pointer_block_entry(bytes, next)? == 0 {
                run_len = run_len.checked_add(1).ok_or(Ext4Error::Overflow)?;
                next = next.checked_add(1).ok_or(Ext4Error::Overflow)?;
            }
            return Ok(BlockMapping::Hole {
                len: BlockCount::new(run_len),
            });
        }

        let mut run_len = 1u32;
        let mut next = logical.checked_add(1).ok_or(Ext4Error::Overflow)?;
        while next < pointers_per_block {
            let expected = u64::from(physical)
                .checked_add(u64::from(run_len))
                .ok_or(Ext4Error::Overflow)?;
            if u64::from(legacy_pointer_block_entry(bytes, next)?) != expected {
                break;
            }
            if !self.is_inode_physical_block_valid(
                inode.number(),
                u64::from(physical),
                u64::from(run_len) + 1,
            ) {
                break;
            }
            run_len = run_len.checked_add(1).ok_or(Ext4Error::Overflow)?;
            next = next.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
        self.legacy_mapped_block(inode, physical, run_len)
    }

    fn legacy_mapped_block(
        &self,
        inode: &Ext4Inode,
        physical: u32,
        len: u32,
    ) -> Ext4Result<BlockMapping> {
        if !self.is_inode_physical_block_valid(inode.number(), u64::from(physical), u64::from(len))
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        Ok(BlockMapping::Mapped {
            physical: PhysicalBlock::new(u64::from(physical)),
            len: BlockCount::new(len),
        })
    }

    fn validate_legacy_pointer_block(&self, inode: &Ext4Inode, block: u32) -> Ext4Result<()> {
        if !self.is_inode_physical_block_valid(inode.number(), u64::from(block), 1) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        Ok(())
    }
}

fn legacy_pointers_per_block(block_size: usize) -> Ext4Result<u64> {
    if block_size < LEGACY_BLOCK_POINTER_SIZE
        || !block_size.is_multiple_of(LEGACY_BLOCK_POINTER_SIZE)
    {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockSize));
    }
    u64::try_from(block_size / LEGACY_BLOCK_POINTER_SIZE).map_err(|_| Ext4Error::Overflow)
}

fn legacy_tree_span(pointers_per_block: u64, depth: u8) -> Ext4Result<u64> {
    let mut span = 1u64;
    for _ in 0..depth {
        span = span
            .checked_mul(pointers_per_block)
            .ok_or(Ext4Error::Overflow)?;
    }
    Ok(span)
}

fn logical_span_remaining(span: u64, logical: u64) -> Ext4Result<u64> {
    span.checked_sub(logical)
        .filter(|remaining| *remaining != 0)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
}

fn legacy_hole(len: u64) -> Ext4Result<BlockMapping> {
    let len = len.min(u64::from(u32::MAX));
    let len = u32::try_from(len).map_err(|_| Ext4Error::Overflow)?;
    Ok(BlockMapping::Hole {
        len: BlockCount::new(len),
    })
}

fn legacy_inode_block_pointer(inode: &Ext4Inode, index: usize) -> Ext4Result<u32> {
    let offset = index
        .checked_mul(LEGACY_BLOCK_POINTER_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    codec::le_u32(inode.raw_i_block(), offset)
}

fn legacy_pointer_block_entry(bytes: &[u8], index: u64) -> Ext4Result<u32> {
    let index = usize::try_from(index).map_err(|_| Ext4Error::Overflow)?;
    let offset = index
        .checked_mul(LEGACY_BLOCK_POINTER_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    codec::le_u32(bytes, offset)
}
