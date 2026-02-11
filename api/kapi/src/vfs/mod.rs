// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual filesystems

pub mod dev;
mod proc;
mod tmp;

use fs_ng_vfs::{
    Filesystem, NodePermission,
    path::{Path, PathBuf},
};
pub use kcore::vfs::{Device, DeviceOps, DirMapping, SimpleFs};
use kerrno::{LinuxError, LinuxResult};
use kfs::{FS_CONTEXT, FsContext};
pub use tmp::MemoryFs;

const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);

/// Mount a filesystem at the specified path, creating the path if it doesn't exist
fn mount_at(fs: &FsContext, path: &str, mount_fs: Filesystem) -> LinuxResult<()> {
    error!("Mounting {} at {}", mount_fs.name(), path);
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
    error!("Mounting {} at {}", mount_fs.name(), path);
    fs.resolve(path)?.mount(&mount_fs)?;
    info!("Mounted {} at {}", mount_fs.name(), path);
    Ok(())
}

/// Mount all filesystems
/// Mount all virtual filesystems (/dev, /tmp, /proc, /sys, etc.)
pub fn mount_all() -> LinuxResult<()> {
    let fs = FS_CONTEXT.lock();
    mount_at(&fs, "/dev", dev::new_devfs())?;
    mount_at(&fs, "/dev/shm", tmp::MemoryFs::new())?;
    mount_at(&fs, "/tmp", tmp::MemoryFs::new())?;
    mount_at(&fs, "/proc", proc::new_procfs())?;

    mount_at(&fs, "/sys", tmp::MemoryFs::new())?;
    let mut path = PathBuf::new();
    for comp in Path::new("/sys/class/graphics/fb0/device").components() {
        path.push(comp.as_str());
        if fs.resolve(&path).is_err() {
            fs.create_dir(&path, DIR_PERMISSION)?;
        }
    }
    error!("Creating symlink /sys/class/graphics/fb0/device/subsystem");
    path.push("subsystem");
    error!("Creating symlink /sys/class/graphics/fb0/device/subsystem");
    if let Err(err) = fs.symlink("whatever", &path) {
        let linux_err = LinuxError::from(err);
        if linux_err != LinuxError::EEXIST {
            return Err(linux_err);
        }
    }
    error!("Creating symlink /sys/class/graphics/fb0/device/subsystem");
    drop(fs);
    error!("Creating symlink /sys/class/graphics/fb0/device/subsystem");

    #[cfg(feature = "dev-log")]
    if let Err(err) = dev::bind_dev_log() {
        if err != LinuxError::ENOSYS && err != LinuxError::EOPNOTSUPP {
            return Err(err);
        }
        warn!("/dev/log not available: {err}");
    }

    Ok(())
}
