// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 filesystem adapter (ext4_rs backend).
use alloc::sync::Arc;
use core::cell::OnceCell;

use ext4_rs::Ext4;
use fs_ng_vfs::{
    DirEntry, DirNode, Filesystem, FilesystemOps, Reference, StatFs, VfsResult, path::MAX_NAME_LEN,
};
use kdriver::BlockDevice as KBlockDevice;
use kspin::{SpinNoPreempt as Mutex, SpinNoPreemptGuard as MutexGuard};

use super::{Ext4Disk, Inode};

const EXT4_ROOT_INODE: u32 = 2;

/// Ext4 filesystem implementation.
pub struct Ext4Filesystem {
    inner: Mutex<Ext4>,
    root_dir: OnceCell<DirEntry>,
}

impl Ext4Filesystem {
    /// Create a new ext4 filesystem instance backed by a block device.
    pub fn new(dev: KBlockDevice) -> VfsResult<Filesystem> {
        let ext4 = Ext4::open(Arc::new(Ext4Disk::new(dev)));
        let fs = Arc::new(Self {
            inner: Mutex::new(ext4),
            root_dir: OnceCell::new(),
        });
        let _ = fs.root_dir.set(DirEntry::new_dir(
            |this| DirNode::new(Inode::new(fs.clone(), EXT4_ROOT_INODE, Some(this), None)),
            Reference::root(),
        ));
        Ok(Filesystem::new(fs))
    }

    /// Lock the inner ext4 filesystem.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Ext4> {
        self.inner.lock()
    }
}

unsafe impl Send for Ext4Filesystem {}

unsafe impl Sync for Ext4Filesystem {}

impl FilesystemOps for Ext4Filesystem {
    fn name(&self) -> &str {
        "ext4"
    }

    fn root_dir(&self) -> DirEntry {
        self.root_dir.get().unwrap().clone()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        let fs = self.lock();
        let superblock = fs.super_block;
        let block_size = superblock.block_size();
        let blocks = superblock.blocks_count() as u64;
        let blocks_free = superblock.free_blocks_count();
        Ok(StatFs {
            fs_type: 0xef53,
            block_size,
            blocks,
            blocks_free,
            blocks_available: blocks_free,

            file_count: superblock.total_inodes() as u64,
            free_file_count: superblock.free_inodes_count() as u64,

            name_length: MAX_NAME_LEN as _,
            fragment_size: 0,
            mount_flags: 0,
        })
    }

    fn flush(&self) -> VfsResult<()> {
        Ok(())
    }
}
