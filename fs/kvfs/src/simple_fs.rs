// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple filesystem scaffolding for the in-kernel VFS.

use alloc::{string::String, sync::Arc};
use core::time::Duration;

use ksync::Mutex;
use slab::Slab;

use crate::{
    Dentry, DeviceId, InodeOperations, Metadata, MetadataUpdate, NodePermission, NodeType, StatFs,
    StatFsFlags, SuperBlock, SuperBlockOperations, VfsInodeInit, VfsResult,
    libfs::simple_statfs_with_flags, simple_dir::DirMaker,
};

/// A simple filesystem implementation that uses a slab allocator for inodes.
pub struct SimpleFs {
    name: String,
    fs_type: u32,
    mount_flags: StatFsFlags,
    inodes: Mutex<Slab<()>>,
}

impl SimpleFs {
    /// Creates a superblock backed by a simple filesystem.
    pub fn new_with(
        name: String,
        fs_type: u32,
        root: impl FnOnce(Arc<Self>) -> DirMaker,
    ) -> Arc<SuperBlock> {
        Self::new_with_flags(name, fs_type, StatFsFlags::empty(), root)
    }

    /// Creates a superblock backed by a simple filesystem with explicit mount flags.
    pub fn new_with_flags(
        name: String,
        fs_type: u32,
        mut mount_flags: StatFsFlags,
        root: impl FnOnce(Arc<Self>) -> DirMaker,
    ) -> Arc<SuperBlock> {
        let is_readonly = mount_flags.contains(StatFsFlags::RDONLY);
        mount_flags.remove(StatFsFlags::RDONLY);
        let fs = Arc::new(Self {
            name,
            fs_type,
            mount_flags,
            inodes: Mutex::new(Slab::new()),
        });
        let root = root(fs.clone());
        let root = Dentry::new_dir_from_inode(root(), None, String::new());
        SuperBlock::new_with_readonly(fs, root, is_readonly)
    }

    fn alloc_inode(&self) -> u64 {
        self.inodes.lock().insert(()) as u64 + 1
    }

    fn release_inode(&self, ino: u64) {
        self.inodes.lock().remove(ino as usize - 1);
    }
}

impl SuperBlockOperations for SimpleFs {
    fn name(&self) -> &str {
        &self.name
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        Ok(simple_statfs_with_flags(self.fs_type, self.mount_flags))
    }
}

/// Filesystem node for [`SimpleFs`].
pub struct SimpleFsNode {
    fs: Arc<SimpleFs>,
    ino: u64,
    pub(crate) metadata: Mutex<Metadata>,
}

impl SimpleFsNode {
    /// Creates a new filesystem node.
    pub fn new(fs: Arc<SimpleFs>, node_type: NodeType, mode: NodePermission) -> Self {
        Self::new_with_owner(fs, node_type, mode, 0, 0)
    }

    /// Creates a new filesystem node with an explicit owner.
    pub fn new_with_owner(
        fs: Arc<SimpleFs>,
        node_type: NodeType,
        mode: NodePermission,
        uid: u32,
        gid: u32,
    ) -> Self {
        let ino = fs.alloc_inode();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: if node_type == NodeType::Directory {
                2
            } else {
                1
            },
            mode: crate::Umode::new(node_type, mode),
            uid,
            gid,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: Duration::default(),
            mtime: Duration::default(),
            ctime: Duration::default(),
        };
        Self {
            fs,
            ino,
            metadata: Mutex::new(metadata),
        }
    }

    /// Updates the special-file `i_rdev` stored in this node's metadata.
    pub fn set_rdev(&self, rdev: DeviceId) {
        self.metadata.lock().rdev = rdev;
    }

    pub(crate) fn filesystem(&self) -> Arc<SimpleFs> {
        self.fs.clone()
    }

    /// Returns this node's `inode::i_ino`.
    pub fn inode(&self) -> u64 {
        self.ino
    }

    /// Returns the inode fields used when materializing this simple node.
    pub fn inode_init(&self) -> VfsInodeInit {
        VfsInodeInit::from_metadata(&self.metadata.lock())
    }

    /// Returns this node's `inode::i_size`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64> {
        Ok(0)
    }

    /// Writes back this simple node's inode-owned state.
    pub fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }
}

impl Drop for SimpleFsNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.ino);
    }
}

impl InodeOperations for SimpleFsNode {
    fn getattr(
        &self,
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut metadata = self.metadata.lock().clone();
        metadata.size = self.len()?;
        Ok(metadata)
    }

    fn setattr(
        &self,
        _idmap: &crate::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<()> {
        let mut metadata = self.metadata.lock();
        if let Some(size) = update.size {
            metadata.size = size;
        }
        if let Some(mode) = update.mode {
            metadata.mode = metadata.mode.with_permission(mode);
        }
        if let Some((uid, gid)) = update.owner {
            metadata.uid = uid;
            metadata.gid = gid;
        }
        if let Some(atime) = update.atime {
            metadata.atime = atime;
        }
        if let Some(mtime) = update.mtime {
            metadata.mtime = mtime;
        }
        if let Some(ctime) = update.ctime {
            metadata.ctime = ctime;
        }
        Ok(())
    }
}
