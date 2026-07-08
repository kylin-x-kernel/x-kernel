// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! # 文件操作模块
//!
//! 提供对 ext4 文件系统中文件的读写、创建、删除等操作功能。

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};

use log::{debug, error, warn};

use crate::{
    blockdev::*,
    config::*,
    dir::*,
    disknode::*,
    endian::DiskFormat,
    entries::*,
    error::*,
    ext4::*,
    extents_tree::*,
    hashtree::{self, Ext4InodeHashTreeExt},
    loopfile::*,
};

const ZERO_INODE_BYTES: [u8; 4] = 0u32.to_le_bytes();

/// A directory entry located by a single parent-directory scan.
pub(crate) struct ParentDirEntry {
    pub ino: u32,
    pub phys_block: u64,
    pub file_type: u8,
}

/// Searches a single directory data block for a named entry.
fn find_dentry_in_dir_block(data: &[u8], name_bytes: &[u8]) -> Option<(u32, u8)> {
    let block_bytes = BLOCK_SIZE;
    let mut offset: usize = 0;
    while offset + 8 <= block_bytes {
        let inode = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let rec_len = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
        if rec_len < 8 {
            break;
        }
        let name_len = data[offset + 6] as usize;
        let entry_end = offset + rec_len as usize;
        if entry_end > block_bytes {
            break;
        }
        if name_len > 0 && offset + 8 + name_len <= entry_end {
            let name = &data[offset + 8..offset + 8 + name_len];
            if inode != 0 && name == name_bytes {
                return Some((inode, data[offset + 7]));
            }
        }
        if entry_end >= block_bytes {
            break;
        }
        offset = entry_end;
    }
    None
}

/// Removes a dentry from a single directory block.
fn remove_dentry_in_dir_block(data: &mut [u8], name_bytes: &[u8]) -> bool {
    let block_bytes = BLOCK_SIZE;
    let mut offset: usize = 0;
    let mut prev_off: Option<usize> = None;
    let mut prev_rec_len: u16 = 0;
    while offset + 8 <= block_bytes {
        let inode = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let rec_len = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
        if rec_len < 8 {
            break;
        }
        let name_len = data[offset + 6] as usize;
        let entry_end = offset + rec_len as usize;
        if entry_end > block_bytes {
            break;
        }

        if name_len > 0 && offset + 8 + name_len <= entry_end {
            let name = &data[offset + 8..offset + 8 + name_len];
            if inode != 0 && name == name_bytes {
                if let Some(poff) = prev_off {
                    // Ext4 reuses deleted dirent space through the previous rec_len.
                    let new_len = prev_rec_len.saturating_add(rec_len);
                    let bytes = new_len.to_le_bytes();
                    data[poff + 4] = bytes[0];
                    data[poff + 5] = bytes[1];

                    data[offset..offset + 4].copy_from_slice(&ZERO_INODE_BYTES);
                } else {
                    data[offset..offset + 4].copy_from_slice(&ZERO_INODE_BYTES);
                }
                return true;
            }
        }
        if entry_end >= block_bytes {
            break;
        }
        prev_off = Some(offset);
        prev_rec_len = rec_len;
        offset = entry_end;
    }
    false
}

/// Replaces the inode number and file type of a named directory entry.
fn replace_dentry_in_dir_block(
    data: &mut [u8],
    name_bytes: &[u8],
    new_ino: u32,
    file_type: u8,
) -> bool {
    let block_bytes = BLOCK_SIZE;
    let mut offset: usize = 0;
    while offset + 8 <= block_bytes {
        let existing_ino = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let rec_len = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
        if rec_len < 8 {
            break;
        }
        let name_len = data[offset + 6] as usize;
        let entry_end = offset + rec_len as usize;
        if entry_end > block_bytes {
            break;
        }

        if name_len > 0 && offset + 8 + name_len <= entry_end {
            let name = &data[offset + 8..offset + 8 + name_len];
            if existing_ino != 0 && name == name_bytes {
                data[offset..offset + 4].copy_from_slice(&new_ino.to_le_bytes());
                data[offset + 7] = file_type;
                return true;
            }
        }
        if entry_end >= block_bytes {
            break;
        }
        offset = entry_end;
    }
    false
}

fn try_remove_dentry_in_block<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    phys: u64,
    name_bytes: &[u8],
) -> BlockDevResult<bool> {
    let mut removed = false;
    fs.datablock_cache.modify(block_dev, phys, |data| {
        removed = remove_dentry_in_dir_block(data, name_bytes);
    })?;
    Ok(removed)
}

fn try_replace_dentry_in_block<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    phys: u64,
    name_bytes: &[u8],
    new_ino: u32,
    file_type: u8,
) -> BlockDevResult<bool> {
    let mut replaced = false;
    fs.datablock_cache.modify(block_dev, phys, |data| {
        replaced = replace_dentry_in_dir_block(data, name_bytes, new_ino, file_type);
    })?;
    Ok(replaced)
}

fn dir_data_blocks<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    dir_inode: &mut Ext4Inode,
) -> BlockDevResult<Vec<u64>> {
    let total_blocks = dir_inode.size().div_ceil(BLOCK_SIZE as u64);
    if total_blocks == 0 {
        return Ok(Vec::new());
    }
    if total_blocks > u32::MAX as u64 {
        return Err(BlockDevError::Unsupported);
    }

    if dir_inode.have_extend_header_and_use_extend() {
        let blocks = resolve_inode_block_allextend(fs, block_dev, dir_inode)?;
        remember_dir_blocks(fs, &blocks);
        let mut collected = Vec::new();
        for lbn in 0..total_blocks as u32 {
            let Some(&phys) = blocks.get(&lbn) else {
                if lbn == 0 {
                    return Err(BlockDevError::Corrupted);
                }
                continue;
            };
            collected.push(phys);
        }
        return Ok(collected);
    }

    if total_blocks > 12 {
        return Err(BlockDevError::Unsupported);
    }

    let mut collected = Vec::new();
    for lbn in 0..total_blocks as usize {
        let phys = dir_inode.i_block[lbn] as u64;
        if phys == 0 {
            if lbn == 0 {
                return Err(BlockDevError::Corrupted);
            }
            continue;
        }
        remember_dir_block(fs, phys);
        collected.push(phys);
    }
    Ok(collected)
}

/// Finds a child name in `parent_inode` with one directory scan.
pub(crate) fn find_named_entry_in_parent<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    _parent_ino: u32,
    parent_inode: &Ext4Inode,
    name_bytes: &[u8],
) -> BlockDevResult<Option<ParentDirEntry>> {
    if !parent_inode.is_dir() {
        return Ok(None);
    }

    // Try htree-indexed lookup first.
    if parent_inode.is_htree_indexed()
        && let Ok(result) =
            hashtree::lookup_directory_entry(fs, block_dev, parent_inode, name_bytes)
    {
        return Ok(Some(ParentDirEntry {
            ino: result.entry.inode,
            phys_block: result.block_num as u64,
            file_type: result.entry.file_type,
        }));
    }

    // Fall back to linear scan across all data blocks.
    let mut parent_inode_copy = *parent_inode;
    for phys in dir_data_blocks(fs, block_dev, &mut parent_inode_copy)? {
        let cached = fs.datablock_cache.get_or_load(block_dev, phys)?;
        let data = &cached.data[..BLOCK_SIZE];
        if let Some((inode, file_type)) = find_dentry_in_dir_block(data, name_bytes) {
            return Ok(Some(ParentDirEntry {
                ino: inode,
                phys_block: phys,
                file_type,
            }));
        }
    }

    Ok(None)
}

/// Removes a dentry on a block returned by [`find_named_entry_in_parent`].
pub(crate) fn remove_named_entry_at<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    phys: u64,
    name_bytes: &[u8],
) -> BlockDevResult<bool> {
    try_remove_dentry_in_block(fs, block_dev, phys, name_bytes)
}

fn replace_named_entry_at<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    phys: u64,
    name_bytes: &[u8],
    new_ino: u32,
    file_type: u8,
) -> BlockDevResult<bool> {
    try_replace_dentry_in_block(fs, block_dev, phys, name_bytes, new_ino, file_type)
}

fn is_dot_or_dotdot(name: &str) -> bool {
    matches!(name, "." | "..")
}

fn is_dir_empty<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    inode: &mut Ext4Inode,
) -> BlockDevResult<bool> {
    for phys in dir_data_blocks(fs, block_dev, inode)? {
        let cached = fs.datablock_cache.get_or_load(block_dev, phys)?;
        let data = &cached.data[..BLOCK_SIZE];
        for (entry, _) in DirEntryIterator::new(data) {
            if !entry.is_dot() && !entry.is_dotdot() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn free_dir_inode<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    ino: u32,
    inode: &mut Ext4Inode,
) -> BlockDevResult<()> {
    // The inode slot is freed below, so no separate link-count writeback is needed.
    inode.i_links_count = 0;
    free_inode_storage(block_dev, fs, inode)?;
    fs.free_inode(block_dev, ino)?;
    let (group_idx, _) = fs.inode_allocator.global_to_group(ino);
    if let Some(desc) = fs.get_group_desc_mut(group_idx) {
        let used_dirs = desc.used_dirs_count().saturating_sub(1);
        desc.bg_used_dirs_count_lo = (used_dirs & 0xFFFF) as u16;
        desc.bg_used_dirs_count_hi = (used_dirs >> 16) as u16;
    }
    Ok(())
}

fn free_replaced_inode<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    ino: u32,
    inode: &mut Ext4Inode,
) -> BlockDevResult<()> {
    if inode.is_dir() {
        if !is_dir_empty(fs, block_dev, inode)? {
            return Err(BlockDevError::DirectoryNotEmpty);
        }
        return free_dir_inode(fs, block_dev, ino, inode);
    }

    inode.i_links_count = inode.i_links_count.saturating_sub(1);
    fs.modify_inode(block_dev, ino, |on_disk| {
        on_disk.i_links_count = inode.i_links_count;
    })?;
    if inode.i_links_count == 0 {
        free_inode_storage(block_dev, fs, inode)?;
        fs.free_inode(block_dev, ino)?;
    }
    Ok(())
}

fn zero_data_block() -> Vec<u8> {
    alloc::vec![0u8; BLOCK_SIZE]
}

fn checked_block_num(block_num: u64) -> BlockDevResult<u32> {
    u32::try_from(block_num).map_err(|_| BlockDevError::BlockOutOfRange {
        block_id: u32::MAX,
        max_blocks: u32::MAX as u64,
    })
}

pub fn read_data_block_direct<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    block_num: u64,
) -> BlockDevResult<Vec<u8>> {
    let mut data = zero_data_block();
    device.read_blocks(&mut data, checked_block_num(block_num)?, 1)?;
    Ok(data)
}

pub fn write_data_block_direct<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    block_num: u64,
    data: &[u8],
) -> BlockDevResult<()> {
    if data.len() != BLOCK_SIZE {
        return Err(BlockDevError::InvalidInput);
    }
    device.write_blocks(data, checked_block_num(block_num)?, 1, false)
}

/// 重命名文件或目录
///
/// # 参数
///
/// * `device` - 可变引用的块设备
/// * `fs` - 可变引用的文件系统
/// * `old_path` - 旧路径
/// * `new_path` - 新路径
///
/// # 返回值
///
/// 成功时返回 `Ok(())`，失败时返回错误
pub fn rename<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    old_path: &str,
    new_path: &str,
) -> BlockDevResult<()> {
    let old_norm = normalize_path(old_path)?;
    let new_norm = normalize_path(new_path)?;

    mv(fs, device, &old_norm, &new_norm)?;

    // 校验
    if get_inode_with_num(fs, device, &old_norm)?.is_some() {
        return Err(BlockDevError::WriteError);
    }
    if get_inode_with_num(fs, device, &new_norm)?.is_none() {
        return Err(BlockDevError::WriteError);
    }

    Ok(())
}
pub fn truncate<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
    truncate_size: u64,
) -> BlockDevResult<()> {
    let norm_path = normalize_path(path)?;

    // 首先找到目标文件。
    let (inode_num, _inode) = match get_inode_with_num(fs, device, &norm_path)? {
        Some(v) => v,
        None => return Err(BlockDevError::InvalidInput),
    };

    truncate_with_ino(device, fs, inode_num, truncate_size)
}

/// TODO:shrink暂时不要用不成熟   记得更新inodesize extendtree不负责更新inodesize
pub fn truncate_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    truncate_size: u64,
) -> BlockDevResult<()> {
    let mut inode = fs.get_inode_by_num(device, inode_num)?;

    if !inode.is_file() {
        warn!("trubcate abnormal file")
    } else if inode.is_symlink() {
        error!("Can't truncate symlink file!");
        return Err(BlockDevError::Unsupported);
    }

    let old_size = inode.size();
    if truncate_size == old_size {
        return Ok(());
    }

    let block_bytes = BLOCK_SIZE as u64;
    let old_blocks = if old_size == 0 {
        0u64
    } else {
        old_size.div_ceil(block_bytes)
    };
    let new_blocks = if truncate_size == 0 {
        0u64
    } else {
        truncate_size.div_ceil(block_bytes)
    };

    // extent 分支：支持 grow；shrink 仅支持 truncate 到 0（否则需要删/裁剪 extent）
    if fs.superblock.has_extents() && inode.have_extend_header_and_use_extend() {
        if truncate_size < old_size {
            // Clear bytes past new EOF inside the boundary block only.
            // Full-range logical cleanup requires robust extent removal; keep
            // this minimal to avoid corrupting unrelated mappings.
            let tail_off = truncate_size % block_bytes;
            if tail_off != 0 {
                let lbn = (truncate_size / block_bytes) as u32;
                if let Some(phys) = resolve_inode_block(device, &mut inode, lbn)? {
                    let mut data = read_data_block_direct(device, phys as u64)?;
                    data[tail_off as usize..].fill(0);
                    write_data_block_direct(device, phys as u64, &data)?;
                    fs.datablock_cache.invalidate(phys as u64);
                }
            }

            // Remove all mapped blocks beyond new EOF so later growth does not
            // expose stale pre-truncate bytes.
            if old_blocks > u32::MAX as u64 {
                return Err(BlockDevError::Unsupported);
            }
            for lbn in new_blocks as u32..old_blocks as u32 {
                if resolve_inode_block(device, &mut inode, lbn)?.is_some() {
                    let mut tree = ExtentTree::new(&mut inode);
                    tree.remove_extend(fs, Ext4Extent::new(lbn, 0, 1), device)?;
                }
            }
        }

        if truncate_size > old_size {
            // Truncate-up must expose zeroes in [old_size, truncate_size).
            // If old stale mappings still exist above EOF, clear the visible
            // bytes in mapped blocks without forcing new allocation for holes.
            let mut pos = old_size;
            while pos < truncate_size {
                let lbn = (pos / block_bytes) as u32;
                let block_start = lbn as u64 * block_bytes;
                let in_block_off = (pos - block_start) as usize;
                let block_end = block_start + block_bytes;
                let seg_end = core::cmp::min(truncate_size, block_end);
                let seg_len = (seg_end - pos) as usize;

                if let Some(phys) = resolve_inode_block(device, &mut inode, lbn)? {
                    let mut data = read_data_block_direct(device, phys as u64)?;
                    data[in_block_off..in_block_off + seg_len].fill(0);
                    write_data_block_direct(device, phys as u64, &data)?;
                    fs.datablock_cache.invalidate(phys as u64);
                }

                pos = seg_end;
            }
        }

        // grow: keep sparse semantics for extents files.
        // Extending i_size should not preallocate/initialize all intermediate blocks.
        // Physical blocks are allocated on write/fallocate as needed.

        inode.i_size_lo = (truncate_size & 0xffff_ffff) as u32;
        inode.i_size_high = (truncate_size >> 32) as u32;
        // i_blocks includes both data blocks and extent-tree metadata blocks.
        recompute_extent_inode_iblocks(device, fs, &mut inode)?;

        fs.modify_inode(device, inode_num, |td| {
            *td = inode;
        })?;
        return Ok(());
    }

    // todo:
    // 非 extent：仅支持 12 个直接块（现有实现本来就不支持间接块）
    if new_blocks > 12 {
        return Err(BlockDevError::Unsupported);
    }

    // grow：分配新块并填 0，写入 i_block
    if new_blocks > old_blocks {
        for lbn in old_blocks as u32..new_blocks as u32 {
            let phys = fs.alloc_block(device)?;
            let data = zero_data_block();
            if let Err(e) = write_data_block_direct(device, phys, &data) {
                let _ = fs.free_block(device, phys);
                return Err(e);
            }
            fs.datablock_cache.invalidate(phys);
            inode.i_block[lbn as usize] = phys as u32;
        }
    }

    // shrink：释放尾部块，并清空 i_block
    if new_blocks < old_blocks {
        for lbn in new_blocks as u32..old_blocks as u32 {
            let phys = inode.i_block[lbn as usize] as u64;
            if phys != 0 {
                fs.free_block(device, phys)?;
            }
            inode.i_block[lbn as usize] = 0;
        }
    }

    inode.i_size_lo = (truncate_size & 0xffff_ffff) as u32;
    inode.i_size_high = (truncate_size >> 32) as u32;
    let iblocks_used = new_blocks.saturating_mul(BLOCK_SIZE as u64 / 512);
    inode.i_blocks_lo = (iblocks_used & 0xffff_ffff) as u32;
    inode.l_i_blocks_high = ((iblocks_used >> 32) & 0xffff) as u16;

    fs.modify_inode(device, inode_num, |td| {
        *td = inode;
    })?;

    Ok(())
}

fn collect_extent_tree_blocks<B: BlockDevice>(
    dev: &mut Jbd2Dev<B>,
    node: &ExtentNode,
    out: &mut Vec<u64>,
) -> BlockDevResult<()> {
    match node {
        ExtentNode::Leaf { .. } => Ok(()),
        ExtentNode::Index { entries, .. } => {
            for idx in entries {
                let child_block = ((idx.ei_leaf_hi as u64) << 32) | (idx.ei_leaf_lo as u64);
                out.push(child_block);
                dev.read_block(child_block as u32)?;
                let child = ExtentTree::parse_node(dev.buffer()).ok_or(BlockDevError::Corrupted)?;
                collect_extent_tree_blocks(dev, &child, out)?;
            }
            Ok(())
        }
    }
}

fn extents_from_lbn_map(map: &BTreeMap<u32, u64>) -> Vec<Ext4Extent> {
    let mut out = Vec::new();
    let mut iter = map.iter();

    let Some((&mut_lbn0, &mut_phys0)) = iter.next() else {
        return out;
    };

    let mut run_lbn = mut_lbn0;
    let mut run_phys = mut_phys0;
    let mut prev_lbn = mut_lbn0;
    let mut prev_phys = mut_phys0;
    let mut run_len: u16 = 1;

    for (&lbn, &phys) in iter {
        let contiguous = lbn == prev_lbn.saturating_add(1)
            && phys == prev_phys.saturating_add(1)
            && run_len < 0x7fff;
        if contiguous {
            run_len += 1;
            prev_lbn = lbn;
            prev_phys = phys;
            continue;
        }

        out.push(Ext4Extent::new(run_lbn, run_phys, run_len));
        run_lbn = lbn;
        run_phys = phys;
        prev_lbn = lbn;
        prev_phys = phys;
        run_len = 1;
    }

    out.push(Ext4Extent::new(run_lbn, run_phys, run_len));
    out
}

fn reset_extent_root(inode: &mut Ext4Inode) {
    let mut hdr = Ext4ExtentHeader::new();
    hdr.eh_magic = Ext4ExtentHeader::EXT4_EXT_MAGIC;
    hdr.eh_depth = 0;
    hdr.eh_entries = 0;
    hdr.eh_max = ((15usize * 4usize).saturating_sub(Ext4ExtentHeader::disk_size())
        / Ext4Extent::disk_size()) as u16;
    let empty = ExtentNode::Leaf {
        header: hdr,
        entries: Vec::new(),
    };
    let mut tree = ExtentTree::new(inode);
    tree.store_root_to_inode(&empty);
}

fn rebuild_extent_tree_from_map<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode: &mut Ext4Inode,
    map: &BTreeMap<u32, u64>,
) -> BlockDevResult<()> {
    let mut old_tree_blocks = Vec::new();
    {
        let tree = ExtentTree::new(inode);
        if let Some(root) = tree.load_root_from_inode() {
            collect_extent_tree_blocks(device, &root, &mut old_tree_blocks)?;
        }
    }

    for blk in old_tree_blocks {
        fs.free_block(device, blk)?;
    }

    reset_extent_root(inode);

    for ext in extents_from_lbn_map(map) {
        let mut tree = ExtentTree::new(inode);
        tree.insert_extent(fs, ext, device)?;
    }

    Ok(())
}

fn recompute_extent_inode_iblocks<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode: &mut Ext4Inode,
) -> BlockDevResult<()> {
    let data_blocks = resolve_inode_block_allextend(fs, device, inode)?.len() as u64;

    let mut tree_blocks = Vec::new();
    {
        let tree = ExtentTree::new(inode);
        if let Some(root) = tree.load_root_from_inode() {
            collect_extent_tree_blocks(device, &root, &mut tree_blocks)?;
        }
    }

    let total_blocks = data_blocks.saturating_add(tree_blocks.len() as u64);
    let iblocks = total_blocks.saturating_mul(BLOCK_SIZE as u64 / 512);
    inode.i_blocks_lo = (iblocks & 0xffff_ffff) as u32;
    inode.l_i_blocks_high = ((iblocks >> 32) & 0xffff) as u16;

    Ok(())
}

fn free_inode_storage<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode: &mut Ext4Inode,
) -> BlockDevResult<()> {
    let mut used_blocks: Vec<u64> = resolve_inode_block_allextend(fs, device, inode)?
        .into_values()
        .collect();

    if inode.have_extend_header_and_use_extend() {
        let tree = ExtentTree::new(inode);
        if let Some(root) = tree.load_root_from_inode() {
            collect_extent_tree_blocks(device, &root, &mut used_blocks)?;
        }
    }

    used_blocks.sort_unstable();
    used_blocks.dedup();

    for blk in used_blocks {
        fs.free_block(device, blk)?;
    }

    Ok(())
}

pub fn collapse_range_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    len: u64,
) -> BlockDevResult<()> {
    if len == 0 {
        return Ok(());
    }

    let block_bytes = BLOCK_SIZE as u64;
    if !offset.is_multiple_of(block_bytes) || !len.is_multiple_of(block_bytes) {
        return Err(BlockDevError::InvalidInput);
    }

    let mut inode = fs.get_inode_by_num(device, inode_num)?;
    if !inode.have_extend_header_and_use_extend() {
        return Err(BlockDevError::Unsupported);
    }

    let old_size = inode.size();
    let end = offset.checked_add(len).ok_or(BlockDevError::InvalidInput)?;
    if end > old_size {
        return Err(BlockDevError::InvalidInput);
    }

    let start_lbn = (offset / block_bytes) as u32;
    let shift_blocks = (len / block_bytes) as u32;
    let end_lbn = start_lbn.saturating_add(shift_blocks);

    let old_map = resolve_inode_block_allextend(fs, device, &mut inode)?;
    let mut new_map = BTreeMap::new();
    let mut removed_phys = Vec::new();

    for (lbn, phys) in old_map {
        if lbn < start_lbn {
            new_map.insert(lbn, phys);
        } else if lbn >= end_lbn {
            new_map.insert(lbn - shift_blocks, phys);
        } else {
            removed_phys.push(phys);
        }
    }

    for phys in removed_phys {
        fs.free_block(device, phys)?;
    }

    rebuild_extent_tree_from_map(device, fs, &mut inode, &new_map)?;

    let new_size = old_size - len;
    inode.i_size_lo = (new_size & 0xffff_ffff) as u32;
    inode.i_size_high = (new_size >> 32) as u32;
    recompute_extent_inode_iblocks(device, fs, &mut inode)?;

    fs.modify_inode(device, inode_num, |on_disk| {
        *on_disk = inode;
    })?;

    Ok(())
}

pub fn insert_range_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    len: u64,
) -> BlockDevResult<()> {
    if len == 0 {
        return Ok(());
    }

    let block_bytes = BLOCK_SIZE as u64;
    if !offset.is_multiple_of(block_bytes) || !len.is_multiple_of(block_bytes) {
        return Err(BlockDevError::InvalidInput);
    }

    let mut inode = fs.get_inode_by_num(device, inode_num)?;
    if !inode.have_extend_header_and_use_extend() {
        return Err(BlockDevError::Unsupported);
    }

    let old_size = inode.size();
    if offset > old_size {
        return Err(BlockDevError::InvalidInput);
    }

    let start_lbn = (offset / block_bytes) as u32;
    let shift_blocks = (len / block_bytes) as u32;

    let old_map = resolve_inode_block_allextend(fs, device, &mut inode)?;
    let mut new_map = BTreeMap::new();

    for (lbn, phys) in old_map {
        if lbn < start_lbn {
            new_map.insert(lbn, phys);
        } else {
            let shifted = lbn
                .checked_add(shift_blocks)
                .ok_or(BlockDevError::InvalidInput)?;
            new_map.insert(shifted, phys);
        }
    }

    rebuild_extent_tree_from_map(device, fs, &mut inode, &new_map)?;

    let new_size = old_size
        .checked_add(len)
        .ok_or(BlockDevError::InvalidInput)?;
    inode.i_size_lo = (new_size & 0xffff_ffff) as u32;
    inode.i_size_high = (new_size >> 32) as u32;
    recompute_extent_inode_iblocks(device, fs, &mut inode)?;

    fs.modify_inode(device, inode_num, |on_disk| {
        *on_disk = inode;
    })?;

    Ok(())
}

pub fn create_symbol_link<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    src_path: &str,
    dst_path: &str,
) -> BlockDevResult<()> {
    // 首先判断两个目标文件是否存在，被链接不存在报错，链接文件存在报错。
    let src_norm = normalize_path(src_path)?;
    let dst_norm = normalize_path(dst_path)?;

    if get_file_inode(fs, device, &src_norm)?.is_none() {
        return Err(BlockDevError::InvalidInput);
    }
    if get_file_inode(fs, device, &dst_norm)?.is_some() {
        return Err(BlockDevError::InvalidInput);
    }

    // 拆 parent / child（父目录必须存在）
    let (parent, child) = if let Some(pos) = dst_norm.rfind('/') {
        let p = if pos == 0 {
            "/".to_string()
        } else {
            dst_norm[..pos].to_string()
        };
        let c = dst_norm[pos + 1..].to_string();
        (p, c)
    } else {
        ("/".to_string(), dst_norm)
    };

    let (parent_ino_num, parent_inode) =
        match get_inode_with_num(fs, device, &parent).ok().flatten() {
            Some(v) => v,
            None => return Err(BlockDevError::InvalidInput),
        };
    if !parent_inode.is_dir() {
        return Err(BlockDevError::InvalidInput);
    }

    // 为新链接分配 inode
    let new_ino = fs.alloc_inode(device)?;

    let target_bytes = src_path.as_bytes();
    let target_len = target_bytes.len();
    let size_lo = (target_len as u64 & 0xffffffff) as u32;
    let size_hi = ((target_len as u64) >> 32) as u32;

    let mut new_inode = Ext4Inode {
        i_mode: Ext4Inode::S_IFLNK | 0o777,
        i_links_count: 1,
        i_size_lo: size_lo,
        i_size_high: size_hi,
        ..Default::default()
    };

    if target_len == 0 {
        new_inode.i_blocks_lo = 0;
        new_inode.l_i_blocks_high = 0;
        new_inode.i_block = [0; 15];
    } else if target_len <= 60 {
        // fast symlink：目标路径直接写入 i_block
        let mut raw = [0u8; 60];
        raw[..target_len].copy_from_slice(target_bytes);
        for i in 0..15 {
            new_inode.i_block[i] =
                u32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]]);
        }
        new_inode.i_blocks_lo = 0;
        new_inode.l_i_blocks_high = 0;
    } else {
        // 普通 symlink：用数据块存储目标路径
        let mut data_blocks: Vec<u64> = Vec::new();
        let mut remaining = target_len;
        let mut src_off = 0usize;

        while remaining > 0 {
            if !fs.superblock.has_extents() && data_blocks.len() >= 12 {
                return Err(BlockDevError::Unsupported);
            }

            let blk = fs.alloc_block(device)?;
            let write_len = core::cmp::min(remaining, BLOCK_SIZE);
            let mut data = zero_data_block();
            let end = src_off + write_len;
            data[..write_len].copy_from_slice(&target_bytes[src_off..end]);
            if let Err(e) = write_data_block_direct(device, blk, &data) {
                let _ = fs.free_block(device, blk);
                for old_blk in data_blocks {
                    let _ = fs.free_block(device, old_blk);
                }
                return Err(e);
            }
            fs.datablock_cache.invalidate(blk);

            data_blocks.push(blk);
            remaining -= write_len;
            src_off += write_len;
        }

        let used_datablocks = data_blocks.len() as u64;
        let iblocks_used = used_datablocks.saturating_mul(BLOCK_SIZE as u64 / 512) as u32;
        new_inode.i_blocks_lo = iblocks_used;
        new_inode.l_i_blocks_high = 0; // iblocks_used is u32, so high part is 0

        build_file_block_mapping(fs, &mut new_inode, &data_blocks, device);
    }

    fs.modify_inode(device, new_ino, |on_disk| {
        *on_disk = new_inode;
    })?;

    // 插入父目录目录项（symlink 类型）
    let mut parent_inode_copy = parent_inode;
    insert_dir_entry(
        fs,
        device,
        parent_ino_num,
        &mut parent_inode_copy,
        new_ino,
        &child,
        Ext4DirEntry2::EXT4_FT_SYMLINK,
    )?;

    Ok(())
}

pub fn read_symlink_target<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode: &mut Ext4Inode,
) -> BlockDevResult<Vec<u8>> {
    let size = inode.size() as usize;
    if size == 0 {
        return Ok(Vec::new());
    }

    if size <= 60 {
        let mut raw = [0u8; 60];
        for (i, word) in inode.i_block.iter().take(15).enumerate() {
            raw[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        return Ok(raw[..size].to_vec());
    }

    let block_bytes = BLOCK_SIZE;
    let total_blocks = size.div_ceil(block_bytes);
    let mut buf = Vec::with_capacity(size);

    if inode.have_extend_header_and_use_extend() {
        let blocks = resolve_inode_block_allextend(fs, device, inode)?;
        for &phys in blocks.values() {
            let data = read_data_block_direct(device, phys)?;
            buf.extend_from_slice(&data[..block_bytes]);
            if buf.len() >= size {
                break;
            }
        }
    } else {
        for lbn in 0..total_blocks {
            let phys = match resolve_inode_block(device, inode, lbn as u32)? {
                Some(b) => b,
                None => break,
            };
            let data = read_data_block_direct(device, phys as u64)?;
            buf.extend_from_slice(&data[..block_bytes]);
        }
    }

    buf.truncate(size);

    Ok(buf)
}

fn resolve_symlink_path(current_path: &str, target: &str) -> BlockDevResult<String> {
    if target.starts_with('/') {
        return normalize_path(target);
    }
    let parent = match current_path.rfind('/') {
        Some(0) | None => "/",
        Some(pos) => &current_path[..pos],
    };
    let mut combined = String::new();
    if parent == "/" {
        combined.push('/');
        combined.push_str(target);
    } else {
        combined.push_str(parent);
        combined.push('/');
        combined.push_str(target);
    }
    normalize_path(&combined)
}

fn read_file_follow<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
    depth: usize,
) -> BlockDevResult<Option<Vec<u8>>> {
    if depth > 8 {
        return Err(BlockDevError::InvalidInput);
    }

    let mut inode = match get_file_inode(fs, device, path) {
        Ok(Some((_ino_num, ino))) => ino,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e),
    };

    if inode.is_symlink() {
        let target_bytes = read_symlink_target(device, fs, &mut inode)?;
        let target = match core::str::from_utf8(&target_bytes) {
            Ok(s) => s,
            Err(_) => return Err(BlockDevError::Corrupted),
        };
        let resolved = resolve_symlink_path(path, target)?;
        return read_file_follow(device, fs, &resolved, depth + 1);
    }

    if !inode.is_file() {
        error!("Entry:{path} not aa file");
        return BlockDevResult::Err(BlockDevError::ReadError);
    }

    let size = inode.size() as usize;
    if size == 0 {
        return Ok(Some(Vec::new()));
    }

    let block_bytes = BLOCK_SIZE;
    let total_blocks = size.div_ceil(block_bytes);

    let mut buf = Vec::with_capacity(size);

    if inode.have_extend_header_and_use_extend() {
        let blocks = resolve_inode_block_allextend(fs, device, &mut inode)?;
        for &phys in blocks.values() {
            let data = read_data_block_direct(device, phys)?;
            buf.extend_from_slice(&data[..block_bytes]);
            if buf.len() >= size {
                break;
            }
        }
    } else {
        for lbn in 0..total_blocks {
            let phys = match resolve_inode_block(device, &mut inode, lbn as u32)? {
                Some(b) => b,
                None => break,
            };

            let data = read_data_block_direct(device, phys as u64)?;
            buf.extend_from_slice(&data[..block_bytes]);
        }
    }

    buf.truncate(size);

    Ok(Some(buf))
}

// mv
pub fn mv<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    old_path: &str,
    new_path: &str,
) -> BlockDevResult<()> {
    // 找到对应entry，找不到就返回。
    // 判断new_path的父目录是否已经存在不存在就返回，存在继续判断new_path是否有对应的entry，存在就返回
    // 判断被移动的entry类型，如果是目录
    // 对entry的父目录的link-1.
    // 将旧entry使用insertnewentry插入到新目录修改文件名称，更新长度信息，使用removeentry...删除旧entry
    // 对新父目录的link+1.
    // 如果是文件或者链接
    // 将旧entry使用insertnewentry插入到新目录修改文件名称，更新长度信息，使用removeentry...删除旧entry

    let old_norm = normalize_path(old_path)?;
    let new_norm = normalize_path(new_path)?;

    let (old_parent, old_name) = match old_norm.rfind('/') {
        Some(pos) => {
            let parent = if pos == 0 {
                "/".to_string()
            } else {
                old_norm[..pos].to_string()
            };
            let name = old_norm[pos + 1..].to_string();
            (parent, name)
        }
        None => {
            error!("mv invalid old_path(no '/'): old_path={old_path}");
            return Err(BlockDevError::InvalidInput);
        }
    };
    let (new_parent, new_name) = match new_norm.rfind('/') {
        Some(pos) => {
            let parent = if pos == 0 {
                "/".to_string()
            } else {
                new_norm[..pos].to_string()
            };
            let name = new_norm[pos + 1..].to_string();
            (parent, name)
        }
        None => {
            error!("mv invalid new_path(no '/'): new_path={new_path}");
            return Err(BlockDevError::InvalidInput);
        }
    };

    // 找到 old entry（inode + file_type），找不到就返回
    let (old_pino, old_parent_inode) = match get_inode_with_num(fs, block_dev, &old_parent)
        .ok()
        .flatten()
    {
        Some(v) => v,
        None => {
            error!("mv old parent not found: old_path={old_path} old_parent={old_parent}");
            return Err(BlockDevError::InvalidInput);
        }
    };

    let old_entry = match find_named_entry_in_parent(
        fs,
        block_dev,
        old_pino,
        &old_parent_inode,
        old_name.as_bytes(),
    ) {
        Ok(Some(v)) => v,
        Ok(None) => {
            error!(
                "mv source entry not found in old parent: old_path={old_path} \
                 old_parent={old_parent} old_name={old_name}"
            );
            return Err(BlockDevError::InvalidInput);
        }
        Err(e) => {
            error!("mv lookup failed: {e:?} old_path={old_path}");
            return Err(BlockDevError::InvalidInput);
        }
    };
    let src_ino = old_entry.ino;
    let src_ft = old_entry.file_type;

    // new_parent 必须存在且是目录
    let (new_pino, new_parent_inode) = match get_inode_with_num(fs, block_dev, &new_parent)
        .ok()
        .flatten()
    {
        Some(v) => v,
        None => {
            error!("mv new parent not found: new_path={new_path} new_parent={new_parent}");
            return Err(BlockDevError::InvalidInput);
        }
    };
    if !new_parent_inode.is_dir() {
        error!("mv new parent is not dir: new_path={new_path} new_parent={new_parent}");
        return Err(BlockDevError::InvalidInput);
    }

    // old_path 不允许为根目录
    if old_norm == "/" {
        error!("mv refuses to move root: old_path={old_path}");
        return Err(BlockDevError::InvalidInput);
    }

    let dst_entry = find_named_entry_in_parent(
        fs,
        block_dev,
        new_pino,
        &new_parent_inode,
        new_name.as_bytes(),
    )?;
    if let Some(dst_entry) = &dst_entry
        && dst_entry.ino == src_ino
    {
        return Ok(());
    }

    let src_inode = fs.get_inode_by_num(block_dev, src_ino)?;
    let src_is_dir = src_inode.is_dir();
    let mut replaced_inode = if let Some(dst_entry) = &dst_entry {
        Some((
            dst_entry.ino,
            dst_entry.file_type,
            fs.get_inode_by_num(block_dev, dst_entry.ino)?,
        ))
    } else {
        None
    };
    if let Some((_, _, inode)) = &mut replaced_inode {
        if src_is_dir && !inode.is_dir() {
            return Err(BlockDevError::NotDirectory);
        }
        if !src_is_dir && inode.is_dir() {
            return Err(BlockDevError::IsDirectory);
        }
        if inode.is_dir() && !is_dir_empty(fs, block_dev, inode)? {
            return Err(BlockDevError::DirectoryNotEmpty);
        }
    }

    let inserted_new_entry = if let Some(dst_entry) = &dst_entry {
        let replaced = replace_named_entry_at(
            fs,
            block_dev,
            dst_entry.phys_block,
            new_name.as_bytes(),
            src_ino,
            src_ft,
        )?;
        if !replaced {
            error!("mv replace destination entry failed: new_path={new_path}");
            return Err(BlockDevError::WriteError);
        }
        false
    } else {
        let mut new_parent_inode_copy = new_parent_inode;
        insert_dir_entry(
            fs,
            block_dev,
            new_pino,
            &mut new_parent_inode_copy,
            src_ino,
            &new_name,
            src_ft,
        )
        .map_err(|_| {
            error!(
                "mv insert_dir_entry failed: old_path={old_path} new_path={new_path} \
                 new_parent={new_parent} new_name={new_name} src_ino={src_ino}"
            );
            BlockDevError::WriteError
        })?;
        true
    };

    if !remove_inodeentry_from_parentdir(fs, block_dev, &old_parent, &old_name)? {
        if inserted_new_entry {
            let _ = remove_inodeentry_from_parentdir(fs, block_dev, &new_parent, &new_name);
        } else if let (Some(dst_entry), Some((old_dst_ino, old_dst_file_type, _))) =
            (&dst_entry, &replaced_inode)
        {
            let _ = replace_named_entry_at(
                fs,
                block_dev,
                dst_entry.phys_block,
                new_name.as_bytes(),
                *old_dst_ino,
                *old_dst_file_type,
            );
        }
        error!(
            "mv remove old entry failed: old_parent={old_parent} old_name={old_name} (rollback \
             new_parent={new_parent} new_name={new_name})"
        );
        return Err(BlockDevError::WriteError);
    }

    let replaced_dir = replaced_inode
        .as_ref()
        .is_some_and(|(_, _, inode)| inode.is_dir());
    if let Some((old_dst_ino, _, old_dst_inode)) = &mut replaced_inode {
        free_replaced_inode(fs, block_dev, *old_dst_ino, old_dst_inode)?;
    }
    if replaced_dir {
        fs.modify_inode(block_dev, new_pino, |td| {
            td.i_links_count = td.i_links_count.saturating_sub(1);
        })?;
    }

    // 目录跨父目录移动：更新 link 以及 '..'
    let mut moved_inode = src_inode;
    if moved_inode.is_dir() {
        // 父目录不同才需要改
        let old_pino = match get_inode_with_num(fs, block_dev, &old_parent)
            .ok()
            .flatten()
        {
            Some((n, _)) => n,
            None => {
                error!("mv old parent vanished while moving dir: old_parent={old_parent}");
                return Err(BlockDevError::InvalidInput);
            }
        };
        if old_pino != new_pino {
            let _ = fs.modify_inode(block_dev, old_pino, |td| {
                td.i_links_count = td.i_links_count.saturating_sub(1);
            });
            let _ = fs.modify_inode(block_dev, new_pino, |td| {
                td.i_links_count = td.i_links_count.saturating_add(1);
            });

            // 更新被移动目录的 ".." 指向新父目录 inode
            let first_blk = match resolve_inode_block(block_dev, &mut moved_inode, 0) {
                Ok(Some(b)) => b,
                _ => {
                    error!("mv resolve_inode_block failed for moved dir ino={src_ino}");
                    return Err(BlockDevError::Corrupted);
                }
            };
            remember_dir_block(fs, first_blk as u64);
            let _ = fs
                .datablock_cache
                .modify(block_dev, first_blk as u64, |data| {
                    let block_bytes = BLOCK_SIZE;
                    if block_bytes < 24 {
                        return;
                    }
                    // '.' entry at offset 0
                    let rec_len0 = u16::from_le_bytes([data[4], data[5]]) as usize;
                    if rec_len0 == 0 || rec_len0 + 8 > block_bytes {
                        return;
                    }
                    let off1 = rec_len0;
                    if off1 + 4 > block_bytes {
                        return;
                    }
                    let bytes = new_pino.to_le_bytes();
                    data[off1] = bytes[0];
                    data[off1 + 1] = bytes[1];
                    data[off1 + 2] = bytes[2];
                    data[off1 + 3] = bytes[3];
                });
        }
    }

    Ok(())
}

/// Unlinks a non-directory entry.
pub fn unlink<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    link_path: &str,
) -> BlockDevResult<()> {
    delete_file(fs, block_dev, link_path)
}

/// Link
pub fn link<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    link_path: &str,
    linked_path: &str,
) {
    let Ok(link_norm) = normalize_path(link_path) else {
        return;
    };
    let Ok(linked_norm) = normalize_path(linked_path) else {
        return;
    };

    // 1.检查 被链接文件本身是否存在，不存在返回。
    let (target_ino, target_inode) = match get_file_inode(fs, block_dev, &linked_norm) {
        Ok(Some(v)) => v,
        _ => return,
    };

    // 1.5 不允许链接目录
    if target_inode.is_dir() {
        return;
    }

    // 2.检查链接文件本身是否已经存在同名entry，存在返回
    if get_file_inode(fs, block_dev, &link_norm)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }

    // link_path 的父目录必须存在且是目录
    let (parent_path, child_name) = if let Some(pos) = link_norm.rfind('/') {
        let parent = if pos == 0 {
            "/".to_string()
        } else {
            link_norm[..pos].to_string()
        };
        let child = link_norm[pos + 1..].to_string();
        (parent, child)
    } else {
        ("/".to_string(), link_norm)
    };
    let (parent_ino, mut parent_inode) = match get_inode_with_num(fs, block_dev, &parent_path)
        .ok()
        .flatten()
    {
        Some(v) => v,
        None => return,
    };
    if !parent_inode.is_dir() {
        return;
    }

    // 3.复制目标entry（主要复制 file_type），插入到当前父目录（新名字）
    let (linked_parent_path, linked_child_name) = if let Some(pos) = linked_norm.rfind('/') {
        let parent = if pos == 0 {
            "/".to_string()
        } else {
            linked_norm[..pos].to_string()
        };
        let child = linked_norm[pos + 1..].to_string();
        (parent, child)
    } else {
        ("/".to_string(), linked_norm.clone())
    };

    let mut copied_ft: Option<u8> = None;
    if let Ok(Some((linked_parent_ino, linked_parent_inode))) =
        get_inode_with_num(fs, block_dev, &linked_parent_path)
        && let Ok(Some(entry)) = find_named_entry_in_parent(
            fs,
            block_dev,
            linked_parent_ino,
            &linked_parent_inode,
            linked_child_name.as_bytes(),
        )
    {
        copied_ft = Some(entry.file_type);
    }

    let file_type = copied_ft.unwrap_or_else(|| {
        if target_inode.is_file() {
            Ext4DirEntry2::EXT4_FT_REG_FILE
        } else if target_inode.is_symlink() {
            Ext4DirEntry2::EXT4_FT_SYMLINK
        } else {
            Ext4DirEntry2::EXT4_FT_UNKNOWN
        }
    });

    // insert_dir_entry 会根据 child_name 重新计算 name_len/rec_len（满足“更新名字和长度信息”）
    if insert_dir_entry(
        fs,
        block_dev,
        parent_ino,
        &mut parent_inode,
        target_ino,
        &child_name,
        file_type,
    )
    .is_err()
    {
        return;
    }

    // 4.更新目标inode的link+1，失败则回滚刚插入的目录项
    if fs
        .modify_inode(block_dev, target_ino, |td| {
            td.i_links_count = td.i_links_count.saturating_add(1);
        })
        .is_err()
    {
        let _ = remove_inodeentry_from_parentdir(fs, block_dev, &parent_path, &child_name);
    }
}

pub fn remove_inodeentry_from_parentdir<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    parent_path: &str,
    child_name: &str,
) -> BlockDevResult<bool> {
    let (parent_ino_num, parent_inode) = match get_inode_with_num(fs, block_dev, parent_path)
        .ok()
        .flatten()
    {
        Some(v) => v,
        None => {
            warn!("Parent directory not found for path {parent_path}, remove entry failed");
            return Ok(false);
        }
    };

    let entry = match find_named_entry_in_parent(
        fs,
        block_dev,
        parent_ino_num,
        &parent_inode,
        child_name.as_bytes(),
    )? {
        Some(v) => v,
        None => {
            warn!("Dir entry '{child_name}' not found under parent {parent_path}");
            return Ok(false);
        }
    };

    remove_named_entry_at(fs, block_dev, entry.phys_block, child_name.as_bytes())
}

pub fn rmdir<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    path: &str,
) -> BlockDevResult<()> {
    let norm_path = normalize_path(path)?;
    if norm_path == "/" {
        return Err(BlockDevError::DeviceBusy);
    }

    let (parent_path, child_name) = if let Some(pos) = norm_path.rfind('/') {
        let parent = if pos == 0 {
            "/".to_string()
        } else {
            norm_path[..pos].to_string()
        };
        let child = norm_path[pos + 1..].to_string();
        if is_dot_or_dotdot(&child) {
            return Err(BlockDevError::InvalidInput);
        }
        (parent, child)
    } else {
        if is_dot_or_dotdot(&norm_path) {
            return Err(BlockDevError::InvalidInput);
        }
        ("/".to_string(), norm_path)
    };

    let (parent_ino, parent_inode) =
        get_inode_with_num(fs, block_dev, &parent_path)?.ok_or(BlockDevError::InvalidInput)?;
    let entry = find_named_entry_in_parent(
        fs,
        block_dev,
        parent_ino,
        &parent_inode,
        child_name.as_bytes(),
    )?
    .ok_or(BlockDevError::InvalidInput)?;

    let mut target_inode = fs.get_inode_by_num(block_dev, entry.ino)?;
    if !target_inode.is_dir() {
        return Err(BlockDevError::NotDirectory);
    }
    if !is_dir_empty(fs, block_dev, &mut target_inode)? {
        return Err(BlockDevError::DirectoryNotEmpty);
    }

    let removed = remove_named_entry_at(fs, block_dev, entry.phys_block, child_name.as_bytes())?;
    if !removed {
        return Err(BlockDevError::WriteError);
    }

    fs.modify_inode(block_dev, parent_ino, |inode| {
        inode.i_links_count = inode.i_links_count.saturating_sub(1);
    })?;
    free_dir_inode(fs, block_dev, entry.ino, &mut target_inode)
}

/// 删除目录
pub fn delete_dir<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    path: &str,
) -> BlockDevResult<()> {
    #[derive(Clone)]
    struct DirFrame {
        path: alloc::string::String,
        ino_num: u32,
        inode: Ext4Inode,
        parent_path: Option<alloc::string::String>,
        name_in_parent: Option<alloc::string::String>,
        stage: u8, // 0=scan, 1=cleanup
    }

    let norm_path = normalize_path(path)?;
    let (root_ino_num, root_inode) =
        get_file_inode(fs, block_dev, &norm_path)?.ok_or(BlockDevError::InvalidInput)?;
    if !root_inode.is_dir() {
        return Err(BlockDevError::InvalidInput);
    }

    let (parent_path, child_name) = if norm_path == "/" {
        (None, None)
    } else if let Some(pos) = norm_path.rfind('/') {
        let parent = if pos == 0 {
            "/".to_string()
        } else {
            norm_path[..pos].to_string()
        };
        let child = norm_path[pos + 1..].to_string();
        (Some(parent), Some(child))
    } else {
        (Some("/".to_string()), Some(norm_path.clone()))
    };

    let mut stack: Vec<DirFrame> = Vec::new();
    stack.push(DirFrame {
        path: norm_path,
        ino_num: root_ino_num,
        inode: root_inode,
        parent_path,
        name_in_parent: child_name,
        stage: 0,
    });

    // 算法采用while显式栈实现。
    while let Some(mut frame) = stack.pop() {
        // 1.首先遍历对应目录块。DirEntryIterator遍历所有entry（跳过. ..）。
        if frame.stage == 0 {
            let block_bytes = BLOCK_SIZE;

            let dir_blocks = match resolve_inode_block_allextend(fs, block_dev, &mut frame.inode) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Parse dir blocks failed: {:?} path={}", e, frame.path);
                    return Err(e);
                }
            };
            remember_dir_blocks(fs, &dir_blocks);

            let mut to_descend: Vec<(
                alloc::string::String,
                u32,
                Ext4Inode,
                alloc::string::String,
            )> = Vec::new();

            for &phys in dir_blocks.values() {
                // 先收集 entry，避免在持有 datablock_cache 借用时再次可变借用 fs
                let mut child_entries: Vec<(u32, alloc::string::String)> = Vec::new();
                {
                    let cached = match fs.datablock_cache.get_or_load(block_dev, phys) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(
                                "load dir block {} failed: {:?} path={}",
                                phys, e, frame.path
                            );
                            return Err(e);
                        }
                    };
                    let data = &cached.data[..block_bytes];
                    let iter = DirEntryIterator::new(data);
                    for (entry, _) in iter {
                        if entry.is_dot() || entry.is_dotdot() {
                            continue;
                        }
                        let child_name_bytes = entry.name.to_vec();
                        let child_name_str = match core::str::from_utf8(&child_name_bytes) {
                            Ok(s) => s,
                            Err(_) => {
                                warn!("invalid child name utf8 under dir {}", frame.path);
                                continue;
                            }
                        };
                        child_entries.push((entry.inode, child_name_str.to_string()));
                    }
                }

                for (child_ino, child_name) in child_entries {
                    let child_path = if frame.path == "/" {
                        alloc::format!("/{child_name}")
                    } else {
                        alloc::format!("{}/{}", frame.path, child_name)
                    };

                    // 每次扫描到的entry把entry的path 用error输出。
                    debug!("scan entry path={child_path}");

                    // 2.判断entry类型。
                    let child_inode = match fs.get_inode_by_num(block_dev, child_ino) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("get child inode {child_ino} failed: {e:?} path={child_path}");
                            continue;
                        }
                    };

                    // 是普通文件或者是链接，调用deletefile删除对应文件。
                    if !child_inode.is_dir() {
                        delete_file(fs, block_dev, &child_path)?;
                        continue;
                    }

                    // 是dir类型就更新父目录的inode链接数-1 然后继续深入这个目录（跳过. ..）。
                    let _ = fs.modify_inode(block_dev, frame.ino_num, |td| {
                        td.i_links_count = td.i_links_count.saturating_sub(1);
                    });

                    to_descend.push((child_path, child_ino, child_inode, child_name));
                }
            }

            // 深度优先：反向压栈
            let parent_path_for_children = frame.path.clone();

            frame.stage = 1;
            stack.push(frame);

            for (child_path, child_ino, child_inode, child_name) in to_descend.into_iter().rev() {
                stack.push(DirFrame {
                    path: child_path,
                    ino_num: child_ino,
                    inode: child_inode,
                    parent_path: Some(parent_path_for_children.clone()),
                    name_in_parent: Some(child_name),
                    stage: 0,
                });
            }
            continue;
        }

        // 当深入的目录为空时（只剩下.和..）返回上一级
        let mut cur_inode = match fs.get_inode_by_num(block_dev, frame.ino_num) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "get inode {} failed in cleanup: {:?} path={}",
                    frame.ino_num, e, frame.path
                );
                return Err(e);
            }
        };

        // 如果此时的dir类型的entrylinks数不是2就warn发出警告然后继续
        if cur_inode.i_links_count != 2 {
            warn!(
                "dir inode links_count != 2 (links={}) path={} ino={}",
                cur_inode.i_links_count, frame.path, frame.ino_num
            );
        }

        // 调用函数从父目录删除这条entry。
        if let (Some(pp), Some(name)) = (&frame.parent_path, &frame.name_in_parent) {
            let removed_path = if pp == "/" {
                alloc::format!("/{name}")
            } else {
                alloc::format!("{pp}/{name}")
            };
            // 删除entry时一样。
            debug!("delete entry path={removed_path}");

            let removed = remove_inodeentry_from_parentdir(fs, block_dev, pp, name)?;
            if !removed {
                warn!(
                    "Dir entry '{}' not found under parent {} (path={})",
                    name, pp, frame.path
                );
                return Err(BlockDevError::WriteError);
            }

            if let Some((pino, _)) = get_inode_with_num(fs, block_dev, pp).ok().flatten() {
                let _ = fs.modify_inode(block_dev, pino, |td| {
                    td.i_links_count = td.i_links_count.saturating_sub(1);
                });
            }
        }

        // 然后仿照deletefile的逻辑释放entry对应的inode的blocks和inode。
        if let Err(e) = free_inode_storage(block_dev, fs, &mut cur_inode) {
            warn!(
                "free inode storage failed for dir inode {}: {:?} path={}",
                frame.ino_num, e, frame.path
            );
            return Err(e);
        }
        if let Err(e) = fs.free_inode(block_dev, frame.ino_num) {
            warn!(
                "free_inode failed for inode {}: {:?} path={}",
                frame.ino_num, e, frame.path
            );
            return Err(e);
        }

        // 最后更新块组的dir计数-1。
        let (group_idx, _idx_in_group) = fs.inode_allocator.global_to_group(frame.ino_num);
        if let Some(desc) = fs.get_group_desc_mut(group_idx) {
            let before = desc.used_dirs_count();
            let new_count = before.saturating_sub(1);
            desc.bg_used_dirs_count_lo = (new_count & 0xFFFF) as u16;
            desc.bg_used_dirs_count_hi = (new_count >> 16) as u16;
        }
    }
    Ok(())
}

/// 删除文件/删除链接文件
pub fn delete_file<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
    path: &str,
) -> BlockDevResult<()> {
    let norm_path = normalize_path(path)?;

    // Resolve parent path and child name for directory-entry lookup.
    let (parent_path, child_name) = if let Some(pos) = norm_path.rfind('/') {
        let parent = if pos == 0 {
            "/".to_string()
        } else {
            norm_path[..pos].to_string()
        };
        let child = norm_path[pos + 1..].to_string();
        if is_dot_or_dotdot(&child) {
            return Err(BlockDevError::InvalidInput);
        }
        (parent, child)
    } else {
        if is_dot_or_dotdot(&norm_path) {
            return Err(BlockDevError::InvalidInput);
        }
        ("/".to_string(), norm_path)
    };

    let (parent_ino, parent_inode) =
        get_inode_with_num(fs, block_dev, &parent_path)?.ok_or(BlockDevError::InvalidInput)?;
    let entry = find_named_entry_in_parent(
        fs,
        block_dev,
        parent_ino,
        &parent_inode,
        child_name.as_bytes(),
    )?
    .ok_or(BlockDevError::InvalidInput)?;

    let ino_num = entry.ino;
    let mut target_inode = fs.get_inode_by_num(block_dev, ino_num)?;
    if target_inode.is_dir() {
        return Err(BlockDevError::IsDirectory);
    }

    let removed = remove_named_entry_at(fs, block_dev, entry.phys_block, child_name.as_bytes())?;
    if !removed {
        return Err(BlockDevError::WriteError);
    }

    target_inode.i_links_count = target_inode.i_links_count.saturating_sub(1);
    fs.modify_inode(block_dev, ino_num, |td| {
        td.i_links_count = target_inode.i_links_count;
    })?;

    if target_inode.i_links_count == 0 {
        debug!("Will free inode:{ino_num} path:{path}");
        free_inode_storage(block_dev, fs, &mut target_inode)?;
        fs.free_inode(block_dev, ino_num)?;
    } else {
        error!(
            "Inode num:{} links:{} >0 ,only remove entry!",
            ino_num, target_inode.i_links_count
        );
    }
    Ok(())
}

/// 根据数据块列表为普通文件 inode 构建块映射：
/// - 否则使用传统直接块指针（i_block[0..]）。
pub fn build_file_block_mapping<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    inode: &mut Ext4Inode,
    data_blocks: &[u64],
    block_dev: &mut Jbd2Dev<B>,
) {
    if data_blocks.is_empty() {
        inode.i_blocks_lo = 0;
        inode.l_i_blocks_high = 0;
        inode.i_block = [0; 15];
        return;
    }

    if fs.superblock.has_extents() {
        // 使用 extent 映射数据块，优先合并连续块
        inode.i_flags |= Ext4Inode::EXT4_EXTENTS_FL;
        inode.i_block = [0; 15];

        // 初始头构建
        if !inode.have_extend_header_and_use_extend() {
            inode.i_flags |= Ext4Inode::EXT4_EXTENTS_FL;
            inode.write_extend_header();
        }

        let mut exts_vec: Vec<Ext4Extent> = Vec::new();

        let mut run_start_lbn: u32 = 0;
        let mut run_start_pblk: u64 = data_blocks[0];
        let mut run_len: u32 = 1;

        for (idx, &pblk) in data_blocks.iter().enumerate().skip(1) {
            let lbn = idx as u32;
            let prev_lbn = lbn - 1;
            let prev_pblk = data_blocks[prev_lbn as usize];

            let is_contiguous = pblk == prev_pblk.saturating_add(1);

            if is_contiguous {
                run_len = run_len.saturating_add(1);
            } else {
                // 结束当前 run，生成一个 extent
                let ext = Ext4Extent::new(run_start_lbn, run_start_pblk, run_len as u16);
                exts_vec.push(ext);

                run_start_lbn = lbn;
                run_start_pblk = pblk;
                run_len = 1;
            }
        }

        let ext = Ext4Extent::new(run_start_lbn, run_start_pblk, run_len as u16);
        exts_vec.push(ext);

        // 构造一个叶子根节点，并通过 ExtentTree 将其写入 inode.i_block
        let mut tree = ExtentTree::new(inode);
        for extend in exts_vec {
            tree.insert_extent(fs, extend, block_dev)
                .expect("Extend insert Failed!");
        }
    } else {
        error!("not support tranditional block pointer");
    }
}

/// 创建文件类型entry通用接口
/// 传入文件名称,可选初始数据
/// file_type 可选文件entry类型，None表示默认普通文件,传entry类型,别传inode类型
pub fn mkfile<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
    initial_data: Option<&[u8]>,
    file_type: Option<u8>,
) -> Option<Ext4Inode> {
    mkfile_with_ino(device, fs, path, initial_data, file_type, None).map(|(_, inode)| inode)
}

pub fn mkfile_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
    initial_data: Option<&[u8]>,
    file_type: Option<u8>,
    inode_mode: Option<u16>,
) -> Option<(u32, Ext4Inode)> {
    // 规范化路径
    let norm_path = normalize_path(path).ok()?;

    // 如果目标已存在，直接返回
    if let Ok(Some((_ino_num, inode))) = get_file_inode(fs, device, &norm_path) {
        let ino = match get_inode_with_num(fs, device, &norm_path).ok().flatten() {
            Some((ino, _)) => ino,
            None => {
                error!("mkfile_with_ino existing file but failed to get ino path={path}");
                return None;
            }
        };
        return Some((ino, inode));
    }

    // 拆 parent / child
    let mut valid_path = norm_path;
    let split_point = match valid_path.rfind('/') {
        Some(v) => v,
        None => {
            error!("mkfile invalid path(no '/'): path={path}");
            return None;
        }
    };
    let child = valid_path.split_off(split_point)[1..].to_string();
    let parent = if valid_path.is_empty() {
        String::from("/")
    } else {
        valid_path
    };

    // 确保父目录存在
    if mkdir(device, fs, &parent).is_none() {
        error!("mkfile mkdir parent failed path={path} parent={parent}");
        return None;
    }

    // 重新获取父目录 inode 及其 inode 号
    let (parent_ino_num, parent_inode) =
        match get_inode_with_num(fs, device, &parent).ok().flatten() {
            Some((n, ino)) => (n, ino),
            None => {
                error!("mkfile get parent inode failed path={path} parent={parent}");
                return None;
            }
        };

    // 为新文件分配 inode（内部自动选择块组）
    let new_file_ino = match fs.alloc_inode(device) {
        Ok(ino) => ino,
        Err(e) => {
            error!("mkfile alloc_inode failed path={path} err={e:?} ({e})");
            return None;
        }
    };

    // 如有初始数据，为文件分配一个或多个数据块并写入
    let mut data_blocks: Vec<u64> = Vec::new();
    let mut total_written: usize = 0;
    if let Some(buf) = initial_data {
        let mut remaining = buf.len();
        let mut src_off = 0usize;

        while remaining > 0 {
            // 如果未启用 extents，则最多只使用 12 个直接块
            if !fs.superblock.has_extents() && data_blocks.len() >= 12 {
                break;
            }

            let blk = match fs.alloc_block(device) {
                Ok(b) => b,
                Err(e) => {
                    error!("mkfile alloc_block failed path={path} err={e:?} ({e})");
                    break;
                }
            };

            let write_len = core::cmp::min(remaining, BLOCK_SIZE);

            // 将数据写入新分配的数据块，其余部分填零
            let mut data = zero_data_block();
            let end = src_off + write_len;
            data[..write_len].copy_from_slice(&buf[src_off..end]);
            if let Err(e) = write_data_block_direct(device, blk, &data) {
                error!("mkfile write data block failed path={path} err={e:?} ({e})");
                let _ = fs.free_block(device, blk);
                for old_blk in data_blocks {
                    let _ = fs.free_block(device, old_blk);
                }
                return None;
            }
            fs.datablock_cache.invalidate(blk);

            data_blocks.push(blk);
            total_written += write_len;
            remaining -= write_len;
            src_off += write_len;
        }
    }

    // 构造新文件 inode 的内存版本，然后通过 modify_inode 一次性写回
    let mut new_inode = Ext4Inode::default();
    let imode = if let Some(mode) = inode_mode {
        mode
    } else if let Some(ft) = file_type {
        match ft {
            Ext4DirEntry2::EXT4_FT_SYMLINK => Ext4Inode::S_IFLNK | 0o777,
            Ext4DirEntry2::EXT4_FT_REG_FILE => Ext4Inode::S_IFREG | 0o644,
            Ext4DirEntry2::EXT4_FT_DIR => Ext4Inode::S_IFDIR | 0o755,
            Ext4DirEntry2::EXT4_FT_BLKDEV => Ext4Inode::S_IFBLK | 0o600,
            Ext4DirEntry2::EXT4_FT_CHRDEV => Ext4Inode::S_IFCHR | 0o600,
            Ext4DirEntry2::EXT4_FT_FIFO => Ext4Inode::S_IFIFO | 0o644,
            Ext4DirEntry2::EXT4_FT_SOCK => Ext4Inode::S_IFSOCK | 0o644,
            _ => Ext4Inode::S_IFREG | 0o644,
        }
    } else {
        Ext4Inode::S_IFREG | 0o644
    };

    new_inode.i_mode = imode;

    // extend是否开启
    if fs.superblock.has_extents() {
        new_inode.write_extend_header();
    }

    new_inode.i_links_count = 1;

    let size_lo = (total_written & 0xffffffff) as u32;
    let size_hi = ((total_written as u64) >> 32) as u32;

    if !data_blocks.is_empty() {
        // 有初始数据：多块或单块文件
        let used_databyte = data_blocks.len() as u64;
        let iblocks_used = used_databyte.saturating_mul(BLOCK_SIZE as u64 / 512);
        let used_blocks_lo = iblocks_used as u32;
        new_inode.i_size_lo = size_lo;
        new_inode.i_size_high = size_hi;
        new_inode.i_blocks_lo = used_blocks_lo;
        new_inode.l_i_blocks_high = (iblocks_used >> 32) as u16;

        build_file_block_mapping(fs, &mut new_inode, &data_blocks, device);
    } else {
        // 无初始数据：空文件
        new_inode.i_size_lo = 0;
        new_inode.i_size_high = 0;
        new_inode.i_blocks_lo = 0;
        new_inode.l_i_blocks_high = 0;
        if fs.superblock.has_extents() {
            new_inode.i_flags |= Ext4Inode::EXT4_EXTENTS_FL;
            new_inode.write_extend_header();
        } else {
            new_inode.i_block = [0; 15];
        }
    }

    if fs
        .modify_inode(device, new_file_ino, |on_disk| {
            *on_disk = new_inode;
        })
        .is_err()
    {
        error!("mkfile modify_inode failed path={path} ino={new_file_ino}");
        return None;
    }

    // 在父目录中插入一个普通文件类型的目录项（必要时自动扩展目录块）

    let file_type = match file_type {
        Some(ft) => ft,
        None => Ext4DirEntry2::EXT4_FT_REG_FILE,
    };

    let mut parent_inode_copy = parent_inode;
    if insert_dir_entry(
        fs,
        device,
        parent_ino_num,
        &mut parent_inode_copy,
        new_file_ino,
        &child,
        file_type,
    )
    .is_err()
    {
        error!(
            "mkfile insert_dir_entry failed path={path} parent_ino={parent_ino_num} child={child} \
             ino={new_file_ino}"
        );
        return None;
    }

    // 返回新文件 inode
    match fs.get_inode_by_num(device, new_file_ino) {
        Ok(inode) => Some((new_file_ino, inode)),
        Err(e) => {
            error!("mkfile get_inode_by_num failed path={path} ino={new_file_ino} err={e:?} ({e})");
            None
        }
    }
}

/// 读取指定路径的整个文件内容
pub fn read_file<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
) -> BlockDevResult<Option<Vec<u8>>> {
    let norm_path = normalize_path(path)?;
    read_file_follow(device, fs, &norm_path, 0)
}

pub fn read_file_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    buf: &mut [u8],
) -> BlockDevResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }

    let mut inode = fs.get_inode_by_num(device, inode_num)?;
    let file_size = inode.size();
    if offset >= file_size {
        return Ok(0);
    }

    let to_read = core::cmp::min(buf.len() as u64, file_size - offset) as usize;
    if to_read == 0 {
        return Ok(0);
    }

    if !inode.have_extend_header_and_use_extend() {
        return Err(BlockDevError::Unsupported);
    }

    let block_bytes = BLOCK_SIZE as u64;
    let start_off = offset;
    let end_off = start_off + to_read as u64;
    let start_lbn = start_off / block_bytes;
    let end_lbn = (end_off - 1) / block_bytes;

    let mut copied = 0usize;
    for lbn in start_lbn..=end_lbn {
        let lbn_start = lbn * block_bytes;
        let lbn_end = lbn_start + block_bytes;
        let copy_start = core::cmp::max(start_off, lbn_start) - lbn_start;
        let copy_end = core::cmp::min(end_off, lbn_end) - lbn_start;
        let copy_len = copy_end.saturating_sub(copy_start) as usize;
        if copy_len == 0 {
            continue;
        }

        if let Some(phys) = resolve_inode_block(device, &mut inode, lbn as u32)? {
            let data = read_data_block_direct(device, phys as u64)?;
            buf[copied..copied + copy_len]
                .copy_from_slice(&data[copy_start as usize..copy_start as usize + copy_len]);
        } else {
            buf[copied..copied + copy_len].fill(0);
        }

        copied += copy_len;
        if copied >= to_read {
            break;
        }
    }

    Ok(copied)
}

pub fn write_file<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
    offset: u64,
    data: &[u8],
) -> BlockDevResult<()> {
    if data.is_empty() {
        return Ok(());
    }

    // 获取 inode 及其 inode 号
    let info = match get_inode_with_num(fs, device, path)? {
        Some(v) => v,
        None => return Err(BlockDevError::WriteError),
    };
    let (inode_num, _inode) = info;

    write_file_with_ino(device, fs, inode_num, offset, data)
}

pub fn write_file_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    data: &[u8],
) -> BlockDevResult<()> {
    if data.is_empty() {
        return Ok(());
    }

    ext4_map_blocks_for_write(device, fs, inode_num, offset, data.len())?;
    ext4_write_prepared(device, fs, inode_num, offset, data)?;
    ext4_da_write_end(device, fs, inode_num, offset, data.len(), data.len())
}

pub fn ext4_da_write_begin<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    _offset: u64,
    len: usize,
) -> BlockDevResult<()> {
    if len == 0 {
        return Ok(());
    }

    let mut inode = fs.get_inode_by_num(device, inode_num)?;

    if fs.superblock.has_extents() && !inode.have_extend_header_and_use_extend() {
        inode.i_flags |= Ext4Inode::EXT4_EXTENTS_FL;
        inode.write_extend_header();
        fs.modify_inode(device, inode_num, |td| {
            *td = inode;
        })?;
        return Ok(());
    }

    if !inode.have_extend_header_and_use_extend() {
        return Err(BlockDevError::Unsupported);
    }

    Ok(())
}

fn ext4_map_blocks_for_write<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    len: usize,
) -> BlockDevResult<()> {
    if len == 0 {
        return Ok(());
    }

    ext4_da_write_begin(device, fs, inode_num, offset, len)?;

    let mut inode = fs.get_inode_by_num(device, inode_num)?;
    let block_bytes = BLOCK_SIZE as u64;
    let end = offset.saturating_add(len as u64);

    let start_lbn = offset / block_bytes;
    let end_lbn = (end - 1) / block_bytes;

    let mut changed = false;
    for lbn in start_lbn..=end_lbn {
        if inode.have_extend_header_and_use_extend() {
            if resolve_inode_block(device, &mut inode, lbn as u32)?.is_none() {
                let new_phys = fs.alloc_block(device)?;
                let zero = zero_data_block();
                if let Err(e) = write_data_block_direct(device, new_phys, &zero) {
                    let _ = fs.free_block(device, new_phys);
                    return Err(e);
                }
                fs.datablock_cache.invalidate(new_phys);
                {
                    let mut tree = ExtentTree::new(&mut inode);
                    let ext = Ext4Extent::new(lbn as u32, new_phys, 1);
                    tree.insert_extent(fs, ext, device)?;
                }

                let add_iblocks = (BLOCK_SIZE / 512) as u32;
                inode.i_blocks_lo = inode.i_blocks_lo.saturating_add(add_iblocks);
                inode.l_i_blocks_high = inode
                    .l_i_blocks_high
                    .saturating_add(((add_iblocks as u64) >> 32) as u16);
                changed = true;
            }
        } else {
            if resolve_inode_block(device, &mut inode, lbn as u32)?.is_none() {
                return Err(BlockDevError::Unsupported);
            }
        }
    }

    if changed {
        fs.modify_inode(device, inode_num, |td| {
            *td = inode;
        })?;
    }

    Ok(())
}

fn ext4_write_prepared<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    data: &[u8],
) -> BlockDevResult<()> {
    if data.is_empty() {
        return Ok(());
    }

    let mut inode = fs.get_inode_by_num(device, inode_num)?;
    let block_bytes = BLOCK_SIZE as u64;
    let end = offset.saturating_add(data.len() as u64);
    let start_lbn = offset / block_bytes;
    let end_lbn = (end - 1) / block_bytes;

    for lbn in start_lbn..=end_lbn {
        let phys = resolve_inode_block(device, &mut inode, lbn as u32)?
            .ok_or(BlockDevError::Unsupported)? as u64;

        let block_start = lbn * block_bytes;
        let block_end = block_start + block_bytes;

        let write_start = core::cmp::max(offset, block_start);
        let write_end = core::cmp::min(end, block_end);
        if write_start >= write_end {
            continue;
        }

        let src_off = write_start - offset;
        let dst_off = (write_start - block_start) as usize;
        let len = write_end - write_start;

        let mut blk = if dst_off == 0 && len as usize == BLOCK_SIZE {
            zero_data_block()
        } else {
            read_data_block_direct(device, phys as u64)?
        };
        blk[dst_off..dst_off + len as usize]
            .copy_from_slice(&data[src_off as usize..(src_off + len) as usize]);
        write_data_block_direct(device, phys as u64, &blk)?;
        fs.datablock_cache.invalidate(phys as u64);
    }

    Ok(())
}

pub fn ext4_da_write_end<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    inode_num: u32,
    offset: u64,
    len: usize,
    copied: usize,
) -> BlockDevResult<()> {
    if len == 0 || copied == 0 {
        return Ok(());
    }

    let mut inode = fs.get_inode_by_num(device, inode_num)?;
    let end = offset.saturating_add(copied.min(len) as u64);

    if end > inode.size() {
        inode.i_size_lo = (end & 0xffff_ffff) as u32;
        inode.i_size_high = (end >> 32) as u32;
    }

    fs.modify_inode(device, inode_num, |td| {
        *td = inode;
    })?;

    Ok(())
}
