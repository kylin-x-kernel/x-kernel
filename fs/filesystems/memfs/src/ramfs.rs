// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ramfs mount entry points.

use alloc::sync::Arc;

use kvfs::{
    FileSystemType, FsContext, FsContextOperations, NodePermission, SuperBlock, SuperBlockFlags,
    VfsResult, get_tree_nodev,
};

use crate::{MemoryFs, RAMFS_MAGIC};

fn ramfs_get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        new_ramfs_with_file_system_type_and_flags(file_system_type, flags)
    })
}

static RAMFS_CONTEXT_OPERATIONS: FsContextOperations = FsContextOperations::new(ramfs_get_tree);

fn init_ramfs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&RAMFS_CONTEXT_OPERATIONS);
    Ok(())
}

/// Registered ramfs filesystem type.
pub(crate) static RAMFS_TYPE: FileSystemType =
    FileSystemType::nodev_userns("ramfs", init_ramfs_context);

/// Internal mutable bootstrap-root filesystem type.
static ROOTFS_TYPE: FileSystemType = FileSystemType::internal("rootfs");

/// Creates a ramfs superblock.
///
/// Requires sleepable task context with IRQs/preemption enabled, outside
/// non-sleepable RCU and without locks needed by reclaim, I/O or filesystem
/// callbacks. Returns `NoMemory` if private-owner allocation fails. Nested
/// allocations retain their existing behavior.
pub fn new_ramfs() -> VfsResult<Arc<SuperBlock>> {
    new_ramfs_with_superblock_flags(SuperBlockFlags::empty())
}

/// Creates a ramfs superblock with explicit VFS-wide flags.
///
/// Requires sleepable task context with IRQs/preemption enabled, outside
/// non-sleepable RCU and without locks needed by reclaim, I/O or filesystem
/// callbacks. Returns `NoMemory` if private-owner allocation fails. Nested
/// allocations retain their existing behavior.
pub fn new_ramfs_with_superblock_flags(
    superblock_flags: SuperBlockFlags,
) -> VfsResult<Arc<SuperBlock>> {
    new_ramfs_with_file_system_type_and_flags(&RAMFS_TYPE, superblock_flags)
}

/// Creates the mutable bootstrap root filesystem.
///
/// Requires sleepable task context with IRQs/preemption enabled, outside
/// non-sleepable RCU and without locks needed by reclaim, I/O or filesystem
/// callbacks. Returns `NoMemory` if private-owner allocation fails. Nested
/// allocations retain their existing behavior.
pub fn new_rootfs(superblock_flags: SuperBlockFlags) -> VfsResult<Arc<SuperBlock>> {
    new_ramfs_with_file_system_type_and_flags(&ROOTFS_TYPE, superblock_flags)
}

/// Creates a ramfs-shaped tree: `RAMFS_MAGIC` and a 0755 root directory.
///
/// Both are fixed here rather than taken from the caller, so a ramfs root is
/// recognisable by its magic; a filesystem that needs a different magic or
/// root mode (mqueue) constructs its tree directly.
pub(crate) fn new_ramfs_with_file_system_type_and_flags(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
) -> VfsResult<Arc<SuperBlock>> {
    MemoryFs::new_with_name_superblock_flags_and_root_mode(
        file_system_type,
        RAMFS_MAGIC,
        superblock_flags,
        NodePermission::from_bits_truncate(0o755),
    )
}
