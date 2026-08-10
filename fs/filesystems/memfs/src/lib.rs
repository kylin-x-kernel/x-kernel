// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! In-memory filesystem implementation for the VFS layer.

#![no_std]

extern crate alloc;

pub mod ramfs;
pub mod shmem;

use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::{borrow::Borrow, cmp::Ordering};

use hashbrown::HashMap;
use iov_iter::{IovIterDest, IovIterSource};
use kcred::Cred;
use klazy::Once;
use ksync::Mutex;
use ktime_types::SystemTime;
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FileDirOperations,
    FileOperations, FileSystemType, FsContext, InodeCache, InodeDirOperations, InodeOperations,
    InodeSymlinkOperations, Kiocb, LockedDentry, Metadata, MetadataUpdate, Mount, NodeFlags,
    NodePermission, NodeType, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations, Umode,
    VfsError, VfsFile, VfsInodeInit, VfsResult, WriteBeginRequest, WriteEndRequest, get_tree_nodev,
    inode_init_owner,
    libfs::{simple_getattr, simple_rename, simple_statfs, simple_write_end},
};
use slab::Slab;

pub(crate) const RAMFS_MAGIC: u32 = 0x8584_58f6;
pub(crate) const TMPFS_MAGIC: u32 = 0x0102_1994;
static SYSFS: Once<(Arc<SuperBlock>, Arc<Mount>)> = Once::new();

fn tmpfs_get_tree(
    context: &FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |flags| Ok(shmem::new_tmpfs(flags)))
}

fn sysfs_get_tree(
    context: &FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |flags| Ok(new_sysfs(flags)))
}

/// Registered tmpfs filesystem type.
pub const TMPFS_TYPE: FileSystemType = FileSystemType::nodev("tmpfs", tmpfs_get_tree);

/// Registered sysfs filesystem type.
pub const SYSFS_TYPE: FileSystemType = FileSystemType::nodev("sysfs", sysfs_get_tree);

/// Returns the shared sysfs superblock.
///
/// The singleton retains an internal root mount because the directory tree is
/// owned by this superblock rather than by a separate kernfs root. Visible
/// mounts therefore cannot tear down the shared kernel-populated tree.
pub fn new_sysfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    let (super_block, _internal_mount) = SYSFS.call_once(|| {
        let super_block =
            ramfs::new_ramfs_with_name_and_superblock_flags("sysfs", superblock_flags);
        let internal_mount = Mount::new_root(&super_block);
        (super_block, internal_mount)
    });
    super_block.clone()
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
    name: &'static str,
    fs_type: u32,
    inodes: Mutex<Slab<Arc<Inode>>>,
    inode_cache: InodeCache,
}

impl MemoryFs {
    /// Creates an in-memory superblock.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<SuperBlock> {
        Self::new_with_name_and_superblock_flags("memfs", SuperBlockFlags::empty())
    }

    /// Creates an in-memory superblock with explicit VFS-wide flags.
    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_superblock_flags(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
        Self::new_with_name_and_superblock_flags("memfs", superblock_flags)
    }

    /// Creates an in-memory superblock with a custom name and VFS-wide flags.
    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_name_and_superblock_flags(
        name: &'static str,
        superblock_flags: SuperBlockFlags,
    ) -> Arc<SuperBlock> {
        Self::new_with_name_superblock_flags_and_root_mode(
            name,
            RAMFS_MAGIC,
            superblock_flags,
            NodePermission::from_bits_truncate(0o755),
        )
    }

    pub(crate) fn new_with_name_superblock_flags_and_root_mode(
        name: &'static str,
        fs_type: u32,
        superblock_flags: SuperBlockFlags,
        root_mode: NodePermission,
    ) -> Arc<SuperBlock> {
        let fs = Arc::new(Self {
            name,
            fs_type,
            inodes: Mutex::new(Slab::new()),
            inode_cache: InodeCache::new(),
        });
        let root_ino = Inode::new(
            &fs,
            None,
            NodeType::Directory,
            root_mode,
            0,
            0,
            DeviceId::default(),
        );
        let root_inode = MemoryNode::vfs_inode_from_inode(&fs, root_ino);
        let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
        SuperBlock::new_with_flags(fs, root, superblock_flags)
    }

    fn get(&self, ino: u64) -> Arc<Inode> {
        self.inodes.lock()[ino as usize - 1].clone()
    }
}

impl SuperBlockOperations for MemoryFs {
    fn name(&self) -> &str {
        self.name
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        Ok(simple_statfs(self.fs_type))
    }
}

fn release_inode(fs: &MemoryFs, inode: &Arc<Inode>, nlink: u64) {
    let mut inodes = fs.inodes.lock();
    let mut metadata = inode.metadata.lock();
    metadata.nlink -= nlink;
    if metadata.nlink == 0 && Arc::strong_count(inode) == 2 {
        inodes.remove(metadata.inode as usize - 1);
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
    ino: u64,
    metadata: Mutex<Metadata>,
    content: Option<InodeContent>,
    private: Mutex<InodePrivate>,
}

impl Inode {
    pub fn new(
        fs: &Arc<MemoryFs>,
        parent: Option<u64>,
        node_type: NodeType,
        permission: NodePermission,
        uid: u32,
        gid: u32,
        rdev: DeviceId,
    ) -> Arc<Inode> {
        let mut inodes = fs.inodes.lock();
        let entry = inodes.vacant_entry();
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
            ino,
            metadata: Mutex::new(metadata),
            content,
            private: Mutex::default(),
        });
        entry.insert(result.clone());
        drop(inodes);
        if let Some(InodeContent::Dir(dir)) = &result.content {
            let mut entries = dir.entries.lock();
            entries.insert(".".into(), InodeRef::new(fs.clone(), ino));
            entries.insert(
                "..".into(),
                InodeRef::new(fs.clone(), parent.unwrap_or(ino)),
            );
        }
        result
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

struct InodeRef {
    fs: Arc<MemoryFs>,
    ino: u64,
}

impl InodeRef {
    pub fn new(fs: Arc<MemoryFs>, ino: u64) -> Self {
        fs.get(ino).metadata.lock().nlink += 1;
        Self { fs, ino }
    }

    fn get(&self) -> Arc<Inode> {
        self.fs.get(self.ino)
    }
}

impl Drop for InodeRef {
    fn drop(&mut self) {
        release_inode(&self.fs, &self.get(), 1);
    }
}

struct MemoryNode {
    fs: Arc<MemoryFs>,
    inode: Arc<Inode>,
}

impl MemoryNode {
    pub fn new(fs: Arc<MemoryFs>, inode: Arc<Inode>) -> Arc<Self> {
        Arc::new(Self { fs, inode })
    }

    fn vfs_inode_from_inode(fs: &Arc<MemoryFs>, inode: Arc<Inode>) -> Arc<kvfs::VfsInode> {
        let metadata = inode.metadata.lock();
        let node_type = metadata.mode.node_type();
        let init = VfsInodeInit::from_metadata(&metadata);
        drop(metadata);
        match node_type {
            NodeType::Directory => fs.inode_cache.get_or_insert_openable_dir_with_init(
                NodeFlags::empty(),
                init,
                || MemoryNode::new(fs.clone(), inode),
            ),
            NodeType::RegularFile => {
                fs.inode_cache
                    .get_or_insert_file_with_init(NodeFlags::ALWAYS_CACHE, init, || {
                        MemoryNode::new(fs.clone(), inode)
                    })
            }
            NodeType::Symlink => {
                fs.inode_cache
                    .get_or_insert_symlink_with_init(NodeFlags::empty(), init, || {
                        MemoryNode::new(fs.clone(), inode)
                    })
            }
            _ => fs
                .inode_cache
                .get_or_insert_special_with_init(NodeFlags::empty(), init, || {
                    MemoryNode::new(fs.clone(), inode)
                }),
        }
    }

    fn ramfs_get_inode(
        &self,
        parent: Option<u64>,
        dir: &kvfs::VfsInode,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> Arc<Inode> {
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        Inode::new(
            &self.fs,
            parent,
            mode.node_type(),
            mode.permission(),
            uid,
            gid,
            device,
        )
    }

    fn ramfs_mknod(
        &self,
        dir_inode: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = self.ramfs_get_inode(Some(self.inode.ino), dir_inode, mode, device, cred);
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&self.fs, inode);
        dentry.instantiate(inode)
    }

    fn remove_dir_links(inode: &Arc<Inode>) {
        if let Some(InodeContent::Dir(dir)) = &inode.content {
            dir.entries.lock().clear();
        }
    }

    fn reparent_dir(&self, inode: &Arc<Inode>, new_parent: u64) {
        if let Some(InodeContent::Dir(dir)) = &inode.content {
            dir.entries
                .lock()
                .insert("..".into(), InodeRef::new(self.fs.clone(), new_parent));
        }
    }

    fn node_type(&self) -> NodeType {
        self.inode.metadata.lock().mode.node_type()
    }
}

impl InodeOperations for MemoryNode {
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
    ) -> VfsResult<()> {
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
        Ok(())
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
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let entries = content.entries.lock();

        let Some(entry) = entries.get(name) else {
            return Ok(None);
        };
        let inode = entry.get();
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&self.fs, inode);
        dentry.instantiate_or_alias(inode)
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        let mode = mode.with_node_type(NodeType::RegularFile);
        self.ramfs_mknod(_dir, dentry, mode, DeviceId::default(), cred)
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
        self.ramfs_mknod(dir_inode, dentry, mode, DeviceId::default(), cred)?;
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
        self.ramfs_mknod(dir, dentry, mode, device, cred)
    }

    fn symlink(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir_inode: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<()> {
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
            &self.fs,
            Some(self.inode.ino),
            mode.node_type(),
            mode.permission(),
            uid,
            gid,
            DeviceId::default(),
        );
        let file = inode.as_file()?;
        *file.symlink.lock() = Some(target.to_owned());
        inode.metadata.lock().size = target.len() as u64;
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&self.fs, inode);
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        target_dentry: &Dentry,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        let target = target_dentry.downcast::<Self>()?;

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = target.inode.clone();
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        target_dentry.increment_link_count();
        drop(entries);
        let inode = Self::vfs_inode_from_inode(&self.fs, inode);
        dentry.instantiate(inode)
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
            self.reparent_dir(&moved_inode, dst_node.inode.ino);
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

impl FileOperations for MemoryNode {
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
        iocb.generic_file_read_iter(iter)
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
                entry.ino,
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

impl AddressSpaceOperations for MemoryNode {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn writepages(
        &self,
        _mapping: &AddressSpace,
        _control: &mut kvfs::WritebackControl,
    ) -> VfsResult<()> {
        Ok(())
    }

    fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        self.inode.as_file()?;
        self.inode.metadata.lock().size = len;
        mapping.truncate_setsize(len)
    }

    fn write_begin(&self, _mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        self.inode.as_file()?;
        Ok(())
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let copied = simple_write_end(mapping, request)?;
        if copied != 0 {
            let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
            self.inode.metadata.lock().size = inode.size();
        }
        Ok(copied)
    }
}

#[cfg(unittest)]
mod tests {
    use linux_raw_sys::general::{O_CREAT, O_EXCL, O_WRONLY};
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn tmpfs_readonly_policy_can_be_reconfigured() {
        let fs = shmem::new_tmpfs(SuperBlockFlags::RDONLY);

        assert!(fs.flags().contains(SuperBlockFlags::RDONLY));
        fs.reconfigure_readonly(false).unwrap();
        assert!(!fs.flags().contains(SuperBlockFlags::RDONLY));
        fs.reconfigure_readonly(true).unwrap();
        assert!(fs.flags().contains(SuperBlockFlags::RDONLY));
    }

    #[def_test]
    fn tmpfs_write_end_updates_vfs_inode_size() {
        let fs = shmem::new_tmpfs(SuperBlockFlags::empty());
        let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
        let file = kvfs::Filename::new("tmp")
            .open_with_flags_at(
                &root,
                &root,
                O_WRONLY | O_CREAT | O_EXCL,
                NodePermission::from_bits_truncate(0o600),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();
        let mut pos = 0;

        assert_eq!(file.write_from(b"hello", &mut pos).unwrap(), 5);
        assert_eq!(file.path().getattr().unwrap().size, 5);
        assert_eq!(file.inode().size(), 5);
    }

    #[def_test]
    fn created_inode_uses_fs_ids_and_inherits_setgid_directory() {
        let fs = MemoryFs::new();
        let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
        root.chmod(
            NodePermission::from_bits_truncate(0o777),
            &kcred::initial_cred(),
        )
        .unwrap();

        let mut cred = Cred::root();
        cred.set_resgid(Some(100), Some(200), Some(300)).unwrap();
        assert_eq!(cred.set_fsgid(300), 200);
        cred.set_resuid(Some(1000), Some(2000), Some(3000)).unwrap();
        assert_eq!(cred.set_fsuid(3000), 2000);

        let file = root
            .create("owned", NodePermission::from_bits_truncate(0o600), &cred)
            .unwrap();
        let file_metadata = file.metadata();
        assert_eq!(file_metadata.uid, 3000);
        assert_eq!(file_metadata.gid, 300);

        root.chown(None, Some(4242), &kcred::initial_cred())
            .unwrap();
        root.chmod(
            NodePermission::from_bits_truncate(0o2777),
            &kcred::initial_cred(),
        )
        .unwrap();
        let dir = root
            .mkdir(
                "inherited",
                NodePermission::from_bits_truncate(0o755),
                &cred,
            )
            .unwrap();
        let dir_metadata = dir.metadata();
        assert_eq!(dir_metadata.uid, 3000);
        assert_eq!(dir_metadata.gid, 4242);
        assert!(
            dir_metadata
                .mode
                .permission()
                .contains(NodePermission::SET_GID)
        );
    }

    #[def_test]
    fn rename_rejects_nonempty_directory_after_child_handle_is_dropped() {
        let fs = MemoryFs::new();
        let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
        let permission = NodePermission::from_bits_truncate(0o755);
        let cred = kcred::initial_cred();
        let source = root.mkdir("source", permission, &cred).unwrap();
        let target = root.mkdir("target", permission, &cred).unwrap();
        let child = target.create("child", permission, &cred).unwrap();
        drop(child);
        drop(source);

        assert_eq!(
            root.rename("source", &root, "target", kvfs::RenameFlags::empty(), &cred,),
            Err(VfsError::DirectoryNotEmpty)
        );
    }
}

impl Drop for MemoryNode {
    fn drop(&mut self) {
        if let Some(InodeContent::Dir(dir)) = &self.inode.content {
            dir.entries.lock().clear();
        }
        release_inode(&self.fs, &self.inode, 0);
    }
}
