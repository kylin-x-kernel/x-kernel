// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVFS bridge for the checked KExt4 filesystem core.
//!
//! The crate adapts KExt4 superblock, inode, file, address-space, and
//! extended-attribute operations to KVFS. The KVFS inode cache is the sole
//! resident identity table. Each cached VFS inode composes one
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
use fs::Ext4MountOptions;

fn ext4_get_tree(
    context: &kvfs::FsContext<'_>,
    lookup_root: &kvfs::Path,
    lookup_pwd: &kvfs::Path,
) -> kvfs::VfsResult<alloc::sync::Arc<kvfs::SuperBlock>> {
    let mount_options = Ext4MountOptions::parse(context.data())?;
    kvfs::get_tree_bdev(context, lookup_root, lookup_pwd, move |super_block| {
        Ext4Filesystem::fill_super(super_block, mount_options)
    })
}

/// The canonical ext4 filesystem type registered with KVFS.
static FILE_SYSTEM_TYPE: kvfs::FileSystemType =
    kvfs::FileSystemType::device_backed("ext4", ext4_get_tree);

#[macros::register_init]
fn init_ext4_fs() {
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE).expect("ext4 filesystem type must register once");
}
