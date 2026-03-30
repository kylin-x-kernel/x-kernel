// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate alloc;

mod hooks;
mod mounts;
mod root;
mod task;

use fs_ng_vfs::{Filesystem, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};
pub use hooks::ProcFsHooks;
use kcore::vfs::SimpleFs;

const PROC_MOUNT_FLAGS: u32 = ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME;

/// Create a new procfs filesystem for process information.
pub fn new_procfs(hooks: ProcFsHooks) -> Filesystem {
    SimpleFs::new_with_flags("proc".into(), 0x9fa0, PROC_MOUNT_FLAGS, move |fs| {
        root::builder(fs, hooks)
    })
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::String, vec::Vec};

    use fs_ng_vfs::VfsError;
    use unittest::{assert_eq, def_test};

    use super::*;

    fn test_irq_count() -> usize {
        7
    }

    fn test_fd_ids(_: &ktask::KtaskRef) -> Vec<u32> {
        Vec::new()
    }

    fn test_fd_path(_: &ktask::KtaskRef, _: u32) -> fs_ng_vfs::VfsResult<String> {
        Err(VfsError::NotFound)
    }

    #[def_test]
    fn test_new_procfs_has_expected_name_and_flags() {
        let fs = new_procfs(ProcFsHooks {
            irq_count: test_irq_count,
            fd_ids: test_fd_ids,
            fd_path: test_fd_path,
        });

        assert_eq!(fs.name(), "proc");
        let stat = fs.stat().unwrap();
        assert_eq!(stat.fs_type, 0x9fa0);
        assert_eq!(stat.mount_flags, PROC_MOUNT_FLAGS);
    }
}
