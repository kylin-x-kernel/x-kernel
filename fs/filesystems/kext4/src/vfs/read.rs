// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File block mapping, direct folio submission and read completion.

use alloc::{vec, vec::Vec};

use block::{BioVec, DriverResult};
use kvfs::{AddressSpace, VfsError, VfsResult};
use pagecache::{FolioRead, FolioReadPart};

use super::{PAGE_SIZE_4K, ext4_private, into_vfs_err};
use crate::{
    BlockMapping, CorruptKind, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, FilesystemBlock,
    InodeKind, LogicalBlock, io::FilesystemDevice,
};

enum FolioMapping {
    Zero,
    Contiguous {
        first: FilesystemBlock,
        len_bytes: usize,
    },
    Fragmented([Option<FilesystemBlock>; PAGE_SIZE_4K / 1024]),
    LargeBlock,
}

pub(super) fn submit_folios(
    mapping: &AddressSpace,
    folios: impl Iterator<Item = FolioRead>,
) -> VfsResult<()> {
    let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
    let inode = vfs_inode.private::<Ext4Inode>()?;
    let super_block = vfs_inode.super_block()?;
    let filesystem = ext4_private(&super_block)?.read();
    if inode.kind() == InodeKind::RegularFile {
        read_regular(&filesystem, inode, folios)
    } else {
        // Symlink bytes may live in the inode or journaled metadata cache.
        let file_len = inode.size();
        let mut result = Ok(());
        for folio in folios {
            result = result.and(read_synchronously(&filesystem, inode, folio, file_len));
        }
        result
    }
}

fn read_regular(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    folios: impl Iterator<Item = FolioRead>,
) -> VfsResult<()> {
    // Invalidate shared excludes shrink through mapping and submission. Once
    // submitted, the folio token keeps truncate from releasing these blocks.
    // Completion needs only the device, page tokens and this EOF snapshot.
    let file_len = inode.size();
    let block_size = filesystem.layout().block_size() as usize;
    let mut cached_mapping = None;
    let mut pending = None;
    let mut next_block = 0;
    let mut result = Ok(());
    for mut folio in folios {
        match map_folio(
            filesystem,
            inode,
            folio.index(),
            file_len,
            &mut cached_mapping,
        ) {
            Err(error) => result = result.and(Err(into_vfs_err(error))),
            Ok(FolioMapping::Zero) => {
                folio.data().fill(0);
                let _ = folio.complete(Ok(()));
            }
            Ok(FolioMapping::Fragmented(blocks)) => {
                result = result.and(submit_pending(&filesystem.device, &mut pending, file_len));
                result = result.and(read_fragmented(
                    &filesystem.device,
                    folio,
                    &blocks,
                    file_len,
                ));
            }
            Ok(FolioMapping::LargeBlock) => {
                result = result.and(submit_pending(&filesystem.device, &mut pending, file_len));
                result = result.and(read_synchronously(filesystem, inode, folio, file_len));
            }
            Ok(FolioMapping::Contiguous { first, len_bytes }) => {
                if first.get() != next_block {
                    result = result.and(submit_pending(&filesystem.device, &mut pending, file_len));
                }
                next_block = first
                    .get()
                    .checked_add((len_bytes / block_size) as u64)
                    .ok_or(VfsError::InvalidInput)?;
                folio.data()[len_bytes..].fill(0);
                let vector = BioVec::from_memory(folio, 0..len_bytes)
                    .map_err(|(error, _)| into_vfs_err(Ext4Error::Device(error)))?;
                pending
                    .get_or_insert_with(|| (first, Vec::new()))
                    .1
                    .push(vector);
            }
        }
    }
    result.and(submit_pending(&filesystem.device, &mut pending, file_len))
}

fn read_fragmented(
    device: &FilesystemDevice,
    folio: FolioRead,
    blocks: &[Option<FilesystemBlock>],
    file_len: u64,
) -> VfsResult<()> {
    let block_size = device.block_size();
    let blocks = &blocks[..PAGE_SIZE_4K / block_size];
    // Split only where the physical run changes. Holes and unwritten blocks
    // share a zeroed run; adjacent data blocks share one owner and one bio.
    let mut runs = blocks.chunk_by(|previous, next| match (previous, next) {
        (None, None) => true,
        (Some(previous), Some(next)) => previous.get().checked_add(1) == Some(next.get()),
        _ => false,
    });
    let first_run = runs.next().ok_or(VfsError::InvalidInput)?;
    let (first, mut remaining) = folio
        .split_at(first_run.len() * block_size)
        .map_err(|_| VfsError::InvalidInput)?;
    let mut result = submit_part(device, first, first_run[0], file_len);
    for run in runs {
        let len_bytes = run.len() * block_size;
        if len_bytes == remaining.data().len() {
            return result.and(submit_part(device, remaining, run[0], file_len));
        }
        let (part, tail) = remaining
            .split_at(len_bytes)
            .map_err(|_| VfsError::InvalidInput)?;
        remaining = tail;
        result = result.and(submit_part(device, part, run[0], file_len));
    }
    result
}

fn submit_part(
    device: &FilesystemDevice,
    mut part: FolioReadPart,
    physical: Option<FilesystemBlock>,
    file_len: u64,
) -> VfsResult<()> {
    let Some(physical) = physical else {
        part.data().fill(0);
        return part.complete(Ok(()));
    };
    let len = part.data().len();
    let vector = BioVec::from_memory(part, 0..len)
        .map_err(|(error, _)| into_vfs_err(Ext4Error::Device(error)))?;
    device
        .submit_read(physical, vec![vector], move |status, vectors| {
            let result = status.map_err(|error| into_vfs_err(Ext4Error::Device(error)));
            for vector in vectors {
                let Ok(mut part) = vector.into_memory::<FolioReadPart>() else {
                    error!("ext4 read completion received an unexpected part owner");
                    continue;
                };
                if result.is_ok() {
                    let offset = part.index() * PAGE_SIZE_4K as u64 + part.offset() as u64;
                    let data = part.data();
                    let valid = file_len.saturating_sub(offset).min(data.len() as u64) as usize;
                    data[valid..].fill(0);
                }
                let _ = part.complete(result);
            }
        })
        .map_err(into_vfs_err)
}

fn submit_pending(
    device: &FilesystemDevice,
    pending: &mut Option<(FilesystemBlock, Vec<BioVec>)>,
    file_len: u64,
) -> VfsResult<()> {
    let Some((first, vectors)) = pending.take() else {
        return Ok(());
    };
    device
        .submit_read(first, vectors, move |status, vectors| {
            complete_read(status, vectors, file_len);
        })
        .map_err(into_vfs_err)
}

fn complete_read(status: DriverResult, vectors: Vec<BioVec>, file_len: u64) {
    let result = status.map_err(|error| into_vfs_err(Ext4Error::Device(error)));
    for vector in vectors {
        let Ok(mut folio) = vector.into_memory::<FolioRead>() else {
            error!("ext4 read completion received an unexpected memory owner");
            continue;
        };
        if result.is_ok() {
            // map_folio checked this byte offset before submission. The last
            // filesystem block may contain disk bytes beyond the sampled EOF.
            let offset = folio.index() * PAGE_SIZE_4K as u64;
            let valid = file_len.saturating_sub(offset).min(PAGE_SIZE_4K as u64) as usize;
            folio.data()[valid..].fill(0);
        }
        let _ = folio.complete(result);
    }
}

fn read_synchronously(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    mut folio: FolioRead,
    file_len: u64,
) -> VfsResult<()> {
    let offset = folio
        .index()
        .checked_mul(PAGE_SIZE_4K as u64)
        .ok_or(VfsError::InvalidInput)?;
    let expected = file_len.saturating_sub(offset).min(PAGE_SIZE_4K as u64) as usize;
    let read = match inode.kind() {
        InodeKind::RegularFile => filesystem.read_at(inode, offset, folio.data()),
        InodeKind::Symlink => filesystem.read_link_at(inode, offset, folio.data()),
        _ => return Err(VfsError::InvalidInput),
    }
    .map_err(into_vfs_err)?;
    if read < expected || read > PAGE_SIZE_4K {
        return Err(VfsError::Io);
    }
    folio.data()[expected..].fill(0);
    folio.complete(Ok(()))
}

fn map_folio(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    index: u64,
    file_len: u64,
    cached: &mut Option<(u64, BlockMapping)>,
) -> Ext4Result<FolioMapping> {
    let offset = index
        .checked_mul(PAGE_SIZE_4K as u64)
        .ok_or(Ext4Error::Overflow)?;
    let valid = file_len.saturating_sub(offset).min(PAGE_SIZE_4K as u64) as usize;
    if valid == 0 {
        return Ok(FolioMapping::Zero);
    }
    let block_size = filesystem.layout().block_size() as usize;
    if block_size > PAGE_SIZE_4K {
        return Ok(FolioMapping::LargeBlock);
    }
    let logical = offset / block_size as u64;
    let mut first = None;
    let mut mapped_blocks = 0;
    let mut has_hole = false;
    let mut is_contiguous = true;
    let mut blocks = [None; PAGE_SIZE_4K / 1024];
    for (relative, block) in blocks
        .iter_mut()
        .enumerate()
        .take(valid.div_ceil(block_size))
    {
        *block = mapped_block(filesystem, inode, logical + relative as u64, cached)?;
        match *block {
            None => has_hole = true,
            Some(physical) => {
                let first = first.get_or_insert(physical);
                is_contiguous &=
                    !has_hole && first.get().checked_add(relative as u64) == Some(physical.get());
                mapped_blocks += 1;
            }
        }
    }
    Ok(match first {
        Some(first) if is_contiguous => FolioMapping::Contiguous {
            first,
            len_bytes: mapped_blocks * block_size,
        },
        Some(_) => FolioMapping::Fragmented(blocks),
        None => FolioMapping::Zero,
    })
}

// A returned extent can cover many folios. Reuse it during this submission
// while the filesystem read lock excludes changes to the extent tree.
fn mapped_block(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    logical: u64,
    cached: &mut Option<(u64, BlockMapping)>,
) -> Ext4Result<Option<FilesystemBlock>> {
    loop {
        if let Some((start, mapping)) = *cached {
            let len_blocks = match mapping {
                BlockMapping::Mapped { len, .. }
                | BlockMapping::Hole { len, .. }
                | BlockMapping::Unwritten { len, .. } => u64::from(len.get()),
            };
            if len_blocks == 0 {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            if let Some(relative) = logical
                .checked_sub(start)
                .filter(|&relative| relative < len_blocks)
            {
                return match mapping {
                    BlockMapping::Mapped { physical, .. } => {
                        if physical.get() == 0 {
                            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                        }
                        physical
                            .get()
                            .checked_add(relative)
                            .map(FilesystemBlock::new)
                            .map(Some)
                            .ok_or(Ext4Error::Overflow)
                    }
                    BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => Ok(None),
                };
            }
        }
        *cached = Some((
            logical,
            filesystem.map_blocks(inode, LogicalBlock::new(logical))?,
        ));
    }
}
