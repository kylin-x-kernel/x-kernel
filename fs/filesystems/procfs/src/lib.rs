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

use kvfs::{SimpleFs, SuperBlock, SuperBlockFlags};

/// Creates a procfs superblock for process information.
pub fn new_procfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    SimpleFs::new_with_superblock_flags("proc".into(), 0x9fa0, superblock_flags, root::builder)
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
