// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registered filesystem types.

use alloc::{sync::Arc, vec::Vec};

use klazy::Lazy;

use crate::{Mutex, SuperBlock, SuperBlockFlags, VfsError, VfsResult};

type MountNodevFn = fn(SuperBlockFlags) -> VfsResult<Arc<SuperBlock>>;

/// A filesystem implementation known to the VFS.
#[derive(Clone, Copy)]
pub struct FileSystemType {
    name: &'static str,
    mount_nodev_fn: Option<MountNodevFn>,
}

impl FileSystemType {
    /// Describes a filesystem that does not require a backing device.
    pub const fn nodev(name: &'static str, mount_nodev_fn: MountNodevFn) -> Self {
        Self {
            name,
            mount_nodev_fn: Some(mount_nodev_fn),
        }
    }

    /// Describes a filesystem that requires a backing device.
    pub const fn device_backed(name: &'static str) -> Self {
        Self {
            name,
            mount_nodev_fn: None,
        }
    }

    /// Returns the registered filesystem name.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Returns whether this filesystem requires a backing device.
    pub const fn requires_device(self) -> bool {
        self.mount_nodev_fn.is_none()
    }

    /// Creates a superblock for a device-less mount.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::NoSuchDevice`] for device-backed filesystem types,
    /// or propagates an error from the filesystem factory.
    pub fn mount_nodev(self, superblock_flags: SuperBlockFlags) -> VfsResult<Arc<SuperBlock>> {
        self.mount_nodev_fn
            .ok_or(VfsError::NoSuchDevice)
            .and_then(|mount_fn| mount_fn(superblock_flags))
    }
}

static FILE_SYSTEMS: Lazy<Mutex<Vec<FileSystemType>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Registers one filesystem type.
///
/// # Errors
///
/// Returns [`VfsError::ResourceBusy`] when the name is already registered.
pub fn register_filesystem(file_system_type: FileSystemType) -> VfsResult<()> {
    let mut file_systems = FILE_SYSTEMS.lock();
    if file_systems
        .iter()
        .any(|registered| registered.name == file_system_type.name)
    {
        return Err(VfsError::ResourceBusy);
    }
    file_systems.push(file_system_type);
    Ok(())
}

/// Finds a registered filesystem type by its exact name.
pub fn get_filesystem_type(name: &str) -> Option<FileSystemType> {
    FILE_SYSTEMS
        .lock()
        .iter()
        .copied()
        .find(|file_system_type| file_system_type.name == name)
}

/// Returns a snapshot of all registered filesystem types.
pub fn registered_filesystems() -> Vec<FileSystemType> {
    FILE_SYSTEMS.lock().clone()
}
