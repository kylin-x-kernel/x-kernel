// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/shm directory entry.

use alloc::sync::Arc;

use kvfs::{DirMapping, SimpleDir, SimpleFs};

/// Register /dev/shm as an empty directory.
///
/// This directory is typically mounted to a tmpfs instance at runtime.
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add("shm", SimpleDir::new_maker(fs, Arc::new(DirMapping::new())));
}
