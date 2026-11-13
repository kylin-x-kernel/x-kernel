// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/shm directory entry.

use alloc::sync::Arc;

use kvfs::{DirMaker, DirMapping, NodePermission, SimpleDir, SimpleFs};

use crate::dynamic::WritableDir;

/// Register /dev/shm as an empty, world-writable sticky directory.
///
/// This directory is typically mounted to a tmpfs instance at runtime. When a
/// later init grafts a fresh devtmpfs mount over `/dev`, that mount shadows
/// the tmpfs and plain file creation lands here instead — devtmpfs is
/// ramfs-backed on Linux, so the directory must accept content files.
///
/// The mode is 01777 rather than the devtmpfs default 0755 because POSIX
/// shared memory is defined to be world-writable: Linux ends up with a 1777
/// tmpfs here, and an unprivileged `sem_open` must work even while no tmpfs
/// is mounted over the directory.
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    let ops = Arc::new(WritableDir::new(fs.clone(), DirMapping::new()));
    let maker: DirMaker = Arc::new(move || {
        SimpleDir::new_inode_with_owner(
            fs.clone(),
            ops.clone(),
            NodePermission::from_bits_truncate(0o1777),
            0,
            0,
        )
    });
    root.add("shm", maker);
}
