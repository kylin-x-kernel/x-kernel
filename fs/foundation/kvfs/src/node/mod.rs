// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS node types and directory entry wrappers.
mod device;
mod dir;
mod file;
mod inode;

use alloc::{
    borrow::ToOwned,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    any::{Any, TypeId},
    fmt, iter,
    ops::Deref,
    task::Context,
};

use bitflags::bitflags;
pub use device::*;
pub use dir::*;
pub use file::*;
use inherit_methods_macro::inherit_methods;
pub use inode::*;
use kpoll::{IoEvents, Pollable};
use smallvec::SmallVec;

use crate::{
    FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard, NodeType, VfsResult, path::PathBuf,
};

bitflags! {
    /// Flags describing special node behaviors.
    #[derive(Debug, Clone, Copy)]
    pub struct NodeFlags: u32 {
        /// Indicates that this file behaves like a stream.
        ///
        /// Presence of this flag could inform the higher layers to omit
        /// maintaining a position for this file. `read_at` and `write_at` would
        /// be called with zero offset instead.
        const STREAM = 0x0001;

        /// Indicates that this file should not be cached.
        ///
        /// For instance, files in `/proc` or `/sys` may contain dynamic data
        /// that should not be cached.
        const NON_CACHEABLE = 0x0002;

        /// Indicates that this file should always be cached.
        ///
        /// For instance, files in tmpfs relies on page caching and do not have
        /// a backing device.
        const ALWAYS_CACHE = 0x0004;

        /// Indicates that operations on this file are always blocking.
        ///
        /// This could prevent higher layers from attempting to add unnecessary
        /// non-blocking handling.
        const BLOCKING = 0x0008;
    }
}

/// Filesystem node operations.
#[allow(clippy::len_without_is_empty)]
pub trait NodeOps: Send + Sync + 'static {
    /// Gets the inode number of the node.
    fn inode(&self) -> u64;

    /// Gets the metadata of the node.
    fn metadata(&self) -> VfsResult<Metadata>;

    /// Updates the metadata of the node.
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    /// Gets the filesystem owning this node.
    fn filesystem(&self) -> &dyn FilesystemOps;

    /// Gets the size of the node.
    fn len(&self) -> VfsResult<u64> {
        self.metadata().map(|m| m.size)
    }

    /// Synchronizes the file to disk.
    fn sync(&self, data_only: bool) -> VfsResult<()>;

    /// Casts the node to a `&dyn core::any::Any`.
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;

    /// Returns the flags of the node.
    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }
}

enum Node {
    File(FileNode),
    Dir(DirNode),
}

impl Node {
    pub fn clone_ops_arc(&self) -> Arc<dyn NodeOps> {
        match self {
            Node::File(file) => file.inner().clone(),
            Node::Dir(dir) => dir.inner().clone(),
        }
    }
}

impl Deref for Node {
    type Target = dyn NodeOps;

    fn deref(&self) -> &Self::Target {
        match &self {
            Node::File(file) => file.deref(),
            Node::Dir(dir) => dir.deref(),
        }
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::File(file) => write!(f, "FileNode({})", file.inode()),
            Node::Dir(dir) => write!(f, "DirNode({})", dir.inode()),
        }
    }
}

/// Key type for dentry cache lookups.
///
/// Combines the parent's inode number and the entry name to uniquely identify
/// a directory entry within its parent directory.
pub type ReferenceKey = (usize, String);

/// Reference to a directory entry within a parent.
#[derive(Debug)]
pub struct Reference {
    parent: Option<DirEntry>,
    name: String,
}

impl Reference {
    /// Create a new reference with an optional parent and name.
    pub fn new(parent: Option<DirEntry>, name: String) -> Self {
        Self { parent, name }
    }

    /// Create the root reference.
    pub fn root() -> Self {
        Self::new(None, String::new())
    }

    /// Build a key suitable for cache lookup.
    pub fn key(&self) -> ReferenceKey {
        let address = self
            .parent
            .as_ref()
            .map_or(0, |it| Arc::as_ptr(&it.0) as usize);
        (address, self.name.clone())
    }
}

/// Type-indexed metadata storage for nodes.
#[derive(Default)]
pub struct TypeMap(SmallVec<[(TypeId, Arc<dyn Any + Send + Sync>); 2]>);
impl TypeMap {
    /// Create an empty `TypeMap`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value by its concrete type.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        self.0.push((TypeId::of::<T>(), Arc::new(value)));
    }

    /// Get a value by its concrete type.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.0
            .iter()
            .find_map(|(id, value)| {
                if id == &TypeId::of::<T>() {
                    Some(value.clone())
                } else {
                    None
                }
            })
            .and_then(|value| value.downcast().ok())
    }

    /// Get a value by type or insert one created by `f`.
    pub fn get_or_insert_with<T: Any + Send + Sync>(&mut self, f: impl FnOnce() -> T) -> Arc<T> {
        if let Some(value) = self.get::<T>() {
            value
        } else {
            let value = f();
            self.insert(value);
            self.get::<T>().unwrap()
        }
    }
}

struct Inner {
    inode: Arc<VfsInode>,
    reference: Reference,
    dentry_data: Mutex<TypeMap>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("inode", &self.inode)
            .field("reference", &self.reference)
            .finish()
    }
}

/// Strong reference to a directory entry.
#[derive(Debug, Clone)]
pub struct DirEntry(Arc<Inner>);

/// Weak reference to a directory entry.
#[derive(Debug, Clone)]
pub struct WeakDirEntry(Weak<Inner>);

impl WeakDirEntry {
    /// Upgrade to a strong reference if the entry still exists.
    pub fn upgrade(&self) -> Option<DirEntry> {
        self.0.upgrade().map(DirEntry)
    }
}

impl From<Node> for Arc<dyn NodeOps> {
    fn from(node: Node) -> Self {
        match node {
            Node::File(file) => file.into(),
            Node::Dir(dir) => dir.into(),
        }
    }
}

#[inherit_methods(from = "self.0.inode")]
impl DirEntry {
    pub fn inode(&self) -> u64;

    pub fn filesystem(&self) -> &dyn FilesystemOps;

    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64>;

    pub fn flags(&self) -> NodeFlags;

    pub fn sync(&self, data_only: bool) -> VfsResult<()>;
}

impl DirEntry {
    /// Construct a file entry with the given node and reference.
    pub fn new_file(node: FileNode, node_type: NodeType, reference: Reference) -> Self {
        Self::new_file_from_inode(VfsInode::new_file(node, node_type), reference)
    }

    /// Construct a directory entry with a node builder.
    pub fn new_dir(node_fn: impl FnOnce(WeakDirEntry) -> DirNode, reference: Reference) -> Self {
        Self(Arc::new_cyclic(|this| Inner {
            inode: VfsInode::new_dir(node_fn(WeakDirEntry(this.clone()))),
            reference,
            dentry_data: Mutex::default(),
        }))
    }

    /// Construct a file entry that points at an existing inode identity.
    pub fn new_file_from_inode(inode: Arc<VfsInode>, reference: Reference) -> Self {
        debug_assert!(inode.is_file());
        Self(Arc::new(Inner {
            inode,
            reference,
            dentry_data: Mutex::default(),
        }))
    }

    /// Construct a directory entry that points at an existing inode identity.
    pub fn new_dir_from_inode(inode: Arc<VfsInode>, reference: Reference) -> Self {
        debug_assert!(inode.is_dir());
        Self(Arc::new(Inner {
            inode,
            reference,
            dentry_data: Mutex::default(),
        }))
    }

    /// Returns metadata for this entry, filling in its node type.
    pub fn metadata(&self) -> VfsResult<Metadata> {
        self.0.inode.metadata()
    }

    /// Attempt to downcast the entry to a concrete node type.
    pub fn downcast<T: NodeOps>(&self) -> VfsResult<Arc<T>> {
        self.0.inode.downcast()
    }

    /// Downgrade to a weak reference.
    pub fn downgrade(&self) -> WeakDirEntry {
        WeakDirEntry(Arc::downgrade(&self.0))
    }

    /// Return the inode identity referenced by this directory entry.
    pub fn vfs_inode(&self) -> &Arc<VfsInode> {
        &self.0.inode
    }

    /// Returns the cache key for this entry.
    pub fn key(&self) -> ReferenceKey {
        self.0.reference.key()
    }

    /// Returns the node type of this entry.
    pub fn node_type(&self) -> NodeType {
        self.0.inode.node_type()
    }

    /// Returns the parent directory entry, if any.
    pub fn parent(&self) -> Option<Self> {
        self.0.reference.parent.clone()
    }

    /// Returns the entry name within its parent directory.
    pub fn name(&self) -> &str {
        &self.0.reference.name
    }

    /// Checks if the entry is a root of a mount point.
    pub fn is_root_of_mount(&self) -> bool {
        self.0.reference.parent.is_none()
    }

    /// Returns whether `self` is an ancestor of `other`.
    pub fn is_ancestor_of(&self, other: &Self) -> VfsResult<bool> {
        let mut current = other.clone();
        loop {
            if current.ptr_eq(self) {
                return Ok(true);
            }
            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
        Ok(false)
    }

    pub(crate) fn collect_absolute_path(&self, components: &mut Vec<String>) {
        let mut current = self.clone();
        loop {
            components.push(current.name().to_owned());
            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
    }

    /// Returns the absolute path for this entry.
    pub fn absolute_path(&self) -> VfsResult<PathBuf> {
        let mut components = vec![];
        self.collect_absolute_path(&mut components);
        Ok(iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect())
    }

    /// Returns `true` if this entry is a file.
    pub fn is_file(&self) -> bool {
        self.0.inode.is_file()
    }

    /// Returns `true` if this entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.0.inode.is_dir()
    }

    /// Returns a file node reference if this entry is a file.
    pub fn as_file(&self) -> VfsResult<&FileNode> {
        self.0.inode.as_file()
    }

    /// Returns a directory node reference if this entry is a directory.
    pub fn as_dir(&self) -> VfsResult<&DirNode> {
        self.0.inode.as_dir()
    }

    /// Returns `true` if two entries point to the same node.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Returns `true` if two entries point to the same inode identity.
    pub fn is_same_inode(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0.inode, &other.0.inode)
    }

    /// Returns the raw pointer value for this entry.
    pub fn as_ptr(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// Read the symlink target as a string.
    pub fn read_link(&self) -> VfsResult<String> {
        self.0.inode.read_link()
    }

    /// Issue an ioctl to the underlying file node.
    pub fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.0.inode.ioctl(cmd, arg)
    }

    /// Access per-dentry attachment storage.
    pub fn dentry_data(&self) -> MutexGuard<'_, TypeMap> {
        self.0.dentry_data.lock()
    }

    /// Access per-dentry attachment storage.
    ///
    /// This is kept for compatibility with older call sites. New shared state
    /// that belongs to the underlying file object should use [`Self::inode_data`].
    pub fn user_data(&self) -> MutexGuard<'_, TypeMap> {
        self.dentry_data()
    }

    /// Access inode-scoped attachment storage.
    pub fn inode_data(&self) -> MutexGuard<'_, TypeMap> {
        self.0.inode.inode_data()
    }
}

impl Pollable for DirEntry {
    fn poll(&self) -> IoEvents {
        self.0.inode.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.0.inode.register(context, events)
    }
}

#[cfg(unittest)]
mod tests_node {
    use alloc::{string::String, sync::Arc, vec::Vec};
    use core::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use kpoll::{IoEvents, Pollable};
    use unittest::def_test;

    use super::{
        DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, InodeCache, NodeOps,
        Reference, TypeMap, VfsInode,
    };
    use crate::{
        FilesystemOps, Metadata, MetadataUpdate, NodePermission, NodeType, StatFs, VfsError,
        VfsResult,
    };

    struct MockFilesystem;

    impl FilesystemOps for MockFilesystem {
        fn name(&self) -> &str {
            "mockfs"
        }

        fn root_dir(&self) -> DirEntry {
            panic!("root_dir is not used in these tests")
        }

        fn stat(&self) -> VfsResult<StatFs> {
            Ok(StatFs {
                fs_type: 0,
                block_size: 0,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 0,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 0,
                mount_flags: 0,
            })
        }
    }

    struct MockFileNodeOps {
        fs: Arc<MockFilesystem>,
        inode: u64,
        data: crate::Mutex<Vec<u8>>,
        owner: crate::Mutex<Option<(u32, u32)>>,
        update_count: AtomicUsize,
    }

    impl MockFileNodeOps {
        fn new(fs: Arc<MockFilesystem>, inode: u64, data: &[u8]) -> Self {
            Self {
                fs,
                inode,
                data: crate::Mutex::new(data.to_vec()),
                owner: crate::Mutex::new(None),
                update_count: AtomicUsize::new(0),
            }
        }
    }

    impl NodeOps for MockFileNodeOps {
        fn inode(&self) -> u64 {
            self.inode
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.inode,
                nlink: 1,
                mode: NodePermission::default(),
                node_type: NodeType::Unknown,
                uid: 0,
                gid: 0,
                size: self.data.lock().len() as u64,
                block_size: 512,
                blocks: 1,
                rdev: Default::default(),
                atime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
            })
        }

        fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
            *self.owner.lock() = update.owner;
            self.update_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            self.fs.as_ref()
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl Pollable for MockFileNodeOps {
        fn poll(&self) -> IoEvents {
            IoEvents::IN | IoEvents::OUT
        }

        fn register(&self, _context: &mut core::task::Context<'_>, _events: IoEvents) {}
    }

    impl FileNodeOps for MockFileNodeOps {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
            let data = self.data.lock();
            let start = offset as usize;
            if start >= data.len() {
                return Ok(0);
            }
            let count = buf.len().min(data.len() - start);
            buf[..count].copy_from_slice(&data[start..start + count]);
            Ok(count)
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
            let mut data = self.data.lock();
            let start = offset as usize;
            if start + buf.len() > data.len() {
                data.resize(start + buf.len(), 0);
            }
            data[start..start + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
            let mut data = self.data.lock();
            data.extend_from_slice(buf);
            Ok((buf.len(), data.len() as u64))
        }

        fn set_len(&self, len: u64) -> VfsResult<()> {
            self.data.lock().resize(len as usize, 0);
            Ok(())
        }

        fn set_symlink(&self, target: &str) -> VfsResult<()> {
            *self.data.lock() = target.as_bytes().to_vec();
            Ok(())
        }
    }

    struct MockDirNodeOps {
        fs: Arc<MockFilesystem>,
        inode: u64,
    }

    impl MockDirNodeOps {
        fn new(fs: Arc<MockFilesystem>, inode: u64) -> Self {
            Self { fs, inode }
        }
    }

    impl NodeOps for MockDirNodeOps {
        fn inode(&self) -> u64 {
            self.inode
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.inode,
                nlink: 1,
                mode: NodePermission::default(),
                node_type: NodeType::Directory,
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 512,
                blocks: 1,
                rdev: Default::default(),
                atime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            self.fs.as_ref()
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl DirNodeOps for MockDirNodeOps {
        fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            Ok(0)
        }

        fn lookup(&self, _name: &str) -> VfsResult<DirEntry> {
            Err(VfsError::NotFound)
        }

        fn create(
            &self,
            _name: &str,
            _node_type: NodeType,
            _permission: NodePermission,
        ) -> VfsResult<DirEntry> {
            Err(VfsError::OperationNotSupported)
        }

        fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
            Err(VfsError::OperationNotSupported)
        }

        fn unlink(&self, _name: &str) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }

        fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }
    }

    fn make_file_entry(
        fs: Arc<MockFilesystem>,
        inode: u64,
        parent: Option<DirEntry>,
        name: &str,
    ) -> (DirEntry, Arc<MockFileNodeOps>) {
        let ops = Arc::new(MockFileNodeOps::new(fs, inode, b"payload"));
        let entry = DirEntry::new_file(
            FileNode::new(ops.clone()),
            NodeType::RegularFile,
            Reference::new(parent, String::from(name)),
        );
        (entry, ops)
    }

    fn make_file_inode(
        fs: Arc<MockFilesystem>,
        inode: u64,
    ) -> (Arc<VfsInode>, Arc<MockFileNodeOps>) {
        let ops = Arc::new(MockFileNodeOps::new(fs, inode, b"payload"));
        let inode = VfsInode::new_file(FileNode::new(ops.clone()), NodeType::RegularFile);
        (inode, ops)
    }

    #[def_test]
    fn test_reference_root_and_key() {
        let root = Reference::root();
        assert_eq!(root.key(), (0, String::new()));

        let fs = Arc::new(MockFilesystem);
        let (parent, _) = make_file_entry(fs, 1, None, "parent");
        let child = Reference::new(Some(parent.clone()), String::from("child"));
        let key = child.key();

        assert_eq!(key.1, "child");
        assert_ne!(key.0, 0);
    }

    #[def_test]
    fn test_typemap_insert_get_and_get_or_insert() {
        let mut map = TypeMap::new();
        assert!(map.get::<u32>().is_none());

        map.insert(7_u32);
        assert_eq!(*map.get::<u32>().unwrap(), 7);

        let created = map.get_or_insert_with::<u32>(|| 99);
        assert_eq!(*created, 7);

        let text = map.get_or_insert_with::<String>(|| String::from("node"));
        assert_eq!(text.as_str(), "node");
        assert_eq!(map.get::<String>().unwrap().as_str(), "node");
    }

    #[def_test]
    fn test_direntry_file_helpers_and_userdata() {
        let fs = Arc::new(MockFilesystem);
        let (entry, ops) = make_file_entry(fs, 2, None, "leaf");

        assert!(entry.is_file());
        assert!(!entry.is_dir());
        assert_eq!(entry.name(), "leaf");
        assert_eq!(entry.filesystem().name(), "mockfs");
        assert_eq!(entry.node_type(), NodeType::RegularFile);
        assert!(matches!(entry.as_dir(), Err(VfsError::NotADirectory)));
        assert_eq!(entry.as_file().unwrap().inode(), 2);

        let metadata = entry.metadata().unwrap();
        assert_eq!(metadata.node_type, NodeType::RegularFile);
        assert_eq!(metadata.size, 7);

        let weak = entry.downgrade();
        assert!(weak.upgrade().unwrap().ptr_eq(&entry));

        entry.user_data().insert(42_u32);
        assert_eq!(*entry.user_data().get::<u32>().unwrap(), 42);

        assert_eq!(ops.update_count.load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn test_shared_inode_identity_and_attachments() {
        let fs = Arc::new(MockFilesystem);
        let (inode, _) = make_file_inode(fs, 30);
        let first = DirEntry::new_file_from_inode(
            inode.clone(),
            Reference::new(None, String::from("first")),
        );
        let second =
            DirEntry::new_file_from_inode(inode, Reference::new(None, String::from("second")));

        assert!(!first.ptr_eq(&second));
        assert!(first.is_same_inode(&second));

        first.inode_data().insert(42_u32);
        assert_eq!(*second.inode_data().get::<u32>().unwrap(), 42);

        first.user_data().insert(String::from("first dentry"));
        assert!(second.user_data().get::<String>().is_none());
    }

    #[def_test]
    fn test_legacy_constructors_create_distinct_inode_identities() {
        let fs = Arc::new(MockFilesystem);
        let (first, _) = make_file_entry(fs.clone(), 40, None, "first");
        let (second, _) = make_file_entry(fs, 40, None, "second");

        assert!(!first.ptr_eq(&second));
        assert!(!first.is_same_inode(&second));
    }

    #[def_test]
    fn test_inode_cache_reuses_live_inode_identity() {
        let cache = InodeCache::new();
        let fs = Arc::new(MockFilesystem);
        let first = cache.get_or_insert_file(50, NodeType::RegularFile, || {
            let ops = Arc::new(MockFileNodeOps::new(fs.clone(), 50, b"first"));
            FileNode::new(ops)
        });
        let second = cache.get_or_insert_file(50, NodeType::RegularFile, || {
            panic!("live inode should be reused")
        });

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.inode(), 50);
        assert!(cache.lookup(50).is_some());

        drop(first);
        drop(second);
        assert!(cache.lookup(50).is_none());
    }

    #[def_test]
    fn test_direntry_parent_path_and_ancestor_helpers() {
        let fs = Arc::new(MockFilesystem);
        let root = DirEntry::new_dir(
            |_| DirNode::new(Arc::new(MockDirNodeOps::new(fs.clone(), 10))),
            Reference::root(),
        );
        let child = DirEntry::new_dir(
            |_| DirNode::new(Arc::new(MockDirNodeOps::new(fs.clone(), 11))),
            Reference::new(Some(root.clone()), String::from("child")),
        );
        let (leaf, _) = make_file_entry(fs, 12, Some(child.clone()), "leaf");

        assert!(root.is_root_of_mount());
        assert_eq!(child.parent().unwrap().as_ptr(), root.as_ptr());
        assert_eq!(leaf.absolute_path().unwrap().as_str(), "/child/leaf");
        assert!(root.is_ancestor_of(&leaf).unwrap());
        assert!(child.is_ancestor_of(&leaf).unwrap());
        assert!(!leaf.is_ancestor_of(&child).unwrap());
    }

    #[def_test]
    fn test_direntry_read_link_and_downcast_paths() {
        let fs = Arc::new(MockFilesystem);
        let (regular, _) = make_file_entry(fs.clone(), 20, None, "plain");
        let symlink_ops = Arc::new(MockFileNodeOps::new(fs.clone(), 21, b"/tmp/target"));
        let symlink = DirEntry::new_file(
            FileNode::new(symlink_ops.clone()),
            NodeType::Symlink,
            Reference::new(None, String::from("ln")),
        );
        let dir = DirEntry::new_dir(
            |_| DirNode::new(Arc::new(MockDirNodeOps::new(fs, 22))),
            Reference::new(None, String::from("dir")),
        );

        assert!(matches!(regular.read_link(), Err(VfsError::InvalidData)));
        assert_eq!(symlink.read_link().unwrap(), "/tmp/target");
        assert!(regular.downcast::<MockFileNodeOps>().is_ok());
        assert!(matches!(
            symlink.downcast::<MockDirNodeOps>(),
            Err(VfsError::InvalidInput)
        ));
        assert!(dir.as_dir().is_ok());
        assert!(matches!(dir.as_file(), Err(VfsError::IsADirectory)));
    }

    #[def_test]
    fn test_reference_key_uniqueness() {
        let ref1 = Reference::new(None, String::from("file1.txt"));
        let ref2 = Reference::new(None, String::from("file2.txt"));
        let ref3 = Reference::new(None, String::from("file1.txt"));

        assert_ne!(ref1.key(), ref2.key());
        assert_eq!(ref1.key(), ref3.key());
    }

    #[def_test]
    fn test_direntry_weak_relationship() {
        let fs = Arc::new(MockFilesystem);
        let (entry, _) = make_file_entry(fs, 99, None, "weak_test");

        let weak = entry.downgrade();
        assert!(weak.upgrade().is_some());
        assert!(weak.upgrade().unwrap().ptr_eq(&entry));
    }
}
