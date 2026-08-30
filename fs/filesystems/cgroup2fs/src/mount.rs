// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcgroup::{Cgroup, CgroupNamespace};
use kvfs::{
    FileSystemType, FsContext, FsContextOperations, SimpleFs, SuperBlock, SuperBlockFlags,
    VfsResult, get_tree_nodev,
};

const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;

fn get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    let root = kprocess::current_user_process().cgroup_ns()?.root();
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_with_type(file_system_type, root, flags))
    })
}

static OPERATIONS: FsContextOperations = FsContextOperations::new(get_tree);

fn init_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&OPERATIONS);
    Ok(())
}

/// Registered cgroup v2 filesystem type.
pub static FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("cgroup2", init_context);

#[macros::register_init]
fn init_fs() {
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE)
        .expect("cgroup2 filesystem type must register once");
}

/// Creates a cgroup v2 superblock over `root`.
pub fn new_cgroup2fs(root: Arc<Cgroup>, flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_with_type(&FILE_SYSTEM_TYPE, root, flags)
}

/// Creates a cgroup v2 superblock over the system's initial hierarchy.
///
/// This constructor is valid during boot, before a current user process
/// exists. Ordinary user mounts use [`FILE_SYSTEM_TYPE`] and select the
/// calling process's cgroup namespace view instead.
pub fn new_initial_cgroup2fs(flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_cgroup2fs(CgroupNamespace::initial().root(), flags)
}

fn new_with_type(
    file_system_type: &'static FileSystemType,
    root: Arc<Cgroup>,
    flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    SimpleFs::new_with_superblock_flags(file_system_type, CGROUP2_SUPER_MAGIC, flags, move |fs| {
        let state = crate::state::CgroupFsState::new(&fs, root.clone());
        fs.set_private(state.clone());
        let root = state
            .node(root.clone())
            .expect("cgroup2 root node must be materialized");
        let inode = root.directory();
        Arc::new(move || inode.clone())
    })
}
