// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS inode identity and inode cache helpers.

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec,
};
use core::{fmt, task::Context};

use hashbrown::HashMap;
use kpoll::{IoEvents, Pollable};

use super::{DirNode, FileNode, Node, NodeFlags, NodeOps, TypeMap};
use crate::{
    AddressSpace, AddressSpaceOperations, DirNodeInodeOperations, FileNodeFileOperations,
    FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard, NodeType, VfsError, VfsResult,
};

/// VFS inode identity shared by one or more directory entries.
///
/// This is the VFS-level owner for inode-scoped state. Path-specific state
/// belongs on a dentry, while data that must be shared by hard links or other
/// aliases belongs in this inode's attachment map.
pub struct VfsInode {
    node: Node,
    node_type: NodeType,
    attachments: Mutex<TypeMap>,
}

/// Weak reference to a VFS inode identity.
pub type WeakVfsInode = Weak<VfsInode>;

impl VfsInode {
    /// Construct an inode identity for a file-like node.
    pub fn new_file(node: FileNode, node_type: NodeType) -> Arc<Self> {
        Arc::new(Self {
            node: Node::File(node),
            node_type,
            attachments: Mutex::default(),
        })
    }

    /// Construct an inode identity for a directory node.
    pub fn new_dir(node: DirNode) -> Arc<Self> {
        Arc::new(Self {
            node: Node::Dir(node),
            node_type: NodeType::Directory,
            attachments: Mutex::default(),
        })
    }

    /// Gets the inode number of the node.
    pub fn inode(&self) -> u64 {
        self.node.inode()
    }

    /// Gets metadata for this inode, filling in its VFS node type.
    pub fn metadata(&self) -> VfsResult<Metadata> {
        self.node.metadata().map(|mut metadata| {
            metadata.node_type = self.node_type;
            metadata
        })
    }

    /// Updates the metadata of the node.
    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.node.update_metadata(update)
    }

    /// Gets the filesystem owning this node.
    pub fn filesystem(&self) -> &dyn FilesystemOps {
        self.node.filesystem()
    }

    /// Gets the size of the node.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64> {
        self.node.len()
    }

    /// Synchronizes the file to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.node.sync(data_only)
    }

    /// Returns the flags of the node.
    pub fn flags(&self) -> NodeFlags {
        self.node.flags()
    }

    /// Returns the VFS node type for this inode.
    pub fn node_type(&self) -> NodeType {
        self.node_type
    }

    /// Returns `true` if this inode wraps a file-like node.
    pub fn is_file(&self) -> bool {
        matches!(self.node, Node::File(_))
    }

    /// Returns `true` if this inode wraps a directory node.
    pub fn is_dir(&self) -> bool {
        matches!(self.node, Node::Dir(_))
    }

    /// Returns a file node reference if this inode wraps a file-like node.
    pub fn as_file(&self) -> VfsResult<&FileNode> {
        match &self.node {
            Node::File(file) => Ok(file),
            _ => Err(VfsError::IsADirectory),
        }
    }

    /// Returns a file-operations adapter for file-like inodes.
    pub fn file_operations(&self) -> VfsResult<FileNodeFileOperations<'_>> {
        self.as_file().map(FileNodeFileOperations::new)
    }

    /// Returns a directory node reference if this inode wraps a directory node.
    pub fn as_dir(&self) -> VfsResult<&DirNode> {
        match &self.node {
            Node::Dir(dir) => Ok(dir),
            _ => Err(VfsError::NotADirectory),
        }
    }

    /// Returns an inode-operations adapter for directory inodes.
    pub fn inode_operations(&self) -> VfsResult<DirNodeInodeOperations<'_>> {
        self.as_dir().map(DirNodeInodeOperations::new)
    }

    /// Attempt to downcast the inode to a concrete node type.
    pub fn downcast<T: NodeOps>(&self) -> VfsResult<Arc<T>> {
        self.node
            .clone_ops_arc()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }

    /// Access inode-scoped attachment storage.
    pub fn inode_data(&self) -> MutexGuard<'_, TypeMap> {
        self.attachments.lock()
    }

    /// Returns this inode's address-space object, if one has been attached.
    pub fn address_space(&self) -> Option<Arc<AddressSpace>> {
        self.attachments.lock().get::<AddressSpace>()
    }

    /// Return or create this inode's address-space object.
    pub fn get_or_insert_address_space(
        self: &Arc<Self>,
        ops: Arc<dyn AddressSpaceOperations>,
    ) -> Arc<AddressSpace> {
        self.get_or_insert_address_space_with(|| AddressSpace::new(Arc::downgrade(self), ops))
    }

    /// Return or create this inode's address-space object with a custom builder.
    pub fn get_or_insert_address_space_with(
        &self,
        create: impl FnOnce() -> AddressSpace,
    ) -> Arc<AddressSpace> {
        self.attachments.lock().get_or_insert_with(create)
    }

    /// Read the symlink target as a string.
    pub fn read_link(&self) -> VfsResult<String> {
        if self.node_type() != NodeType::Symlink {
            return Err(VfsError::InvalidData);
        }
        let file = self.as_file()?;
        let mut buf = vec![0; file.len()? as usize];
        file.read_at(&mut buf, 0)?;
        String::from_utf8(buf).map_err(|_| VfsError::InvalidData)
    }

    /// Issue an ioctl to the underlying file node.
    pub fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match &self.node {
            Node::File(file) => file.ioctl(cmd, arg),
            Node::Dir(_) => Err(VfsError::NotATty),
        }
    }
}

impl fmt::Debug for VfsInode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfsInode")
            .field("node", &self.node)
            .field("node_type", &self.node_type)
            .finish()
    }
}

impl Pollable for VfsInode {
    fn poll(&self) -> IoEvents {
        match &self.node {
            Node::File(file) => file.poll(),
            Node::Dir(_dir) => IoEvents::IN | IoEvents::OUT,
        }
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        match &self.node {
            Node::File(file) => file.register(context, events),
            Node::Dir(_) => {}
        }
    }
}

/// Per-filesystem cache for live VFS inode identities.
///
/// The cache stores weak references so dentry and open-file lifetimes decide
/// when an inode wrapper can disappear. Filesystems should route lookup,
/// create, and hard-link paths through this cache when they can provide a
/// stable inode number.
#[derive(Default)]
pub struct InodeCache {
    inodes: Mutex<HashMap<u64, WeakVfsInode>>,
}

impl InodeCache {
    /// Create an empty inode cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a live inode by inode number.
    pub fn lookup(&self, inode_number: u64) -> Option<Arc<VfsInode>> {
        let mut inodes = self.inodes.lock();
        let inode = inodes.get(&inode_number).and_then(WeakVfsInode::upgrade);
        if inode.is_none() {
            inodes.remove(&inode_number);
        }
        inode
    }

    /// Return the live inode for `inode_number`, or insert a newly created one.
    ///
    /// The constructor runs outside the cache lock. A concurrent caller may win
    /// the insert race; in that case this method returns the already cached
    /// inode and drops the newly created one.
    pub fn get_or_insert_with(
        &self,
        inode_number: u64,
        create_inode_fn: impl FnOnce() -> Arc<VfsInode>,
    ) -> Arc<VfsInode> {
        if let Some(inode) = self.lookup(inode_number) {
            return inode;
        }

        let new_inode = create_inode_fn();
        debug_assert_eq!(new_inode.inode(), inode_number);

        let mut inodes = self.inodes.lock();
        if let Some(inode) = inodes.get(&inode_number).and_then(WeakVfsInode::upgrade) {
            return inode;
        }

        inodes.insert(inode_number, Arc::downgrade(&new_inode));
        new_inode
    }

    /// Return or create a file-like VFS inode for a stable filesystem inode.
    pub fn get_or_insert_file(
        &self,
        inode_number: u64,
        node_type: NodeType,
        create_node_fn: impl FnOnce() -> FileNode,
    ) -> Arc<VfsInode> {
        debug_assert_ne!(node_type, NodeType::Directory);
        self.get_or_insert_with(inode_number, || {
            VfsInode::new_file(create_node_fn(), node_type)
        })
    }

    /// Return or create a directory VFS inode for a stable filesystem inode.
    ///
    /// This is the directory counterpart of [`Self::get_or_insert_file`].
    /// Filesystems should use it after their directory node no longer stores
    /// dentry/path context inside the inode object.
    pub fn get_or_insert_dir(
        &self,
        inode_number: u64,
        create_node_fn: impl FnOnce() -> DirNode,
    ) -> Arc<VfsInode> {
        self.get_or_insert_with(inode_number, || VfsInode::new_dir(create_node_fn()))
    }

    /// Remove dead cache entries and return the number removed.
    pub fn prune_stale(&self) -> usize {
        let mut removed = 0;
        self.inodes.lock().retain(|_, inode| {
            let is_live = inode.strong_count() > 0;
            if !is_live {
                removed += 1;
            }
            is_live
        });
        removed
    }
}
