// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![feature(likely_unlikely)]
#![feature(bstr)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

#[macro_use]
extern crate klogger;

extern crate alloc;

use kvfs::{ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};

pub mod file;
pub mod mm;
pub mod task;
pub use ktty::terminal;
#[cfg(feature = "tee")]
pub use tee_kernel::tee;

#[cfg(unittest)]
mod unittest_task;
#[cfg(unittest)]
pub use unittest_task::{register_unittest_runtime, run_with_test_user_thread};

/// Initializes VFS and alarm task.
pub fn init() {
    info!("Initialize VFS...");
    devfs::capture_firmware_dtb_snapshot();
    let mounts = kfs::VirtualFsMounts {
        devfs: devfs::new_devfs(),
        dev_shm: memfs::MemoryFs::new_with_name_and_flags(
            "tmpfs",
            ST_NOSUID | ST_NODEV | ST_RELATIME,
        ),
        tmpfs: memfs::MemoryFs::new_with_name_and_flags(
            "tmpfs",
            ST_NOSUID | ST_NODEV | ST_RELATIME,
        ),
        procfs: procfs::new_procfs(),
        sysfs: memfs::MemoryFs::new_with_name_and_flags(
            "sysfs",
            ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME,
        ),
    };
    kfs::mount_virtual_filesystems(mounts).expect("Failed to mount vfs");

    #[cfg(feature = "dev-log")]
    if let Err(err) = devfs::bind_dev_log() {
        if err != kerrno::LinuxError::ENOSYS && err != kerrno::LinuxError::EOPNOTSUPP {
            panic!("Failed to bind dev-log: {err}");
        }
        warn!("/dev/log not available: {err}");
    }

    info!("Initialize alarm...");
    kthread::spawn_alarm_task();
}
