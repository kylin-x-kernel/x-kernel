// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate alloc;

mod basic_nodes;
mod device_nodes;
mod irq_nodes;
mod mem_nodes;
mod root;
#[cfg(feature = "sysrq")]
mod sysrq_nodes;
mod task_nodes;
mod trace_nodes;

use alloc::sync::Arc;

use kvfs::{
    FileSystemType, FsContext, FsContextOperations, SimpleFs, SuperBlock, SuperBlockFlags,
    VfsResult, get_tree_nodev,
};

fn get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_procfs_with_type(file_system_type, flags))
    })
}

static FS_CONTEXT_OPERATIONS: FsContextOperations = FsContextOperations::new(get_tree);

fn init_fs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    Ok(())
}

/// Registered proc filesystem type.
pub static FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("proc", init_fs_context);

#[macros::register_init]
fn init_proc_fs() {
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE).expect("proc filesystem type must register once");
}

/// Creates a procfs superblock for process information.
pub fn new_procfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_procfs_with_type(&FILE_SYSTEM_TYPE, superblock_flags)
}

fn new_procfs_with_type(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    SimpleFs::new_with_superblock_flags(file_system_type, 0x9fa0, superblock_flags, root::builder)
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_new_procfs_has_expected_name_and_flags() {
        let fs = new_procfs(SuperBlockFlags::empty());

        assert_eq!(fs.name(), "proc");
        let stat = fs.stat().unwrap();
        assert_eq!(stat.fs_type, 0x9fa0);
    }
}
