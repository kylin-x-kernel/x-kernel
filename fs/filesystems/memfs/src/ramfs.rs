// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ramfs mount entry points.

use alloc::sync::Arc;

use kvfs::{NodePermission, SuperBlock, SuperBlockFlags};

use crate::{MemoryFs, RAMFS_MAGIC};

/// Creates a ramfs superblock.
pub fn new_ramfs() -> Arc<SuperBlock> {
    new_ramfs_with_superblock_flags(SuperBlockFlags::empty())
}

/// Creates a ramfs superblock with explicit VFS-wide flags.
pub fn new_ramfs_with_superblock_flags(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_ramfs_with_name_and_superblock_flags("ramfs", superblock_flags)
}

/// Creates a ramfs-semantics superblock with a custom name and VFS-wide flags.
pub fn new_ramfs_with_name_and_superblock_flags(
    name: &'static str,
    superblock_flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    MemoryFs::new_with_name_superblock_flags_and_root_mode(
        name,
        RAMFS_MAGIC,
        superblock_flags,
        NodePermission::from_bits_truncate(0o755),
    )
}
