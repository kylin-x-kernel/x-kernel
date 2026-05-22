// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual filesystems

pub mod dev;
mod tmp;

pub use kcore::vfs::{Device, DeviceOps};
#[cfg(feature = "dev-log")]
use kerrno::LinuxError;
use kerrno::LinuxResult;
use kvfs::{ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};
pub use kvfs_simple::{DirMapping, SimpleFs};
pub use tmp::MemoryFs;

/// Build virtual filesystem instances for the filesystem runtime to mount.
pub fn virtual_filesystems() -> kfs::VirtualFsMounts {
    kfs::VirtualFsMounts {
        devfs: dev::new_devfs(),
        dev_shm: tmp::MemoryFs::new_with_flags(ST_NOSUID | ST_NODEV | ST_RELATIME),
        tmpfs: tmp::MemoryFs::new_with_flags(ST_NOSUID | ST_NODEV | ST_RELATIME),
        procfs: procfs::new_procfs(),
        sysfs: tmp::MemoryFs::new_with_flags(ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME),
    }
}

/// Finish virtual filesystem service binding after runtime mounts are ready.
pub fn finish_virtual_filesystems() -> LinuxResult<()> {
    #[cfg(feature = "dev-log")]
    if let Err(err) = dev::bind_dev_log() {
        if err != LinuxError::ENOSYS && err != LinuxError::EOPNOTSUPP {
            return Err(err);
        }
        warn!("/dev/log not available: {err}");
    }

    Ok(())
}
