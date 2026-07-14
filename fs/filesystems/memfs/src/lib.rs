// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! In-memory filesystem implementation for the VFS layer.

#![no_std]

extern crate alloc;

pub mod ramfs;
pub mod shmem;

use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::{borrow::Borrow, cmp::Ordering, time::Duration};

use hashbrown::HashMap;
use iov_iter::{IovIterDest, IovIterSource};
use ksync::Mutex;
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FileDirOperations,
    FileOperations, InodeCache, InodeDirOperations, InodeOperations, InodeSymlinkOperations, Kiocb,
    LockedDentry, Metadata, MetadataUpdate, NodeFlags, NodePermission, NodeType, StatFs,
    StatFsFlags, SuperBlock, SuperBlockOperations, Umode, VfsError, VfsFile, VfsInodeInit,
    VfsResult, WriteBeginRequest, WriteEndRequest, simple_getattr, simple_rename,
    simple_statfs_with_flags, simple_write_end,
};
use slab::Slab;

pub(crate) const RAMFS_MAGIC: u32 = 0x8584_58f6;
pub(crate) const TMPFS_MAGIC: u32 = 0x0102_1994;

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
    mount_flags: StatFsFlags,
    inodes: Mutex<Slab<Arc<Inode>>>,
    inode_cache: InodeCache,
}

impl MemoryFs {
    /// Creates an in-memory superblock.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<SuperBlock> {
        Self::new_with_name_and_flags("memfs", StatFsFlags::empty())
    }

    /// Creates an in-memory superblock with explicit mount flags.
    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_flags(mount_flags: StatFsFlags) -> Arc<SuperBlock> {
        Self::new_with_name_and_flags("memfs", mount_flags)
    }

    /// Creates an in-memory superblock with a custom name and mount flags.
    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_name_and_flags(
        name: &'static str,
        mount_flags: StatFsFlags,
    ) -> Arc<SuperBlock> {
        Self::new_with_name_flags_and_root_mode(
            name,
            RAMFS_MAGIC,
            mount_flags,
            NodePermission::from_bits_truncate(0o755),
        )
    }

    pub(crate) fn new_with_name_flags_and_root_mode(
        name: &'static str,
        fs_type: u32,
        mount_flags: StatFsFlags,
        root_mode: NodePermission,
    ) -> Arc<SuperBlock> {
        let fs = Arc::new(Self {
            name,
            fs_type,
            mount_flags,
            inodes: Mutex::new(Slab::new()),
            inode_cache: InodeCache::new(),
        });
        let root_ino = Inode::new(
            &fs,
            None,
            NodeType::Directory,
            root_mode,
            DeviceId::default(),
        );
        let root = MemoryNode::dentry_from_inode(&fs, None, String::new(), root_ino);
        SuperBlock::new(fs, root)
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
        Ok(simple_statfs_with_flags(self.fs_type, self.mount_flags))
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
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev,
            atime: Duration::default(),
            mtime: Duration::default(),
            ctime: Duration::default(),
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

    fn dentry_from_inode(
        fs: &Arc<MemoryFs>,
        d_parent: Option<Dentry>,
        d_name: String,
        inode: Arc<Inode>,
    ) -> Dentry {
        let metadata = inode.metadata.lock();
        let node_type = metadata.mode.node_type();
        let init = VfsInodeInit::from_metadata(&metadata);
        drop(metadata);
        let vfs_inode = match node_type {
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
        };
        if node_type == NodeType::Directory {
            Dentry::new_dir_from_inode(vfs_inode, d_parent, d_name)
        } else {
            Dentry::new_file_from_inode(vfs_inode, d_parent, d_name)
        }
    }

    fn new_entry(&self, parent: &Dentry, name: &str, inode: Arc<Inode>) -> Dentry {
        Self::dentry_from_inode(&self.fs, Some(parent.clone()), name.to_owned(), inode)
    }

    fn ramfs_get_inode(
        &self,
        parent: Option<u64>,
        mode: kvfs::Umode,
        device: DeviceId,
    ) -> Arc<Inode> {
        Inode::new(
            &self.fs,
            parent,
            mode.node_type(),
            mode.permission(),
            device,
        )
    }

    fn ramfs_mknod(
        &self,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
    ) -> VfsResult<Dentry> {
        let dir = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = self.ramfs_get_inode(Some(self.inode.ino), mode, device);
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        drop(entries);
        Ok(self.new_entry(&dir, name, inode))
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
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let entries = content.entries.lock();

        let entry = entries.get(name).ok_or(VfsError::NotFound)?;
        let inode = entry.get();
        Ok(self.new_entry(&parent, name, inode))
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
    ) -> VfsResult<Dentry> {
        let mode = mode.with_node_type(NodeType::RegularFile);
        self.ramfs_mknod(dentry, mode, DeviceId::default())
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir_inode: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
    ) -> VfsResult<Dentry> {
        let mode = mode.with_node_type(NodeType::Directory);
        let entry = self.ramfs_mknod(dentry, mode, DeviceId::default())?;
        dir_inode.increment_link_count();
        Ok(entry)
    }

    fn mknod(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
    ) -> VfsResult<Dentry> {
        self.ramfs_mknod(dentry, mode, device)
    }

    fn symlink(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
    ) -> VfsResult<Dentry> {
        let dir = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let content = self.inode.as_dir()?;
        let mut entries = content.entries.lock();

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = Inode::new(
            &self.fs,
            Some(self.inode.ino),
            NodeType::Symlink,
            NodePermission::from_bits_truncate(0o777),
            DeviceId::default(),
        );
        let file = inode.as_file()?;
        *file.symlink.lock() = Some(target.to_owned());
        inode.metadata.lock().size = target.len() as u64;
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        Ok(self.new_entry(&dir, name, inode))
    }

    fn link(
        &self,
        target_dentry: &Dentry,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<Dentry> {
        let dir = dentry.parent().ok_or(VfsError::InvalidInput)?;
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
        Ok(self.new_entry(&dir, name, inode))
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

    fn set_len(&self, _mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        self.inode.as_file()?;
        self.inode.metadata.lock().size = len;
        Ok(())
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
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn tmpfs_write_end_updates_vfs_inode_size() {
        let fs = shmem::new_tmpfs(StatFsFlags::empty());
        let root = kvfs::Path::new(kvfs::Mount::new_root(&fs), fs.root_dir());
        let file = kvfs::Filename::new("tmp")
            .open_with_flags_at(
                &root,
                &root,
                O_WRONLY | O_CREAT | O_EXCL,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let mut pos = 0;

        assert_eq!(file.write_from(b"hello", &mut pos).unwrap(), 5);
        assert_eq!(file.path().getattr().unwrap().size, 5);
        assert_eq!(file.inode().size(), 5);
    }

    #[def_test]
    fn rename_rejects_nonempty_directory_after_child_handle_is_dropped() {
        let fs = MemoryFs::new();
        let root = fs.root_dir();
        let permission = NodePermission::from_bits_truncate(0o755);
        let source = root.mkdir("source", permission).unwrap();
        let target = root.mkdir("target", permission).unwrap();
        let child = target.create("child", permission).unwrap();
        drop(child);
        drop(source);

        assert_eq!(
            root.rename("source", &root, "target", kvfs::RenameFlags::empty()),
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
