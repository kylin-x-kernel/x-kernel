// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `nullfs` used as the immutable VFS hierarchy anchor.

use alloc::sync::Arc;
use core::time::Duration;

use crate::{
    Dentry, DirContext, FileDirOperations, FileOperations, GetattrQueryFlags, GetattrRequestMask,
    InodeDirOperations, InodeLookupFlags, InodeOperations, Metadata, MetadataUpdate, MountIdmap,
    NodeFlags, NodePermission, NodeType, StatFs, StatFsFlags, SuperBlock, SuperBlockOperations,
    Umode, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
    libfs::{generic_read_dir, noop_fsync, simple_statfs_with_flags},
    path::{DOT, DOTDOT},
};

const NULLFS_NAME: &str = "nullfs";
const NULLFS_MAGIC: u32 = 0x4E55_4C4C;
const NULLFS_ROOT_INO: u64 = 1;
const NULLFS_BLOCK_SIZE: u64 = 4096;
const NULLFS_MOUNT_FLAGS: StatFsFlags = StatFsFlags::NODEV.union(StatFsFlags::NOEXEC);

/// Creates the single-purpose filesystem used as the initial namespace root.
pub(crate) fn new_superblock() -> Arc<SuperBlock> {
    SuperBlock::new(Arc::new(NullFs {
        root: nullfs_root_dentry(),
    }))
}

struct NullFs {
    root: Dentry,
}

impl SuperBlockOperations for NullFs {
    fn name(&self) -> &str {
        NULLFS_NAME
    }

    fn root_dentry(&self) -> Dentry {
        self.root.clone()
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        Ok(simple_statfs_with_flags(NULLFS_MAGIC, NULLFS_MOUNT_FLAGS))
    }
}

struct NullFsRoot;

impl NullFsRoot {
    fn inode_init() -> VfsInodeInit {
        VfsInodeInit::new(
            NULLFS_ROOT_INO,
            0,
            Umode::new(
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o555),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 2, Default::default())
        .with_stat_data(
            NULLFS_BLOCK_SIZE,
            0,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        )
    }

    fn metadata() -> Metadata {
        Metadata {
            device: 0,
            inode: NULLFS_ROOT_INO,
            nlink: 2,
            mode: Umode::new(
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o555),
            ),
            uid: 0,
            gid: 0,
            size: 0,
            block_size: NULLFS_BLOCK_SIZE,
            blocks: 0,
            rdev: Default::default(),
            atime: Duration::ZERO,
            mtime: Duration::ZERO,
            ctime: Duration::ZERO,
        }
    }
}

impl InodeOperations for NullFsRoot {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        Some(self)
    }

    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(Self::metadata())
    }

    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        _update: MetadataUpdate,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}

impl InodeDirOperations for NullFsRoot {
    fn lookup(
        &self,
        _dir: &VfsInode,
        _dentry: &Dentry,
        _flags: InodeLookupFlags,
    ) -> VfsResult<Dentry> {
        Err(VfsError::NotFound)
    }
}

impl FileOperations for NullFsRoot {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        Some(self)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        generic_read_dir(buf, offset)
    }

    fn fsync(&self, _file: &VfsFile, data_only: bool) -> VfsResult<()> {
        noop_fsync(data_only)
    }
}

impl FileDirOperations for NullFsRoot {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let dentry = file.path().dentry();
        let entries = [
            (DOT, NULLFS_ROOT_INO),
            (
                DOTDOT,
                dentry
                    .parent()
                    .map_or(NULLFS_ROOT_INO, |parent| parent.metadata().inode),
            ),
        ];

        let mut count = 0;
        for (index, (name, inode)) in entries.into_iter().enumerate().skip(ctx.pos() as usize) {
            if !ctx.emit(name, inode, NodeType::Directory, index as u64 + 1) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
}

fn nullfs_root_dentry() -> Dentry {
    let root = Arc::new(NullFsRoot);
    let private_data: Arc<dyn core::any::Any + Send + Sync> = root.clone();
    let inode_operations: Arc<dyn InodeOperations> = root.clone();
    let file_operations: Arc<dyn FileOperations> = root;
    let inode = VfsInode::new_dir_with_operations(
        private_data,
        inode_operations,
        file_operations,
        NodeFlags::PRIVATE,
        NullFsRoot::inode_init(),
    );
    Dentry::new_dir_from_inode(inode, None, alloc::string::String::new())
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn nullfs_is_empty_internal_anchor() {
        let fs = new_superblock();
        let root = fs.root_dir();

        assert_eq!(fs.name(), NULLFS_NAME);
        assert_eq!(root.metadata().inode, NULLFS_ROOT_INO);
        assert_eq!(root.metadata().mode.node_type(), NodeType::Directory);
        assert!(root.lookup("anything").is_err());
        assert_eq!(fs.stat().unwrap().fs_type, NULLFS_MAGIC);
        assert_eq!(fs.stat().unwrap().mount_flags, NULLFS_MOUNT_FLAGS);
    }
}
