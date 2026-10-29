// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux cgroup v2 filesystem adapter.
//!
//! The crate translates VFS operations into the canonical cgroup and process
//! ownership APIs. It does not own hierarchy or task-membership state.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;

mod command;
mod control;
mod controller;
mod dir;
mod mount;
mod pids;
mod process;
mod state;

use kvfs::{SimpleFs, VfsFile};
pub use mount::{FILE_SYSTEM_TYPE, new_cgroup2fs, new_initial_cgroup2fs};

/// Resolves the cgroup object a cgroup2 directory file refers to.
///
/// `bpf(2)` attaches device programs through a cgroup directory fd; the
/// syscall layer hands the opened file here to identify the target cgroup.
///
/// Returns `None` for files on other filesystems or non-directory inodes —
/// the caller answers `EBADF`/`EINVAL`, mirroring `cgroup_get_from_fd`.
pub fn cgroup_from_file(file: &VfsFile) -> Option<Arc<kcgroup::Cgroup>> {
    // Identify the filesystem by its state object rather than by
    // `SuperBlock::magic()`: KVFS does not publish a magic value on
    // superblocks yet, and only a cgroup2fs superblock carries a
    // `CgroupFsState`.
    let inode = file.inode();
    let super_block = inode.super_block().ok()?;
    let fs = super_block.private::<Arc<SimpleFs>>().ok()?;
    fs.private::<state::CgroupFsState>()?.cgroup_of(inode)
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
