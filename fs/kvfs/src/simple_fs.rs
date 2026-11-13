// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple filesystem scaffolding for the in-kernel VFS.

use alloc::{string::String, sync::Arc};
use core::any::Any;

use kalloc_types::{AllocFlags, KBox};
use ksync::Mutex;
use ktime_types::SystemTime;
use slab::Slab;

use crate::{
    Dentry, DeviceId, FileSystemType, InodeOperations, Metadata, MetadataUpdate, NodeFlags,
    NodePermission, NodeType, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations, VfsInode,
    VfsInodeInit, VfsResult, libfs::simple_statfs, simple_dir::DirMaker, type_map::TypeMap,
};

/// A simple filesystem implementation that uses a slab allocator for inodes.
pub struct SimpleFs {
    fs_type: u32,
    inodes: Mutex<Slab<()>>,
    private: Mutex<TypeMap>,
}

impl SimpleFs {
    /// Creates a superblock backed by a simple filesystem.
    ///
    /// Requires sleepable task context with IRQs/preemption enabled, outside
    /// non-sleepable RCU and without locks needed by reclaim, I/O or filesystem
    /// callbacks. Returns `NoMemory` if the private owner cannot be
    /// allocated; nested allocations retain their existing infallible behavior.
    pub fn new_with(
        file_system_type: &'static FileSystemType,
        fs_type: u32,
        root: impl FnOnce(Arc<Self>) -> DirMaker,
    ) -> VfsResult<Arc<SuperBlock>> {
        Self::new_with_superblock_flags(file_system_type, fs_type, SuperBlockFlags::empty(), root)
    }

    /// Creates a superblock backed by a simple filesystem with explicit flags.
    ///
    /// Uses the allocation and execution-context contract of [`Self::new_with`].
    pub fn new_with_superblock_flags(
        file_system_type: &'static FileSystemType,
        fs_type: u32,
        superblock_flags: SuperBlockFlags,
        root: impl FnOnce(Arc<Self>) -> DirMaker,
    ) -> VfsResult<Arc<SuperBlock>> {
        Self::new_with_allocation_flags(
            file_system_type,
            fs_type,
            superblock_flags,
            AllocFlags::KERNEL,
            root,
        )
    }

    /// Creates a superblock with an explicit private-owner allocation policy.
    ///
    /// `allocation_flags` govern only the `KBox` holding filesystem state.
    /// `KERNEL` requires the context of [`Self::new_with`]; `NOWAIT` avoids
    /// reclaim re-entry while singleton initialization is in progress. Neither
    /// policy relaxes the root builder's own locking or sleepability requirements.
    /// Returns allocation errors before constructing or publishing the root.
    pub fn new_with_allocation_flags(
        file_system_type: &'static FileSystemType,
        fs_type: u32,
        superblock_flags: SuperBlockFlags,
        allocation_flags: AllocFlags,
        root: impl FnOnce(Arc<Self>) -> DirMaker,
    ) -> VfsResult<Arc<SuperBlock>> {
        let fs = Arc::new(Self {
            fs_type,
            inodes: Mutex::new(Slab::new()),
            private: Mutex::new(TypeMap::default()),
        });
        let private = KBox::new_with_flags(fs.clone(), allocation_flags)?;
        let root = root(fs.clone());
        let root = Dentry::new_dir_from_inode(root(), None, String::new());
        Ok(SuperBlock::new_with_flags_and_private(
            file_system_type,
            &SIMPLE_SUPER_OPERATIONS,
            private,
            superblock_flags,
            1,
            crate::MAX_LFS_FILESIZE,
            |_| root,
        ))
    }

    fn alloc_inode(&self) -> u64 {
        self.inodes.lock().insert(()) as u64 + 1
    }

    fn release_inode(&self, ino: u64) {
        self.inodes.lock().remove(ino as usize - 1);
    }

    /// Attaches filesystem-instance state owned for the superblock lifetime.
    pub fn set_private<T>(&self, value: Arc<T>)
    where
        T: Any + Send + Sync,
    {
        self.private.lock().insert_arc(value);
    }

    /// Returns filesystem-instance state of type `T` when installed.
    pub fn private<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        self.private.lock().get::<T>()
    }
}

struct SimpleSuperOperations;

static SIMPLE_SUPER_OPERATIONS: SimpleSuperOperations = SimpleSuperOperations;

impl SuperBlockOperations for SimpleSuperOperations {
    fn timestamp_limits(&self, _super_block: &SuperBlock) -> crate::TimestampLimits {
        crate::TimestampLimits::NANOSECOND
    }

    fn statfs(&self, super_block: &SuperBlock, _dentry: &crate::Dentry) -> VfsResult<StatFs> {
        Ok(simple_statfs(
            super_block.private::<Arc<SimpleFs>>()?.fs_type,
        ))
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
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        };
        Self {
            fs,
            ino,
            metadata: Mutex::new(metadata),
        }
    }

    /// Creates a cached symbolic-link inode owned by `fs`.
    ///
    /// The returned inode carries `target` as its link body and is suitable
    /// for static directory entries whose target is fixed for the mount's
    /// lifetime.
    ///
    /// This is the provider side used by `SimpleFs`-based filesystems such as
    /// devfs: those filesystems create the node, while `kvfs` only stores the
    /// cached link body. The dependency therefore points from devfs to kvfs,
    /// not the other way around.
    pub fn new_symlink_inode(fs: Arc<SimpleFs>, target: impl Into<String>) -> Arc<VfsInode> {
        let target = target.into();
        let node = Self::new(
            fs,
            NodeType::Symlink,
            NodePermission::from_bits_truncate(0o777),
        );
        node.metadata.lock().size = target.len() as u64;
        let init = node.inode_init();
        VfsInode::new_cached_symlink(Arc::new(node), NodeFlags::empty(), init, target)
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
    #[expect(
        clippy::len_without_is_empty,
        reason = "CG-024-lint-exceptions: clippy::len_without_is_empty: Length is the inode byte \
                  size, not collection membership; directories have separate entry semantics"
    )]
    pub fn len(&self) -> VfsResult<u64> {
        Ok(self.metadata.lock().size)
    }

    /// Writes back this simple node's inode-owned state.
    pub fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    /// Sets this node's `inode::i_size`.
    ///
    /// Buffered-write completion paths that grow the file outside `setattr`
    /// use this to keep the served metadata in step with the page cache.
    pub fn set_size(&self, size: u64) {
        self.metadata.lock().size = size;
    }
}

impl Drop for SimpleFsNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.ino);
    }
}

impl InodeOperations for SimpleFsNode {
    fn symlink_operations(&self) -> Option<&dyn crate::InodeSymlinkOperations> {
        (self.metadata.lock().mode.node_type() == NodeType::Symlink).then_some(self)
    }

    fn getattr(
        &self,
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(self.metadata.lock().clone())
    }

    fn setattr(
        &self,
        _idmap: &crate::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
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
        Ok(update)
    }
}

impl crate::InodeSymlinkOperations for SimpleFsNode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        inode: &crate::VfsInode,
        _done: &mut crate::DelayedCall,
    ) -> VfsResult<String> {
        inode.cached_link().ok_or(crate::VfsError::InvalidData)
    }
}
