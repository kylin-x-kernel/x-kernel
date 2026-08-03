// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FAT filesystem adapter.
use alloc::{string::String, sync::Arc};
use core::{
    marker::PhantomPinned,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use kclass::{BlockDeviceImpl as KBlockDevice, ClassDevice};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    Dentry, NodePermission, NodeType, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    VfsInode, VfsInodeInit, VfsResult, path::MAX_NAME_LEN,
};
use slab::Slab;

use super::{FatDisk, dir::FatDirInode, ff, util::into_vfs_err};

/// Inner FAT filesystem state.
pub(crate) struct FatFilesystemInner {
    pub(crate) inner: ff::FileSystem,
    inode_allocator: Slab<()>,
    _pinned: PhantomPinned,
}

impl FatFilesystemInner {
    /// Allocate a new inode number.
    pub(crate) fn alloc_inode(&mut self) -> u64 {
        self.inode_allocator.insert(()) as u64 + 1
    }

    /// Release a previously allocated inode number.
    pub(crate) fn release_inode(&mut self, ino: u64) {
        self.inode_allocator.remove(ino as usize - 1);
    }
}

/// FAT filesystem implementation.
pub struct FatFilesystem {
    inner: Mutex<FatFilesystemInner>,
}

pub(crate) struct FatFilesystemGuard<'a> {
    owner: NonNull<FatFilesystem>,
    guard: MutexGuard<'a, FatFilesystemInner>,
}

impl FatFilesystemGuard<'_> {
    pub(crate) fn owner_ptr(&self) -> NonNull<FatFilesystem> {
        self.owner
    }
}

impl Deref for FatFilesystemGuard<'_> {
    type Target = FatFilesystemInner;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for FatFilesystemGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl FatFilesystem {
    /// Mount a FAT filesystem backed by a block device.
    pub fn mount_bdev(
        dev: ClassDevice<KBlockDevice>,
        superblock_flags: SuperBlockFlags,
    ) -> Arc<SuperBlock> {
        let mut inner = FatFilesystemInner {
            inner: ff::FileSystem::new(FatDisk::new(dev), fatfs::FsOptions::new())
                .expect("failed to initialize FAT filesystem"),
            inode_allocator: Slab::new(),
            _pinned: PhantomPinned,
        };
        let root_inode_number = inner.alloc_inode();
        let root_block_size = inner.inner.cluster_size() as u64;
        let result = Arc::new(Self {
            inner: Mutex::new(inner),
        });

        let root_node = {
            let fs = result.lock();
            let root = fs.inner.root_dir();
            FatDirInode::new(result.clone(), result.as_ref(), root, root_inode_number)
        };
        let root_inode = VfsInode::new_openable_dir(
            root_node,
            VfsInodeInit::new(
                root_inode_number,
                root_block_size,
                kvfs::Umode::new(NodeType::Directory, NodePermission::default()),
            )
            .with_owner_links_and_rdev(0, 0, 1, Default::default())
            .with_stat_data(
                root_block_size,
                1,
                Default::default(),
                Default::default(),
                Default::default(),
            ),
        );
        let root_dir = Dentry::new_dir_from_inode(root_inode, None, String::new());
        SuperBlock::new_with_flags(result, root_dir, superblock_flags)
    }
}

impl FatFilesystem {
    pub(crate) fn lock(&self) -> FatFilesystemGuard<'_> {
        FatFilesystemGuard {
            owner: NonNull::from(self),
            guard: self.inner.lock(),
        }
    }
}

impl SuperBlockOperations for FatFilesystem {
    fn name(&self) -> &str {
        "vfat"
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        let fs = self.inner.lock();
        let stats = fs.inner.stats().map_err(into_vfs_err)?;
        Ok(StatFs {
            fs_type: 0x65735546, // fuse
            block_size: stats.cluster_size() as _,
            blocks: stats.total_clusters() as _,
            blocks_free: stats.free_clusters() as _,
            blocks_available: stats.free_clusters() as _,

            file_count: 0,
            free_file_count: 0,

            name_length: MAX_NAME_LEN as _,
            fragment_size: 0,
        })
    }
}
