// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ext4 superblock operations.
use alloc::{string::String, sync::Arc};

use kclass::{BlockDeviceImpl as KBlockDevice, ClassDevice};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    Dentry, InodeCache, NodeFlags, NodeType, StatFs, StatFsFlags, SuperBlock, SuperBlockOperations,
    Umode, VfsInode, VfsInodeInit, VfsResult, path::MAX_NAME_LEN,
};
use rsext4::{Jbd2Dev, disknode::Ext4Inode};

use super::{
    Ext4Disk,
    inode::Inode,
    util::{inode_fast_symlink_target, inode_rdev, into_vfs_err},
};

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
    root_dir: Mutex<Option<Dentry>>,
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

    /// Mount an ext4 filesystem backed by a block device.
    pub fn mount_bdev(dev: ClassDevice<KBlockDevice>) -> VfsResult<Arc<SuperBlock>> {
        let mut dev = Jbd2Dev::initial_jbd2dev(0, Ext4Disk(dev), true);
        let fs = rsext4::mount(&mut dev).map_err(into_vfs_err)?;

        let fs = Arc::new(Self {
            inner: Mutex::new(Ext4State { fs, dev }),
            inode_cache: InodeCache::new(),
            root_dir: Mutex::new(None),
        });
        let root_inode = Self::iget(&fs, EXT4_ROOT_INO)?;
        *fs.root_dir.lock() = Some(Dentry::new_dir_from_inode(root_inode, None, String::new()));
        Ok(SuperBlock::new(fs))
    }

    /// Lock the inner ext4 filesystem state.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Ext4State> {
        self.inner.lock()
    }

    pub(crate) fn iget(fs: &Arc<Self>, ino: u32) -> VfsResult<Arc<VfsInode>> {
        if let Some(inode) = fs.inode_cache.lookup(ino as u64) {
            return Ok(inode);
        }

        let mut state = fs.lock();
        let (inner, dev) = state.split();
        let disk_inode = inner.get_inode_by_num(dev, ino).map_err(into_vfs_err)?;
        Ok(Self::iget_from_disk_inode(fs, ino, &disk_inode))
    }

    #[expect(
        dead_code,
        reason = "the lightweight writeback helper is kept for the follow-up sync path split"
    )]
    pub(crate) fn writeback_to_disk(&self) -> VfsResult<()> {
        let mut state = self.lock();
        let (fs, dev) = state.split();
        Self::writeback_locked(fs, dev)
    }

    pub(crate) fn iget_from_disk_inode(
        fs: &Arc<Self>,
        ino: u32,
        disk_inode: &Ext4Inode,
    ) -> Arc<VfsInode> {
        let mode = Umode::from_bits(disk_inode.i_mode);
        let node_type = mode.node_type();
        let init = VfsInodeInit::new(ino as u64, disk_inode.size(), mode)
            .with_owner_links_and_rdev(
                disk_inode.uid(),
                disk_inode.gid(),
                disk_inode.i_links_count as u64,
                inode_rdev(disk_inode),
            )
            .with_generation(disk_inode.i_generation);
        match node_type {
            NodeType::Directory => fs.inode_cache.get_or_insert_openable_dir_with_init(
                NodeFlags::empty(),
                init,
                || Inode::new(fs.clone(), ino, node_type),
            ),
            NodeType::RegularFile | NodeType::Unknown => fs
                .inode_cache
                .get_or_insert_file_with_init(NodeFlags::empty(), init, || {
                    Inode::new(fs.clone(), ino, node_type)
                }),
            NodeType::Symlink => {
                let inode = fs.inode_cache.get_or_insert_symlink_with_init(
                    NodeFlags::empty(),
                    init,
                    || Inode::new(fs.clone(), ino, node_type),
                );
                if let Some(link) = inode_fast_symlink_target(disk_inode) {
                    inode.set_cached_link(link);
                }
                inode
            }
            NodeType::CharacterDevice
            | NodeType::BlockDevice
            | NodeType::Fifo
            | NodeType::Socket => {
                fs.inode_cache
                    .get_or_insert_special_with_init(NodeFlags::empty(), init, || {
                        Inode::new(fs.clone(), ino, node_type)
                    })
            }
        }
    }

    pub(crate) fn sync_to_disk(&self) -> VfsResult<()> {
        let mut state = self.lock();
        let (fs, dev) = state.split();
        Self::writeback_locked(fs, dev)?;
        // Explicit sync must force the pending metadata journal transaction and
        // then flush the device so the prior writeback becomes durable.
        if dev.is_use_journal() {
            dev.umount_commit();
        }
        dev.cantflush().map_err(into_vfs_err)
    }
}

impl SuperBlockOperations for Ext4Filesystem {
    fn name(&self) -> &str {
        "ext4"
    }

    fn root_dentry(&self) -> Dentry {
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
            mount_flags: StatFsFlags::RELATIME,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        self.sync_to_disk()
    }
}
