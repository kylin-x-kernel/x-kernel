// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 adapter utilities and HAL integration.
use fs_ng_vfs::{NodeType, VfsError};
use kerrno::LinuxError;
use lwext4_rust::{Ext4Error, InodeType, SystemHal};

use super::Ext4Disk;

/// Ext4 HAL implementation for timekeeping.
pub struct AxHal;
impl SystemHal for AxHal {
    fn now() -> Option<core::time::Duration> {
        if cfg!(feature = "times") {
            Some(khal::time::wall_time())
        } else {
            None
        }
    }
}

/// Type alias for the ext4 filesystem implementation used by this crate.
pub type LwExt4Filesystem = lwext4_rust::Ext4Filesystem<AxHal, Ext4Disk>;

/// Convert ext4 errors into VFS errors.
pub fn into_vfs_err(err: Ext4Error) -> VfsError {
    let linux_error = {
        let e = LinuxError::new(err.code);
        if e.name().is_some() {
            e
        } else {
            LinuxError::EIO
        }
    };
    VfsError::from(linux_error).canonicalize()
}

/// Convert ext4 inode types to VFS node types.
pub fn into_vfs_type(ty: InodeType) -> NodeType {
    match ty {
        InodeType::RegularFile => NodeType::RegularFile,
        InodeType::Directory => NodeType::Directory,
        InodeType::CharacterDevice => NodeType::CharacterDevice,
        InodeType::BlockDevice => NodeType::BlockDevice,
        InodeType::Fifo => NodeType::Fifo,
        InodeType::Socket => NodeType::Socket,
        InodeType::Symlink => NodeType::Symlink,
        InodeType::Unknown => NodeType::Unknown,
    }
}
