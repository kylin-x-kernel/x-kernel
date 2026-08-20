// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registered filesystem types.

use alloc::{sync::Arc, vec::Vec};

use klazy::Lazy;

use crate::{FsContext, Mutex, SuperBlock, VfsError, VfsResult};

bitflags::bitflags! {
    /// Filesystem type flags, mirroring Linux `struct file_system_type::fs_flags`.
    ///
    /// Bit numbering follows `include/linux/fs.h` (`FS_REQUIRES_DEV` = 1,
    /// `FS_BINARY_MOUNTDATA` = 2, `FS_HAS_SUBTYPE` = 4, `FS_USERNS_MOUNT` = 8,
    /// ...), so future ports of those flags keep the same numeric layout
    /// without renumbering existing bits.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct FileSystemTypeFlags: u32 {
        /// Filesystem requires a backing block device.
        const REQUIRES_DEV = 1 << 0;
    }
}

/// Linux `file_system_type::init_fs_context`/`fs_context_operations::get_tree`
/// collapsed into the current one-shot mount callback.
pub type GetTreeFn =
    for<'a> fn(&FsContext<'a>, &crate::Path, &crate::Path) -> VfsResult<Arc<SuperBlock>>;

/// A filesystem implementation known to the VFS.
///
/// Registered implementations expose one static descriptor, matching the
/// identity and lifetime of Linux `struct file_system_type` objects. A
/// superblock created through this descriptor retains the same reference as
/// its Linux-equivalent `s_type` identity.
pub struct FileSystemType {
    name: &'static str,
    get_tree: GetTreeFn,
    fs_flags: FileSystemTypeFlags,
}

impl FileSystemType {
    /// Describes a filesystem that does not require a backing device.
    pub const fn nodev(name: &'static str, get_tree: GetTreeFn) -> Self {
        Self {
            name,
            get_tree,
            fs_flags: FileSystemTypeFlags::empty(),
        }
    }

    /// Describes a filesystem that requires a backing device.
    pub const fn device_backed(name: &'static str, get_tree: GetTreeFn) -> Self {
        Self {
            name,
            get_tree,
            fs_flags: FileSystemTypeFlags::REQUIRES_DEV,
        }
    }

    /// Returns the registered filesystem name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns whether this filesystem requires a backing device.
    pub const fn requires_device(&self) -> bool {
        self.fs_flags.contains(FileSystemTypeFlags::REQUIRES_DEV)
    }

    /// Runs the filesystem's `->get_tree` equivalent.
    ///
    /// # Errors
    ///
    /// Propagates an error from the filesystem factory, e.g.
    /// [`VfsError::InvalidInput`] when a device-backed filesystem is mounted
    /// without a device name, or [`VfsError::NoSuchDevice`] when the named
    /// backing device does not exist.
    pub(crate) fn get_tree(
        &self,
        context: &FsContext<'_>,
        lookup_root: &crate::Path,
        lookup_pwd: &crate::Path,
    ) -> VfsResult<Arc<SuperBlock>> {
        (self.get_tree)(context, lookup_root, lookup_pwd)
    }
}

static FILE_SYSTEMS: Lazy<Mutex<Vec<&'static FileSystemType>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

/// Registers one canonical static filesystem type.
///
/// # Errors
///
/// Returns [`VfsError::ResourceBusy`] when the name is already registered.
pub fn register_filesystem(file_system_type: &'static FileSystemType) -> VfsResult<()> {
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

/// Finds the registered static filesystem type with the exact name.
pub fn get_filesystem_type(name: &str) -> Option<&'static FileSystemType> {
    FILE_SYSTEMS
        .lock()
        .iter()
        .copied()
        .find(|file_system_type| file_system_type.name == name)
}

/// Returns a snapshot of references to all registered filesystem types.
pub fn registered_filesystems() -> Vec<&'static FileSystemType> {
    FILE_SYSTEMS.lock().clone()
}
