// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVFS bridge for the checked KExt4 filesystem core.
//!
//! The crate adapts KExt4 superblock, inode, file, address-space, and
//! extended-attribute operations to KVFS and provides the KExt4 implementation
//! [`fs_block::RootFileSystem`] when selected by Kconfig. The KVFS inode cache
//! is the sole resident identity table. Each cached VFS inode composes one
//! `kext4::Ext4Inode` private state object; KExt4 has no second inode-number
//! cache or resident lifecycle.

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
