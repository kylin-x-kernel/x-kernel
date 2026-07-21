// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS inode, dentry, directory, and device node objects.
mod dentry;
mod device;
mod dir;
mod inode;

pub use dentry::{Dentry, DentryOperations, LockedDentry};
pub(crate) use dentry::{
    DentryKey, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive,
};
pub use device::{DeviceFileOps, MmapMapper, bdev_add, bdev_del, cdev_add, cdev_del};
pub use dir::{DirContext, DirEntrySink};
pub use inode::{
    GetattrQueryFlags, GetattrRequestMask, InodeCache, InodeDirOperations, InodeLookupFlags,
    InodeOperations, InodeSymlinkOperations, NodeFlags, RenameFlags, VfsInode, VfsInodeInit,
    WeakVfsInode, inode_init_owner,
};
