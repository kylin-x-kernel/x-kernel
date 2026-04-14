// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual filesystems

pub mod dev;
mod tmp;

use alloc::{string::String, vec::Vec};

use fs_ng_vfs::{
    Filesystem, NodePermission, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME, VfsError, VfsResult,
    path::{Path, PathBuf},
};
use kcore::task::AsThread;
pub use kcore::vfs::{Device, DeviceOps, DirMapping, SimpleFs};
use kerrno::{LinuxError, LinuxResult};
use kfs::{FS_CONTEXT, FsContext};
use ktask::KtaskRef;
use procfs::ProcFsHooks;
pub use tmp::MemoryFs;

const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);

fn procfs_fd_ids(task: &KtaskRef) -> Vec<u32> {
    crate::file::FD_TABLE
        .scope(&task.as_thread().proc_data.scope.read())
        .read()
        .ids()
        .map(|id| id as u32)
        .collect()
}

fn procfs_fd_path(task: &KtaskRef, fd: u32) -> VfsResult<String> {
    crate::file::FD_TABLE
        .scope(&task.as_thread().proc_data.scope.read())
        .read()
        .get(fd as _)
        .ok_or(VfsError::NotFound)
        .map(|entry| entry.inner.path().into_owned())
}

/// Mount a filesystem at the specified path, creating the path if it doesn't exist
fn mount_at(fs: &FsContext, path: &str, mount_fs: Filesystem) -> LinuxResult<()> {
    match fs.resolve(path) {
        Ok(loc) => {
            if loc.check_is_dir().is_err() {
                // Path exists but is not a directory, replace it with a directory.
                fs.remove_file(path)?;
                fs.create_dir(path, DIR_PERMISSION)?;
            }
        }
        Err(_) => {
            fs.create_dir(path, DIR_PERMISSION)?;
        }
    }
    fs.resolve(path)?.mount(&mount_fs)?;
    Ok(())
}

/// Mount all filesystems
/// Mount all virtual filesystems (/dev, /tmp, /proc, /sys, etc.)
pub fn mount_all() -> LinuxResult<()> {
    let fs = FS_CONTEXT.lock();
    mount_at(&fs, "/dev", dev::new_devfs())?;
    mount_at(
        &fs,
        "/dev/shm",
        tmp::MemoryFs::new_with_flags(ST_NOSUID | ST_NODEV | ST_RELATIME),
    )?;
    mount_at(
        &fs,
        "/tmp",
        tmp::MemoryFs::new_with_flags(ST_NOSUID | ST_NODEV | ST_RELATIME),
    )?;
    mount_at(
        &fs,
        "/proc",
        procfs::new_procfs(ProcFsHooks {
            irq_count: crate::time::irq_cnt,
            fd_ids: procfs_fd_ids,
            fd_path: procfs_fd_path,
        }),
    )?;

    mount_at(
        &fs,
        "/sys",
        tmp::MemoryFs::new_with_flags(ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME),
    )?;
    let mut path = PathBuf::new();
    for comp in Path::new("/sys/class/graphics/fb0/device").components() {
        path.push(comp.as_str());
        if fs.resolve(&path).is_err() {
            fs.create_dir(&path, DIR_PERMISSION)?;
        }
    }
    path.push("subsystem");
    if let Err(err) = fs.symlink("whatever", &path) {
        let linux_err = LinuxError::from(err);
        if linux_err != LinuxError::EEXIST {
            return Err(linux_err);
        }
    }
    drop(fs);

    #[cfg(feature = "dev-log")]
    if let Err(err) = dev::bind_dev_log() {
        if err != LinuxError::ENOSYS && err != LinuxError::EOPNOTSUPP {
            return Err(err);
        }
        warn!("/dev/log not available: {err}");
    }

    Ok(())
}
