// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! # 目录操作模块
//!
//! 提供对 ext4 文件系统中目录的创建、删除、遍历等操作功能。

use alloc::{collections::BTreeMap, string::String, vec::Vec};

use log::{debug, error};

use crate::{
    alloc::string::ToString, blockdev::*, config::*, disknode::*, endian::*, entries::*, error::*,
    ext4::*, extents_tree::*, file::*, loopfile::*,
};

/// 文件操作错误类型
#[derive(Debug)]
pub enum FileError {
    /// 目录已存在
    DirExist,
    /// 文件已存在
    FileExist,
    /// 目录未找到
    DirNotFound,
    /// 文件未找到
    FileNotFound,
}

pub(crate) fn remember_dir_block(fs: &mut Ext4FileSystem, block_num: u64) {
    fs.datablock_cache.remember_metadata_block(block_num);
}

pub(crate) fn remember_dir_blocks(fs: &mut Ext4FileSystem, blocks: &BTreeMap<u32, u64>) {
    for &phys in blocks.values() {
        remember_dir_block(fs, phys);
    }
}

/// Clears `EXT4_INDEX_FL` before converting an indexed directory to linear lookup.
///
/// Linux ext4 clears this flag and falls back to linear insertion when HTree
/// insertion reports an unusable index and metadata checksums are disabled.
/// rsext4 does not yet maintain HTree indexes during insertion, so it takes
/// that fallback before overwriting directory-index metadata with a regular
/// directory entry.
fn clear_dir_index(inode: &mut Ext4Inode) {
    inode.i_flags &= !Ext4Inode::EXT4_INDEX_FL;
}

/// Inserts `new_entry` into reusable space in one linear ext4 directory block.
///
/// Each record stores `inode`, `rec_len`, `name_len`, and `file_type` in its
/// first eight bytes, followed by the name. A zero inode marks a reusable
/// record. For an occupied record, only the aligned header and name are needed;
/// the unused tail covered by `rec_len` can be split off for the new entry.
fn try_insert_dir_entry_in_block(data: &mut [u8], new_entry: &Ext4DirEntry2) -> bool {
    let block_bytes = data.len();
    let new_rec_len = new_entry.rec_len as usize;
    let mut offset = 0usize;

    while offset + 8 <= block_bytes {
        let inode = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let rec_len = u16::from_le_bytes([data[offset + 4], data[offset + 5]]) as usize;
        if rec_len < 8 {
            return false;
        }
        let entry_end = offset + rec_len;
        if entry_end > block_bytes {
            return false;
        }

        let insert_offset = if inode == 0 {
            (rec_len >= new_rec_len).then_some(offset)
        } else {
            let current_name_len = data[offset + 6] as usize;
            let ideal_len = (8 + current_name_len).div_ceil(4) * 4;
            let tail_len = rec_len.saturating_sub(ideal_len);
            if tail_len >= new_rec_len {
                data[offset + 4..offset + 6].copy_from_slice(&(ideal_len as u16).to_le_bytes());
                Some(offset + ideal_len)
            } else {
                None
            }
        };

        if let Some(insert_offset) = insert_offset {
            let mut entry = *new_entry;
            entry.rec_len = (entry_end - insert_offset) as u16;
            entry.to_disk_bytes(&mut data[insert_offset..insert_offset + 8]);
            let name_len = entry.name_len as usize;
            data[insert_offset + 8..insert_offset + 8 + name_len]
                .copy_from_slice(&entry.name[..name_len]);
            return true;
        }

        if entry_end == block_bytes {
            return false;
        }
        offset = entry_end;
    }

    false
}

/// Normalizes an rsext4 API path to an absolute path under the filesystem root.
///
/// Paths are rooted at this filesystem. Parent components at the root are
/// therefore no-ops, matching Linux pathname resolution at a lookup root.
///
/// # Errors
///
/// Returns [`BlockDevError::InvalidInput`] for an empty path or an embedded NUL.
pub fn normalize_path(path: &str) -> BlockDevResult<String> {
    if path.is_empty() || path.contains('\0') {
        return Err(BlockDevError::InvalidInput);
    }

    let mut components = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let _ = components.pop();
            }
            name => components.push(name),
        }
    }

    if components.is_empty() {
        return Ok(String::from("/"));
    }

    let mut normalized = String::new();
    for component in components {
        normalized.push('/');
        normalized.push_str(component);
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_clamps_parent_components_at_root() {
        assert_eq!(normalize_path("/..").unwrap(), "/");
        assert_eq!(normalize_path("/../host").unwrap(), "/host");
        assert_eq!(normalize_path("/../../host").unwrap(), "/host");
        assert_eq!(normalize_path("../host").unwrap(), "/host");
    }

    #[test]
    fn normalize_path_normalizes_inside_root() {
        assert_eq!(normalize_path("/a/./b/../c").unwrap(), "/a/c");
        assert_eq!(normalize_path("a//b/").unwrap(), "/a/b");
    }

    #[test]
    fn normalize_path_rejects_nul() {
        assert!(normalize_path("/a\0b").is_err());
    }

    #[test]
    fn normalize_path_rejects_empty() {
        assert!(normalize_path("").is_err());
    }

    #[test]
    fn linear_insert_downgrades_htree_root_layout() {
        let mut inode = Ext4Inode {
            i_flags: Ext4Inode::EXT4_EXTENTS_FL | Ext4Inode::EXT4_INDEX_FL,
            ..Ext4Inode::default()
        };
        assert_ne!(inode.i_flags & Ext4Inode::EXT4_INDEX_FL, 0);
        clear_dir_index(&mut inode);
        assert_eq!(inode.i_flags, Ext4Inode::EXT4_EXTENTS_FL);

        let mut block = alloc::vec![0u8; BLOCK_SIZE];
        let dot_len = Ext4DirEntry2::entry_len(1);
        let dot = Ext4DirEntry2::new(38, dot_len, Ext4DirEntry2::EXT4_FT_DIR, b".");
        dot.to_disk_bytes(&mut block[..8]);
        block[8] = b'.';

        let dotdot = Ext4DirEntry2::new(
            2,
            BLOCK_SIZE as u16 - dot_len,
            Ext4DirEntry2::EXT4_FT_DIR,
            b"..",
        );
        let dotdot_offset = dot_len as usize;
        dotdot.to_disk_bytes(&mut block[dotdot_offset..dotdot_offset + 8]);
        block[dotdot_offset + 8..dotdot_offset + 10].copy_from_slice(b"..");

        // An indexed directory stores its dx root in the slack covered by '..'.
        // Linear insertion must replace that metadata after clearing EXT4_INDEX_FL.
        block[dotdot_offset + 12..dotdot_offset + 20].fill(0xa5);
        let child = Ext4DirEntry2::new(
            99,
            Ext4DirEntry2::entry_len(6),
            Ext4DirEntry2::EXT4_FT_REG_FILE,
            b"weston",
        );
        assert!(try_insert_dir_entry_in_block(&mut block, &child));

        assert_eq!(
            u16::from_le_bytes([block[dotdot_offset + 4], block[dotdot_offset + 5]]),
            Ext4DirEntry2::entry_len(2)
        );
        assert_eq!(
            classic_dir::find_entry(&block, b"weston").map(|entry| entry.inode),
            Some(99)
        );
    }
}

/// Resolves a filesystem path component by component to its inode.
///
/// Each component uses the same per-directory lookup as other rsext4 callers,
/// preserving indexed lookup with linear fallback for HTree directories.
pub fn get_inode_with_num<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    device: &mut Jbd2Dev<B>,
    path: &str,
) -> BlockDevResult<Option<(u32, Ext4Inode)>> {
    let norm_path = normalize_path(path)?;

    let mut current = (fs.root_inode, fs.get_root(device)?);
    for name in norm_path
        .split('/')
        .filter(|component| !component.is_empty())
    {
        let Some(next) = get_inode_by_name(fs, device, current.0, name)? else {
            return Ok(None);
        };
        current = next;
    }

    Ok(Some(current))
}

/// Looks up the inode number and `Ext4Inode` for a given entry name
/// within the ext4 filesystem.
///
/// Returns `Ok(Some((ino, inode)))` if found, `Ok(None)` if the entry
/// does not exist, or `Err` on I/O or corruption errors.
pub fn get_inode_by_name<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    device: &mut Jbd2Dev<B>,
    parent_ino: u32,
    name: &str,
) -> BlockDevResult<Option<(u32, Ext4Inode)>> {
    if name.is_empty() || name.contains('\0') {
        return Err(BlockDevError::InvalidInput);
    }
    let parent_inode = fs.get_inode_by_num(device, parent_ino)?;
    let entry = find_named_entry_in_parent(fs, device, parent_ino, &parent_inode, name.as_bytes())?;
    if let Some(entry) = entry {
        let inode = fs.get_inode_by_num(device, entry.ino)?;
        Ok(Some((entry.ino, inode)))
    } else {
        Ok(None)
    }
}

/// Inserts a child entry into a parent directory, extending it when necessary.
pub fn insert_dir_entry<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    device: &mut Jbd2Dev<B>,
    parent_ino_num: u32,
    parent_inode: &mut Ext4Inode,
    child_ino: u32,
    child_name: &str,
    file_type: u8,
) -> BlockDevResult<()> {
    let name_bytes = child_name.as_bytes();
    let name_len = core::cmp::min(name_bytes.len(), Ext4DirEntry2::MAX_NAME_LEN as usize);
    let new_entry = Ext4DirEntry2::new(
        child_ino,
        Ext4DirEntry2::entry_len(name_len as u8),
        file_type,
        &name_bytes[..name_len],
    );

    let was_indexed = parent_inode.i_flags & Ext4Inode::EXT4_INDEX_FL != 0;
    if was_indexed {
        fs.modify_inode(device, parent_ino_num, |inode| {
            clear_dir_index(inode);
        })?;
        clear_dir_index(parent_inode);
    }

    let total_size = parent_inode.size() as usize;
    let block_bytes = BLOCK_SIZE;
    let total_blocks = if total_size == 0 {
        0
    } else {
        total_size.div_ceil(block_bytes)
    };

    let mut inserted = false;

    let blocks = resolve_inode_block_allextend(fs, device, parent_inode)?;
    remember_dir_blocks(fs, &blocks);

    for lbn in 0..total_blocks {
        if inserted {
            break;
        }

        let phys = match blocks.get(&(lbn as u32)) {
            Some(&b) => b,
            None => {
                error!(
                    "insert_dir_entry: missing extent mapping for parent_ino={parent_ino_num} \
                     lbn={lbn} name={child_name:?}"
                );
                return Err(BlockDevError::Corrupted);
            }
        };

        fs.datablock_cache.modify(device, phys, |data| {
            inserted = try_insert_dir_entry_in_block(data, &new_entry);
        })?;
    }

    if inserted {
        return Ok(());
    }

    // 所有现有逻辑块都无法容纳新目录项：为目录分配一个新数据块，并扩展 inode 映射
    let new_block = fs.alloc_block(device)?;
    remember_dir_block(fs, new_block);

    // 更新 parent_inode 的块映射（extent 或直接块）和大小统计
    let block_bytes = BLOCK_SIZE;
    let old_blocks = if total_size == 0 {
        0
    } else {
        total_size.div_ceil(block_bytes)
    };
    let new_lbn = old_blocks as u32; // 新块对应的逻辑块号

    if fs.superblock.has_extents() && parent_inode.have_extend_header_and_use_extend() {
        // extent 目录：通过 ExtentTree 追加一个长度为 1 的 extent
        let new_ext = Ext4Extent::new(new_lbn, new_block, 1);
        let mut tree = ExtentTree::new(parent_inode);
        tree.insert_extent(fs, new_ext, device)?;
    } else {
        // 传统直接块模式：仅支持追加到前 12 个直接块
        if old_blocks >= 12 {
            return Err(BlockDevError::Unsupported);
        }
        parent_inode.i_block[old_blocks] = new_block as u32;
    }

    // 更新 parent_inode 的 i_size / i_blocks，并写回 inode 表
    let new_size = total_size + block_bytes;
    parent_inode.i_size_lo = new_size as u32;
    parent_inode.i_size_high = ((new_size as u64) >> 32) as u32;
    // fix:extend元数据也会占block，不能仅仅靠现有blocks_count计算，需要考虑extent树的开销
    let cur = parent_inode.blocks_count();
    let add_sectors = BLOCK_SIZE as u64 / 512;
    let newv = cur.saturating_add(add_sectors);
    parent_inode.i_blocks_lo = (newv & 0xffff_ffff) as u32;
    parent_inode.l_i_blocks_high = ((newv >> 32) & 0xffff) as u16;

    let (p_group, _pidx) = fs.inode_allocator.global_to_group(parent_ino_num);
    let inode_table_start = match fs.group_descs.get(p_group as usize) {
        Some(desc) => desc.inode_table(),
        None => return Err(BlockDevError::Corrupted),
    };
    let (p_block_num, p_offset, _pg) = fs.inodetable_cache.calc_inode_location(
        parent_ino_num,
        fs.layout.inodes_per_group,
        inode_table_start,
        BLOCK_SIZE,
    );

    fs.inodetable_cache.modify(
        device,
        parent_ino_num as u64,
        p_block_num,
        p_offset,
        |inode| {
            inode.i_size_lo = parent_inode.i_size_lo;
            inode.i_size_high = parent_inode.i_size_high;
            inode.i_blocks_lo = parent_inode.i_blocks_lo;
            inode.l_i_blocks_high = parent_inode.l_i_blocks_high;
            inode.i_flags = parent_inode.i_flags;
            inode.i_block = parent_inode.i_block;
        },
    )?;

    // 在新分配的数据块中写入唯一的目录项，占满整个块
    fs.datablock_cache.modify(device, new_block, |data| {
        for b in data.iter_mut() {
            *b = 0;
        }
        let mut full_entry = new_entry;
        full_entry.rec_len = BLOCK_SIZE as u16;
        full_entry.to_disk_bytes(&mut data[0..8]);
        let nlen = full_entry.name_len as usize;
        data[8..8 + nlen].copy_from_slice(&full_entry.name[..nlen]);
    })?;

    Ok(())
}

/// 默认开启hashtree查找
/// 通用文件创建：支持多级路径、递归创建父目录
pub fn mkdir<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
) -> Option<Ext4Inode> {
    mkdir_with_ino(device, fs, path).map(|(_, inode)| inode)
}

pub fn mkdir_with_ino<B: BlockDevice>(
    device: &mut Jbd2Dev<B>,
    fs: &mut Ext4FileSystem,
    path: &str,
) -> Option<(u32, Ext4Inode)> {
    // 先对传入路径做规范化（去掉重复的 '/' 等）
    let norm_path = normalize_path(path).ok()?;

    // 若目标已存在，直接返回
    if let Ok(Some(inode)) = get_file_inode(fs, device, &norm_path) {
        return Some(inode);
    }

    // 根目录和空路径的特殊情况
    if norm_path.is_empty() || norm_path == "/" {
        debug!("Creating root directory");
        if let Err(e) = create_root_directory_entry(fs, device) {
            error!("mkdir create_root_directory_entry failed path={path} err={e:?} ({e})");
            return None;
        }
        return match fs.get_root(device) {
            Ok(inode) => Some((fs.root_inode, inode)),
            Err(e) => {
                error!("mkdir get_root failed path={path} err={e:?} ({e})");
                None
            }
        };
    }

    // 拆分规范化路径，构建 path_vec
    let parts: Vec<&str> = norm_path.split('/').filter(|s| !s.is_empty()).collect();

    if parts.is_empty() {
        return match fs.get_root(device) {
            Ok(inode) => Some((fs.root_inode, inode)),
            Err(e) => {
                error!("mkdir get_root failed(empty parts) path={path} err={e:?} ({e})");
                None
            }
        };
    }

    // 从头逐一判断父路径是否存在，不存在则递归创建
    // 只针对中间父目录，最后一个组件留给当前 mkd 创建
    let mut cur_path = String::from("");
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        cur_path.push('/');
        cur_path.push_str(part);

        if let Ok(None) = get_file_inode(fs, device, &cur_path)
            && mkdir(device, fs, &cur_path).is_none()
        {
            error!("mkdir recursive parent create failed path={path} parent={cur_path}");
            return None;
        }
    }

    // 计算 parent 与 child
    let child = parts.last().unwrap().to_string();
    let parent = if parts.len() == 1 {
        "/".to_string()
    } else {
        let mut p = String::from("");
        for part in parts.iter().take(parts.len() - 1) {
            p.push('/');
            p.push_str(part);
        }
        p
    };

    // 再次获取父目录 inode 及其 inode 号
    let (parent_ino_num, mut parent_inode) =
        match get_inode_with_num(fs, device, &parent).ok().flatten() {
            Some((n, ino)) => (n, ino),
            None => {
                error!("mkdir get parent inode failed path={path} parent={parent} child={child}");
                return None;
            }
        };

    // 特殊情况：根目录本身
    if (parent.is_empty() || parent == "/") && child.is_empty() {
        debug!("Creating root directory");
        if let Err(e) = create_root_directory_entry(fs, device) {
            error!("mkdir create_root_directory_entry failed path={path} err={e:?} ({e})");
            return None;
        }
        return match fs.get_root(device) {
            Ok(inode) => Some((fs.root_inode, inode)),
            Err(e) => {
                error!("mkdir get_root failed path={path} err={e:?} ({e})");
                None
            }
        };
    }

    // 特殊情况：/lost+found
    if (parent.is_empty() || parent == "/") && child == "lost+found" {
        debug!("Creating /lost+found directory");
        if let Err(e) = create_lost_found_directory(fs, device) {
            error!("mkdir create_lost_found_directory failed path={path} err={e:?} ({e})");
            return None;
        }
        return match get_inode_with_num(fs, device, "/lost+found").ok().flatten() {
            Some((ino, inode)) => Some((ino, inode)),
            None => {
                error!("mkdir post-create lost+found lookup failed path={path}");
                None
            }
        };
    }

    // 为新目录分配 inode（内部自动选择块组）
    let new_dir_ino = match fs.alloc_inode(device) {
        Ok(ino) => ino,
        Err(e) => {
            error!(
                "mkdir alloc_inode failed path={path} parent={parent} child={child} err={e:?} \
                 ({e})"
            );
            return None;
        }
    };

    // 为新目录分配数据块（内部自动选择块组）
    let data_block = match fs.alloc_block(device) {
        Ok(b) => b,
        Err(e) => {
            error!("mkdir alloc_block failed path={path} ino={new_dir_ino} err={e:?} ({e})");
            return None;
        }
    };

    // 初始化新目录的数据块：写 '.' 和 '..'
    {
        remember_dir_block(fs, data_block);
        let cached = fs.datablock_cache.create_new(data_block);
        let data = &mut cached.data;

        let dot_name = b".";
        let dot_rec_len = Ext4DirEntry2::entry_len(dot_name.len() as u8);
        let dot = Ext4DirEntry2::new(
            new_dir_ino,
            dot_rec_len,
            Ext4DirEntry2::EXT4_FT_DIR,
            dot_name,
        );

        let dotdot_name = b"..";
        let dotdot_rec_len = (BLOCK_SIZE as u16).saturating_sub(dot_rec_len);
        let dotdot = Ext4DirEntry2::new(
            parent_ino_num,
            dotdot_rec_len,
            Ext4DirEntry2::EXT4_FT_DIR,
            dotdot_name,
        );

        {
            dot.to_disk_bytes(&mut data[0..8]);
            let name_len = dot.name_len as usize;
            data[8..8 + name_len].copy_from_slice(&dot.name[..name_len]);
        }

        {
            let offset = dot_rec_len as usize;
            dotdot.to_disk_bytes(&mut data[offset..offset + 8]);
            let name_len = dotdot.name_len as usize;
            data[offset + 8..offset + 8 + name_len].copy_from_slice(&dotdot.name[..name_len]);
        }
    }

    // 写新目录 inode（单块目录，按特性选择 extent 或直接块）
    let (group_idx, _idx) = fs.inode_allocator.global_to_group(new_dir_ino);
    // 仅仅的视图，修改过后的

    let mut inode_pre = fs
        .get_inode_by_num(device, new_dir_ino)
        .expect("Can't getinode");
    build_file_block_mapping(fs, &mut inode_pre, &[data_block], device);
    if fs
        .modify_inode(device, new_dir_ino, |inode| {
            inode.i_block = inode_pre.i_block;
            inode.i_mode = Ext4Inode::S_IFDIR | 0o755;
            inode.i_links_count = 2; // . 和 entires本身
            inode.i_size_lo = BLOCK_SIZE as u32;
            inode.i_size_high = 0;
            inode.i_blocks_lo = (BLOCK_SIZE / 512) as u32;
            inode.l_i_blocks_high = 0;
            inode.i_dtime = 0;
            inode.i_flags |= inode_pre.i_flags

            // 由于借用冲突，暂时先把mapping移步到外面
        })
        .is_err()
    {
        error!("mkdir modify_inode failed path={path} ino={new_dir_ino}");
        return None;
    }

    // 更新父目录的i_links_count+1
    {
        let (p_group, _pidx) = fs.inode_allocator.global_to_group(parent_ino_num);
        let p_inode_table_start = match fs.group_descs.get(p_group as usize) {
            Some(desc) => desc.inode_table(),
            None => {
                error!(
                    "mkdir parent group desc missing path={path} parent_ino={parent_ino_num} \
                     group={p_group}"
                );
                return None;
            }
        };
        let (p_block_num, p_offset, _pg) = fs.inodetable_cache.calc_inode_location(
            parent_ino_num,
            fs.layout.inodes_per_group,
            p_inode_table_start,
            BLOCK_SIZE,
        );

        let _ = fs.inodetable_cache.modify(
            device,
            parent_ino_num as u64,
            p_block_num,
            p_offset,
            |inode| {
                inode.i_links_count = inode.i_links_count.saturating_add(1);
            },
        );
    }

    // 更新新目录所属块组的目录计数
    if let Some(desc) = fs.get_group_desc_mut(group_idx) {
        let newc = desc.used_dirs_count().saturating_add(1);
        desc.bg_used_dirs_count_lo = (newc & 0xFFFF) as u16;
        desc.bg_used_dirs_count_hi = ((newc >> 16) & 0xFFFF) as u16;
    }

    // 在父目录的数据块中插入新目录项（线性目录，多块遍历，必要时自动扩展目录块）
    if insert_dir_entry(
        fs,
        device,
        parent_ino_num,
        &mut parent_inode,
        new_dir_ino,
        &child,
        Ext4DirEntry2::EXT4_FT_DIR,
    )
    .is_err()
    {
        error!(
            "mkdir insert_dir_entry failed path={path} parent_ino={parent_ino_num} child={child} \
             ino={new_dir_ino}"
        );
        return None;
    }

    match fs.get_inode_by_num(device, new_dir_ino) {
        Ok(inode) => Some((new_dir_ino, inode)),
        Err(e) => {
            error!("mkdir get_inode_by_num failed path={path} ino={new_dir_ino} err={e:?} ({e})");
            None
        }
    }
}

/// 根目录创建实现
pub fn create_root_directory_entry<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
) -> BlockDevResult<()> {
    debug!("Initializing root directory...");
    // 是否需要创建根目录由挂载流程基于 inode 内容判断，这里只负责真正的创建

    //  为根目录分配一个数据块（内部自动选择块组）
    let root_inode_num = fs.root_inode;
    let data_block = fs.alloc_block(block_dev)?;

    //  写入目录项 . 和 ..
    {
        remember_dir_block(fs, data_block);
        let cached = fs.datablock_cache.create_new(data_block);
        let data = &mut cached.data;

        // . 目录项
        let dot_name = b".";
        let dot_rec_len = Ext4DirEntry2::entry_len(dot_name.len() as u8);
        let dot = Ext4DirEntry2::new(
            root_inode_num,
            dot_rec_len,
            Ext4DirEntry2::EXT4_FT_DIR,
            dot_name,
        );

        // ..目录项（根的父目录仍为自己）
        let dotdot_name = b"..";
        let dotdot_rec_len = (BLOCK_SIZE as u16).saturating_sub(dot_rec_len);
        let dotdot = Ext4DirEntry2::new(
            root_inode_num,
            dotdot_rec_len,
            Ext4DirEntry2::EXT4_FT_DIR,
            dotdot_name,
        );

        {
            dot.to_disk_bytes(&mut data[0..8]);
            let name_len = dot.name_len as usize;
            data[8..8 + name_len].copy_from_slice(&dot.name[..name_len]);
        }

        {
            let offset = dot_rec_len as usize;
            dotdot.to_disk_bytes(&mut data[offset..offset + 8]);
            let name_len = dotdot.name_len as usize;
            data[offset + 8..offset + 8 + name_len].copy_from_slice(&dotdot.name[..name_len]);
        }
    }

    // 仅仅的视图，修改过后的
    let root_inode_num = fs.root_inode;
    let mut inode_pre = fs
        .get_inode_by_num(block_dev, root_inode_num)
        .expect("Can't getinode");
    build_file_block_mapping(fs, &mut inode_pre, &[data_block], block_dev);

    fs.modify_inode(block_dev, fs.root_inode, |inode| {
        inode.i_flags = inode_pre.i_flags;
        inode.i_block = inode_pre.i_block;
        inode.i_mode = Ext4Inode::S_IFDIR | 0o755; // 目录 + 权限
        inode.i_links_count = 2; // . 和 ..
        inode.i_size_lo = BLOCK_SIZE as u32;
        inode.i_size_high = 0;
        // i_blocks 以 512 字节为单位
        inode.i_blocks_lo = (BLOCK_SIZE / 512) as u32;
        inode.l_i_blocks_high = 0;
    })?;

    // 块组描述符更新 目录数
    if let Some(desc) = fs.get_group_desc_mut(0) {
        let newc = desc.used_dirs_count().saturating_add(1);
        desc.bg_used_dirs_count_lo = (newc & 0xFFFF) as u16;
        desc.bg_used_dirs_count_hi = ((newc >> 16) & 0xFFFF) as u16;
    }

    debug!(
        "Root directory created: inode={}, data_block={}",
        fs.root_inode, data_block
    );
    Ok(())
}

/// 创建 /lost+found 目录，并将其挂到根目录下
pub fn create_lost_found_directory<B: BlockDevice>(
    fs: &mut Ext4FileSystem,
    block_dev: &mut Jbd2Dev<B>,
) -> BlockDevResult<()> {
    // 如果已经存在则直接返回
    if file_entry_exisr(fs, block_dev, "/lost+found") {
        return Ok(());
    }

    let root_inode_num = fs.root_inode;

    //  分配 inode（内部自动选择块组）
    let lost_ino = fs.alloc_inode(block_dev)?;
    debug!("lost+found inode: {lost_ino}");

    //  分配数据块（内部自动选择块组）
    let data_block = fs.alloc_block(block_dev)?;

    //  初始化 lost+found 目录块（".", ".."）
    {
        remember_dir_block(fs, data_block);
        let cached = fs.datablock_cache.create_new(data_block);
        let data = &mut cached.data;

        let dot_name = b".";
        let dot_rec_len = Ext4DirEntry2::entry_len(dot_name.len() as u8);
        let dot = Ext4DirEntry2::new(lost_ino, dot_rec_len, Ext4DirEntry2::EXT4_FT_DIR, dot_name);

        let dotdot_name = b"..";
        let dotdot_rec_len = (BLOCK_SIZE as u16).saturating_sub(dot_rec_len);
        let dotdot = Ext4DirEntry2::new(
            root_inode_num,
            dotdot_rec_len,
            Ext4DirEntry2::EXT4_FT_DIR,
            dotdot_name,
        );

        {
            dot.to_disk_bytes(&mut data[0..8]);
            let name_len = dot.name_len as usize;
            data[8..8 + name_len].copy_from_slice(&dot.name[..name_len]);
        }

        {
            let offset = dot_rec_len as usize;
            dotdot.to_disk_bytes(&mut data[offset..offset + 8]);
            let name_len = dotdot.name_len as usize;
            data[offset + 8..offset + 8 + name_len].copy_from_slice(&dotdot.name[..name_len]);
        }
    }

    //  写 lost+found inode
    let (lf_group, _idx) = fs.inode_allocator.global_to_group(lost_ino);

    // 仅仅的视图，修改过后的
    let mut inode_pre = fs
        .get_inode_by_num(block_dev, lost_ino)
        .expect("Can't getinode");
    build_file_block_mapping(fs, &mut inode_pre, &[data_block], block_dev);
    debug!(
        "When create lost+found inode iblock,:{:?} ,data_block:{:?}",
        inode_pre.i_block, data_block
    );
    // lost+found 的数据块映射与根目录保持一致：单块目录，按特性选择 extent 或直接块
    fs.modify_inode(block_dev, lost_ino, |inode| {
        // 写回 build_block_dir_mapping 已经构建好的块映射和标志
        inode.i_block = inode_pre.i_block;
        inode.i_flags = inode_pre.i_flags;
        inode.i_mode = Ext4Inode::S_IFDIR | 0o755;
        inode.i_links_count = 2;
        inode.i_size_lo = BLOCK_SIZE as u32;
        inode.i_blocks_lo = (BLOCK_SIZE / 512) as u32;
    })?;

    if let Some(desc) = fs.get_group_desc_mut(lf_group) {
        let newc = desc.used_dirs_count().saturating_add(1);
        desc.bg_used_dirs_count_lo = (newc & 0xFFFF) as u16;
        desc.bg_used_dirs_count_hi = ((newc >> 16) & 0xFFFF) as u16;
    }

    //  更新根目录数据块：加入 lost+found 目录项

    // 这里也需要根据extend来解析
    let mut root_inode = fs.get_root(block_dev)?;
    let root_block = resolve_inode_block(block_dev, &mut root_inode, 0)?
        .expect("lost+found logical_block can't map to physical blcok!");
    remember_dir_block(fs, root_block as u64);

    if root_block == 0 {
        return Err(BlockDevError::Corrupted);
    }

    fs.datablock_cache
        .modify(block_dev, root_block as u64, move |data| {
            let dot_name = b".";
            let dot_rec_len = Ext4DirEntry2::entry_len(dot_name.len() as u8);
            let dot = Ext4DirEntry2::new(
                root_inode_num,
                dot_rec_len,
                Ext4DirEntry2::EXT4_FT_DIR,
                dot_name,
            );

            let dotdot_name = b"..";
            let dotdot_rec_len = Ext4DirEntry2::entry_len(dotdot_name.len() as u8);
            let dotdot = Ext4DirEntry2::new(
                root_inode_num,
                dotdot_rec_len,
                Ext4DirEntry2::EXT4_FT_DIR,
                dotdot_name,
            );

            let lf_name = b"lost+found";
            let lf_rec_len = (BLOCK_SIZE as u16).saturating_sub(dot_rec_len + dotdot_rec_len);
            let lost =
                Ext4DirEntry2::new(lost_ino, lf_rec_len, Ext4DirEntry2::EXT4_FT_DIR, lf_name);

            // 清零整个块
            for b in data.iter_mut() {
                *b = 0;
            }

            // 写 .
            dot.to_disk_bytes(&mut data[0..8]);
            let name_len = dot.name_len as usize;
            data[8..8 + name_len].copy_from_slice(&dot.name[..name_len]);

            // 写 ..
            let mut offset = dot_rec_len as usize;
            dotdot.to_disk_bytes(&mut data[offset..offset + 8]);
            let dd_len = dotdot.name_len as usize;
            data[offset + 8..offset + 8 + dd_len].copy_from_slice(&dotdot.name[..dd_len]);

            // 写 lost+found
            offset += dotdot_rec_len as usize;
            lost.to_disk_bytes(&mut data[offset..offset + 8]);
            let lf_len = lost.name_len as usize;
            data[offset + 8..offset + 8 + lf_len].copy_from_slice(&lost.name[..lf_len]);
        })?;

    //  更新根 inode 的链接计数（多了一个子目录）
    let inode_table_start = match fs.group_descs.first() {
        Some(desc) => desc.inode_table(),
        None => return Err(BlockDevError::Corrupted),
    };
    let (block_num, offset, _group_idx) = fs.inodetable_cache.calc_inode_location(
        fs.root_inode,
        fs.layout.inodes_per_group,
        inode_table_start,
        BLOCK_SIZE,
    );

    fs.inodetable_cache.modify(
        block_dev,
        fs.root_inode as u64,
        block_num,
        offset,
        |inode| {
            inode.i_links_count = inode.i_links_count.saturating_add(1);
        },
    )?;

    //  记录到超级块
    fs.superblock.s_lpf_ino = lost_ino;

    debug!("lost+found directory created: inode={lost_ino}, data_block={data_block}");

    Ok(())
}
