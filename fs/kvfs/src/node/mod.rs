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
    DentryKey, LookupCreateResult, d_inode, d_is_dir, d_is_negative, d_is_symlink,
    d_really_is_positive,
};
pub use device::{DeviceFileOps, MmapMapper, cdev_add, cdev_del};
pub use dir::{DirContext, DirEntrySink};
pub use inode::{
    FiemapCapability, GetattrQueryFlags, GetattrRequestMask, InodeAttributeOperations,
    InodeDirOperations, InodeFiemapOperations, InodeLookupFlags, InodeOperations,
    InodeSymlinkOperations, InodeUpdateTime, NodeFlags, RenameFlags, VfsInode, VfsInodeInit,
    WeakVfsInode, inode_init_owner,
};
pub(crate) use inode::{get_or_try_init_inode, lookup_inode};
