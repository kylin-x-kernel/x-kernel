// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked v2 headers and record codecs.

use kerrno::{KResult, LinuxError};

use crate::{
    Dqblk, QuotaFormat, QuotaIo, QuotaType, corrupt, read_exact, tree::QuotaTree, write_exact,
};

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(core::array::from_fn(|index| bytes[offset + index]))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(core::array::from_fn(|index| bytes[offset + index]))
}

pub(crate) fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn read_info(
    io: &dyn QuotaIo,
    kind: QuotaType,
    format: QuotaFormat,
) -> KResult<(u32, u32, QuotaTree)> {
    let mut header = [0; 32];
    read_exact(io, kind, 0, &mut header)?;
    if u32_at(&header, 0) != kind.magic() || u32_at(&header, 4) != format.version() {
        return Err(LinuxError::EINVAL.into());
    }
    let tree = QuotaTree::new(
        u32_at(&header, 20),
        u32_at(&header, 24),
        u32_at(&header, 28),
        io.file_len(kind)?,
    )?;
    Ok((u32_at(&header, 8), u32_at(&header, 12), tree))
}

pub(crate) fn write_info(
    io: &dyn QuotaIo,
    kind: QuotaType,
    space_grace_seconds: u32,
    inode_grace_seconds: u32,
    tree: &mut QuotaTree,
) -> KResult<()> {
    let mut info = [0; 24];
    put_u32(&mut info, 0, space_grace_seconds);
    put_u32(&mut info, 4, inode_grace_seconds);
    put_u32(&mut info, 12, tree.blocks);
    put_u32(&mut info, 16, tree.free_block);
    put_u32(&mut info, 20, tree.free_entry);
    write_exact(io, kind, 8, &info)?;
    tree.is_dirty = false;
    Ok(())
}

fn decode_limit(blocks: u64) -> KResult<i64> {
    let maximum_blocks = (i64::MAX as u64).div_ceil(1024);
    if blocks > maximum_blocks {
        return Err(corrupt());
    }
    // The largest signed limit rounds to a final partial 1 KiB unit.
    Ok((blocks * 1024).min(i64::MAX as u64) as i64)
}

pub(crate) fn decode_record(format: QuotaFormat, bytes: &[u8]) -> KResult<Dqblk> {
    let mut data = match format {
        QuotaFormat::VfsV0 => Dqblk {
            inode_hard_limit: i64::from(u32_at(bytes, 4)),
            inode_soft_limit: i64::from(u32_at(bytes, 8)),
            used_inodes: i64::from(u32_at(bytes, 12)),
            space_hard_limit_bytes: i64::from(u32_at(bytes, 16)) << 10,
            space_soft_limit_bytes: i64::from(u32_at(bytes, 20)) << 10,
            used_bytes: u64_at(bytes, 24) as i64,
            space_expires_at: u64_at(bytes, 32) as i64,
            inode_expires_at: u64_at(bytes, 40) as i64,
            ..Dqblk::default()
        },
        QuotaFormat::VfsV1 => Dqblk {
            inode_hard_limit: i64::try_from(u64_at(bytes, 8)).map_err(|_| corrupt())?,
            inode_soft_limit: i64::try_from(u64_at(bytes, 16)).map_err(|_| corrupt())?,
            used_inodes: u64_at(bytes, 24) as i64,
            space_hard_limit_bytes: decode_limit(u64_at(bytes, 32))?,
            space_soft_limit_bytes: decode_limit(u64_at(bytes, 40))?,
            used_bytes: u64_at(bytes, 48) as i64,
            space_expires_at: u64_at(bytes, 56) as i64,
            inode_expires_at: u64_at(bytes, 64) as i64,
            ..Dqblk::default()
        },
    };
    let timer_offset = format.entry_size() - 8;
    if u64_at(bytes, timer_offset) == 1 && bytes[..timer_offset].iter().all(|&byte| byte == 0) {
        data.inode_expires_at = 0;
    }
    Ok(data)
}

pub(crate) fn encode_record(
    format: QuotaFormat,
    id: u32,
    data: Dqblk,
    bytes: &mut [u8],
) -> KResult<()> {
    bytes.fill(0);
    put_u32(bytes, 0, id);
    let hard_blocks = (data.space_hard_limit_bytes as u64).div_ceil(1024);
    let soft_blocks = (data.space_soft_limit_bytes as u64).div_ceil(1024);
    match format {
        QuotaFormat::VfsV0 => {
            put_u32(
                bytes,
                4,
                u32::try_from(data.inode_hard_limit).map_err(|_| crate::overflow())?,
            );
            put_u32(
                bytes,
                8,
                u32::try_from(data.inode_soft_limit).map_err(|_| crate::overflow())?,
            );
            put_u32(
                bytes,
                12,
                u32::try_from(data.used_inodes).map_err(|_| crate::overflow())?,
            );
            put_u32(
                bytes,
                16,
                u32::try_from(hard_blocks).map_err(|_| crate::overflow())?,
            );
            put_u32(
                bytes,
                20,
                u32::try_from(soft_blocks).map_err(|_| crate::overflow())?,
            );
            put_u64(bytes, 24, data.used_bytes as u64);
            put_u64(bytes, 32, data.space_expires_at as u64);
            put_u64(bytes, 40, data.inode_expires_at as u64);
        }
        QuotaFormat::VfsV1 => {
            put_u64(bytes, 8, data.inode_hard_limit as u64);
            put_u64(bytes, 16, data.inode_soft_limit as u64);
            put_u64(bytes, 24, data.used_inodes as u64);
            put_u64(bytes, 32, hard_blocks);
            put_u64(bytes, 40, soft_blocks);
            put_u64(bytes, 48, data.used_bytes as u64);
            put_u64(bytes, 56, data.space_expires_at as u64);
            put_u64(bytes, 64, data.inode_expires_at as u64);
        }
    }
    // An all-zero slot means unused; preserve the real ID-zero record.
    if bytes.iter().all(|&byte| byte == 0) {
        put_u64(bytes, format.entry_size() - 8, 1);
    }
    Ok(())
}
