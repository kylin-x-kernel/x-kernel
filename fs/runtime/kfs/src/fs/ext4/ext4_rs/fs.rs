// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 filesystem adapter (ext4_rs backend).
use alloc::sync::Arc;

use ext4_rs::Ext4;
use kclass::{BlockDeviceImpl as KBlockDevice, ClassDevice};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    DirEntry, DirNode, FileNode, Filesystem, InodeCache, Location, NodeType, Reference,
    ST_RELATIME, StatFs, SuperBlockOperations, VfsError, VfsInode, VfsResult, path::MAX_NAME_LEN,
};

use super::{Ext4Disk, Inode};

const EXT4_ROOT_INODE: u32 = 2;

/// Ext4 filesystem implementation.
pub struct Ext4Filesystem {
    inner: Mutex<Ext4>,
    inode_cache: InodeCache,
    root_dir: Mutex<Option<DirEntry>>,
}

impl Ext4Filesystem {
    /// Create a new ext4 filesystem instance backed by a block device.
    pub fn new(dev: ClassDevice<KBlockDevice>) -> VfsResult<Filesystem> {
        let ext4 = Ext4::open(Arc::new(Ext4Disk::new(dev)));
        let fs = Arc::new(Self {
            inner: Mutex::new(ext4),
            inode_cache: InodeCache::new(),
            root_dir: Mutex::new(None),
        });
        *fs.root_dir.lock() = Some(DirEntry::new_dir(
            |this| DirNode::new(Inode::new(fs.clone(), EXT4_ROOT_INODE, Some(this), None)),
            Reference::root(),
        ));
        Ok(Filesystem::new(fs))
    }

    /// Lock the inner ext4 filesystem.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Ext4> {
        self.inner.lock()
    }

    pub(crate) fn get_file_vfs_inode(
        fs: &Arc<Self>,
        ino: u32,
        node_type: NodeType,
    ) -> Arc<VfsInode> {
        fs.inode_cache
            .get_or_insert_file(ino as u64, node_type, || {
                FileNode::new(Inode::new(fs.clone(), ino, None, None))
            })
    }

    pub(crate) fn range_shift(
        _location: &Location,
        _offset: u64,
        _len: u64,
        _insert: bool,
    ) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }
}

impl SuperBlockOperations for Ext4Filesystem {
    fn name(&self) -> &str {
        "ext4"
    }

    fn root_dentry(&self) -> DirEntry {
        self.root_dir.lock().clone().unwrap()
    }

    fn statfs(&self) -> VfsResult<StatFs> {
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
            mount_flags: ST_RELATIME,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        Ok(())
    }
}
