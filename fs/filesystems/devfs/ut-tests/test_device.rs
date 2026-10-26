// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helper for node tests that must observe an opened file.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use kvfs::{
    Dentry, DeviceFileOps, DeviceId, Mount, NodeFlags, Path, SimpleDir, SimpleFs, VfsFile,
    VfsInode, dentry_open,
};

use crate::DeviceFile;

/// Each case gets its own registration number, so nothing collides with a
/// device the boot already published or with another case.
static NEXT_DEVICE: AtomicU32 = AtomicU32::new(0);

/// Opens a character-device node through the real VFS open path.
///
/// The node is built the way `device_dentry` builds it and the number is
/// registered, so the returned file carries the file operation set the VFS
/// installs for a character device — which is what `poll` capability questions
/// are actually about.
pub(crate) fn open_character_node(name: &str, ops: Arc<dyn DeviceFileOps>) -> Arc<VfsFile> {
    let super_block = SimpleFs::new_with(&crate::FILE_SYSTEM_TYPE, crate::DEVFS_MAGIC, |fs| {
        SimpleDir::new_maker(fs, Arc::new(kvfs::DirMapping::new()))
    });
    let fs = super_block.private::<Arc<SimpleFs>>().unwrap().clone();
    let device = DeviceFile::new_character(
        fs,
        DeviceId::new(240, NEXT_DEVICE.fetch_add(1, Ordering::Relaxed)),
        ops,
    )
    .unwrap();
    let i_flags = NodeFlags::NON_CACHEABLE | device.flags();
    let inode = VfsInode::new_special(device.clone(), i_flags, device.inode_init());
    let dentry = Dentry::new_file_from_inode(inode, Some(super_block.root_dir()), name.into());
    let path = Path::new(Mount::new_root(&super_block), dentry);
    dentry_open(
        path,
        linux_raw_sys::general::O_RDWR,
        kcred::initial_cred(),
    )
    .unwrap()
}
