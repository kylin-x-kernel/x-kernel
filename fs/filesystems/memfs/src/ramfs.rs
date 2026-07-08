// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ramfs mount entry points.

use alloc::sync::Arc;

use kvfs::{NodePermission, SuperBlock};

use crate::{MemoryFs, RAMFS_MAGIC};

/// Creates a ramfs superblock.
pub fn new_ramfs() -> Arc<SuperBlock> {
    new_ramfs_with_flags(0)
}

/// Creates a ramfs superblock with explicit mount flags.
pub fn new_ramfs_with_flags(mount_flags: u32) -> Arc<SuperBlock> {
    new_ramfs_with_name_and_flags("ramfs", mount_flags)
}

/// Creates a ramfs-semantics superblock with a custom name.
pub fn new_ramfs_with_name_and_flags(name: &'static str, mount_flags: u32) -> Arc<SuperBlock> {
    MemoryFs::new_with_name_flags_and_root_mode(
        name,
        RAMFS_MAGIC,
        mount_flags,
        NodePermission::from_bits_truncate(0o755),
    )
}
