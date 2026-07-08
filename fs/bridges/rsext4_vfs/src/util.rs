// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 adapter utilities.
use alloc::{borrow::ToOwned, string::String};

use kerrno::LinuxError;
use kvfs::{DeviceId, NodeType, Umode, VfsError};
use rsext4::{disknode::Ext4Inode, error::BlockDevError};

/// Convert rsext4 block device errors into VFS errors.
pub(crate) fn into_vfs_err(err: BlockDevError) -> VfsError {
    let linux_error = match err {
        BlockDevError::InvalidInput => LinuxError::EINVAL,
        BlockDevError::NotDirectory => LinuxError::ENOTDIR,
        BlockDevError::IsDirectory => LinuxError::EISDIR,
        BlockDevError::DirectoryNotEmpty => LinuxError::ENOTEMPTY,
        BlockDevError::ReadError | BlockDevError::WriteError | BlockDevError::IoError => {
            LinuxError::EIO
        }
        BlockDevError::BlockOutOfRange { .. } => LinuxError::EINVAL,
        BlockDevError::InvalidBlockSize { .. } => LinuxError::EINVAL,
        BlockDevError::BufferTooSmall { .. } => LinuxError::EINVAL,
        BlockDevError::DeviceNotOpen | BlockDevError::DeviceClosed => LinuxError::EIO,
        BlockDevError::AlignmentError { .. } => LinuxError::EINVAL,
        BlockDevError::DeviceBusy => LinuxError::EBUSY,
        BlockDevError::Timeout => LinuxError::ETIMEDOUT,
        BlockDevError::Unsupported => LinuxError::EOPNOTSUPP,
        BlockDevError::ReadOnly => LinuxError::EROFS,
        BlockDevError::NoSpace => LinuxError::ENOSPC,
        BlockDevError::PermissionDenied => LinuxError::EACCES,
        BlockDevError::Corrupted | BlockDevError::ChecksumError | BlockDevError::Unknown => {
            LinuxError::EIO
        }
    };
    VfsError::from(linux_error).canonicalize()
}

/// Decode an ext4 old-format device number.
pub(crate) fn old_decode_dev(encoded: u16) -> DeviceId {
    let major = ((encoded >> 8) & 0xff) as u32;
    let minor = (encoded & 0xff) as u32;
    DeviceId::new(major, minor)
}

/// Decode an ext4 new-format device number.
pub(crate) fn new_decode_dev(encoded: u32) -> DeviceId {
    let major = (encoded & 0x000f_ff00) >> 8;
    let minor = (encoded & 0x0000_00ff) | ((encoded >> 12) & 0x000f_ff00);
    DeviceId::new(major, minor)
}

/// Encode a device number in ext4's old on-disk format.
pub(crate) fn old_encode_dev(device: DeviceId) -> u16 {
    ((device.major() << 8) | device.minor()) as u16
}

/// Encode a device number in ext4's new on-disk format.
pub(crate) fn new_encode_dev(device: DeviceId) -> u32 {
    let major = device.major();
    let minor = device.minor();
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// Returns whether a device number fits ext4's old on-disk format.
pub(crate) fn old_valid_dev(device: DeviceId) -> bool {
    device.major() < 256 && device.minor() < 256
}

/// Decode the device number stored in an ext4 special inode.
pub(crate) fn inode_rdev(inode: &Ext4Inode) -> DeviceId {
    match Umode::from_bits(inode.i_mode).node_type() {
        NodeType::CharacterDevice | NodeType::BlockDevice => {
            if inode.i_block[0] != 0 {
                old_decode_dev(inode.i_block[0] as u16)
            } else {
                new_decode_dev(inode.i_block[1])
            }
        }
        _ => DeviceId::default(),
    }
}

/// Store a device number in an ext4 special inode.
pub(crate) fn set_inode_rdev(inode: &mut Ext4Inode, device: DeviceId) {
    inode.i_block = [0; 15];
    if old_valid_dev(device) {
        inode.i_block[0] = old_encode_dev(device) as u32;
    } else {
        inode.i_block[1] = new_encode_dev(device);
    }
}

pub(crate) fn inode_fast_symlink_target(inode: &Ext4Inode) -> Option<String> {
    let size = usize::try_from(inode.size()).ok()?;
    if !inode.is_symlink() || size > 60 {
        return None;
    }

    let mut raw = [0u8; 60];
    for (index, word) in inode.i_block.iter().enumerate() {
        raw[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    core::str::from_utf8(&raw[..size])
        .ok()
        .map(ToOwned::to_owned)
}

/// Convert ext4 directory entry file types to VFS node types.
pub(crate) fn dir_entry_type_to_vfs(file_type: u8) -> NodeType {
    match file_type {
        rsext4::entries::Ext4DirEntry2::EXT4_FT_REG_FILE => NodeType::RegularFile,
        rsext4::entries::Ext4DirEntry2::EXT4_FT_DIR => NodeType::Directory,
        rsext4::entries::Ext4DirEntry2::EXT4_FT_CHRDEV => NodeType::CharacterDevice,
        rsext4::entries::Ext4DirEntry2::EXT4_FT_BLKDEV => NodeType::BlockDevice,
        rsext4::entries::Ext4DirEntry2::EXT4_FT_FIFO => NodeType::Fifo,
        rsext4::entries::Ext4DirEntry2::EXT4_FT_SOCK => NodeType::Socket,
        rsext4::entries::Ext4DirEntry2::EXT4_FT_SYMLINK => NodeType::Symlink,
        _ => NodeType::Unknown,
    }
}

/// Convert VFS node types to ext4 directory entry file types.
pub(crate) fn vfs_type_to_dir_entry(ty: NodeType) -> Option<u8> {
    Some(match ty {
        NodeType::RegularFile => rsext4::entries::Ext4DirEntry2::EXT4_FT_REG_FILE,
        NodeType::Directory => rsext4::entries::Ext4DirEntry2::EXT4_FT_DIR,
        NodeType::CharacterDevice => rsext4::entries::Ext4DirEntry2::EXT4_FT_CHRDEV,
        NodeType::BlockDevice => rsext4::entries::Ext4DirEntry2::EXT4_FT_BLKDEV,
        NodeType::Fifo => rsext4::entries::Ext4DirEntry2::EXT4_FT_FIFO,
        NodeType::Socket => rsext4::entries::Ext4DirEntry2::EXT4_FT_SOCK,
        NodeType::Symlink => rsext4::entries::Ext4DirEntry2::EXT4_FT_SYMLINK,
        NodeType::Unknown => return None,
    })
}
