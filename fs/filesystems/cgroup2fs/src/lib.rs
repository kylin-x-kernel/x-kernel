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
mod cpu;
mod cpuset;
mod dir;
mod memory;
mod mount;
mod pids;
mod process;
mod state;

use kvfs::{Permission, SimpleFs, VfsError, VfsFile, VfsInode, VfsResult};
pub use mount::{FILE_SYSTEM_TYPE, new_cgroup2fs, new_initial_cgroup2fs};

/// Resolves the cgroup object a cgroup2 directory file refers to.
///
/// `bpf(2)` attaches device programs through a cgroup directory fd; the
/// syscall layer hands the opened file here to identify the target cgroup.
///
/// Returns `None` for files on other filesystems or non-directory inodes —
/// the caller answers `EBADF`/`EINVAL`, mirroring `cgroup_get_from_fd`.
pub fn cgroup_from_file(file: &VfsFile) -> Option<Arc<kcgroup::Cgroup>> {
    directory_state(file.inode()).map(|(_, cgroup)| cgroup)
}

/// Resolves and authorizes a `clone3(CLONE_INTO_CGROUP)` target directory.
///
/// The clone syscall hands the opened cgroup directory file here; the child
/// is then forked directly into the returned cgroup. Resolution mirrors
/// [`cgroup_from_file`]. Authorization follows the Linux clone3 path
/// (`cgroup_css_set_fork` -> `cgroup_may_write` / `cgroup_procs_write_permission`):
/// the caller's current cgroup and the target must both be visible from this
/// mount's view root, and the **caller's current credentials** need write
/// access to the target's and the common ancestor's `cgroup.procs`. Current
/// credentials are used deliberately rather than the fd opener's, so a
/// process that dropped privileges cannot spawn into a cgroup via an
/// inherited privileged descriptor.
///
/// # Errors
///
/// `EBADF` for files that are not cgroup2 directories, `ENODEV` for a
/// removed target, `ENOENT` when either endpoint lies outside the mount's
/// cgroup namespace view, `EXDEV` across hierarchies, and `EACCES` when the
/// credential lacks the delegation write access.
pub fn cgroup_clone_target(file: &VfsFile) -> VfsResult<Arc<kcgroup::Cgroup>> {
    let (state, target) = directory_state(file.inode()).ok_or(VfsError::BadFileDescriptor)?;
    let _operation = target.begin_operation()?;

    let source = kprocess::current_cgroup().ok_or(VfsError::NotFound)?;
    if !source.is_descendant_of(state.view_root()) || !target.is_descendant_of(state.view_root()) {
        return Err(VfsError::NotFound);
    }

    let cred = kprocess::current_cred();
    let target_node = state.node(target.clone())?;
    let target_procs = target_node
        .file("cgroup.procs")
        .ok_or(VfsError::PermissionDenied)?;
    target_procs.permission(Permission::MAY_WRITE, &cred)?;

    let common = source.common_ancestor(&target)?;
    let common_node = state.node(common)?;
    let common_procs = common_node
        .file("cgroup.procs")
        .ok_or(VfsError::PermissionDenied)?;
    common_procs.permission(Permission::MAY_WRITE, &cred)?;

    Ok(target)
}

/// Identifies the cgroup2 state and directory node an inode materializes.
///
/// Identify the filesystem by its state object rather than by
/// `SuperBlock::magic()`: KVFS does not publish a magic value on superblocks
/// yet, and only a cgroup2fs superblock carries a `CgroupFsState`.
fn directory_state(
    inode: &Arc<VfsInode>,
) -> Option<(Arc<state::CgroupFsState>, Arc<kcgroup::Cgroup>)> {
    let super_block = inode.super_block().ok()?;
    let fs = super_block.private::<Arc<SimpleFs>>().ok()?;
    let state = fs.private::<state::CgroupFsState>()?;
    let cgroup = state.cgroup_of(inode)?;
    Some((state, cgroup))
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
