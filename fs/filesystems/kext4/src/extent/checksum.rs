// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::validate::decode_header;
use crate::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, Ext4SbInfo, PhysicalBlock,
    disk::{checksum, extent as disk_extent},
    inode::{Ext4Inode, inode_checksum_seed},
};

pub(super) fn verify_extent_block_checksum(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    block: PhysicalBlock,
    bytes: &[u8],
) -> Ext4Result<()> {
    let header = decode_header(bytes)?;
    let tail_offset = disk_extent::tail_offset(header)?;
    let expected = disk_extent::tail_checksum(bytes, header)?;
    let checksum_bytes = bytes
        .get(..tail_offset)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let seed = inode_checksum_seed(
        filesystem.superblock().checksum_seed(),
        inode.number(),
        inode.generation(),
    );
    let actual = checksum::crc32c(seed, checksum_bytes);
    if actual != expected {
        return Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::ExtentBlock {
                inode: inode.number().get(),
                block: block.get(),
            },
            expected,
            actual,
        });
    }
    Ok(())
}

pub(super) fn update_extent_block_checksum(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    bytes: &mut [u8],
) -> Ext4Result<()> {
    if !filesystem.superblock().features().has_metadata_checksum() {
        return Ok(());
    }
    let header = decode_header(bytes)?;
    let tail_offset = disk_extent::tail_offset(header)?;
    let checksum_bytes = bytes
        .get(..tail_offset)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let seed = inode_checksum_seed(
        filesystem.superblock().checksum_seed(),
        inode.number(),
        inode.generation(),
    );
    let checksum = checksum::crc32c(seed, checksum_bytes);
    disk_extent::write_tail_checksum(bytes, header, checksum)
}
