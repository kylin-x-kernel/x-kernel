// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ramfs mount entry points.

use alloc::sync::Arc;

use kvfs::{
    FileSystemType, FsContext, NodePermission, SuperBlock, SuperBlockFlags, VfsResult,
    get_tree_nodev,
};

use crate::{MemoryFs, RAMFS_MAGIC};

fn ramfs_get_tree(
    context: &FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_ramfs_with_type(file_system_type, flags))
    })
}

fn rootfs_get_tree(
    context: &FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_ramfs_with_type(file_system_type, flags))
    })
}

/// Registered ramfs filesystem type.
pub(crate) static RAMFS_TYPE: FileSystemType = FileSystemType::nodev("ramfs", ramfs_get_tree);

/// Internal mutable root filesystem type.
static ROOTFS_TYPE: FileSystemType = FileSystemType::nodev("rootfs", rootfs_get_tree);

/// Creates a ramfs superblock.
pub fn new_ramfs() -> Arc<SuperBlock> {
    new_ramfs_with_superblock_flags(SuperBlockFlags::empty())
}

/// Creates a ramfs superblock with explicit VFS-wide flags.
pub fn new_ramfs_with_superblock_flags(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_ramfs_with_type(&RAMFS_TYPE, superblock_flags)
}

/// Creates the mutable bootstrap root filesystem.
pub fn new_rootfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_ramfs_with_type(&ROOTFS_TYPE, superblock_flags)
}

pub(crate) fn new_ramfs_with_type(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    MemoryFs::new_with_file_system_type_superblock_flags_and_root_mode(
        file_system_type,
        RAMFS_MAGIC,
        superblock_flags,
        NodePermission::from_bits_truncate(0o755),
    )
}
