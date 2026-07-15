// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! # 文件遍历和操作模块
//!
//! 提供文件内容读取、块解析等功能。

use alloc::{collections::BTreeMap, vec::Vec};

use log::error;

use crate::{
    blockdev::*, dir::get_inode_with_num, disknode::*, error::*, ext4::*, extents_tree::*,
};

/// 支持extend数和多级索引(多级索引将来弃用)
/// 根据 inode 的逻辑块号解析到物理块号，支持 12 个直接块和 1/2/3 级间接块
pub fn resolve_inode_block<B: BlockDevice>(
    block_dev: &mut Jbd2Dev<B>,
    inode: &mut Ext4Inode,
    logical_block: u32,
) -> BlockDevResult<Option<u32>> {
    // 优先走 extent 树（支持多层索引）；失败时再回退到传统多级指针逻辑
    if inode.have_extend_header_and_use_extend() {
        let mut tree = ExtentTree::new(inode);
        if let Some(ext) = tree.find_extent(block_dev, logical_block)? {
            let raw_len = ext.ee_len as u32;
            let is_unwritten = (raw_len & 0x8000) != 0;
            let mut len = raw_len;
            // 最高位表示 uninitialized 标志，长度使用低 15 位
            if (len & 0x8000) != 0 {
                len &= 0x7FFF;
            }
            if len == 0 {
                return Ok(None);
            }

            let start_lbn = ext.ee_block;
            if logical_block < start_lbn || logical_block >= start_lbn.saturating_add(len) {
                return Ok(None);
            }

            // Unwritten(uninitialized) extents represent logical holes and must
            // read as zeroes until converted to initialized extents by writes.
            if is_unwritten {
                return Ok(None);
            }

            let base = ((ext.ee_start_hi as u64) << 32) | ext.ee_start_lo as u64;
            let phys = base + (logical_block - start_lbn) as u64;
            if phys > u32::MAX as u64 {
                return Err(BlockDevError::Corrupted);
            }
            return Ok(Some(phys as u32));
        }
        Ok(None)
    } else {
        error!("Only Support Extend mode!");
        Err(BlockDevError::Unsupported)
    }
}

pub fn resolve_inode_block_allextend<B: BlockDevice>(
    _fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    inode: &mut Ext4Inode,
) -> BlockDevResult<BTreeMap<u32, u64>> {
    if !inode.have_extend_header_and_use_extend() {
        return Ok(BTreeMap::new());
    }

    fn push_extent_blocks(out: &mut Vec<(u32, u64)>, ext: &Ext4Extent) {
        let raw_len = ext.ee_len as u32;
        // Unwritten extent must be treated as hole when building lbn->pbn map.
        if (raw_len & 0x8000) != 0 {
            return;
        }
        let mut len = raw_len;
        // 最高位表示 uninitialized 标志，长度使用低 15 位
        if (len & 0x8000) != 0 {
            len &= 0x7FFF;
        }
        if len == 0 {
            return;
        }
        let base = ((ext.ee_start_hi as u64) << 32) | ext.ee_start_lo as u64;
        for i in 0..len {
            let lbn = ext.ee_block.saturating_add(i);
            out.push((lbn, base + i as u64));
        }
    }

    fn walk_node<B: BlockDevice>(
        dev: &mut Jbd2Dev<B>,
        node: &ExtentNode,
        out: &mut Vec<(u32, u64)>,
    ) -> BlockDevResult<()> {
        match node {
            ExtentNode::Leaf { entries, .. } => {
                for ext in entries {
                    push_extent_blocks(out, ext);
                }
                Ok(())
            }
            ExtentNode::Index { entries, .. } => {
                for idx in entries {
                    let child_block = ((idx.ei_leaf_hi as u64) << 32) | (idx.ei_leaf_lo as u64);
                    dev.read_block(child_block as u32)?;
                    let buf = dev.buffer();
                    let child = ExtentTree::parse_node(buf).ok_or(BlockDevError::Corrupted)?;
                    walk_node(dev, &child, out)?;
                }
                Ok(())
            }
        }
    }

    let tree = ExtentTree::new(inode);
    let root = match tree.load_root_from_inode() {
        Some(n) => n,
        None => return Ok(BTreeMap::new()),
    };

    let mut blocks: Vec<(u32, u64)> = Vec::new();
    walk_node(block_dev, &root, &mut blocks)?;
    blocks.sort_unstable_by_key(|(lbn, _)| *lbn);
    blocks.dedup_by_key(|(lbn, _)| *lbn);

    let mut out = BTreeMap::new();
    for (lbn, phys) in blocks {
        out.insert(lbn, phys);
    }
    Ok(out)
}

/// Resolves a filesystem path to its inode number and inode.
///
/// An empty path retains the legacy root-directory behavior; all other paths
/// use the canonical normalized resolver.
pub fn get_file_inode<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    path: &str,
) -> BlockDevResult<Option<(u32, Ext4Inode)>> {
    let path = if path.is_empty() { "/" } else { path };
    get_inode_with_num(fs, block_dev, path)
}
