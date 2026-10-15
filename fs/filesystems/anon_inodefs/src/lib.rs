// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous-inode pseudo filesystem.

#![no_std]

extern crate alloc;

use alloc::{format, string::String, sync::Arc};
use core::any::Any;

use kcred::Cred;
use klazy::Once;
use kvfs::{
    Dentry, DentryOperations, FMode, FileOperations, FileSystemType, GetattrQueryFlags,
    GetattrRequestMask, InodeOperations, Metadata, MetadataUpdate, Mount, MountFlags, MountIdmap,
    NodeFlags, NodePermission, NodeType, OpenFlags, Path, Umode, VfsError, VfsFile, VfsInode,
    VfsInodeInit, VfsResult, get_next_ino, libfs::new_pseudo_super_block,
};
use memaddr::PAGE_SIZE_4K;

static ANON_INODE_FS: Once<AnonInodeFs> = Once::new();
static ANON_INODE_FS_TYPE: FileSystemType = FileSystemType::internal("anon_inodefs");

const ANON_INODE_FS_MAGIC: u32 = 0x0904_1934;

/// Hidden pseudo filesystem that owns the shared anonymous inode.
pub struct AnonInodeFs {
    mount: Arc<Mount>,
    singleton_inode: Arc<VfsInode>,
}

impl AnonInodeFs {
    fn new() -> Self {
        let super_block = new_pseudo_super_block(
            &ANON_INODE_FS_TYPE,
            ANON_INODE_FS_MAGIC,
            &ANON_INODE_DENTRY_OPERATIONS,
        );
        let mount =
            Mount::new_root_with_flags(&super_block, MountFlags::NODEV | MountFlags::NOEXEC);
        Self {
            mount,
            singleton_inode: new_anon_inode(),
        }
    }

    /// Initializes the global hidden mount and shared inode.
    pub fn init_global() {
        let _ = ANON_INODE_FS.call_once(Self::new);
    }

    /// Returns the initialized anonymous-inode filesystem.
    ///
    /// # Panics
    ///
    /// Panics when boot has not called [`init_anon_inodefs`].
    pub fn global() -> &'static Self {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Boot installs the permanent pseudo \
                      filesystem before any anonymous-file or pipe creation"
        )]
        ANON_INODE_FS
            .get()
            .expect("anonymous inode filesystem must be initialized before use")
    }

    /// Creates an open file description backed by the shared anonymous inode.
    pub fn get_file<T>(
        &self,
        name: &str,
        file_operations: Arc<dyn FileOperations>,
        private_data: Arc<T>,
        mode: FMode,
        open_flags: OpenFlags,
        cred: Arc<Cred>,
    ) -> VfsResult<Arc<VfsFile>>
    where
        T: Any + Send + Sync + 'static,
    {
        let open_flags =
            open_flags & (OpenFlags::WRITE_ONLY | OpenFlags::READ_WRITE | OpenFlags::NONBLOCK);
        let file = self.mount.alloc_file_pseudo(
            self.singleton_inode.clone(),
            name,
            mode,
            open_flags,
            file_operations,
            cred,
        )?;
        file.set_private_data(private_data);
        Ok(file)
    }
}

/// Initializes the hidden anonymous-inode filesystem during boot.
pub fn init_anon_inodefs() {
    AnonInodeFs::init_global();
}

struct AnonInodeOperations;

impl InodeOperations for AnonInodeOperations {
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        path: Option<&Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut metadata = path.ok_or(VfsError::InvalidInput)?.metadata();
        metadata.mode = metadata.mode.with_node_type(NodeType::Unknown);
        Ok(metadata)
    }

    // The common `setattr` fallback assumes each inode's metadata is resident
    // to itself. This table backs the shared singleton inode behind every
    // eventfd, epoll, timerfd, signalfd, pidfd, socket, and BPF anonymous
    // file, so accepting a fallback update would let one `fchown`/`fchmod`/
    // `utimensat` through any of those descriptors mutate metadata that every
    // other process observes on its own anonymous fds. Keep the explicit
    // rejection semantics for those fields instead of inheriting the
    // fallback. Size updates keep the fallback's `EINVAL`: the singleton is
    // not a regular file, so `ftruncate` stays on the documented errno,
    // consistent with anonymous pipes and without publishing any state.
    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        if update.size.is_some() {
            return Err(VfsError::InvalidInput);
        }
        Err(VfsError::OperationNotSupported)
    }
}

fn new_anon_inode() -> Arc<VfsInode> {
    let timestamp = ktime::realtime();
    let init = VfsInodeInit::new(
        get_next_ino(),
        0,
        Umode::new(
            NodeType::Unknown,
            NodePermission::OWNER_READ | NodePermission::OWNER_WRITE,
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, Default::default())
    .with_stat_data(PAGE_SIZE_4K as u64, 0, timestamp, timestamp, timestamp);
    VfsInode::new_file_with_inode_operations(
        Arc::new(()),
        Arc::new(AnonInodeOperations),
        NodeFlags::PRIVATE | NodeFlags::ANON_INODE,
        init,
    )
}

struct AnonInodeDentryOperations;

static ANON_INODE_DENTRY_OPERATIONS: AnonInodeDentryOperations = AnonInodeDentryOperations;

impl DentryOperations for AnonInodeDentryOperations {
    fn d_dname(&self, dentry: &Dentry) -> VfsResult<Option<String>> {
        Ok(Some(format!("anon_inode:{}", dentry.name_snapshot())))
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
