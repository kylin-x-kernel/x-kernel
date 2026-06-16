// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory node traits and helpers.
use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::{
    mem,
    ops::{Deref, DerefMut},
};

use hashbrown::HashMap;

use super::DirEntry;
use crate::{
    MetadataUpdate, Mountpoint, Mutex, MutexGuard, NodeOps, NodePermission, NodeType, VfsError,
    VfsResult,
    path::{DOT, DOTDOT, MAX_NAME_LEN, verify_entry_name},
};

/// A trait for a sink that can receive directory entries.
pub trait DirEntrySink {
    /// Accept a directory entry, returns `false` if the sink is full.
    ///
    /// `offset` is the offset of the next entry to be read.
    ///
    /// It's not recommended to operate on the node inside the `accept`
    /// function, since some filesystem may impose a lock while iterating the
    /// directory, and operating on the node may cause deadlock.
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool;
}

impl<F: FnMut(&str, u64, NodeType, u64) -> bool> DirEntrySink for F {
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool {
        self(name, ino, node_type, offset)
    }
}

type DirChildren = HashMap<String, DirEntry>;

/// Directory node operations.
pub trait DirNodeOps: NodeOps {
    /// Reads directory entries.
    ///
    /// Returns the number of entries read.
    ///
    /// Implementations should ensure that `.` and `..` are present in the
    /// result.
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize>;

    /// Lookups a directory entry by name.
    fn lookup(&self, name: &str) -> VfsResult<DirEntry>;

    /// Returns whether directory entries can be cached.
    ///
    /// Some filesystems may not support caching directory entries because:
    /// - Entries change frequently (e.g., `/proc`, `/sys`)
    /// - Entries are dynamically generated
    /// - The filesystem doesn't have persistent storage
    ///
    /// # Behavior
    ///
    /// - If `true` (default): `DirNode` maintains a dentry cache for fast lookups
    /// - If `false`: Every lookup calls this trait's `lookup()` method directly
    ///
    /// # Implementation Notes
    ///
    /// When returning `false`, implementations should:
    /// - Handle repeated lookups efficiently
    /// - Ensure thread-safety for concurrent lookups
    /// - Consider caching at the filesystem level if needed
    ///
    /// # Examples
    ///
    /// - Regular filesystems (ext4, FAT): return `true`
    /// - Dynamic filesystems (/proc, /sys): return `false`
    /// - Device filesystems (/dev): typically return `true`
    fn supports_dentry_cache(&self) -> bool {
        true
    }

    /// Creates a directory entry.
    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry>;

    /// Creates a link to a node.
    fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry>;

    /// Unlinks a directory entry by name.
    ///
    /// If the entry is a non-empty directory, it should return `ENOTEMPTY`
    /// error.
    fn unlink(&self, name: &str) -> VfsResult<()>;

    /// Renames a directory entry, replacing the original entry (dst) if it
    /// already exists.
    ///
    /// If src and dst link to the same file, this should do nothing and return
    /// `Ok(())`.
    ///
    /// The caller should ensure:
    /// - If `src` is a directory, `dst` must not exist or be an empty
    ///   directory.
    /// - If `src` is not a directory, `dst` must not exist or not be a
    ///   directory.
    fn rename(&self, src_name: &str, dst_dir: &DirNode, dst_name: &str) -> VfsResult<()>;
}

/// Options for opening (or creating) a directory entry.
///
/// See [`DirNode::open_file`] for more details.
/// Options for opening or creating an entry in a directory.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Create the entry if it does not exist.
    pub create: bool,
    /// Fail if the entry already exists.
    pub create_new: bool,
    /// Node type to create.
    pub node_type: NodeType,
    /// Permission bits for new nodes.
    pub permission: NodePermission,
    /// Owner (uid, gid) to apply on creation.
    pub user: Option<(u32, u32)>, // (uid, gid)
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            create: false,
            create_new: false,
            node_type: NodeType::RegularFile,
            permission: NodePermission::default(),
            user: None,
        }
    }
}

/// Directory node wrapper with dentry cache support.
pub struct DirNode {
    ops: Arc<dyn DirNodeOps>,
    dentry_cache: Mutex<DirChildren>,
    pub(crate) mount_at_this_dir: Mutex<Option<Arc<Mountpoint>>>,
}

impl Deref for DirNode {
    type Target = dyn NodeOps;

    fn deref(&self) -> &Self::Target {
        &*self.ops
    }
}

impl From<DirNode> for Arc<dyn NodeOps> {
    fn from(node: DirNode) -> Self {
        node.ops.clone()
    }
}

impl DirNode {
    /// Create a new directory node wrapper.
    pub fn new(ops: Arc<dyn DirNodeOps>) -> Self {
        Self {
            ops,
            dentry_cache: Mutex::default(),
            mount_at_this_dir: Mutex::default(),
        }
    }

    /// Return the underlying directory operations object.
    pub fn inner(&self) -> &Arc<dyn DirNodeOps> {
        &self.ops
    }

    /// Downcast to a concrete directory implementation.
    pub fn downcast<T: DirNodeOps>(&self) -> VfsResult<Arc<T>> {
        self.ops
            .clone()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }

    fn forget_entry(children: &mut DirChildren, name: &str) {
        if let Some(entry) = children.remove(name)
            && let Ok(dir) = entry.as_dir()
        {
            dir.forget();
        }
    }

    fn lookup_locked(&self, name: &str, children: &mut DirChildren) -> VfsResult<DirEntry> {
        use hashbrown::hash_map::Entry;
        match children.entry(name.to_owned()) {
            Entry::Occupied(e) => Ok(e.get().clone()),
            Entry::Vacant(e) => {
                let node = self.ops.lookup(name)?;
                if self.ops.supports_dentry_cache() {
                    e.insert(node.clone());
                }
                Ok(node)
            }
        }
    }

    /// Looks up a directory entry by name.
    pub fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        // Fast path
        if self.ops.supports_dentry_cache() {
            self.lookup_locked(name, &mut self.dentry_cache.lock())
        } else {
            self.ops.lookup(name)
        }
    }

    /// Looks up a directory entry by name in cache.
    pub fn lookup_cache(&self, name: &str) -> Option<DirEntry> {
        if self.ops.supports_dentry_cache() {
            self.dentry_cache.lock().get(name).cloned()
        } else {
            None
        }
    }

    /// Inserts a directory entry into the cache.
    pub fn insert_cache(&self, name: String, entry: DirEntry) -> Option<DirEntry> {
        if self.ops.supports_dentry_cache() {
            self.dentry_cache.lock().insert(name, entry)
        } else {
            None
        }
    }

    /// Read directory entries starting at `offset`.
    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.ops.read_dir(offset, sink)
    }

    /// Creates a link to a node.
    pub fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;

        self.ops.link(name, node).inspect(|entry| {
            self.dentry_cache
                .lock()
                .insert(name.to_owned(), entry.clone());
        })
    }

    /// Unlinks a directory entry by name.
    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        verify_entry_name(name)?;

        let mut children = self.dentry_cache.lock();
        let entry = self.lookup_locked(name, &mut children)?;
        match (entry.is_dir(), is_dir) {
            (true, false) => return Err(VfsError::IsADirectory),
            (false, true) => return Err(VfsError::NotADirectory),
            _ => {}
        }

        self.ops.unlink(name).inspect(|_| {
            Self::forget_entry(&mut children, name);
        })
    }

    /// Returns whether the directory contains children.
    pub fn has_children(&self) -> VfsResult<bool> {
        let mut has_children = false;
        self.read_dir(0, &mut |name: &str, _, _, _| {
            if name != DOT && name != DOTDOT {
                has_children = true;
                false
            } else {
                true
            }
        })?;
        Ok(has_children)
    }

    fn create_locked(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
        children: &mut DirChildren,
    ) -> VfsResult<DirEntry> {
        let entry = self.ops.create(name, node_type, permission)?;
        children.insert(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Creates a directory entry.
    pub fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;
        self.create_locked(name, node_type, permission, &mut self.dentry_cache.lock())
    }

    fn lock_both_cache<'a>(
        &'a self,
        other: &'a Self,
    ) -> (
        MutexGuard<'a, DirChildren>,
        Option<MutexGuard<'a, DirChildren>>,
    ) {
        let src_children = self.dentry_cache.lock();
        let dst_children = if core::ptr::eq(self, other) {
            None
        } else {
            Some(other.dentry_cache.lock())
        };
        (src_children, dst_children)
    }

    /// Renames a directory entry.
    pub fn rename(&self, src_name: &str, dst_dir: &Self, dst_name: &str) -> VfsResult<()> {
        verify_entry_name(src_name)?;
        verify_entry_name(dst_name)?;

        let (mut src_children, mut dst_children) = self.lock_both_cache(dst_dir);

        let src = self.lookup_locked(src_name, &mut src_children)?;
        if let Ok(dst) = dst_dir.lookup_locked(
            dst_name,
            dst_children
                .as_mut()
                .map_or_else(|| src_children.deref_mut(), DerefMut::deref_mut),
        ) {
            if src.node_type() == NodeType::Directory {
                if let Ok(dir) = dst.as_dir()
                    && dir.has_children()?
                {
                    return Err(VfsError::DirectoryNotEmpty);
                }
            } else if dst.node_type() == NodeType::Directory {
                return Err(VfsError::IsADirectory);
            }
        }
        drop(src_children);
        drop(dst_children);

        self.ops.rename(src_name, dst_dir, dst_name).inspect(|_| {
            let (mut src_children, mut dst_children) = self.lock_both_cache(dst_dir);
            Self::forget_entry(&mut src_children, src_name);
            Self::forget_entry(
                dst_children
                    .as_mut()
                    .map_or_else(|| src_children.deref_mut(), DerefMut::deref_mut),
                dst_name,
            );
        })
    }

    /// Opens (or creates) a file in the directory.
    pub fn open_file(&self, name: &str, options: &OpenOptions) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;

        let mut children = self.dentry_cache.lock();
        match self.lookup_locked(name, &mut children) {
            Ok(val) => {
                if options.create_new {
                    return Err(VfsError::AlreadyExists);
                }
                return Ok(val);
            }
            Err(err) if err.canonicalize() == VfsError::NotFound && options.create => {}
            Err(err) => return Err(err),
        }
        let entry =
            self.create_locked(name, options.node_type, options.permission, &mut children)?;
        if options.user.is_some() {
            entry.update_metadata(MetadataUpdate {
                owner: options.user,
                ..Default::default()
            })?;
        }
        Ok(entry)
    }

    /// Returns the mountpoint attached to this directory, if any.
    pub fn mountpoint(&self) -> Option<Arc<Mountpoint>> {
        self.mount_at_this_dir.lock().clone()
    }

    /// Returns `true` if a filesystem is mounted at this directory.
    pub fn is_mountpoint(&self) -> bool {
        self.mount_at_this_dir.lock().is_some()
    }

    /// Clears the cache of directory entries & user data, allowing them to be
    /// released.
    pub(crate) fn forget(&self) {
        for (_, child) in mem::take(self.dentry_cache.lock().deref_mut()) {
            if let Ok(dir) = child.as_dir() {
                dir.forget();
            }
        }
    }
}

#[cfg(unittest)]
mod tests_dir {
    use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
    use core::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use kpoll::{IoEvents, Pollable};
    use unittest::def_test;

    use super::{DirEntrySink, DirNode, DirNodeOps, OpenOptions};
    use crate::{
        DirEntry, FileNode, FileNodeOps, Metadata, MetadataUpdate, NodeOps, NodePermission,
        NodeType, Reference, StatFs, SuperBlockOperations, VfsError, VfsResult, path::MAX_NAME_LEN,
    };

    struct MockFilesystem;

    impl SuperBlockOperations for MockFilesystem {
        fn name(&self) -> &str {
            "mockfs"
        }

        fn root_dentry(&self) -> DirEntry {
            panic!("root_dir is not used in these tests")
        }

        fn statfs(&self) -> VfsResult<StatFs> {
            Ok(StatFs {
                fs_type: 0,
                block_size: 0,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 0,
                free_file_count: 0,
                name_length: MAX_NAME_LEN as u32,
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
    }

    impl MockFileNodeOps {
        fn new(fs: Arc<MockFilesystem>, inode: u64) -> Self {
            Self {
                fs,
                inode,
                data: crate::Mutex::new(Vec::new()),
                owner: crate::Mutex::new(None),
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
                node_type: NodeType::RegularFile,
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
            Ok(())
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
        fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Ok(0)
        }

        fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
            Ok(0)
        }

        fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
            Ok((buf.len(), buf.len() as u64))
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
        cacheable: bool,
        next_inode: AtomicUsize,
        lookup_count: AtomicUsize,
        entries: crate::Mutex<BTreeMap<String, DirEntry>>,
    }

    impl MockDirNodeOps {
        fn new(fs: Arc<MockFilesystem>, inode: u64, cacheable: bool) -> Self {
            Self {
                fs,
                inode,
                cacheable,
                next_inode: AtomicUsize::new(100),
                lookup_count: AtomicUsize::new(0),
                entries: crate::Mutex::new(BTreeMap::new()),
            }
        }

        fn insert_entry(&self, name: &str, entry: DirEntry) {
            self.entries.lock().insert(String::from(name), entry);
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

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl DirNodeOps for MockDirNodeOps {
        fn read_dir(&self, _offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            let mut count = 0;
            for (index, (name, entry)) in self.entries.lock().iter().enumerate() {
                if !sink.accept(name, entry.inode(), entry.node_type(), index as u64 + 1) {
                    break;
                }
                count += 1;
            }
            Ok(count)
        }

        fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
            self.lookup_count.fetch_add(1, Ordering::Relaxed);
            self.entries
                .lock()
                .get(name)
                .cloned()
                .ok_or(VfsError::NotFound)
        }

        fn supports_dentry_cache(&self) -> bool {
            self.cacheable
        }

        fn create(
            &self,
            name: &str,
            node_type: NodeType,
            _permission: NodePermission,
        ) -> VfsResult<DirEntry> {
            let inode = self.next_inode.fetch_add(1, Ordering::Relaxed) as u64;
            let entry = match node_type {
                NodeType::Directory => DirEntry::new_dir(
                    |_| DirNode::new(Arc::new(MockDirNodeOps::new(self.fs.clone(), inode, true))),
                    Reference::new(None, String::from(name)),
                ),
                _ => DirEntry::new_file(
                    FileNode::new(Arc::new(MockFileNodeOps::new(self.fs.clone(), inode))),
                    node_type,
                    Reference::new(None, String::from(name)),
                ),
            };
            self.entries
                .lock()
                .insert(String::from(name), entry.clone());
            Ok(entry)
        }

        fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry> {
            self.entries.lock().insert(String::from(name), node.clone());
            Ok(node.clone())
        }

        fn unlink(&self, name: &str) -> VfsResult<()> {
            self.entries.lock().remove(name).ok_or(VfsError::NotFound)?;
            Ok(())
        }

        fn rename(&self, src_name: &str, _dst_dir: &DirNode, dst_name: &str) -> VfsResult<()> {
            let entry = self
                .entries
                .lock()
                .remove(src_name)
                .ok_or(VfsError::NotFound)?;
            self.entries.lock().insert(String::from(dst_name), entry);
            Ok(())
        }
    }

    fn make_file_entry(
        fs: Arc<MockFilesystem>,
        inode: u64,
        name: &str,
    ) -> (DirEntry, Arc<MockFileNodeOps>) {
        let ops = Arc::new(MockFileNodeOps::new(fs, inode));
        let entry = DirEntry::new_file(
            FileNode::new(ops.clone()),
            NodeType::RegularFile,
            Reference::new(None, String::from(name)),
        );
        (entry, ops)
    }

    #[def_test]
    fn test_open_options_default_values() {
        let options = OpenOptions::default();
        assert!(!options.create);
        assert!(!options.create_new);
        assert_eq!(options.node_type, NodeType::RegularFile);
        assert_eq!(options.permission.bits(), NodePermission::default().bits());
        assert!(options.user.is_none());
    }

    #[def_test]
    fn test_dirnode_lookup_uses_cache_when_enabled() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs.clone(), 1, true));
        let dir = DirNode::new(ops.clone());
        let (entry, _) = make_file_entry(fs, 11, "cached");
        ops.insert_entry("cached", entry.clone());

        let first = dir.lookup("cached").unwrap();
        let second = dir.lookup("cached").unwrap();

        assert!(first.ptr_eq(&second));
        assert!(dir.lookup_cache("cached").is_some());
        assert_eq!(ops.lookup_count.load(Ordering::Relaxed), 1);
    }

    #[def_test]
    fn test_dirnode_lookup_without_cache_hits_backend_each_time() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs.clone(), 2, false));
        let dir = DirNode::new(ops.clone());
        let (entry, _) = make_file_entry(fs, 12, "dynamic");
        ops.insert_entry("dynamic", entry.clone());

        assert!(dir.lookup_cache("dynamic").is_none());
        assert!(
            dir.insert_cache(String::from("dynamic"), entry.clone())
                .is_none()
        );
        assert!(dir.lookup("dynamic").unwrap().ptr_eq(&entry));
        assert!(dir.lookup("dynamic").unwrap().ptr_eq(&entry));
        assert_eq!(ops.lookup_count.load(Ordering::Relaxed), 2);
    }

    #[def_test]
    fn test_dirnode_lookup_rejects_names_too_long() {
        let fs = Arc::new(MockFilesystem);
        let dir = DirNode::new(Arc::new(MockDirNodeOps::new(fs, 3, true)));
        let name = "a".repeat(MAX_NAME_LEN + 1);

        let result = dir.lookup(&name);
        assert!(matches!(result, Err(VfsError::NameTooLong)));
    }

    #[def_test]
    fn test_open_file_handles_existing_and_missing_entries() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs.clone(), 4, true));
        let dir = DirNode::new(ops.clone());
        let (existing, _) = make_file_entry(fs, 13, "exists");
        ops.insert_entry("exists", existing.clone());

        let exists_err = dir.open_file(
            "exists",
            &OpenOptions {
                create_new: true,
                ..OpenOptions::default()
            },
        );
        assert!(matches!(exists_err, Err(VfsError::AlreadyExists)));

        let missing_err = dir.open_file("missing", &OpenOptions::default());
        assert!(matches!(missing_err, Err(VfsError::NotFound)));
    }

    #[def_test]
    fn test_open_file_create_updates_owner_metadata() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs, 5, true));
        let dir = DirNode::new(ops);
        let owner = (1000, 1001);
        let entry = dir
            .open_file(
                "created",
                &OpenOptions {
                    create: true,
                    user: Some(owner),
                    ..OpenOptions::default()
                },
            )
            .unwrap();

        let file = entry.downcast::<MockFileNodeOps>().unwrap();
        assert_eq!(*file.owner.lock(), Some(owner));
    }

    #[def_test]
    fn test_dirnode_link_inserts_cache_and_has_children_skips_dot_entries() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs.clone(), 6, true));
        let dir = DirNode::new(ops);
        let (entry, _) = make_file_entry(fs, 21, "linked");

        let linked = dir.link("linked", &entry).unwrap();
        assert!(linked.ptr_eq(&entry));
        assert!(dir.lookup_cache("linked").unwrap().ptr_eq(&entry));
        assert!(dir.has_children().unwrap());
    }

    #[def_test]
    fn test_dirnode_unlink_rejects_type_mismatch_and_clears_cache() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs.clone(), 7, true));
        let dir = DirNode::new(ops);
        let file = dir
            .create("file", NodeType::RegularFile, NodePermission::default())
            .unwrap();
        let child_dir = dir
            .create("subdir", NodeType::Directory, NodePermission::default())
            .unwrap();

        assert!(matches!(
            dir.unlink("file", true),
            Err(VfsError::NotADirectory)
        ));
        assert!(matches!(
            dir.unlink("subdir", false),
            Err(VfsError::IsADirectory)
        ));

        dir.unlink("file", false).unwrap();
        assert!(dir.lookup_cache("file").is_none());
        assert!(!file.ptr_eq(&child_dir));
    }

    #[def_test]
    fn test_dirnode_create_validates_special_and_long_names() {
        let fs = Arc::new(MockFilesystem);
        let dir = DirNode::new(Arc::new(MockDirNodeOps::new(fs, 8, true)));
        let too_long = "b".repeat(MAX_NAME_LEN + 1);

        assert!(matches!(
            dir.create(".", NodeType::RegularFile, NodePermission::default()),
            Err(VfsError::InvalidInput)
        ));
        assert!(matches!(
            dir.create(&too_long, NodeType::RegularFile, NodePermission::default()),
            Err(VfsError::NameTooLong)
        ));
    }

    #[def_test]
    fn test_dirnode_rename_rejects_non_empty_dir_and_dir_target_for_file() {
        let fs = Arc::new(MockFilesystem);
        let src_ops = Arc::new(MockDirNodeOps::new(fs.clone(), 9, true));
        let dst_ops = Arc::new(MockDirNodeOps::new(fs.clone(), 10, true));
        let src_dir = DirNode::new(src_ops.clone());
        let dst_dir = DirNode::new(dst_ops.clone());

        src_dir
            .create("src", NodeType::RegularFile, NodePermission::default())
            .unwrap();
        let dst_entry = dst_dir
            .create("dst", NodeType::Directory, NodePermission::default())
            .unwrap();
        dst_entry
            .as_dir()
            .unwrap()
            .create("child", NodeType::RegularFile, NodePermission::default())
            .unwrap();

        assert!(matches!(
            src_dir.rename("src", &dst_dir, "dst"),
            Err(VfsError::IsADirectory)
        ));

        let src_subdir = src_dir
            .create("srcdir", NodeType::Directory, NodePermission::default())
            .unwrap();
        let dst_subdir = dst_dir
            .create("dstdir", NodeType::Directory, NodePermission::default())
            .unwrap();
        dst_subdir
            .as_dir()
            .unwrap()
            .create("nested", NodeType::RegularFile, NodePermission::default())
            .unwrap();

        assert!(matches!(
            src_subdir.as_dir().unwrap().rename(".", &dst_dir, "dstdir"),
            Err(VfsError::InvalidInput)
        ));
        assert!(matches!(
            src_dir.rename("srcdir", &dst_dir, "dstdir"),
            Err(VfsError::DirectoryNotEmpty)
        ));
    }

    #[def_test]
    fn test_dirnode_rename_same_dir_updates_cache() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs, 11, true));
        let dir = DirNode::new(ops);

        let original = dir
            .create("old", NodeType::RegularFile, NodePermission::default())
            .unwrap();
        assert!(dir.lookup_cache("old").is_some());

        dir.rename("old", &dir, "new").unwrap();

        assert!(dir.lookup_cache("old").is_none());
        let renamed = dir.lookup("new").unwrap();
        assert_eq!(renamed.name(), "old");
        assert_eq!(renamed.inode(), original.inode());
    }

    #[def_test]
    fn test_dirnode_forget_recursively_clears_child_caches() {
        let fs = Arc::new(MockFilesystem);
        let ops = Arc::new(MockDirNodeOps::new(fs, 12, true));
        let dir = DirNode::new(ops);

        let child = dir
            .create("child", NodeType::Directory, NodePermission::default())
            .unwrap();
        child
            .as_dir()
            .unwrap()
            .create("leaf", NodeType::RegularFile, NodePermission::default())
            .unwrap();

        assert!(dir.lookup_cache("child").is_some());
        assert!(child.as_dir().unwrap().lookup_cache("leaf").is_some());

        dir.forget();

        assert!(dir.lookup_cache("child").is_none());
        assert!(child.as_dir().unwrap().lookup_cache("leaf").is_none());
    }
}
