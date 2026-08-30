// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple directory helpers for the in-kernel VFS.

use alloc::{
    borrow::{Cow, ToOwned},
    boxed::Box,
    collections::btree_map::BTreeMap,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::any::Any;

use ksync::Mutex;

use crate::{
    Dentry, DeviceId, DirContext, DirEntrySink, FileDirOperations, FileOperations,
    InodeDirOperations, InodeOperations, LockedDentry, Metadata, MetadataUpdate, NodeFlags,
    NodePermission, NodeType, SeqFileInode, Umode, VfsError, VfsFile, VfsInode, VfsResult,
    inode_init_owner,
    libfs::{generic_read_dir, noop_fsync},
    path::{DOT, DOTDOT},
    simple_fs::{SimpleFs, SimpleFsNode},
};

/// Factory used by simple directories to materialize a child dentry.
pub(crate) type DirEntryFactory =
    Arc<dyn Fn(Option<Dentry>, &str) -> VfsResult<Dentry> + Send + Sync>;

/// A callback that materializes a simple directory inode.
pub type DirMaker = Arc<dyn Fn() -> Arc<VfsInode> + Send + Sync>;

/// Opaque child entry installed into a simple directory mapping.
pub struct SimpleDirEntry {
    factory: DirEntryFactory,
}

impl SimpleDirEntry {
    fn new(factory: DirEntryFactory) -> Self {
        Self { factory }
    }
}

/// Converts simple-fs child declarations into concrete directory entries.
pub trait IntoDirMappingEntry {
    /// Build an entry that can be installed in a simple directory mapping.
    fn into_dir_mapping_entry(self) -> SimpleDirEntry;
}

impl IntoDirMappingEntry for DirMaker {
    fn into_dir_mapping_entry(self) -> SimpleDirEntry {
        SimpleDirEntry::new(Arc::new(move |parent, name| {
            Ok(make_dir_child(parent, name, self.clone()))
        }))
    }
}

impl IntoDirMappingEntry for Arc<crate::SimpleFile> {
    fn into_dir_mapping_entry(self) -> SimpleDirEntry {
        SimpleDirEntry::new(Arc::new(move |parent, name| {
            Ok(make_file_child(parent, name, self.clone()))
        }))
    }
}

impl<I> IntoDirMappingEntry for Arc<SeqFileInode<I>>
where
    I: crate::SeqIterator,
{
    fn into_dir_mapping_entry(self) -> SimpleDirEntry {
        SimpleDirEntry::new(Arc::new(move |parent, name| {
            let init = self.inode_init();
            let inode = VfsInode::new_file_with_flags(self.clone(), NodeFlags::NON_CACHEABLE, init);
            Ok(Dentry::new_file_from_inode(inode, parent, name.to_owned()))
        }))
    }
}

fn make_dir_child(parent: Option<Dentry>, name: &str, maker: DirMaker) -> Dentry {
    Dentry::new_dir_from_inode(maker(), parent, name.to_owned())
}

fn simple_file_inode(node: Arc<crate::SimpleFile>, flags: NodeFlags) -> Arc<VfsInode> {
    node.new_inode(flags)
}

fn make_file_child(parent: Option<Dentry>, name: &str, node: Arc<crate::SimpleFile>) -> Dentry {
    let inode = simple_file_inode(node, NodeFlags::NON_CACHEABLE);
    Dentry::new_file_from_inode(inode, parent, name.to_owned())
}

/// Parent context passed to simple directory lookup implementations.
#[derive(Clone, Copy)]
pub struct SimpleDirLookup<'a> {
    parent: &'a Dentry,
}

impl<'a> SimpleDirLookup<'a> {
    fn new(parent: &'a Dentry) -> Self {
        Self { parent }
    }

    /// Creates a directory child entry under the lookup parent.
    pub fn dir(&self, name: &str, maker: DirMaker) -> Dentry {
        make_dir_child(Some(self.parent.clone()), name, maker)
    }

    /// Creates a directory child from a caller-supplied persistent inode.
    pub fn dir_from_inode(&self, name: &str, inode: Arc<VfsInode>) -> Dentry {
        Dentry::new_dir_from_inode(inode, Some(self.parent.clone()), name.to_owned())
    }

    /// Creates a file child entry under the lookup parent.
    pub fn file(&self, name: &str, entry: impl IntoDirMappingEntry) -> VfsResult<Dentry> {
        (entry.into_dir_mapping_entry().factory)(Some(self.parent.clone()), name)
    }

    /// Creates a file child from a caller-supplied inode.
    pub fn file_from_inode(&self, name: &str, inode: Arc<VfsInode>) -> Dentry {
        Dentry::new_file_from_inode(inode, Some(self.parent.clone()), name.to_owned())
    }

    pub(crate) fn parent(&self) -> &Dentry {
        self.parent
    }
}

/// Operations for a simple directory.
pub trait SimpleDirOps: Send + Sync + 'static {
    /// Get the names of all children in the directory.
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>>;
    /// Look up a child directory or file by name.
    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry>;

    /// Check if the directory is cacheable.
    fn supports_dentry_cache(&self) -> bool {
        true
    }

    /// Installs a persistent child backed by an already allocated inode.
    fn create_inode_child(
        &self,
        _lookup: SimpleDirLookup<'_>,
        _name: &str,
        _inode: Arc<VfsInode>,
    ) -> VfsResult<Dentry> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Creates a dynamic directory child from VFS-validated context.
    fn mkdir(
        &self,
        _dir: &VfsInode,
        _name: &str,
        _mode: Umode,
        _cred: &kcred::Cred,
    ) -> VfsResult<Arc<VfsInode>> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Removes the VFS-resolved directory child from the backing store.
    fn rmdir(&self, _dir: &VfsInode, _victim: &LockedDentry<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Combines two directories into one.
    fn chain<N: SimpleDirOps>(self, other: N) -> ChainedDirOps<Self, N>
    where
        Self: Sized,
    {
        ChainedDirOps(self, other)
    }
}

impl SimpleDirOps for DirMapping {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let names: Vec<Cow<'a, str>> = self
            .entries
            .lock()
            .keys()
            .map(|name| Cow::Owned(name.clone()))
            .collect();
        Ok(Box::new(names.into_iter()))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        self.lookup_child_with_parent(Some(lookup.parent().clone()), name)
    }

    fn create_inode_child(
        &self,
        lookup: SimpleDirLookup<'_>,
        name: &str,
        inode: Arc<VfsInode>,
    ) -> VfsResult<Dentry> {
        let mut entries = self.entries.lock();
        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }

        let stored_inode = inode.clone();
        entries.insert(
            name.to_owned(),
            Arc::new(move |parent, name| {
                Ok(Dentry::new_file_from_inode(
                    stored_inode.clone(),
                    parent,
                    name.to_owned(),
                ))
            }),
        );
        Ok(lookup.file_from_inode(name, inode))
    }
}

/// A mapping of directory names to entries.
pub struct DirMapping {
    entries: Mutex<BTreeMap<String, DirEntryFactory>>,
}

impl DirMapping {
    /// Create a new empty directory mapping.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Add a directory child to the directory mapping.
    pub fn add_dir(&mut self, name: impl Into<String>, maker: DirMaker) {
        self.add(name, maker);
    }

    /// Add a file-like child to the directory mapping.
    pub fn add_file(&mut self, name: impl Into<String>, entry: impl IntoDirMappingEntry) {
        self.add(name, entry);
    }

    /// Add a simple child to the directory mapping.
    pub fn add(&mut self, name: impl Into<String>, entry: impl IntoDirMappingEntry) {
        self.entries
            .lock()
            .insert(name.into(), entry.into_dir_mapping_entry().factory);
    }

    /// Add a custom child factory to the directory mapping.
    pub fn add_child(
        &mut self,
        name: impl Into<String>,
        factory: impl for<'a, 'b> Fn(SimpleDirLookup<'a>, &'b str) -> VfsResult<Dentry>
        + Send
        + Sync
        + 'static,
    ) {
        self.entries.lock().insert(
            name.into(),
            Arc::new(move |parent, name| {
                let parent = parent.as_ref().ok_or(VfsError::InvalidInput)?;
                factory(SimpleDirLookup::new(parent), name)
            }),
        );
    }

    /// Look up a child without a parent dentry.
    pub fn lookup_child(&self, name: &str) -> VfsResult<Dentry> {
        self.lookup_child_with_parent(None, name)
    }

    /// Returns true when the mapping contains a child with `name`.
    pub fn contains_child(&self, name: &str) -> bool {
        self.entries.lock().contains_key(name)
    }

    /// Returns the number of children in the mapping.
    pub fn child_count(&self) -> usize {
        self.entries.lock().len()
    }

    fn lookup_child_with_parent(&self, parent: Option<Dentry>, name: &str) -> VfsResult<Dentry> {
        let factory = self
            .entries
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        factory(parent, name)
    }
}

impl Default for DirMapping {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory created by [`SimpleDirOps::chain`].
pub struct ChainedDirOps<A, B>(A, B);

impl<A: SimpleDirOps, B: SimpleDirOps> SimpleDirOps for ChainedDirOps<A, B> {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(self.0.child_names()?.chain(self.1.child_names()?)))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        match self.0.lookup_child(lookup, name) {
            Ok(ops) => Ok(ops),
            Err(VfsError::NotFound) => self.1.lookup_child(lookup, name),
            Err(e) => Err(e),
        }
    }

    fn supports_dentry_cache(&self) -> bool {
        // TODO: If one of the ops is not cacheable while the other is, the
        // behavior is undefined.
        self.0.supports_dentry_cache() && self.1.supports_dentry_cache()
    }
}

/// Simple directory.
pub struct SimpleDir<O> {
    node: SimpleFsNode,
    ops: Arc<O>,
}

impl<O: SimpleDirOps> SimpleDir<O> {
    fn new(node: SimpleFsNode, ops: Arc<O>) -> Arc<Self> {
        Arc::new(Self { node, ops })
    }

    /// Create a [`DirMaker`] from given directory operations.
    pub fn new_maker(fs: Arc<SimpleFs>, ops: Arc<O>) -> DirMaker {
        Arc::new(move || {
            Self::new_inode_with_owner(
                fs.clone(),
                ops.clone(),
                NodePermission::from_bits_truncate(0o755),
                0,
                0,
            )
        })
    }

    /// Creates one persistent VFS directory inode with explicit metadata.
    pub fn new_inode_with_owner(
        fs: Arc<SimpleFs>,
        ops: Arc<O>,
        permission: NodePermission,
        uid: u32,
        gid: u32,
    ) -> Arc<VfsInode> {
        let dir = SimpleDir::new(
            SimpleFsNode::new_with_owner(fs, NodeType::Directory, permission, uid, gid),
            ops.clone(),
        );
        let private_data: Arc<dyn Any + Send + Sync> = dir.clone();
        let inode_operations: Arc<dyn InodeOperations> =
            Arc::new(SimpleDirInodeOperations::new(dir.clone()));
        let file_operations: Arc<dyn FileOperations> =
            Arc::new(SimpleDirFileOperations::new(dir.clone()));
        let init = dir.node.inode_init();
        let flags = if ops.supports_dentry_cache() {
            NodeFlags::empty()
        } else {
            NodeFlags::NON_CACHEABLE
        };
        VfsInode::new_dir_with_operations(
            private_data,
            inode_operations,
            file_operations,
            flags,
            init,
        )
    }

    fn read_dir_at(
        &self,
        dir: &Dentry,
        offset: u64,
        sink: &mut dyn DirEntrySink,
    ) -> VfsResult<usize> {
        let children = [DOT, DOTDOT]
            .into_iter()
            .map(Cow::Borrowed)
            .chain(self.ops.child_names()?);

        let mut count = 0;
        for (i, name) in children.enumerate().skip(offset as usize) {
            let metadata = match name.as_ref() {
                DOT => dir.metadata(),
                DOTDOT => dir
                    .parent()
                    .map_or_else(|| dir.metadata(), |parent| parent.metadata()),
                other => {
                    let entry = dir.lookup(other)?;
                    entry.metadata()
                }
            };
            if !sink.accept(
                &name,
                metadata.inode,
                metadata.mode.node_type(),
                i as u64 + 1,
            ) {
                break;
            }
            count += 1;
        }

        Ok(count)
    }
}

struct SimpleDirInodeOperations<O> {
    dir: Arc<SimpleDir<O>>,
}

impl<O> SimpleDirInodeOperations<O> {
    fn new(dir: Arc<SimpleDir<O>>) -> Self {
        Self { dir }
    }
}

impl<O: SimpleDirOps> InodeOperations for SimpleDirInodeOperations<O> {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        Some(self)
    }

    fn getattr(
        &self,
        idmap: &crate::MountIdmap,
        path: Option<&crate::Path>,
        request_mask: crate::GetattrRequestMask,
        query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        self.dir
            .node
            .getattr(idmap, path, request_mask, query_flags)
    }

    fn setattr(
        &self,
        idmap: &crate::MountIdmap,
        dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        self.dir.node.setattr(idmap, dentry, update)
    }
}

impl<O: SimpleDirOps> InodeDirOperations for SimpleDirInodeOperations<O> {
    fn lookup(
        &self,
        _dir: &crate::VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: crate::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let dir = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let entry = match self.dir.ops.lookup_child(SimpleDirLookup::new(&dir), name) {
            Ok(entry) => entry,
            Err(err) if err.canonicalize() == VfsError::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(entry))
    }

    fn mknod(
        &self,
        _idmap: &crate::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        device: DeviceId,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        if !matches!(
            mode.node_type(),
            NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Fifo | NodeType::Socket
        ) {
            return Err(VfsError::InvalidInput);
        }

        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let node = Arc::new(SimpleFsNode::new_with_owner(
            self.dir.node.filesystem(),
            mode.node_type(),
            mode.permission(),
            uid,
            gid,
        ));
        node.set_rdev(device);
        let init = node.inode_init();
        let inode = VfsInode::new_special(node, NodeFlags::empty(), init);
        let entry =
            self.dir
                .ops
                .create_inode_child(SimpleDirLookup::new(&parent), dentry.name(), inode)?;
        let inode = entry.vfs_inode();
        drop(entry);
        dentry.instantiate(inode)
    }

    fn mkdir(
        &self,
        _idmap: &crate::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let inode = self.dir.ops.mkdir(dir, dentry.name(), mode, cred)?;
        dentry.instantiate(inode)
    }

    fn rmdir(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.dir.ops.rmdir(dir, dentry)
    }

    fn symlink(
        &self,
        _idmap: &crate::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let (mode, uid, gid) = inode_init_owner(
            dir,
            Umode::new(NodeType::Symlink, NodePermission::from_bits_truncate(0o777)),
            cred,
        );
        let node = Arc::new(SimpleFsNode::new_with_owner(
            self.dir.node.filesystem(),
            NodeType::Symlink,
            mode.permission(),
            uid,
            gid,
        ));
        node.metadata.lock().size = target.len() as u64;
        let init = node.inode_init();
        let inode = VfsInode::new_cached_symlink(node, NodeFlags::empty(), init, target.to_owned());
        let entry =
            self.dir
                .ops
                .create_inode_child(SimpleDirLookup::new(&parent), dentry.name(), inode)?;
        let inode = entry.vfs_inode();
        drop(entry);
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &crate::VfsInode,
        _new_dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, _dir: &crate::VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rename(
        &self,
        _idmap: &crate::MountIdmap,
        _old_dir: &crate::VfsInode,
        _old_dentry: &LockedDentry<'_>,
        _new_dir: &crate::VfsInode,
        _new_dentry: &LockedDentry<'_>,
        _flags: crate::RenameFlags,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}

struct SimpleDirFileOperations<O> {
    dir: Arc<SimpleDir<O>>,
}

impl<O> SimpleDirFileOperations<O> {
    fn new(dir: Arc<SimpleDir<O>>) -> Self {
        Self { dir }
    }
}

impl<O: SimpleDirOps> FileOperations for SimpleDirFileOperations<O> {
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

impl<O: SimpleDirOps> FileDirOperations for SimpleDirFileOperations<O> {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let start = ctx.pos();
        self.dir.read_dir_at(file.path().dentry(), start, ctx)
    }
}
