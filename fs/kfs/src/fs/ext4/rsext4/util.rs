// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 adapter utilities.
use fs_ng_vfs::{NodeType, VfsError};
use kerrno::LinuxError;
use rsext4::error::{BlockDevError, RSEXT4Error};

/// Convert rsext4 block device errors into VFS errors.
pub fn into_vfs_err(err: BlockDevError) -> VfsError {
    let linux_error = match err {
        BlockDevError::InvalidInput => LinuxError::EINVAL,
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

/// Convert rsext4 mount errors into VFS errors.
pub fn into_vfs_err_mount(err: RSEXT4Error) -> VfsError {
    let linux_error = match err {
        RSEXT4Error::IoError => LinuxError::EIO,
        RSEXT4Error::InvalidMagic | RSEXT4Error::InvalidSuperblock => LinuxError::EINVAL,
        RSEXT4Error::FilesystemHasErrors => LinuxError::EIO,
        RSEXT4Error::UnsupportedFeature => LinuxError::EOPNOTSUPP,
        RSEXT4Error::AlreadyMounted => LinuxError::EBUSY,
    };
    VfsError::from(linux_error).canonicalize()
}

/// Convert ext4 inode types to VFS node types.
pub fn inode_to_vfs_type(is_dir: bool, is_file: bool, is_symlink: bool) -> NodeType {
    if is_dir {
        NodeType::Directory
    } else if is_file {
        NodeType::RegularFile
    } else if is_symlink {
        NodeType::Symlink
    } else {
        NodeType::Unknown
    }
}

/// Convert ext4 directory entry file types to VFS node types.
pub fn dir_entry_type_to_vfs(file_type: u8) -> NodeType {
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
pub fn vfs_type_to_dir_entry(ty: NodeType) -> Option<u8> {
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
