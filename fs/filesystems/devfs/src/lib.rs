// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device filesystem (devfs) implementation.

#![no_std]
#![feature(bstr)]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod device_file;
mod nodes;
mod root;

use alloc::sync::Arc;

pub use device_file::DeviceFile;
pub(crate) use device_file::{add_device_entry, device_dentry};
use klazy::Once;
use kvfs::{
    FileSystemType, FsContext, Mount, SimpleFs, SuperBlock, SuperBlockFlags, VfsResult,
    get_tree_nodev,
};
pub use nodes::pts::Ptmx;

const DEVFS_MAGIC: u32 = 0x0102_1994;
static DEVFS: Once<(Arc<SuperBlock>, Arc<Mount>)> = Once::new();

fn get_tree(
    context: &FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_devfs_with_type(file_system_type, flags))
    })
}

/// Registered devtmpfs filesystem type.
pub static FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("devtmpfs", get_tree);

#[macros::register_init]
fn init_devfs_fs() {
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE)
        .expect("devtmpfs filesystem type must register once");
}

/// Returns the shared devtmpfs superblock for device access.
///
/// The singleton retains the internal root mount used for kernel device-node
/// updates, corresponding to Linux's private devtmpfs mount.
pub fn new_devfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_devfs_with_type(&FILE_SYSTEM_TYPE, superblock_flags)
}

fn new_devfs_with_type(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    let (super_block, _internal_mount) = DEVFS.call_once(|| {
        let super_block = SimpleFs::new_with_superblock_flags(
            file_system_type,
            DEVFS_MAGIC,
            superblock_flags,
            root::builder,
        );
        let internal_mount = Mount::new_root(&super_block);
        (super_block, internal_mount)
    });
    super_block.clone()
}

/// Capture a snapshot of the firmware device tree blob.
pub fn capture_firmware_dtb_snapshot() {
    nodes::dtb::capture_snapshot();
}

#[cfg(feature = "dev-log")]
pub use nodes::log::bind_dev_log;
