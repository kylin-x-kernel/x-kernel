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

use kvfs::{Filesystem, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};
use kvfs_simple::SimpleFs;

const PROC_MOUNT_FLAGS: u32 = ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME;

/// Create a new procfs filesystem for process information.
pub fn new_procfs() -> Filesystem {
    SimpleFs::new_with_flags("proc".into(), 0x9fa0, PROC_MOUNT_FLAGS, root::builder)
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_new_procfs_has_expected_name_and_flags() {
        let fs = new_procfs();

        assert_eq!(fs.name(), "proc");
        let stat = fs.stat().unwrap();
        assert_eq!(stat.fs_type, 0x9fa0);
        assert_eq!(stat.mount_flags, PROC_MOUNT_FLAGS);
    }
}
