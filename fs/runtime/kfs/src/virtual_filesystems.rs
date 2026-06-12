// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime mounting of kernel virtual filesystems.

use kerrno::{LinuxError, LinuxResult};
use kvfs::{
    Filesystem, NodePermission,
    path::{Path, PathBuf},
};

use crate::{FsContext, kernel_fs_context};

const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);

/// Virtual filesystem instances mounted during kernel service initialization.
pub struct VirtualFsMounts {
    /// Device filesystem mounted at `/dev`.
    pub devfs: Filesystem,
    /// Shared memory tmpfs mounted at `/dev/shm`.
    pub dev_shm: Filesystem,
    /// Temporary filesystem mounted at `/tmp`.
    pub tmpfs: Filesystem,
    /// Procfs mounted at `/proc`.
    pub procfs: Filesystem,
    /// Sysfs-compatible temporary filesystem mounted at `/sys`.
    pub sysfs: Filesystem,
}

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

fn create_sys_compat_links(fs: &FsContext) -> LinuxResult<()> {
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
    Ok(())
}

/// Mount virtual filesystems such as `/dev`, `/proc`, `/sys`, and `/tmp`.
pub fn mount_virtual_filesystems(mounts: VirtualFsMounts) -> LinuxResult<()> {
    let fs = kernel_fs_context().lock();
    mount_at(&fs, "/dev", mounts.devfs)?;
    mount_at(&fs, "/dev/shm", mounts.dev_shm)?;
    mount_at(&fs, "/tmp", mounts.tmpfs)?;
    mount_at(&fs, "/proc", mounts.procfs)?;
    mount_at(&fs, "/sys", mounts.sysfs)?;
    #[cfg(feature = "ebpf")]
    {
        if fs.resolve("/sys/fs").is_err() {
            fs.create_dir("/sys/fs", DIR_PERMISSION)?;
        }
        mount_at(&fs, "/sys/fs/bpf", bpffs::new_bpffs())?;
    }
    create_sys_compat_links(&fs)?;
    Ok(())
}
