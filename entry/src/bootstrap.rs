// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entry-owned bootstrap orchestration helpers.

use kvfs::{ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};

pub(crate) fn init_virtual_filesystems() {
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

    if let Err(err) = devfs::bind_dev_log() {
        if err != kerrno::LinuxError::ENOSYS && err != kerrno::LinuxError::EOPNOTSUPP {
            panic!("Failed to bind dev-log: {err}");
        }
        warn!("/dev/log not available: {err}");
    }
}

pub(crate) fn init_alarm_runtime() {
    info!("Initialize alarm...");
    kprocess::init_timer_runtime();
}
