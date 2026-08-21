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

/// A filesystem's `fs_context_operations::get_tree` callback.
pub type GetTreeFn =
    for<'a> fn(&mut FsContext<'a>, &crate::Path, &crate::Path) -> VfsResult<Arc<SuperBlock>>;

/// Installs context operations and initializes filesystem-private transaction state.
pub type InitFsContextFn = for<'a> fn(&mut FsContext<'a>) -> VfsResult<()>;

/// Applies one parsed reconfiguration transaction to an existing superblock.
pub type ReconfigureFn = for<'a> fn(&mut FsContext<'a>) -> VfsResult<()>;

/// Shared filesystem-context operation table.
///
/// This corresponds to Linux `struct fs_context_operations`. Transaction
/// state belongs to [`FsContext`], while this table is immutable and shared by
/// every mount and reconfigure request of a filesystem type.
pub struct FsContextOperations {
    get_tree: GetTreeFn,
    reconfigure: Option<ReconfigureFn>,
}

impl FsContextOperations {
    /// Creates a context operation table with no reconfigure callback.
    pub const fn new(get_tree: GetTreeFn) -> Self {
        Self {
            get_tree,
            reconfigure: None,
        }
    }

    /// Creates a context operation table that supports reconfiguration.
    pub const fn with_reconfigure(get_tree: GetTreeFn, reconfigure: ReconfigureFn) -> Self {
        Self {
            get_tree,
            reconfigure: Some(reconfigure),
        }
    }

    pub(crate) fn get_tree(
        &self,
        context: &mut FsContext<'_>,
        lookup_root: &crate::Path,
        lookup_pwd: &crate::Path,
    ) -> VfsResult<Arc<SuperBlock>> {
        (self.get_tree)(context, lookup_root, lookup_pwd)
    }

    pub(crate) fn reconfigure(&self, context: &mut FsContext<'_>) -> VfsResult<()> {
        self.reconfigure
            .map_or(Ok(()), |reconfigure| reconfigure(context))
    }
}

/// A filesystem implementation known to the VFS.
pub struct FileSystemType {
    name: &'static str,
    /// Initializes `fs_context::ops`, corresponding to Linux
    /// `file_system_type::init_fs_context`.
    init_fs_context: InitFsContextFn,
    fs_flags: FileSystemTypeFlags,
}

impl FileSystemType {
    /// Describes an internal filesystem that is not constructible through a
    /// userspace filesystem context.
    pub const fn internal(name: &'static str) -> Self {
        Self {
            name,
            init_fs_context: init_internal_context,
            fs_flags: FileSystemTypeFlags::empty(),
        }
    }

    /// Describes a filesystem that does not require a backing device.
    pub const fn nodev(name: &'static str, init_fs_context: InitFsContextFn) -> Self {
        Self {
            name,
            init_fs_context,
            fs_flags: FileSystemTypeFlags::empty(),
        }
    }

    /// Describes a filesystem that requires a backing device.
    pub const fn device_backed(name: &'static str, init_fs_context: InitFsContextFn) -> Self {
        Self {
            name,
            init_fs_context,
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

    pub(crate) fn init_context(&self, context: &mut FsContext<'_>) -> VfsResult<()> {
        (self.init_fs_context)(context)
    }
}

fn unsupported_get_tree(
    _context: &mut FsContext<'_>,
    _lookup_root: &crate::Path,
    _lookup_pwd: &crate::Path,
) -> VfsResult<Arc<SuperBlock>> {
    Err(VfsError::NoSuchDevice)
}

static INTERNAL_CONTEXT_OPERATIONS: FsContextOperations =
    FsContextOperations::new(unsupported_get_tree);

fn init_internal_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&INTERNAL_CONTEXT_OPERATIONS);
    Ok(())
}

static FILE_SYSTEMS: Lazy<Mutex<Vec<&'static FileSystemType>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

/// Registers one filesystem type.
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

/// Finds a registered filesystem type by its exact name.
pub fn get_filesystem_type(name: &str) -> Option<&'static FileSystemType> {
    FILE_SYSTEMS
        .lock()
        .iter()
        .copied()
        .find(|file_system_type| file_system_type.name == name)
}

/// Returns a snapshot of all registered filesystem types.
pub fn registered_filesystems() -> Vec<&'static FileSystemType> {
    FILE_SYSTEMS.lock().clone()
}
