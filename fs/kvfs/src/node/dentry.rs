// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS dentry objects.

use alloc::{
    borrow::ToOwned,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{any::Any, fmt, iter, mem};

use bitflags::bitflags;
use hashbrown::HashMap;

use super::{DirEntrySink, NodeFlags, VfsInode};
use crate::{
    DeviceId, Metadata, MetadataUpdate, Mutex, NodePermission, NodeType, RenameFlags, SuperBlock,
    VfsError, VfsResult,
    path::{DOT, DOTDOT, MAX_NAME_LEN, PathBuf},
};

bitflags! {
    /// Dentry state flags.
    #[derive(Debug, Clone, Copy)]
    pub struct DentryFlags: u32 {
        const DONTCACHE = 1 << 7;
        const ENTRY_TYPE = 7 << 19;
        const MISS_TYPE = 0 << 19;
        const DIRECTORY_TYPE = 2 << 19;
        const AUTODIR_TYPE = 3 << 19;
        const REGULAR_TYPE = 4 << 19;
        const SPECIAL_TYPE = 5 << 19;
        const SYMLINK_TYPE = 6 << 19;
        const PERSISTENT = 1 << 27;
    }
}

impl DentryFlags {
    fn for_inode(inode: &VfsInode) -> Self {
        if inode.node_type() == NodeType::Directory {
            return if inode.supports_directory_operations() {
                Self::DIRECTORY_TYPE
            } else {
                Self::AUTODIR_TYPE
            };
        }

        if inode.supports_symlink_operations() {
            return Self::SYMLINK_TYPE;
        }

        match inode.node_type() {
            NodeType::RegularFile => Self::REGULAR_TYPE,
            _ => Self::SPECIAL_TYPE,
        }
    }

    fn is_miss_type(self) -> bool {
        self.bits() & Self::ENTRY_TYPE.bits() == Self::MISS_TYPE.bits()
    }

    fn is_directory_type(self) -> bool {
        self.bits() & Self::ENTRY_TYPE.bits() == Self::DIRECTORY_TYPE.bits()
    }

    fn is_autodir_type(self) -> bool {
        self.bits() & Self::ENTRY_TYPE.bits() == Self::AUTODIR_TYPE.bits()
    }

    fn is_regular_type(self) -> bool {
        self.bits() & Self::ENTRY_TYPE.bits() == Self::REGULAR_TYPE.bits()
    }

    fn is_special_type(self) -> bool {
        self.bits() & Self::ENTRY_TYPE.bits() == Self::SPECIAL_TYPE.bits()
    }

    fn is_symlink_type(self) -> bool {
        self.bits() & Self::ENTRY_TYPE.bits() == Self::SYMLINK_TYPE.bits()
    }
}

/// Key type for dentry cache lookups.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DentryKey {
    parent: usize,
    name: String,
}

impl DentryKey {
    pub(super) fn new(parent: usize, name: String) -> Self {
        Self { parent, name }
    }
}

/// Trait object implementing dentry-level filesystem policy.
///
/// Filesystems install a shared implementation; dentries that do not need
/// custom behaviour leave this slot empty.
pub trait DentryOperations: Send + Sync + 'static {
    /// Produces a dynamic display name when this operation table provides one.
    fn d_dname(&self, _dentry: &Dentry) -> VfsResult<Option<String>> {
        Ok(None)
    }
}

struct DentryInner {
    flags: Mutex<DentryFlags>,
    inode: Mutex<Option<Arc<VfsInode>>>,
    parent: Option<Dentry>,
    name: String,
    operations: Mutex<Option<Arc<dyn DentryOperations>>>,
    super_block: Mutex<Option<Weak<SuperBlock>>>,
    children: Mutex<HashMap<String, Dentry>>,
}

impl DentryInner {
    fn new(
        flags: DentryFlags,
        inode: Option<Arc<VfsInode>>,
        parent: Option<Dentry>,
        name: String,
    ) -> Self {
        let super_block = parent
            .as_ref()
            .and_then(Dentry::super_block)
            .map(|super_block| Arc::downgrade(&super_block));
        Self {
            flags: Mutex::new(flags),
            inode: Mutex::new(inode),
            parent,
            name,
            operations: Mutex::default(),
            super_block: Mutex::new(super_block),
            children: Mutex::default(),
        }
    }
}

impl fmt::Debug for DentryInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DentryInner")
            .field("flags", &*self.flags.lock())
            .field("inode", &*self.inode.lock())
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct DentryAlias(Weak<DentryInner>);

impl DentryAlias {
    pub(super) fn new(dentry: &Dentry) -> Self {
        Self(Arc::downgrade(&dentry.0))
    }

    pub(super) fn is_live(&self) -> bool {
        self.0.strong_count() > 0
    }

    pub(super) fn points_to(&self, dentry: &Dentry) -> bool {
        self.0
            .upgrade()
            .is_some_and(|inner| Arc::ptr_eq(&inner, &dentry.0))
    }
}

/// Strong reference to a VFS directory entry.
#[derive(Debug, Clone)]
pub struct Dentry(Arc<DentryInner>);

impl Dentry {
    /// Gets the inode number of the instantiated dentry.
    pub fn inode(&self) -> u64 {
        self.vfs_inode().inode()
    }

    /// Returns metadata for the instantiated dentry.
    pub fn metadata(&self) -> Metadata {
        self.vfs_inode().metadata()
    }

    /// Increments the instantiated inode link count.
    pub fn increment_link_count(&self) {
        self.vfs_inode().increment_link_count();
    }

    /// Decrements the instantiated inode link count.
    pub fn decrement_link_count(&self) {
        self.vfs_inode().decrement_link_count();
    }

    /// Updates the instantiated inode change time to the current time.
    pub fn set_changed_at_to_now(&self) {
        self.vfs_inode().set_changed_at_to_now();
    }

    /// Updates metadata for the instantiated dentry.
    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.vfs_inode().update_metadata(self, update)
    }

    #[allow(clippy::len_without_is_empty)]
    /// Gets the byte length of the instantiated dentry.
    pub fn len(&self) -> VfsResult<u64> {
        self.vfs_inode().len()
    }

    /// Returns inode flags.
    pub fn node_flags(&self) -> NodeFlags {
        self.vfs_inode().flags()
    }

    /// Synchronizes the instantiated dentry's inode data.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.vfs_inode().sync(data_only)
    }
}

/// Returns the inode attached to a positive dentry.
pub(crate) fn d_inode(dentry: &Dentry) -> Arc<VfsInode> {
    dentry.vfs_inode()
}

/// Returns whether a dentry is negative by dentry type.
pub(crate) fn d_is_negative(dentry: &Dentry) -> bool {
    dentry.is_negative()
}

/// Returns whether a dentry has an inode attached.
pub(crate) fn d_really_is_positive(dentry: &Dentry) -> bool {
    dentry.is_really_positive()
}

/// Returns whether a dentry is directory-like by dentry type.
pub(crate) fn d_is_dir(dentry: &Dentry) -> bool {
    dentry.is_dir_entry()
}

/// symlink dentry predicate.
pub(crate) fn d_is_symlink(dentry: &Dentry) -> bool {
    dentry.is_symlink_entry()
}

impl Dentry {
    fn verify_child_name(name: &str) -> VfsResult<()> {
        if name.is_empty() || name == DOT || name == DOTDOT {
            return Err(VfsError::InvalidInput);
        }
        if name.contains('/') || name.contains('\0') {
            return Err(VfsError::InvalidInput);
        }
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        Ok(())
    }

    fn new_inner(inode: Arc<VfsInode>, parent: Option<Dentry>, name: String) -> Self {
        let flags = DentryFlags::for_inode(&inode);
        let dentry = Self(Arc::new(DentryInner::new(flags, Some(inode), parent, name)));
        dentry.vfs_inode().add_dentry_alias(&dentry);
        if let Some(super_block) = dentry.super_block() {
            dentry.vfs_inode().bind_super_block(&super_block);
        }
        dentry
    }

    /// Construct a negative dentry for pathname lookup.
    pub fn new_negative(parent: Option<Dentry>, name: String) -> Self {
        Self(Arc::new(DentryInner::new(
            DentryFlags::MISS_TYPE,
            None,
            parent,
            name,
        )))
    }

    /// Construct a file entry that points at an existing inode identity.
    pub fn new_file_from_inode(inode: Arc<VfsInode>, parent: Option<Dentry>, name: String) -> Self {
        debug_assert!(inode.is_file());
        Self::new_inner(inode, parent, name)
    }

    /// Construct a directory entry that points at an existing inode identity.
    pub fn new_dir_from_inode(inode: Arc<VfsInode>, parent: Option<Dentry>, name: String) -> Self {
        debug_assert!(inode.is_dir());
        Self::new_inner(inode, parent, name)
    }

    /// Attempt to downcast the entry to a concrete node type.
    pub fn downcast<T: Any + Send + Sync>(&self) -> VfsResult<Arc<T>> {
        self.vfs_inode().downcast()
    }

    /// Return the inode identity referenced by this directory entry.
    pub(crate) fn vfs_inode(&self) -> Arc<VfsInode> {
        self.0
            .inode
            .lock()
            .clone()
            .expect("negative dentry does not have an inode")
    }

    /// Returns this dentry's flags.
    pub(crate) fn dentry_flags(&self) -> DentryFlags {
        *self.0.flags.lock()
    }

    fn operations(&self) -> Option<Arc<dyn DentryOperations>> {
        self.0.operations.lock().clone()
    }

    /// Returns this dentry's dynamic display name, if its operation table provides one.
    pub fn dynamic_name(&self) -> VfsResult<Option<String>> {
        let Some(operations) = self.operations() else {
            return Ok(None);
        };
        operations.d_dname(self)
    }

    /// Returns whether this dentry is currently negative by type flags.
    pub fn is_negative(&self) -> bool {
        self.dentry_flags().is_miss_type()
    }

    /// Returns whether this dentry is currently positive by type flags.
    pub fn is_positive(&self) -> bool {
        !self.is_negative()
    }

    /// Returns whether this dentry has no inode attached.
    pub fn is_really_negative(&self) -> bool {
        self.0.inode.lock().is_none()
    }

    /// Returns whether this dentry has an inode attached.
    pub fn is_really_positive(&self) -> bool {
        !self.is_really_negative()
    }

    /// Returns whether this dentry can be used for child lookup.
    pub fn can_lookup(&self) -> bool {
        self.dentry_flags().is_directory_type()
    }

    /// Returns whether this dentry is an automatically handled directory.
    pub fn is_autodir(&self) -> bool {
        self.dentry_flags().is_autodir_type()
    }

    /// Returns whether this dentry is directory-like by dentry type.
    pub fn is_dir_entry(&self) -> bool {
        self.can_lookup() || self.is_autodir()
    }

    /// Returns whether this dentry is a regular-file dentry.
    pub fn is_regular_entry(&self) -> bool {
        self.dentry_flags().is_regular_type()
    }

    /// Returns whether this dentry is a special-file dentry.
    pub fn is_special_entry(&self) -> bool {
        self.dentry_flags().is_special_type()
    }

    /// Returns whether this dentry is file-like by dentry type.
    pub fn is_file_entry(&self) -> bool {
        self.is_regular_entry() || self.is_special_entry()
    }

    /// Returns whether this dentry is a symbolic-link dentry.
    pub fn is_symlink_entry(&self) -> bool {
        self.dentry_flags().is_symlink_type()
    }

    /// Installs dentry operations.
    pub(crate) fn set_operations(&self, operations: Arc<dyn DentryOperations>) {
        *self.0.operations.lock() = Some(operations);
    }

    /// Returns this dentry's superblock, if it is already attached.
    pub(crate) fn super_block(&self) -> Option<Arc<SuperBlock>> {
        self.0.super_block.lock().as_ref().and_then(Weak::upgrade)
    }

    /// Attaches this dentry subtree to a superblock.
    pub(crate) fn bind_super_block(&self, super_block: &Arc<SuperBlock>) {
        if let Some(inode) = self.0.inode.lock().clone() {
            inode.bind_super_block(super_block);
        }
        {
            let weak = Arc::downgrade(super_block);
            let mut slot = self.0.super_block.lock();
            if slot
                .as_ref()
                .is_none_or(|existing| existing.upgrade().is_none())
            {
                *slot = Some(weak);
            }
        }

        let children: Vec<_> = self.0.children.lock().values().cloned().collect();
        for child in children {
            child.bind_super_block(super_block);
        }
    }

    /// Returns the cache key for this entry.
    pub(crate) fn key(&self) -> DentryKey {
        let parent = self.0.parent.as_ref().map_or(0, Dentry::as_ptr);
        DentryKey::new(parent, self.0.name.clone())
    }

    /// Returns the node type of this entry.
    pub fn node_type(&self) -> NodeType {
        self.vfs_inode().node_type()
    }

    /// Returns the parent directory entry, if any.
    pub fn parent(&self) -> Option<Self> {
        self.0.parent.clone()
    }

    /// Returns the entry name within its parent directory.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// Checks if the entry is a root of a mount point.
    pub fn is_root_of_mount(&self) -> bool {
        self.0.parent.is_none()
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
        self.vfs_inode().is_file()
    }

    /// Returns `true` if this entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.vfs_inode().is_dir()
    }

    /// Returns this entry if it is a directory.
    pub fn as_dir(&self) -> VfsResult<&Self> {
        if self.is_dir() {
            Ok(self)
        } else {
            Err(VfsError::NotADirectory)
        }
    }

    /// Returns `true` if two entries point to the same node.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Returns `true` if two entries point to the same inode identity.
    pub fn is_same_inode(&self, other: &Self) -> bool {
        let this = self.vfs_inode();
        let other = other.vfs_inode();
        Arc::ptr_eq(&this, &other)
    }

    /// Returns the raw pointer value for this entry.
    pub fn as_ptr(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// Read the symlink target as a string.
    pub fn read_link(&self) -> VfsResult<String> {
        self.vfs_inode().read_link(self)
    }

    fn can_cache_children(&self) -> bool {
        !self.node_flags().contains(NodeFlags::NON_CACHEABLE)
    }

    fn can_cache_as_child(&self) -> bool {
        let flags = self.dentry_flags();
        !flags.contains(DentryFlags::DONTCACHE) || flags.contains(DentryFlags::PERSISTENT)
    }

    fn forget_cache_entry(&self, name: &str) {
        if let Some(entry) = self.0.children.lock().remove(name)
            && entry.is_really_positive()
            && let Ok(dir) = entry.as_dir()
        {
            dir.forget();
        }
    }

    /// Looks up a directory entry by name in this dentry's cache.
    pub fn lookup_cache(&self, name: &str) -> Option<Dentry> {
        if self.can_cache_children() {
            self.0.children.lock().get(name).cloned()
        } else {
            None
        }
    }

    /// Inserts a child dentry into this dentry's cache.
    pub fn insert_cache(&self, name: String, entry: Dentry) -> Option<Dentry> {
        if self.can_cache_children() && entry.can_cache_as_child() {
            self.0.children.lock().insert(name, entry)
        } else {
            None
        }
    }

    /// Looks up a child dentry below this directory.
    pub fn lookup(&self, name: &str) -> VfsResult<Dentry> {
        self.as_dir()?;
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if let Some(entry) = self.lookup_cache(name) {
            return if entry.is_negative() {
                Err(VfsError::NotFound)
            } else {
                Ok(entry)
            };
        }

        let entry = self.vfs_inode().lookup(self, name)?;
        if self.can_cache_children() && entry.can_cache_as_child() {
            self.0
                .children
                .lock()
                .entry(name.to_owned())
                .or_insert_with(|| entry.clone());
        }
        if entry.is_negative() {
            Err(VfsError::NotFound)
        } else {
            Ok(entry)
        }
    }

    /// Creates a regular-file child dentry below this directory.
    pub fn create(&self, name: &str, permission: NodePermission) -> VfsResult<Dentry> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let entry = self.vfs_inode().create(self, name, permission)?;
        self.insert_cache(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Creates a directory child dentry below this directory.
    pub fn mkdir(&self, name: &str, permission: NodePermission) -> VfsResult<Dentry> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let entry = self.vfs_inode().mkdir(self, name, permission)?;
        self.insert_cache(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Creates a special child dentry below this directory.
    pub fn mknod(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
        device: DeviceId,
    ) -> VfsResult<Dentry> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let entry = self
            .vfs_inode()
            .mknod(self, name, node_type, permission, device)?;
        self.insert_cache(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Creates a symbolic-link child dentry below this directory.
    pub fn symlink(&self, name: &str, target: &str) -> VfsResult<Dentry> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let entry = self.vfs_inode().symlink(self, name, target)?;
        self.insert_cache(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Creates a hard link below this directory.
    pub fn link(&self, name: &str, node: &Dentry) -> VfsResult<Dentry> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let entry = self.vfs_inode().link(self, name, node)?;
        self.insert_cache(name.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Unlinks a child dentry by name.
    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let entry = self.lookup(name)?;
        match (entry.is_dir(), is_dir) {
            (true, false) => return Err(VfsError::IsADirectory),
            (false, true) => return Err(VfsError::NotADirectory),
            _ => {}
        }

        self.vfs_inode().unlink(&entry)?;
        self.forget_cache_entry(name);
        Ok(())
    }

    /// Returns whether the directory contains children.
    pub fn has_children(&self) -> VfsResult<bool> {
        self.as_dir()?;
        Ok(self.has_positive_children())
    }

    pub(crate) fn has_positive_children(&self) -> bool {
        self.0
            .children
            .lock()
            .iter()
            .any(|(name, entry)| name != DOT && name != DOTDOT && entry.is_really_positive())
    }

    /// Renames a child dentry from this directory to `dst_dir`.
    pub fn rename(
        &self,
        src_name: &str,
        dst_dir: &Self,
        dst_name: &str,
        flags: RenameFlags,
    ) -> VfsResult<()> {
        self.as_dir()?;
        dst_dir.as_dir()?;
        Self::verify_child_name(src_name)?;
        Self::verify_child_name(dst_name)?;

        let src = self.lookup(src_name)?;
        let dst = match dst_dir.lookup(dst_name) {
            Ok(dst) => {
                if src.node_type() == NodeType::Directory {
                    if dst.as_dir().is_ok() && dst.has_children()? {
                        return Err(VfsError::DirectoryNotEmpty);
                    }
                } else if dst.node_type() == NodeType::Directory {
                    return Err(VfsError::IsADirectory);
                }
                dst
            }
            Err(err) if err.canonicalize() == VfsError::NotFound => {
                Dentry::new_negative(Some(dst_dir.clone()), dst_name.to_owned())
            }
            Err(err) => return Err(err),
        };

        self.vfs_inode()
            .rename(&src, &dst_dir.vfs_inode(), &dst, flags)?;
        self.forget_cache_entry(src_name);
        dst_dir.forget_cache_entry(dst_name);
        Ok(())
    }

    /// Emits entries currently present in this dentry's child cache.
    pub fn emit_cached_dirents(
        &self,
        offset: u64,
        sink: &mut dyn DirEntrySink,
    ) -> VfsResult<usize> {
        self.as_dir()?;
        let parent = self.parent();
        let mut children: Vec<_> = self
            .0
            .children
            .lock()
            .iter()
            .filter(|(_, entry)| entry.is_really_positive())
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect();
        children.sort_by(|left, right| left.0.cmp(&right.0));

        let mut count = 0;
        let mut index = 0_u64;
        let mut emit = |name: &str, entry: &Dentry, index: &mut u64| -> VfsResult<bool> {
            let current = *index;
            *index += 1;
            if current < offset {
                return Ok(true);
            }
            let metadata = entry.vfs_inode().metadata();
            Ok(sink.accept(name, metadata.inode, metadata.mode.node_type(), current + 1))
        };

        if emit(DOT, self, &mut index)? {
            count += (index > offset) as usize;
        } else {
            return Ok(count);
        }

        let parent_entry = parent.as_ref().unwrap_or(self);
        if emit(DOTDOT, parent_entry, &mut index)? {
            count += (index > offset) as usize;
        } else {
            return Ok(count);
        }

        for (name, entry) in children {
            if !emit(&name, &entry, &mut index)? {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Clears cached child dentries and per-dentry data recursively.
    pub(crate) fn forget(&self) {
        let children: Vec<_> = mem::take(&mut *self.0.children.lock())
            .into_values()
            .collect();
        for child in children {
            if child.is_really_positive()
                && let Ok(dir) = child.as_dir()
            {
                dir.forget();
            }
        }
    }
}

#[cfg(unittest)]
mod tests_dentry {
    use alloc::{borrow::ToOwned, string::String, sync::Arc, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use unittest::{assert, def_test};

    use super::{Dentry, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive};
    use crate::{
        AddressSpaceOperations, DirContext, FileDirOperations, FileOperations, InodeCache,
        InodeDirOperations, InodeOperations, InodeSymlinkOperations, Metadata, MetadataUpdate,
        NodeFlags, NodePermission, NodeType, StatFs, SuperBlockOperations, VfsError, VfsFile,
        VfsInode, VfsInodeInit, VfsResult,
    };

    struct MockFilesystem;

    impl SuperBlockOperations for MockFilesystem {
        fn name(&self) -> &str {
            "mockfs"
        }

        fn root_dentry(&self) -> Dentry {
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
                name_length: 255,
                fragment_size: 0,
                mount_flags: 0,
            })
        }
    }

    struct MockFileOps {
        inode: u64,
        node_type: NodeType,
        data: crate::Mutex<Vec<u8>>,
        owner: crate::Mutex<Option<(u32, u32)>>,
        update_count: AtomicUsize,
    }

    impl MockFileOps {
        fn new(_fs: Arc<MockFilesystem>, inode: u64, data: &[u8]) -> Self {
            Self::new_with_type(inode, NodeType::RegularFile, data)
        }

        fn new_with_type(inode: u64, node_type: NodeType, data: &[u8]) -> Self {
            Self {
                inode,
                node_type,
                data: crate::Mutex::new(data.to_vec()),
                owner: crate::Mutex::new(None),
                update_count: AtomicUsize::new(0),
            }
        }
    }

    impl InodeOperations for MockFileOps {
        fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
            if self.node_type == NodeType::Symlink {
                Some(self)
            } else {
                None
            }
        }

        fn getattr(
            &self,
            _idmap: &crate::MountIdmap,
            _path: Option<&crate::Path>,
            _request_mask: crate::GetattrRequestMask,
            _query_flags: crate::GetattrQueryFlags,
        ) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.inode,
                nlink: 1,
                mode: crate::Umode::new(self.node_type, NodePermission::default()),
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

        fn setattr(
            &self,
            _idmap: &crate::MountIdmap,
            _dentry: &Dentry,
            update: MetadataUpdate,
        ) -> VfsResult<()> {
            *self.owner.lock() = update.owner;
            self.update_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    impl InodeSymlinkOperations for MockFileOps {
        fn get_link(
            &self,
            _dentry: Option<&Dentry>,
            _inode: &VfsInode,
            _done: &mut crate::DelayedCall,
        ) -> VfsResult<String> {
            String::from_utf8(self.data.lock().clone()).map_err(|_| VfsError::InvalidData)
        }
    }

    impl FileOperations for MockFileOps {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
            let data = self.data.lock();
            let start = offset as usize;
            if start >= data.len() {
                return Ok(0);
            }
            let count = buf.len().min(data.len() - start);
            buf[..count].copy_from_slice(&data[start..start + count]);
            Ok(count)
        }

        fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
            let mut data = self.data.lock();
            let start = offset as usize;
            if start + buf.len() > data.len() {
                data.resize(start + buf.len(), 0);
            }
            data[start..start + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }
    }

    impl AddressSpaceOperations for MockFileOps {
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
    }

    struct MockDirOps {
        inode: u64,
    }

    impl MockDirOps {
        fn new(_fs: Arc<MockFilesystem>, inode: u64) -> Self {
            Self { inode }
        }
    }

    impl InodeOperations for MockDirOps {
        fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
            Some(self)
        }

        fn getattr(
            &self,
            _idmap: &crate::MountIdmap,
            _path: Option<&crate::Path>,
            _request_mask: crate::GetattrRequestMask,
            _query_flags: crate::GetattrQueryFlags,
        ) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.inode,
                nlink: 1,
                mode: crate::Umode::new(NodeType::Directory, NodePermission::default()),
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

        fn setattr(
            &self,
            _idmap: &crate::MountIdmap,
            _dentry: &Dentry,
            _update: MetadataUpdate,
        ) -> VfsResult<()> {
            Ok(())
        }
    }

    impl InodeDirOperations for MockDirOps {
        fn lookup(
            &self,
            _dir: &VfsInode,
            _dentry: &Dentry,
            _flags: crate::InodeLookupFlags,
        ) -> VfsResult<Dentry> {
            Err(VfsError::NotFound)
        }

        fn create(
            &self,
            _idmap: &crate::MountIdmap,
            _dir: &VfsInode,
            _dentry: &Dentry,
            _mode: crate::Umode,
            _exclusive: bool,
        ) -> VfsResult<Dentry> {
            Err(VfsError::OperationNotSupported)
        }

        fn link(
            &self,
            _old_dentry: &Dentry,
            _dir: &VfsInode,
            _new_dentry: &Dentry,
        ) -> VfsResult<Dentry> {
            Err(VfsError::OperationNotSupported)
        }

        fn unlink(&self, _dir: &VfsInode, _dentry: &Dentry) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }

        fn rename(
            &self,
            _idmap: &crate::MountIdmap,
            _old_dir: &VfsInode,
            _old_dentry: &Dentry,
            _new_dir: &VfsInode,
            _new_dentry: &Dentry,
            _flags: crate::RenameFlags,
        ) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }
    }

    impl FileOperations for MockDirOps {
        fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
            Some(self)
        }

        fn supports_read(&self) -> bool {
            true
        }

        fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Err(VfsError::IsADirectory)
        }
    }

    impl FileDirOperations for MockDirOps {
        fn iterate_shared(
            &self,
            _file: &crate::VfsFile,
            _ctx: &mut DirContext<'_>,
        ) -> VfsResult<usize> {
            Ok(0)
        }
    }

    fn make_file_entry(
        fs: Arc<MockFilesystem>,
        inode: u64,
        parent: Option<Dentry>,
        name: &str,
    ) -> (Dentry, Arc<MockFileOps>) {
        let ops = Arc::new(MockFileOps::new(fs, inode, b"payload"));
        let inode = VfsInode::new_file(
            ops.clone(),
            inode_init(inode, NodeType::RegularFile, b"payload".len() as u64),
        );
        let entry = Dentry::new_file_from_inode(inode, parent, String::from(name));
        (entry, ops)
    }

    fn make_dir_entry(fs: Arc<MockFilesystem>, inode: u64, name: &str) -> Dentry {
        make_dir_entry_with_parent(fs, inode, None, name)
    }

    fn make_dir_entry_with_parent(
        fs: Arc<MockFilesystem>,
        inode: u64,
        parent: Option<Dentry>,
        name: &str,
    ) -> Dentry {
        let inode = VfsInode::new_openable_dir(
            Arc::new(MockDirOps::new(fs, inode)),
            inode_init(inode, NodeType::Directory, 0),
        );
        Dentry::new_dir_from_inode(inode, parent, String::from(name))
    }

    fn inode_init(inode: u64, node_type: NodeType, size: u64) -> VfsInodeInit {
        VfsInodeInit::new(
            inode,
            size,
            crate::Umode::new(node_type, NodePermission::default()),
        )
        .with_owner_links_and_rdev(0, 0, 1, Default::default())
        .with_stat_data(4096, 1, Duration::ZERO, Duration::ZERO, Duration::ZERO)
    }

    #[def_test]
    fn test_dentry_file_helpers_and_private_data() {
        let fs = Arc::new(MockFilesystem);
        let (entry, ops) = make_file_entry(fs, 2, None, "leaf");

        assert!(entry.is_file());
        assert!(!entry.is_dir());
        assert_eq!(entry.name(), "leaf");
        assert_eq!(entry.node_type(), NodeType::RegularFile);
        assert!(matches!(entry.as_dir(), Err(VfsError::NotADirectory)));
        assert_eq!(entry.inode(), 2);

        let metadata = d_inode(&entry).metadata();
        assert_eq!(metadata.mode.node_type(), NodeType::RegularFile);
        assert_eq!(metadata.size, 7);

        assert_eq!(ops.update_count.load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn test_dentry_positive_and_negative_helpers() {
        let negative = Dentry::new_negative(None, String::from("missing"));
        assert!(d_is_negative(&negative));
        assert!(!negative.is_positive());
        assert!(!d_is_dir(&negative));
        assert!(!negative.is_regular_entry());
        assert!(!negative.is_special_entry());
        assert!(!d_is_symlink(&negative));
        assert!(negative.is_really_negative());
        assert!(!d_really_is_positive(&negative));

        let _fs = Arc::new(MockFilesystem);
        let unknown_inode = VfsInode::new_file(
            Arc::new(MockFileOps::new_with_type(
                23,
                NodeType::Unknown,
                b"unknown",
            )),
            inode_init(23, NodeType::Unknown, b"unknown".len() as u64),
        );
        let unknown = Dentry::new_file_from_inode(unknown_inode, None, String::from("unknown"));
        assert!(!d_is_negative(&unknown));
        assert!(unknown.is_positive());
        assert!(!d_is_dir(&unknown));
        assert!(!unknown.is_regular_entry());
        assert!(unknown.is_special_entry());
        assert!(unknown.is_file_entry());
        assert!(!d_is_symlink(&unknown));
        assert!(!unknown.is_really_negative());
        assert!(d_really_is_positive(&unknown));
    }

    #[def_test]
    fn test_forget_cache_entry_handles_negative_dentry() {
        let fs = Arc::new(MockFilesystem);
        let root = make_dir_entry(fs, 20, "");
        let negative = Dentry::new_negative(Some(root.clone()), String::from("missing"));

        root.insert_cache(negative.name().to_owned(), negative.clone());
        assert!(root.lookup_cache("missing").unwrap().ptr_eq(&negative));

        root.forget_cache_entry("missing");
        assert!(root.lookup_cache("missing").is_none());
    }

    #[def_test]
    fn test_distinct_entries_create_distinct_inode_identities() {
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
        let first = cache.get_or_insert_file_with_init(
            NodeFlags::empty(),
            inode_init(50, NodeType::RegularFile, b"first".len() as u64),
            || Arc::new(MockFileOps::new(fs, 50, b"first")),
        );
        let second = cache.get_or_insert_file_with_init(
            NodeFlags::empty(),
            inode_init(50, NodeType::RegularFile, b"second".len() as u64),
            || -> Arc<MockFileOps> { panic!("live inode should be reused") },
        );

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
        let root = make_dir_entry(fs.clone(), 10, "");
        let child = make_dir_entry_with_parent(fs.clone(), 11, Some(root.clone()), "child");
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
        let symlink_operations = Arc::new(MockFileOps::new_with_type(
            21,
            NodeType::Symlink,
            b"/tmp/target",
        ));
        let symlink_inode = VfsInode::new_file(
            symlink_operations.clone(),
            inode_init(21, NodeType::Symlink, b"/tmp/target".len() as u64),
        );
        let symlink = Dentry::new_file_from_inode(symlink_inode, None, String::from("ln"));
        let dir = make_dir_entry(fs, 22, "dir");

        assert!(matches!(regular.read_link(), Err(VfsError::InvalidData)));
        assert_eq!(symlink.read_link().unwrap(), "/tmp/target");
        assert!(regular.downcast::<MockFileOps>().is_ok());
        assert!(matches!(
            symlink.downcast::<MockDirOps>(),
            Err(VfsError::InvalidInput)
        ));
        assert!(dir.as_dir().is_ok());
    }

    #[def_test]
    fn test_dentry_rejects_invalid_child_names() {
        for name in ["", ".", "..", "a/b", "a\0b"] {
            assert!(Dentry::verify_child_name(name).is_err());
        }
        assert!(Dentry::verify_child_name("file").is_ok());
    }

    #[def_test]
    fn test_dentry_key_uniqueness() {
        let fs = Arc::new(MockFilesystem);
        let (entry1, _) = make_file_entry(fs.clone(), 1, None, "file1.txt");
        let (entry2, _) = make_file_entry(fs.clone(), 2, None, "file2.txt");
        let (entry3, _) = make_file_entry(fs, 3, None, "file1.txt");

        assert_ne!(entry1.key(), entry2.key());
        assert_eq!(entry1.key(), entry3.key());
    }
}
