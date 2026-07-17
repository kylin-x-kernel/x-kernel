// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous inode file allocation.

use alloc::{format, string::String, sync::Arc};
use core::{any::Any, time::Duration};

use klazy::Once;

use crate::{
    Dentry, DentryOperations, FMode, FileOperations, InodeOperations, Metadata, Mount, MountIdmap,
    NodeFlags, NodePermission, NodeType, OpenFlags, Path, StatFs, StatFsFlags, SuperBlock,
    SuperBlockOperations, Umode, VfsFile, VfsInode, VfsInodeInit, VfsResult,
};

static ANON_INODE_FS: Once<AnonInodeFs> = Once::new();

const ANON_INODE_INO: u64 = 1;
const ANON_INODEFS_ROOT_INO: u64 = 2;
const ANON_INODE_FS_MAGIC: u32 = 0x0904_1934;
const ANON_INODE_BLOCK_SIZE: u64 = 4096;
const ANON_INODE_BLOCK_SIZE_U32: u32 = 4096;
const ANON_INODE_NAME_LEN: u32 = 255;

/// Anonymous-inode pseudo filesystem used by kernel-created file objects.
pub struct AnonInodeFs {
    mount: Arc<Mount>,
    singleton_inode: Arc<VfsInode>,
    dentry_operations: Arc<dyn DentryOperations>,
}

impl AnonInodeFs {
    fn new() -> Self {
        let singleton_inode = AnonInode::new_shared_inode();
        let super_block = SuperBlock::new(Arc::new(AnonInodeSuperBlock), anon_inode_root_dentry());
        Self {
            mount: Mount::new_root(&super_block),
            singleton_inode,
            dentry_operations: Arc::new(AnonInodeDentryOperations),
        }
    }

    /// Initializes the singleton anonymous-inode filesystem.
    ///
    /// This is intended to run from the VFS boot path before user tasks or
    /// parallel unit tests can create anon-inode-backed files. Keeping
    /// initialization out of the first-use path avoids many callers racing into
    /// a complex VFS construction closure.
    pub fn init_global() {
        let _ = ANON_INODE_FS.call_once(Self::new);
    }

    /// Returns the singleton anonymous-inode filesystem instance.
    ///
    /// # Panics
    ///
    /// Panics if [`AnonInodeFs::init_global`] has not completed. This makes
    /// boot-order regressions fail at the call site instead of silently doing
    /// complex VFS initialization from arbitrary runtime paths.
    pub fn global() -> &'static Self {
        ANON_INODE_FS
            .get()
            .expect("anonymous inode filesystem must be initialized before use")
    }

    /// Creates an anonymous-inode-backed open file description.
    ///
    /// All files created through this entry share the singleton anonymous inode.
    /// The caller supplies the file operation table and typed private data.
    pub fn get_file<T>(
        &self,
        name: &str,
        fops: Arc<dyn FileOperations>,
        private_data: Arc<T>,
        flags: FMode,
        open_flags: OpenFlags,
    ) -> VfsResult<Arc<VfsFile>>
    where
        T: Any + Send + Sync + 'static,
    {
        let open_flags =
            open_flags & (OpenFlags::WRITE_ONLY | OpenFlags::READ_WRITE | OpenFlags::NONBLOCK);
        let file = self.alloc_file(name, flags, open_flags, fops)?;
        file.set_private_data(private_data);
        Ok(file)
    }

    fn alloc_file(
        &self,
        name: &str,
        flags: FMode,
        open_flags: OpenFlags,
        fops: Arc<dyn FileOperations>,
    ) -> VfsResult<Arc<VfsFile>> {
        self.mount.alloc_file_pseudo_with_dentry_operations(
            self.singleton_inode.clone(),
            name,
            flags,
            open_flags,
            fops,
            self.dentry_operations.clone(),
        )
    }
}

/// Initializes the VFS-wide anonymous-inode pseudo filesystem.
///
/// Boot code must call this before runtime paths can create eventfd, epoll,
/// timerfd, pidfd, pipe, or other anon-inode-backed files.
pub fn init_anon_inodefs() {
    AnonInodeFs::init_global();
}

struct AnonInode {
    inode: u64,
}

impl AnonInode {
    fn new_shared_inode() -> Arc<VfsInode> {
        VfsInode::new_file_with_inode_operations(
            Arc::new(()),
            Arc::new(Self {
                inode: ANON_INODE_INO,
            }),
            NodeFlags::PRIVATE | NodeFlags::ANON_INODE,
            Self::init(ANON_INODE_INO),
        )
    }

    fn init(inode: u64) -> VfsInodeInit {
        VfsInodeInit::new(
            inode,
            0,
            Umode::new(NodeType::Unknown, NodePermission::from_bits_truncate(0o600)),
        )
        .with_owner_links_and_rdev(0, 0, 1, Default::default())
        .with_stat_data(
            ANON_INODE_BLOCK_SIZE,
            0,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        )
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            device: 0,
            inode: self.inode,
            nlink: 1,
            mode: Umode::new(NodeType::Unknown, NodePermission::from_bits_truncate(0o600)),
            uid: 0,
            gid: 0,
            size: 0,
            block_size: ANON_INODE_BLOCK_SIZE,
            blocks: 0,
            rdev: Default::default(),
            atime: Duration::ZERO,
            mtime: Duration::ZERO,
            ctime: Duration::ZERO,
        }
    }
}

impl InodeOperations for AnonInode {
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(self.metadata())
    }
}

struct AnonInodeDentryOperations;

impl DentryOperations for AnonInodeDentryOperations {
    fn d_dname(&self, dentry: &Dentry) -> VfsResult<Option<String>> {
        Ok(Some(format!("anon_inode:{}", dentry.name_snapshot())))
    }
}

fn anon_inode_root_dentry() -> Dentry {
    let inode = VfsInode::new_dir_with_defaults(NodeFlags::PRIVATE, anon_inode_root_init());
    Dentry::new_dir_from_inode(inode, None, String::new())
}

fn anon_inode_root_init() -> VfsInodeInit {
    VfsInodeInit::new(
        ANON_INODEFS_ROOT_INO,
        0,
        Umode::new(
            NodeType::Directory,
            NodePermission::from_bits_truncate(0o555),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, Default::default())
    .with_stat_data(
        ANON_INODE_BLOCK_SIZE,
        0,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    )
}

struct AnonInodeSuperBlock;

impl SuperBlockOperations for AnonInodeSuperBlock {
    fn name(&self) -> &str {
        "anon_inodefs"
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: ANON_INODE_FS_MAGIC,
            block_size: ANON_INODE_BLOCK_SIZE_U32,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 0,
            free_file_count: 0,
            name_length: ANON_INODE_NAME_LEN,
            fragment_size: ANON_INODE_BLOCK_SIZE_U32,
            mount_flags: StatFsFlags::NODEV | StatFsFlags::NOEXEC,
        })
    }
}
