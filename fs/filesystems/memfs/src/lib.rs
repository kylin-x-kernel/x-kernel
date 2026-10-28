// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! In-memory filesystem implementation for the VFS layer.

#![no_std]

extern crate alloc;

mod quota;

pub mod ramfs;
pub mod shmem;

use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::{borrow::Borrow, cmp::Ordering, ops::ControlFlow};

use hashbrown::HashMap;
use iov_iter::{IovIterDest, IovIterSource, iov_iter_kvec_dest, iov_iter_kvec_source};
use kalloc::global_allocator;
use kcred::Cred;
use kerrno::{KError, KResult};
use ksync::Mutex;
use ktime_types::SystemTime;
#[cfg(unittest)]
use kvfs::Mount;
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FileDirOperations,
    FileOperations, FileSystemType, FsContext, FsContextOperations, InodeDirOperations,
    InodeOperations, InodeSymlinkOperations, Kiocb, LockedDentry, Metadata, MetadataUpdate,
    NodeFlags, NodePermission, NodeType, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    Umode, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult, WriteBeginRequest,
    WriteEndRequest, get_tree_nodev, inode_init_owner,
    libfs::{simple_getattr, simple_rename, simple_statfs, simple_write_end},
};
use pagecache::{CacheResidency, FolioGuard};
use quota::TmpfsAccounting;
use slab::Slab;

static ZERO_PAGE: [u8; memaddr::PAGE_SIZE_4K] = [0; memaddr::PAGE_SIZE_4K];

pub(crate) const RAMFS_MAGIC: u32 = 0x8584_58f6;
pub(crate) const TMPFS_MAGIC: u32 = 0x0102_1994;

fn tmpfs_get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    let options = *context.private::<shmem::TmpfsContextOptions>()?;
    let mount_options = options.mount_options();
    get_tree_nodev(context, move |file_system_type, flags| {
        Ok(shmem::new_tmpfs_with_type_and_mount_options(
            file_system_type,
            flags,
            mount_options,
        ))
    })
}

fn tmpfs_reconfigure(context: &mut FsContext<'_>) -> VfsResult<()> {
    let options = *context.private::<shmem::TmpfsContextOptions>()?;
    let super_block = context.super_block()?;
    let fs = super_block.private::<Arc<MemoryFs>>()?;
    if let Some(accounting) = fs.accounting.as_ref() {
        accounting.reconfigure(
            options.reconfigure_page_limit(),
            options.reconfigure_inode_limit(),
        )?;
    }
    Ok(())
}

static TMPFS_CONTEXT_OPERATIONS: FsContextOperations =
    FsContextOperations::with_reconfigure(tmpfs_get_tree, tmpfs_reconfigure);

fn init_tmpfs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    let total_pages = global_allocator().total_pages();
    context.set_operations(&TMPFS_CONTEXT_OPERATIONS);
    context.set_private(shmem::parse_tmpfs_context_options(
        context.data(),
        total_pages,
    )?)?;
    Ok(())
}

/// Registered tmpfs filesystem type.
pub static TMPFS_TYPE: FileSystemType = FileSystemType::nodev("tmpfs", init_tmpfs_context);

#[macros::register_init]
fn init_memory_filesystems() {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Each statically named filesystem is \
                  registered once by its unique linker initialization entry"
    )]
    kvfs::register_filesystem(&ramfs::RAMFS_TYPE)
        .expect("ramfs filesystem type must register once");
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Each statically named filesystem is \
                  registered once by its unique linker initialization entry"
    )]
    kvfs::register_filesystem(&TMPFS_TYPE).expect("tmpfs filesystem type must register once");
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct FileName(String);

impl PartialOrd for FileName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FileName {
    fn cmp(&self, other: &Self) -> Ordering {
        fn index(s: &str) -> u8 {
            match s {
                "." => 0,
                ".." => 1,
                _ => 2,
            }
        }
        (index(&self.0), &self.0).cmp(&(index(&other.0), &other.0))
    }
}

impl<T> From<T> for FileName
where
    T: Into<String>,
{
    fn from(name: T) -> Self {
        Self(name.into())
    }
}

impl Borrow<str> for FileName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A simple in-memory filesystem that supports basic file operations.
pub struct MemoryFs {
    fs_type: u32,
    inode_numbers: Mutex<Slab<()>>,
    accounting: Option<Arc<TmpfsAccounting>>,
}

static MEMFS_TYPE: FileSystemType = FileSystemType::internal("memfs");

impl MemoryFs {
    /// Creates an in-memory superblock.
    #[expect(
        clippy::new_ret_no_self,
        reason = "CG-024-lint-exceptions: clippy::new_ret_no_self: MemFs construction returns the \
                  mounted SuperBlock that owns the filesystem"
    )]
    pub fn new() -> Arc<SuperBlock> {
        Self::new_with_superblock_flags(SuperBlockFlags::empty())
    }

    /// Creates an in-memory superblock with explicit VFS-wide flags.
    pub fn new_with_superblock_flags(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
        Self::new_with_name_superblock_flags_and_root_mode(
            &MEMFS_TYPE,
            RAMFS_MAGIC,
            superblock_flags,
            NodePermission::from_bits_truncate(0o755),
        )
    }

    pub(crate) fn new_with_name_superblock_flags_and_root_mode(
        file_system_type: &'static FileSystemType,
        fs_type: u32,
        superblock_flags: SuperBlockFlags,
        root_mode: NodePermission,
    ) -> Arc<SuperBlock> {
        Self::new_with_name_superblock_flags_root_metadata_and_accounting(
            file_system_type,
            fs_type,
            superblock_flags,
            root_mode,
            0,
            0,
            None,
        )
    }

    pub(crate) fn new_with_name_superblock_flags_root_metadata_and_accounting(
        file_system_type: &'static FileSystemType,
        fs_type: u32,
        superblock_flags: SuperBlockFlags,
        root_mode: NodePermission,
        root_uid: u32,
        root_gid: u32,
        accounting: Option<Arc<TmpfsAccounting>>,
    ) -> Arc<SuperBlock> {
        let fs = Arc::new(Self {
            fs_type,
            inode_numbers: Mutex::new(Slab::new()),
            accounting,
        });
        SuperBlock::new_with_flags_and_private(
            file_system_type,
            &MEMORY_SUPER_OPERATIONS,
            fs.clone(),
            superblock_flags,
            memaddr::PAGE_SIZE_4K as u64,
            kvfs::MAX_LFS_FILESIZE,
            |super_block| {
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: A fresh filesystem has no \
                              charged inodes and its validated limit is either unlimited or at \
                              least one, so it always admits the root"
                )]
                let root_ino = Inode::new(
                    &fs,
                    None,
                    NodeType::Directory,
                    root_mode,
                    root_uid,
                    root_gid,
                    DeviceId::default(),
                )
                .expect("a fresh filesystem admits its root inode");
                let root_inode = MemoryNode::vfs_inode_from_inode(super_block, root_ino);
                Dentry::new_dir_from_inode(root_inode, None, String::new())
            },
        )
    }

    fn reserve_inode(&self) -> VfsResult<()> {
        if let Some(accounting) = self.accounting.as_ref() {
            accounting.reserve_inode()?;
        }
        Ok(())
    }

    fn free_inode(&self) {
        if let Some(accounting) = self.accounting.as_ref() {
            accounting.free_inode();
        }
    }
}

struct MemorySuperOperations;

static MEMORY_SUPER_OPERATIONS: MemorySuperOperations = MemorySuperOperations;

impl SuperBlockOperations for MemorySuperOperations {
    fn timestamp_limits(&self, _super_block: &SuperBlock) -> kvfs::TimestampLimits {
        kvfs::TimestampLimits::NANOSECOND
    }

    fn statfs(&self, super_block: &SuperBlock) -> VfsResult<StatFs> {
        let fs = super_block.private::<Arc<MemoryFs>>()?;
        let mut stat = simple_statfs(fs.fs_type);
        stat.block_size =
            u32::try_from(super_block.block_size()).map_err(|_| VfsError::InvalidInput)?;
        if let Some(quota) = fs.accounting.as_ref() {
            quota.statfs(&mut stat);
        }
        Ok(stat)
    }
}

#[derive(Default)]
struct FileContent {
    symlink: Mutex<Option<String>>,
}

#[derive(Default)]
struct DirContent {
    entries: Mutex<HashMap<FileName, InodeRef>>,
}

enum InodeContent {
    File(FileContent),
    Dir(DirContent),
}

#[derive(Default)]
enum InodePrivate {
    #[default]
    None,
    Shmem(Arc<shmem::ShmemObjectState>),
}

struct Inode {
    fs: Arc<MemoryFs>,
    ino: u64,
    metadata: Mutex<Metadata>,
    content: Option<InodeContent>,
    private: Mutex<InodePrivate>,
}

impl Inode {
    pub(crate) fn new(
        fs: &Arc<MemoryFs>,
        parent: Option<&Arc<Inode>>,
        node_type: NodeType,
        permission: NodePermission,
        uid: u32,
        gid: u32,
        rdev: DeviceId,
    ) -> VfsResult<Arc<Inode>> {
        fs.reserve_inode()?;
        let mut inode_numbers = fs.inode_numbers.lock();
        let entry = inode_numbers.vacant_entry();
        let ino = entry.key() as u64 + 1;
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 0,
            mode: Umode::new(node_type, permission),
            uid,
            gid,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev,
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        };
        let content = match node_type {
            NodeType::Directory => Some(InodeContent::Dir(DirContent::default())),
            NodeType::RegularFile | NodeType::Symlink => {
                Some(InodeContent::File(FileContent::default()))
            }
            _ => None,
        };
        let result = Arc::new(Self {
            fs: fs.clone(),
            ino,
            metadata: Mutex::new(metadata),
            content,
            private: Mutex::default(),
        });
        entry.insert(());
        drop(inode_numbers);
        if let Some(InodeContent::Dir(dir)) = &result.content {
            let mut entries = dir.entries.lock();
            entries.insert(".".into(), InodeRef::new(result.clone()));
            entries.insert(
                "..".into(),
                InodeRef::new(parent.unwrap_or(&result).clone()),
            );
        }
        Ok(result)
    }

    fn as_file(&self) -> VfsResult<&FileContent> {
        match &self.content {
            Some(InodeContent::File(content)) => Ok(content),
            Some(InodeContent::Dir(_)) => Err(VfsError::IsADirectory),
            None => Err(VfsError::InvalidInput),
        }
    }

    fn as_dir(&self) -> VfsResult<&DirContent> {
        match &self.content {
            Some(InodeContent::Dir(content)) => Ok(content),
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn shmem_state(&self) -> Option<Arc<shmem::ShmemObjectState>> {
        match &*self.private.lock() {
            InodePrivate::Shmem(state) => Some(state.clone()),
            InodePrivate::None => None,
        }
    }

    fn attach_shmem_state(
        &self,
        state: Arc<shmem::ShmemObjectState>,
    ) -> Arc<shmem::ShmemObjectState> {
        let mut private = self.private.lock();
        if let InodePrivate::Shmem(existing) = &*private {
            return existing.clone();
        }
        *private = InodePrivate::Shmem(state.clone());
        state
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        // Namespace entries and VFS operation objects own the backing inode.
        // Keep its number reserved until their final reference is released.
        self.fs.inode_numbers.lock().remove(self.ino as usize - 1);
        self.fs.free_inode();
    }
}

/// Owns one directory entry's link; any additional-link charge must be reserved
/// before construction. Temporary inode references must use `Arc<Inode>`.
struct InodeRef {
    inode: Arc<Inode>,
}

impl InodeRef {
    pub(crate) fn new(inode: Arc<Inode>) -> Self {
        inode.metadata.lock().nlink += 1;
        Self { inode }
    }

    fn get(&self) -> Arc<Inode> {
        self.inode.clone()
    }
}

impl Drop for InodeRef {
    fn drop(&mut self) {
        let mut metadata = self.inode.metadata.lock();
        // The first name shares the inode's allocation charge. Extra names
        // carry separate charges; directory `.` and `..` links never do.
        if metadata.nlink > 1 && metadata.mode.node_type() != NodeType::Directory {
            self.inode.fs.free_inode();
        }
        metadata.nlink -= 1;
    }
}

struct MemoryNode {
    inode: Arc<Inode>,
}

impl MemoryNode {
    pub(crate) fn new(inode: Arc<Inode>) -> Arc<Self> {
        Arc::new(Self { inode })
    }

    fn vfs_inode_from_inode(
        super_block: &Arc<SuperBlock>,
        inode: Arc<Inode>,
    ) -> Arc<kvfs::VfsInode> {
        let metadata = inode.metadata.lock();
        let node_type = metadata.mode.node_type();
        let init = VfsInodeInit::from_metadata(&metadata);
        drop(metadata);
        super_block.get_or_init_inode(init.inode_number(), || match node_type {
            NodeType::Directory => VfsInode::new_openable_dir_with_flags(
                MemoryNode::new(inode),
                NodeFlags::empty(),
                init,
            ),
            NodeType::RegularFile => VfsInode::new_file_with_address_space_and_flags(
                MemoryNode::new(inode),
                NodeFlags::ALWAYS_CACHE,
                &MEMORY_ADDRESS_SPACE_OPERATIONS,
                init,
            ),
            NodeType::Symlink => VfsInode::new_symlink_with_address_space_and_flags(
                MemoryNode::new(inode),
                NodeFlags::empty(),
                &MEMORY_ADDRESS_SPACE_OPERATIONS,
                init,
            ),
            _ => VfsInode::new_special(MemoryNode::new(inode), NodeFlags::empty(), init),
        })
    }

    fn get_inode(
        &self,
        dir: &kvfs::VfsInode,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<Arc<Inode>> {
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        Inode::new(
            &self.inode.fs,
            Some(&self.inode),
            mode.node_type(),
            mode.permission(),
            uid,
            gid,
            device,
        )
    }

    fn create_node(
        &self,
        dir_inode: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<()> {
        let super_block = dir_inode.super_block()?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = self.get_inode(dir_inode, mode, device, cred)?;
        entries.insert(name.into(), InodeRef::new(inode.clone()));
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&super_block, inode);
        // VFS keeps the dentry negative under the parent namespace lock, and
        // this fresh inode has no directory alias. Errors would violate that
        // publication contract; retain defensive rollback.
        if let Err(error) = dentry.instantiate(inode.clone()) {
            content.entries.lock().remove(name);
            return Err(error);
        }
        Ok(())
    }

    fn remove_dir_links(inode: &Arc<Inode>) {
        if let Some(InodeContent::Dir(dir)) = &inode.content {
            dir.entries.lock().clear();
        }
    }

    fn reparent_dir(inode: &Arc<Inode>, new_parent: &Arc<Inode>) {
        if let Some(InodeContent::Dir(dir)) = &inode.content {
            dir.entries
                .lock()
                .insert("..".into(), InodeRef::new(new_parent.clone()));
        }
    }

    fn node_type(&self) -> NodeType {
        self.inode.metadata.lock().mode.node_type()
    }
}

impl InodeOperations for MemoryNode {
    fn cache_residency(&self) -> Option<Arc<dyn CacheResidency>> {
        self.inode
            .fs
            .accounting
            .as_ref()
            .map(|quota| quota.clone() as Arc<dyn CacheResidency>)
    }

    fn set_len(&self, vfs_inode: &VfsInode, len: u64) -> VfsResult<()> {
        self.inode.as_file()?;
        self.inode.metadata.lock().size = len;
        vfs_inode.truncate_setsize(len)
    }

    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        if self.node_type() == NodeType::Directory {
            Some(self)
        } else {
            None
        }
    }

    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        if self.node_type() == NodeType::Symlink {
            Some(self)
        } else {
            None
        }
    }

    fn getattr(
        &self,
        idmap: &kvfs::MountIdmap,
        path: Option<&kvfs::Path>,
        request_mask: kvfs::GetattrRequestMask,
        query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        if path.is_some() {
            return simple_getattr(idmap, path, request_mask, query_flags);
        }
        let mut metadata = self.inode.metadata.lock().clone();
        if let Some(InodeContent::Dir(dir)) = self.inode.content.as_ref() {
            metadata.size = dir.entries.lock().len() as u64;
        }
        Ok(metadata)
    }

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        let mut metadata = self.inode.metadata.lock();
        if let Some(size) = update.size {
            self.inode.as_file()?;
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

impl InodeSymlinkOperations for MemoryNode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &kvfs::VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        let file = self.inode.as_file()?;
        file.symlink.lock().clone().ok_or(VfsError::InvalidData)
    }
}

impl InodeDirOperations for MemoryNode {
    fn lookup(
        &self,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let super_block = dir.super_block()?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let entries = content.entries.lock();

        let Some(entry) = entries.get(name) else {
            return Ok(None);
        };
        let inode = entry.get();
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&super_block, inode);
        dentry.instantiate_or_alias(inode)
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        let mode = mode.with_node_type(NodeType::RegularFile);
        self.create_node(dir, dentry, mode, DeviceId::default(), cred)
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir_inode: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        cred: &Cred,
    ) -> VfsResult<()> {
        let mode = mode.with_node_type(NodeType::Directory);
        self.create_node(dir_inode, dentry, mode, DeviceId::default(), cred)?;
        dir_inode.increment_link_count();
        Ok(())
    }

    fn mknod(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.create_node(dir, dentry, mode, device, cred)
    }

    fn symlink(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir_inode: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<()> {
        let super_block = dir_inode.super_block()?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let (mode, uid, gid) = inode_init_owner(
            dir_inode,
            Umode::new(NodeType::Symlink, NodePermission::from_bits_truncate(0o777)),
            cred,
        );
        let inode = Inode::new(
            &self.inode.fs,
            Some(&self.inode),
            mode.node_type(),
            mode.permission(),
            uid,
            gid,
            DeviceId::default(),
        )?;
        let file = inode.as_file()?;
        *file.symlink.lock() = Some(target.to_owned());
        inode.metadata.lock().size = target.len() as u64;
        entries.insert(name.into(), InodeRef::new(inode.clone()));
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&super_block, inode);
        // VFS keeps the dentry negative under the parent namespace lock, and
        // symlinks allow multiple aliases. Rollback only guards contract errors.
        if let Err(error) = dentry.instantiate(inode.clone()) {
            content.entries.lock().remove(name);
            return Err(error);
        }
        Ok(())
    }

    fn link(
        &self,
        target_dentry: &Dentry,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        let target = target_dentry.downcast::<Self>()?;

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = target.inode.clone();
        // VFS has already rejected directory sources. The inode's allocation
        // pays for its first name; only additional links need another unit.
        if inode.metadata.lock().nlink != 0 {
            self.inode.fs.reserve_inode()?;
        }
        entries.insert(name.into(), InodeRef::new(inode.clone()));
        target_dentry.increment_link_count();
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&super_block, inode);
        // VFS keeps the destination negative under the parent namespace lock
        // and rejects directory sources. Rollback only guards contract errors.
        if let Err(error) = dentry.instantiate(inode) {
            target_dentry.decrement_link_count();
            content.entries.lock().remove(name);
            return Err(error);
        }
        Ok(())
    }

    fn unlink(&self, dir_inode: &kvfs::VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let name = dentry.name();
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        let Some(entry) = entries.get(name) else {
            return Err(VfsError::NotFound);
        };
        let inode = entry.get();
        if let Some(InodeContent::Dir(DirContent { entries })) = &inode.content {
            if entries.lock().len() > 2 {
                return Err(VfsError::DirectoryNotEmpty);
            }
            Self::remove_dir_links(&inode);
            dentry.decrement_link_count();
            dir_inode.decrement_link_count();
        }
        entries.remove(name);
        dentry.decrement_link_count();

        Ok(())
    }

    fn rename(
        &self,
        idmap: &kvfs::MountIdmap,
        old_dir_inode: &kvfs::VfsInode,
        old_dentry: &LockedDentry<'_>,
        new_dir_inode: &kvfs::VfsInode,
        new_dentry: &LockedDentry<'_>,
        flags: kvfs::RenameFlags,
    ) -> VfsResult<()> {
        let old_dir = old_dentry.parent().ok_or(VfsError::InvalidInput)?;
        let dst_dir = new_dentry.parent().ok_or(VfsError::InvalidInput)?;
        let src_name = old_dentry.name();
        let dst_name = new_dentry.name();
        let dst_node = dst_dir.downcast::<Self>()?;
        let replaced_dir =
            if new_dentry.is_really_positive() && new_dentry.node_type() == NodeType::Directory {
                Some(new_dentry.downcast::<Self>()?.inode.clone())
            } else {
                None
            };

        simple_rename(
            idmap,
            old_dir_inode,
            old_dentry,
            new_dir_inode,
            new_dentry,
            flags,
        )?;
        if let Some(inode) = replaced_dir {
            Self::remove_dir_links(&inode);
        }
        let src_entry = self
            .inode
            .as_dir()?
            .entries
            .lock()
            .remove(src_name)
            .ok_or(VfsError::NotFound)?;
        let moved_inode = src_entry.get();
        if moved_inode.metadata.lock().mode.node_type() == NodeType::Directory
            && old_dir.inode() != dst_dir.inode()
        {
            Self::reparent_dir(&moved_inode, &dst_node.inode);
        }
        dst_node
            .inode
            .as_dir()?
            .entries
            .lock()
            .insert(dst_name.into(), src_entry);
        Ok(())
    }
}

// Both ordinary reads and native copies use this filesystem-owned algorithm:
// holes supply zeroes without creating source pages or changing the quota.
fn read_memory_file(iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
    let inode = iocb.file().inode();
    let mapping = iocb.file().mapping();
    let mut offset = iocb.ki_pos();
    let mut total = 0;
    while iter.count() != 0 {
        let file_len = inode.size();
        if offset >= file_len {
            break;
        }
        let page_offset = (offset % memaddr::PAGE_SIZE_4K as u64) as usize;
        let count = (file_len - offset)
            .min(iter.count() as u64)
            .saturating_add(page_offset as u64)
            .div_ceil(memaddr::PAGE_SIZE_4K as u64)
            .min(pagecache::FolioBatch::CAPACITY as u64) as usize;
        let mut is_done = false;
        let result = mapping.visit_resident_folios(
            offset / memaddr::PAGE_SIZE_4K as u64,
            count,
            |index, folio| {
                debug_assert_eq!(index, offset / memaddr::PAGE_SIZE_4K as u64);
                let page_offset = (offset % memaddr::PAGE_SIZE_4K as u64) as usize;
                let step = inode
                    .size()
                    .saturating_sub(offset)
                    .min((memaddr::PAGE_SIZE_4K - page_offset) as u64)
                    as usize;
                let copied = match folio {
                    Some(folio) => {
                        iter.copy_to_iter(&folio.data()[page_offset..page_offset + step])?
                    }
                    None => iter.copy_to_iter(&ZERO_PAGE[..step])?,
                };
                offset += copied as u64;
                total += copied;
                is_done = copied < memaddr::PAGE_SIZE_4K - page_offset || iter.count() == 0;
                Ok(if is_done {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            },
        );
        match result {
            Err(error) if total == 0 => return Err(error),
            Err(_) => break,
            Ok(_) if is_done => break,
            Ok(_) => {}
        }
    }
    iocb.advance(total);
    Ok(total)
}

/// The memfs native chunk copy: reads source data through the sparse memfs
/// reader and writes it into `output` storage. Both
/// sides are already memfs inodes — the caller downcasts before coming here
/// — so the copy never touches the generic buffered pump.
fn move_copy_chunk(
    input: &VfsFile,
    input_pos: u64,
    output: &VfsFile,
    output_pos: u64,
    len: usize,
) -> KResult<usize> {
    let mut buf = alloc::vec![0u8; len];
    let read = {
        let mut iocb = Kiocb::new(input, input_pos);
        let mut iter = iov_iter_kvec_dest(&mut buf);
        read_memory_file(&mut iocb, &mut iter)?
    };
    if read == 0 {
        return Ok(0);
    }
    let mut iocb = Kiocb::new(output, output_pos);
    let mut iter = iov_iter_kvec_source(&buf[..read]);
    let written = iocb.generic_file_write_iter(&mut iter)?;
    Ok(written)
}

impl FileOperations for MemoryNode {
    fn fsync(&self, _file: &VfsFile, _data_only: bool) -> VfsResult<()> {
        // Memory-backed files and directories have no persistent writeback.
        // Special inodes install their own operation tables during VFS open.
        Ok(())
    }

    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        if matches!(self.inode.content.as_ref(), Some(InodeContent::Dir(_))) {
            Some(self)
        } else {
            None
        }
    }

    fn supports_read(&self) -> bool {
        matches!(
            self.inode.content.as_ref(),
            Some(InodeContent::File(_)) | Some(InodeContent::Dir(_))
        )
    }

    fn copy_chunk_from(
        &self,
        input: &VfsFile,
        input_pos: u64,
        output: &VfsFile,
        output_pos: u64,
        len: usize,
    ) -> Option<KResult<usize>> {
        // The memfs implementation serves memfs storage only; any other
        // input store is refused here, not by a VFS identity check. The
        // judgement precedes the empty-range early return so the VFS
        // admission probe sees the refusal too.
        if input.path().inode().downcast::<MemoryNode>().is_err() {
            return Some(Err(KError::CrossesDevices));
        }
        if len == 0 {
            return Some(Ok(0));
        }
        Some(move_copy_chunk(input, input_pos, output, output_pos, len))
    }

    fn supports_write(&self) -> bool {
        matches!(self.inode.content.as_ref(), Some(InodeContent::File(_)))
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let file = self.inode.as_file()?;
        if let Some(symlink) = file.symlink.lock().as_ref() {
            assert_eq!(offset, 0);
            let len = buf.len().min(symlink.len());
            buf[..len].copy_from_slice(&symlink.as_bytes()[..len]);
            return Ok(len);
        }
        unreachable!("page cache should dispatch reading");
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let file = self.inode.as_file()?;
        if let Some(symlink) = file.symlink.lock().as_ref() {
            let offset = usize::try_from(iocb.ki_pos()).map_err(|_| VfsError::InvalidInput)?;
            if offset >= symlink.len() {
                return Ok(0);
            }
            let src = &symlink.as_bytes()[offset..];
            let copied = iter.copy_to_iter(&src[..src.len().min(iter.count())])?;
            iocb.advance(copied);
            return Ok(copied);
        }
        read_memory_file(iocb, iter)
    }

    fn write(&self, _file: &VfsFile, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        match self.inode.content.as_ref() {
            Some(InodeContent::Dir(_)) => Err(VfsError::IsADirectory),
            None => Err(VfsError::InvalidInput),
            Some(InodeContent::File(_)) => {
                unreachable!("page cache should dispatch writing");
            }
        }
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        self.inode.as_file()?;
        iocb.generic_file_write_iter(iter)
    }
}

impl FileDirOperations for MemoryNode {
    fn iterate_shared(&self, _file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let mut count = 0;
        let offset = ctx.pos();
        for (i, (name, entry)) in self
            .inode
            .as_dir()?
            .entries
            .lock()
            .iter()
            .enumerate()
            .skip(offset as usize)
        {
            if !ctx.emit(
                &name.0,
                entry.inode.ino,
                entry.get().metadata.lock().mode.node_type(),
                i as u64 + 1,
            ) {
                return Ok(count);
            }
            count += 1;
        }
        Ok(count)
    }
}

struct MemoryAddressSpaceOperations;

static MEMORY_ADDRESS_SPACE_OPERATIONS: MemoryAddressSpaceOperations = MemoryAddressSpaceOperations;

impl AddressSpaceOperations for MemoryAddressSpaceOperations {
    fn read_folio(&self, _mapping: &AddressSpace, folio: &mut FolioGuard<'_>) -> VfsResult<()> {
        let data = folio.data();
        data.fill(0);
        Ok(())
    }

    fn writepages(
        &self,
        _mapping: &AddressSpace,
        _control: &mut kvfs::WritebackControl,
    ) -> VfsResult<()> {
        Ok(())
    }

    fn write_begin(&self, mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        vfs_inode.private::<MemoryNode>()?.inode.as_file()?;
        Ok(())
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let copied = simple_write_end(mapping, request)?;
        if copied != 0 {
            let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
            inode.private::<MemoryNode>()?.inode.metadata.lock().size = inode.size();
        }
        Ok(copied)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
unittest::path_mod_test!(inode_quota_tests, "../ut-tests/inode_quota.rs");

impl Drop for MemoryNode {
    fn drop(&mut self) {
        if let Some(InodeContent::Dir(dir)) = &self.inode.content {
            dir.entries.lock().clear();
        }
    }
}
