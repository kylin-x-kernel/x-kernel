// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ext4_rs adapter utilities.
use ext4_rs::{Errno, Ext4Error, InodeFileType};
use fs_ng_vfs::{NodeType, VfsError};
use kerrno::LinuxError;

/// Convert ext4_rs errors into VFS errors.
pub fn into_vfs_err(err: Ext4Error) -> VfsError {
    let linux_error = match err.error() {
        Errno::EPERM => LinuxError::EPERM,
        Errno::ENOENT => LinuxError::ENOENT,
        Errno::EINTR => LinuxError::EINTR,
        Errno::EIO => LinuxError::EIO,
        Errno::ENXIO => LinuxError::ENXIO,
        Errno::E2BIG => LinuxError::E2BIG,
        Errno::EBADF => LinuxError::EBADF,
        Errno::EAGAIN => LinuxError::EAGAIN,
        Errno::ENOMEM => LinuxError::ENOMEM,
        Errno::EACCES => LinuxError::EACCES,
        Errno::EFAULT => LinuxError::EFAULT,
        Errno::ENOTBLK => LinuxError::ENOTBLK,
        Errno::EBUSY => LinuxError::EBUSY,
        Errno::EEXIST => LinuxError::EEXIST,
        Errno::EXDEV => LinuxError::EXDEV,
        Errno::ENODEV => LinuxError::ENODEV,
        Errno::ENOTDIR => LinuxError::ENOTDIR,
        Errno::EISDIR => LinuxError::EISDIR,
        Errno::EINVAL => LinuxError::EINVAL,
        Errno::ENFILE => LinuxError::ENFILE,
        Errno::EMFILE => LinuxError::EMFILE,
        Errno::ENOTTY => LinuxError::ENOTTY,
        Errno::ETXTBSY => LinuxError::ETXTBSY,
        Errno::EFBIG => LinuxError::EFBIG,
        Errno::ENOSPC => LinuxError::ENOSPC,
        Errno::ESPIPE => LinuxError::ESPIPE,
        Errno::EROFS => LinuxError::EROFS,
        Errno::EMLINK => LinuxError::EMLINK,
        Errno::EPIPE => LinuxError::EPIPE,
        Errno::ENAMETOOLONG => LinuxError::ENAMETOOLONG,
        Errno::ENOTSUP => LinuxError::EOPNOTSUPP,
    };
    VfsError::from(linux_error).canonicalize()
}

/// Convert ext4 inode file types to VFS node types.
pub fn inode_to_vfs_type(kind: InodeFileType) -> NodeType {
    match kind {
        InodeFileType::S_IFREG => NodeType::RegularFile,
        InodeFileType::S_IFDIR => NodeType::Directory,
        InodeFileType::S_IFCHR => NodeType::CharacterDevice,
        InodeFileType::S_IFBLK => NodeType::BlockDevice,
        InodeFileType::S_IFIFO => NodeType::Fifo,
        InodeFileType::S_IFSOCK => NodeType::Socket,
        InodeFileType::S_IFLNK => NodeType::Symlink,
        _ => NodeType::Unknown,
    }
}

/// Convert ext4 directory entry file types to VFS node types.
pub fn dir_entry_type_to_vfs(file_type: u8) -> NodeType {
    match file_type {
        1 => NodeType::RegularFile,
        2 => NodeType::Directory,
        3 => NodeType::CharacterDevice,
        4 => NodeType::BlockDevice,
        5 => NodeType::Fifo,
        6 => NodeType::Socket,
        7 => NodeType::Symlink,
        _ => NodeType::Unknown,
    }
}

/// Convert VFS node types to ext4 inode file types.
pub fn vfs_type_to_inode(kind: NodeType) -> Option<InodeFileType> {
    Some(match kind {
        NodeType::RegularFile => InodeFileType::S_IFREG,
        NodeType::Directory => InodeFileType::S_IFDIR,
        NodeType::CharacterDevice => InodeFileType::S_IFCHR,
        NodeType::BlockDevice => InodeFileType::S_IFBLK,
        NodeType::Fifo => InodeFileType::S_IFIFO,
        NodeType::Socket => InodeFileType::S_IFSOCK,
        NodeType::Symlink => InodeFileType::S_IFLNK,
        NodeType::Unknown => return None,
    })
}
