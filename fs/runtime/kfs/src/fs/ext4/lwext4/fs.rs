// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kclass::{BlockDeviceImpl as KBlockDevice, ClassDevice};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    DirEntry, DirNode, FileNode, Filesystem, InodeCache, Location, NodeType, Reference,
    ST_RELATIME, StatFs, SuperBlockOperations, VfsError, VfsInode, VfsResult, path::MAX_NAME_LEN,
};
use lwext4_rust::{FsConfig, ffi::EXT4_ROOT_INO};

use super::{
    Ext4Disk, Inode,
    util::{LwExt4Filesystem, into_vfs_err},
};

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 256 };

pub struct Ext4Filesystem {
    inner: Mutex<LwExt4Filesystem>,
    inode_cache: InodeCache,
    root_dir: Mutex<Option<DirEntry>>,
}

impl Ext4Filesystem {
    pub fn new(dev: ClassDevice<KBlockDevice>) -> VfsResult<Filesystem> {
        let ext4 =
            lwext4_rust::Ext4Filesystem::new(Ext4Disk(dev), EXT4_CONFIG).map_err(into_vfs_err)?;

        let fs = Arc::new(Self {
            inner: Mutex::new(ext4),
            inode_cache: InodeCache::new(),
            root_dir: Mutex::new(None),
        });
        *fs.root_dir.lock() = Some(DirEntry::new_dir(
            |this| DirNode::new(Inode::new(fs.clone(), EXT4_ROOT_INO, Some(this))),
            Reference::root(),
        ));
        Ok(Filesystem::new(fs))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, LwExt4Filesystem> {
        self.inner.lock()
    }

    pub(crate) fn get_file_vfs_inode(
        fs: &Arc<Self>,
        ino: u32,
        node_type: NodeType,
    ) -> Arc<VfsInode> {
        fs.inode_cache
            .get_or_insert_file(ino as u64, node_type, || {
                FileNode::new(Inode::new(fs.clone(), ino, None))
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
        let mut fs = self.lock();
        let stat = fs.stat().map_err(into_vfs_err)?;
        Ok(StatFs {
            fs_type: 0xef53,
            block_size: stat.block_size as _,
            blocks: stat.blocks_count,
            blocks_free: stat.free_blocks_count,
            blocks_available: stat.free_blocks_count,

            file_count: stat.inodes_count as _,
            free_file_count: stat.free_inodes_count as _,

            name_length: MAX_NAME_LEN as _,
            fragment_size: 0,
            mount_flags: ST_RELATIME,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        self.inner.lock().flush().map_err(into_vfs_err)
    }
}
