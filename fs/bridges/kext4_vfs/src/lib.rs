// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVFS bridge for the checked KExt4 filesystem core.
//!
//! The crate adapts KExt4 superblock, inode, file, and address-space operations
//! to KVFS and provides the KExt4 implementation of
//! [`fs_block::RootFileSystem`] when selected by Kconfig.

#![cfg_attr(any(not(test), doc), no_std)]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod fs;
mod inode;
mod util;

pub use fs::Ext4Filesystem;

#[fs_block::kiface::provide]
impl fs_block::RootFileSystem {
    fn name() -> &'static str {
        "ext4"
    }

    fn mount_bdev(
        device: kclass::ClassDevice<kclass::BlockDeviceImpl>,
        flags: kvfs::SuperBlockFlags,
    ) -> kvfs::VfsResult<alloc::sync::Arc<kvfs::SuperBlock>> {
        Ext4Filesystem::mount_bdev(device, flags)
    }
}
