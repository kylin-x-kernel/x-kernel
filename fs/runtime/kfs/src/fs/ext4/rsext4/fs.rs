// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 filesystem adapter (rsext4 backend).
use alloc::{string::String, sync::Arc};

use kclass::{BlockDeviceImpl as KBlockDevice, ClassDevice};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    DirEntry, DirNode, FileNode, Filesystem, InodeCache, Location, NodeType, Reference,
    ST_RELATIME, StatFs, SuperBlockOperations, VfsError, VfsInode, VfsResult, path::MAX_NAME_LEN,
};
use rsext4::Jbd2Dev;

use super::{Ext4Disk, Inode, util::into_vfs_err};

const EXT4_ROOT_INO: u32 = 2;

pub(crate) struct Ext4State {
    pub fs: rsext4::Ext4FileSystem,
    pub dev: Jbd2Dev<Ext4Disk>,
}

impl Ext4State {
    pub(crate) fn split(&mut self) -> (&mut rsext4::Ext4FileSystem, &mut Jbd2Dev<Ext4Disk>) {
        (&mut self.fs, &mut self.dev)
    }
}

/// Ext4 filesystem implementation.
pub struct Ext4Filesystem {
    inner: Mutex<Ext4State>,
    inode_cache: InodeCache,
    root_dir: Mutex<Option<DirEntry>>,
}

impl Ext4Filesystem {
    /// Write dirty ext4 state back to data blocks and metadata structures, but
    /// do not force journal commit or device cache flush yet.
    ///
    /// Keeping data-block writeback ahead of metadata commit preserves the
    /// ordered-mode durability requirement.
    fn writeback_locked(
        fs: &mut rsext4::Ext4FileSystem,
        dev: &mut Jbd2Dev<Ext4Disk>,
    ) -> VfsResult<()> {
        fs.datablock_cache.flush_all(dev).map_err(into_vfs_err)?;
        fs.bitmap_cache.flush_all(dev).map_err(into_vfs_err)?;
        fs.inodetable_cache.flush_all(dev).map_err(into_vfs_err)?;
        fs.sync_superblock(dev).map_err(into_vfs_err)?;
        fs.sync_group_descriptors(dev).map_err(into_vfs_err)
    }

    /// Create a new ext4 filesystem instance backed by a block device.
    pub fn new(dev: ClassDevice<KBlockDevice>) -> VfsResult<Filesystem> {
        let mut dev = Jbd2Dev::initial_jbd2dev(0, Ext4Disk(dev), true);
        let fs = rsext4::mount(&mut dev).map_err(into_vfs_err)?;

        let fs = Arc::new(Self {
            inner: Mutex::new(Ext4State { fs, dev }),
            inode_cache: InodeCache::new(),
            root_dir: Mutex::new(None),
        });
        *fs.root_dir.lock() = Some(DirEntry::new_dir(
            |this| {
                DirNode::new(Inode::new(
                    fs.clone(),
                    EXT4_ROOT_INO,
                    Some(this),
                    Some("/".into()),
                ))
            },
            Reference::root(),
        ));
        Ok(Filesystem::new(fs))
    }

    /// Lock the inner ext4 filesystem state.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Ext4State> {
        self.inner.lock()
    }

    pub(crate) fn get_file_vfs_inode(
        fs: &Arc<Self>,
        ino: u32,
        node_type: NodeType,
        path: Option<String>,
    ) -> Arc<VfsInode> {
        fs.inode_cache
            .get_or_insert_file(ino as u64, node_type, || {
                FileNode::new(Inode::new(fs.clone(), ino, None, path))
            })
    }

    pub(crate) fn writeback_to_disk(&self) -> VfsResult<()> {
        let mut state = self.inner.lock();
        let (fs, dev) = state.split();
        Self::writeback_locked(fs, dev)
    }

    pub(crate) fn sync_to_disk(&self) -> VfsResult<()> {
        let mut state = self.inner.lock();
        let (fs, dev) = state.split();
        Self::writeback_locked(fs, dev)?;
        // Explicit sync must force the pending metadata journal transaction and
        // then flush the device so the prior writeback becomes durable.
        if dev.is_use_journal() {
            dev.umount_commit();
        }
        dev.cantflush().map_err(into_vfs_err)
    }

    pub(crate) fn range_shift(
        location: &Location,
        offset: u64,
        len: u64,
        insert: bool,
    ) -> VfsResult<()> {
        let inode = location
            .entry()
            .downcast::<Inode>()
            .map_err(|_| VfsError::Unsupported)?;
        if insert {
            inode.insert_range(offset, len)
        } else {
            inode.collapse_range(offset, len)
        }
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
        let superblock = &fs.fs.superblock;
        let block_size = superblock.block_size();
        let blocks = superblock.blocks_count();
        let blocks_free = superblock.free_blocks_count();
        Ok(StatFs {
            fs_type: 0xef53,
            block_size: block_size as _,
            blocks,
            blocks_free,
            blocks_available: blocks_free,

            file_count: superblock.s_inodes_count as _,
            free_file_count: superblock.s_free_inodes_count as _,

            name_length: MAX_NAME_LEN as _,
            fragment_size: 0,
            mount_flags: ST_RELATIME,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        self.sync_to_disk()
    }
}
