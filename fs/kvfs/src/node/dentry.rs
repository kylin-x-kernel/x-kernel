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
use core::{
    any::Any,
    fmt,
    hash::{Hash, Hasher},
    iter, mem,
    ops::Deref,
};

use bitflags::bitflags;
use hashbrown::HashMap;
use klazy::Once;

use super::{DirEntrySink, NodeFlags, VfsInode};
use crate::{
    Metadata, MetadataUpdate, Mutex, NodeType, RenameFlags, RwLock, RwLockReadGuard, SuperBlock,
    VfsError, VfsResult,
    path::{DOT, DOTDOT, MAX_NAME_LEN, PathBuf},
};

bitflags! {
    /// Dentry state flags.
    #[derive(Debug, Clone, Copy)]
    pub struct DentryFlags: u32 {
        const PAR_LOOKUP = 1 << 4;
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
#[derive(Clone, Debug)]
pub struct DentryKey {
    parent: Option<Weak<DentryInner>>,
    name: String,
}

impl DentryKey {
    fn new(parent: Option<Weak<DentryInner>>, name: String) -> Self {
        Self { parent, name }
    }
}

impl PartialEq for DentryKey {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && match (&self.parent, &other.parent) {
                (Some(parent), Some(other_parent)) => Weak::ptr_eq(parent, other_parent),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for DentryKey {}

impl Hash for DentryKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent.is_some().hash(state);
        if let Some(parent) = &self.parent {
            parent.as_ptr().hash(state);
        }
        self.name.hash(state);
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

#[derive(Clone)]
struct DentryLocation {
    parent: Option<Dentry>,
    name: String,
}

struct DentryInner {
    flags: Mutex<DentryFlags>,
    lookup_mutex: Mutex<()>,
    inode: Mutex<Option<Arc<VfsInode>>>,
    location: RwLock<DentryLocation>,
    operations: Mutex<Option<Arc<dyn DentryOperations>>>,
    super_block: Once<Weak<SuperBlock>>,
    // A live child pins its parent, while the parent only indexes children.
    // The superblock dentry cache owns hashed children until explicit eviction.
    children: Mutex<HashMap<String, Weak<DentryInner>>>,
}

impl DentryInner {
    fn new(
        flags: DentryFlags,
        inode: Option<Arc<VfsInode>>,
        parent: Option<Dentry>,
        name: String,
    ) -> Self {
        let super_block = Once::new();
        if let Some(parent_super_block) = parent.as_ref().and_then(Dentry::super_block) {
            super_block.call_once(|| Arc::downgrade(&parent_super_block));
        }
        Self {
            flags: Mutex::new(flags),
            lookup_mutex: Mutex::default(),
            inode: Mutex::new(inode),
            location: RwLock::new(DentryLocation { parent, name }),
            operations: Mutex::default(),
            super_block,
            children: Mutex::default(),
        }
    }
}

impl fmt::Debug for DentryInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let location = self.location.read();
        f.debug_struct("DentryInner")
            .field("flags", &*self.flags.lock())
            .field("inode", &*self.inode.lock())
            .field("name", &location.name)
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

    pub(super) fn upgrade(&self) -> Option<Dentry> {
        self.0.upgrade().map(Dentry)
    }
}

/// Strong reference to a VFS directory entry.
#[derive(Debug, Clone)]
pub struct Dentry(Arc<DentryInner>);

/// A dentry view protected by the VFS namespace operation that created it.
///
/// Filesystem namespace callbacks receive this type when they need to read the
/// dentry name. The name borrow is tied to the location guard, so callbacks can
/// inspect `d_name` without cloning while safe Rust prevents the borrow from
/// escaping the protected operation.
pub struct LockedDentry<'a> {
    dentry: &'a Dentry,
    location: RwLockReadGuard<'a, DentryLocation>,
}

struct RenameData<'a> {
    old_parent: &'a Dentry,
    old_name: &'a str,
    new_parent: &'a Dentry,
    new_name: &'a str,
    flags: RenameFlags,
}

pub(crate) enum LookupCreateResult {
    Existing(Dentry),
    Created(Dentry),
}

impl LockedDentry<'_> {
    /// Returns the dentry name within its parent directory.
    pub fn name(&self) -> &str {
        &self.location.name
    }

    /// Returns the parent directory entry, if any.
    pub fn parent(&self) -> Option<Dentry> {
        self.location.parent.clone()
    }

    /// Returns the protected dentry.
    pub fn as_dentry(&self) -> &Dentry {
        self.dentry
    }

    /// Attaches an inode to this negative transaction dentry.
    pub fn instantiate(&self, inode: Arc<VfsInode>) -> VfsResult<()> {
        self.dentry.instantiate(inode)
    }

    /// Instantiates this lookup dentry or returns an existing directory alias.
    pub fn instantiate_or_alias(&self, inode: Arc<VfsInode>) -> VfsResult<Option<Dentry>> {
        if inode.is_dir()
            && let Some(alias) = inode.directory_alias()
        {
            return Ok(Some(alias));
        }
        self.dentry.instantiate(inode)?;
        Ok(None)
    }
}

impl Deref for LockedDentry<'_> {
    type Target = Dentry;

    fn deref(&self) -> &Self::Target {
        self.dentry
    }
}

impl Dentry {
    /// Gets the inode number of the instantiated dentry.
    pub fn inode(&self) -> u64 {
        self.vfs_inode().inode()
    }

    /// Returns metadata for the instantiated dentry.
    pub fn metadata(&self) -> Metadata {
        self.vfs_inode().metadata()
    }

    /// Refreshes cached inode metadata after a completed backing-store mutation.
    ///
    /// The supplied metadata must describe the inode identity already attached
    /// to this dentry. Immutable identity and geometry fields are validated
    /// before any cached attributes are changed.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::NotFound`] when this is a negative dentry, or
    /// [`VfsError::InvalidInput`] when the metadata does not match the attached
    /// inode identity.
    pub fn update_metadata_after_backing_change(&self, metadata: &Metadata) -> VfsResult<()> {
        let inode = self.0.inode.lock().clone().ok_or(VfsError::NotFound)?;
        inode.update_metadata_after_backing_change(metadata)
    }

    /// Increments the instantiated inode link count.
    pub fn increment_link_count(&self) {
        self.vfs_inode().increment_link_count();
    }

    /// Decrements the instantiated inode link count.
    pub fn decrement_link_count(&self) {
        self.vfs_inode().decrement_link_count();
    }

    /// Sets the instantiated inode link count from a backing-filesystem
    /// namespace mutation.
    pub fn set_link_count(&self, link_count: u64) {
        self.vfs_inode().set_link_count(link_count);
    }

    /// Sets the instantiated inode change time from a backing-filesystem
    /// namespace mutation.
    pub fn set_changed_at(&self, timestamp: ktime_types::SystemTime) {
        self.vfs_inode().set_changed_at(timestamp);
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
        let dentry = Self(Arc::new(DentryInner::new(
            flags,
            Some(inode.clone()),
            parent,
            name,
        )));
        if !inode.add_dentry_alias(&dentry) {
            let alias = inode
                .directory_alias()
                .expect("directory alias disappeared while constructing dentry");
            let requested_location = dentry.0.location.read();
            let location = alias.0.location.read();
            let has_same_parent = match (&location.parent, &requested_location.parent) {
                (Some(existing), Some(requested)) => existing.ptr_eq(requested),
                (None, None) => true,
                _ => false,
            };
            assert!(
                has_same_parent && location.name == requested_location.name,
                "directory inode already has a live dentry alias"
            );
            drop(location);
            drop(requested_location);
            return alias;
        }
        if let Some(super_block) = dentry.super_block() {
            inode.bind_super_block(&super_block);
        }
        dentry
    }

    fn instantiate(&self, inode: Arc<VfsInode>) -> VfsResult<()> {
        let mut attached = self.0.inode.lock();
        if attached.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        if !inode.add_dentry_alias(self) {
            return Err(VfsError::InvalidInput);
        }
        let mut flags = self.0.flags.lock();
        let retained =
            *flags & (DentryFlags::PAR_LOOKUP | DentryFlags::DONTCACHE | DentryFlags::PERSISTENT);
        *flags = DentryFlags::for_inode(&inode) | retained;
        drop(flags);
        *attached = Some(inode.clone());
        drop(attached);
        if let Some(super_block) = self.super_block() {
            inode.bind_super_block(&super_block);
        }
        Ok(())
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
    ///
    /// Reuses an existing alias at the same location. A directory inode at a
    /// different live location violates the VFS single-alias invariant.
    pub fn new_dir_from_inode(inode: Arc<VfsInode>, parent: Option<Dentry>, name: String) -> Self {
        debug_assert!(inode.is_dir());
        Self::new_inner(inode, parent, name)
    }

    /// Attempt to downcast the entry to a concrete node type.
    pub fn downcast<T: Any + Send + Sync>(&self) -> VfsResult<Arc<T>> {
        self.vfs_inode().downcast()
    }

    /// Returns the inode identity referenced by this positive directory entry.
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
        self.0.super_block.poll().and_then(Weak::upgrade)
    }

    /// Attaches this dentry subtree to a superblock.
    pub(crate) fn bind_super_block(&self, super_block: &Arc<SuperBlock>) {
        let inode = self.0.inode.lock().clone();
        if let Some(inode) = &inode {
            inode.bind_super_block(super_block);
        }
        let requested = Arc::downgrade(super_block);
        let bound = self.0.super_block.call_once(|| requested.clone());
        // Linux assigns `d_sb` at allocation and never rebinds a dentry.
        assert!(
            Weak::ptr_eq(bound, &requested),
            "one VFS dentry identity must not belong to multiple superblocks"
        );

        // Only publish children into the dcache when they are cacheable,
        // mirroring `insert_cache`. `bind_super_block` must not defeat
        // `NON_CACHEABLE` filesystems (e.g. procfs), where every child dentry
        // would otherwise be retained by the superblock dcache forever.
        // Negative dentries carry no inode; `can_cache_children` requires one,
        // so check the already-held inode's flags directly instead.
        let cache_children = inode
            .as_ref()
            .is_some_and(|inode| !inode.flags().contains(NodeFlags::NON_CACHEABLE));
        let children: Vec<_> = self.0.children.lock().values().cloned().collect();
        for child in children {
            if let Some(child) = child.upgrade().map(Dentry) {
                child.bind_super_block(super_block);
                if cache_children && child.can_cache_as_child() {
                    super_block.cache_dentry(child);
                }
            }
        }
    }

    /// Returns the cache key for this entry.
    pub(crate) fn key(&self) -> DentryKey {
        let location = self.0.location.read();
        let parent = location
            .parent
            .as_ref()
            .map(|parent| Arc::downgrade(&parent.0));
        DentryKey::new(parent, location.name.clone())
    }

    /// Returns the node type of this entry.
    pub fn node_type(&self) -> NodeType {
        self.vfs_inode().node_type()
    }

    /// Returns the parent directory entry, if any.
    pub fn parent(&self) -> Option<Self> {
        self.0.location.read().parent.clone()
    }

    /// Returns a snapshot of this entry's name within its parent directory.
    pub fn name_snapshot(&self) -> String {
        self.0.location.read().name.clone()
    }

    pub(crate) fn lock_location(&self) -> LockedDentry<'_> {
        LockedDentry {
            dentry: self,
            location: self.0.location.read(),
        }
    }

    /// Checks if the entry is a root of a mount point.
    pub fn is_root_of_mount(&self) -> bool {
        self.0.location.read().parent.is_none()
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
            components.push(current.name_snapshot());
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

    fn is_parallel_lookup(&self) -> bool {
        self.dentry_flags().contains(DentryFlags::PAR_LOOKUP)
    }

    fn begin_parallel_lookup(&self) {
        self.0.flags.lock().insert(DentryFlags::PAR_LOOKUP);
    }

    fn end_parallel_lookup(&self) {
        self.0.flags.lock().remove(DentryFlags::PAR_LOOKUP);
    }

    fn lookup_cache_entry(&self, name: &str) -> Option<Dentry> {
        let mut children = self.0.children.lock();
        let entry = children
            .get(name)
            .and_then(|entry| entry.upgrade().map(Dentry));
        if entry.is_none() {
            children.remove(name);
        }
        entry
    }

    fn cache_lookup_candidate(&self, name: &str, candidate: &Dentry) -> Option<Dentry> {
        let mut children = self.0.children.lock();
        if let Some(entry) = children
            .get(name)
            .and_then(|entry| entry.upgrade().map(Dentry))
        {
            return Some(entry);
        }
        children.insert(name.to_owned(), Arc::downgrade(&candidate.0));
        drop(children);
        if let Some(super_block) = candidate.super_block() {
            super_block.cache_dentry(candidate.clone());
        }
        None
    }

    fn replace_lookup_candidate(&self, name: &str, candidate: &Dentry, entry: &Dentry) {
        let mut children = self.0.children.lock();
        if children
            .get(name)
            .is_some_and(|cached| Weak::ptr_eq(cached, &Arc::downgrade(&candidate.0)))
        {
            *children.get_mut(name).expect("checked cache entry") = Arc::downgrade(&entry.0);
        }
        if let Some(super_block) = candidate.super_block() {
            super_block.cache_dentry(entry.clone());
        }
        drop(children);
    }

    fn uncache_lookup_candidate(&self, name: &str, candidate: &Dentry) {
        let mut children = self.0.children.lock();
        let is_candidate = children
            .get(name)
            .is_some_and(|entry| Weak::ptr_eq(entry, &Arc::downgrade(&candidate.0)));
        if is_candidate {
            children.remove(name);
        }
        drop(children);
        if is_candidate && let Some(super_block) = candidate.super_block() {
            super_block.uncache_dentry(candidate);
        }
    }

    fn remove_cache_entry(&self, name: &str) -> Option<Dentry> {
        let entry = self
            .0
            .children
            .lock()
            .remove(name)
            .and_then(|entry| entry.upgrade().map(Dentry));
        if let Some(entry) = &entry
            && let Some(super_block) = entry.super_block()
        {
            super_block.uncache_dentry(entry);
        }
        entry
    }

    fn forget_cache_entry(&self, name: &str) {
        if let Some(entry) = self.remove_cache_entry(name)
            && entry.is_really_positive()
            && let Ok(dir) = entry.as_dir()
        {
            dir.forget();
        }
    }

    /// Looks up a directory entry by name in this dentry's cache.
    pub fn lookup_cache(&self, name: &str) -> Option<Dentry> {
        if self.can_cache_children() {
            self.lookup_cache_entry(name)
                .filter(|entry| !entry.is_parallel_lookup())
        } else {
            None
        }
    }

    /// Inserts a child dentry if the cache has no live entry with the same name.
    ///
    /// Returns the existing entry when the name is already cached.
    pub fn insert_cache(&self, name: String, entry: Dentry) -> Option<Dentry> {
        if self.can_cache_children() && entry.can_cache_as_child() {
            let mut children = self.0.children.lock();
            if let Some(existing) = children
                .get(&name)
                .and_then(|entry| entry.upgrade().map(Dentry))
            {
                return Some(existing);
            }
            children.insert(name, Arc::downgrade(&entry.0));
            drop(children);
            if let Some(super_block) = entry.super_block() {
                super_block.cache_dentry(entry);
            }
        }
        None
    }

    fn swap_locations(left: &Dentry, right: &Dentry) {
        if left.as_ptr() < right.as_ptr() {
            let mut left_location = left.0.location.write();
            let mut right_location = right.0.location.write();
            mem::swap(&mut *left_location, &mut *right_location);
        } else {
            let mut right_location = right.0.location.write();
            let mut left_location = left.0.location.write();
            mem::swap(&mut *left_location, &mut *right_location);
        }
    }

    fn rebind(&self, parent: Option<Dentry>, name: String) {
        *self.0.location.write() = DentryLocation { parent, name };
    }

    fn d_move(
        &self,
        src: &Dentry,
        dst_dir: &Self,
        dst: &Dentry,
        destination_name: String,
        keys: (&DentryKey, &DentryKey),
    ) {
        if dst.is_really_positive()
            && let Ok(dir) = dst.as_dir()
        {
            dir.forget();
        }

        let source_location = src.0.location.read();
        let target_location = dst.0.location.read();
        if self.ptr_eq(dst_dir) {
            let mut children = self.0.children.lock();
            children.remove(source_location.name.as_str());
            if let Some(target_slot) = children.get_mut(target_location.name.as_str()) {
                *target_slot = Arc::downgrade(&src.0);
            }
        } else {
            self.0.children.lock().remove(source_location.name.as_str());
            if let Some(target_slot) = dst_dir
                .0
                .children
                .lock()
                .get_mut(target_location.name.as_str())
            {
                *target_slot = Arc::downgrade(&src.0);
            }
        }
        drop(target_location);
        drop(source_location);

        if let Some(super_block) = src.super_block() {
            super_block.move_cached_dentry(keys.0, keys.1, src);
        }
        src.rebind(Some(dst_dir.clone()), destination_name);
    }

    fn d_exchange(
        &self,
        src_name: &str,
        src: &Dentry,
        dst_dir: &Self,
        dst_name: &str,
        dst: &Dentry,
        keys: (&DentryKey, &DentryKey),
    ) {
        if self.ptr_eq(dst_dir) {
            let mut children = self.0.children.lock();
            let source_cached = children.contains_key(src_name);
            let target_cached = children.contains_key(dst_name);
            if source_cached && target_cached {
                *children.get_mut(src_name).expect("checked cache entry") = Arc::downgrade(&dst.0);
                *children.get_mut(dst_name).expect("checked cache entry") = Arc::downgrade(&src.0);
            } else {
                children.remove(src_name);
                children.remove(dst_name);
            }
        } else {
            let source_cached = self.0.children.lock().contains_key(src_name);
            let target_cached = dst_dir.0.children.lock().contains_key(dst_name);
            if source_cached && target_cached {
                *self
                    .0
                    .children
                    .lock()
                    .get_mut(src_name)
                    .expect("checked cache entry") = Arc::downgrade(&dst.0);
                *dst_dir
                    .0
                    .children
                    .lock()
                    .get_mut(dst_name)
                    .expect("checked cache entry") = Arc::downgrade(&src.0);
            } else {
                self.0.children.lock().remove(src_name);
                dst_dir.0.children.lock().remove(dst_name);
            }
        }

        if let Some(super_block) = src.super_block() {
            super_block.exchange_cached_dentries(keys.0, src, keys.1, dst);
        }
        Self::swap_locations(src, dst);
    }

    /// Looks up a child dentry below this directory.
    pub fn lookup(&self, name: &str) -> VfsResult<Dentry> {
        self.as_dir()?;
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        let dir_inode = self.vfs_inode();
        let _namespace_guard = dir_inode.lock_namespace_shared();
        self.lookup_positive_locked(&dir_inode, name)
    }

    fn lookup_locked(&self, dir_inode: &VfsInode, name: &str) -> VfsResult<Dentry> {
        loop {
            if let Some(entry) = self.lookup_cache_entry(name) {
                if entry.is_parallel_lookup() {
                    let _lookup_guard = entry.0.lookup_mutex.lock();
                    continue;
                }
                return Ok(entry);
            }

            let candidate = Dentry::new_negative(Some(self.clone()), name.to_owned());
            let lookup_guard = candidate.0.lookup_mutex.lock();
            candidate.begin_parallel_lookup();
            if let Some(entry) = self.cache_lookup_candidate(name, &candidate) {
                candidate.end_parallel_lookup();
                drop(lookup_guard);
                if entry.is_parallel_lookup() {
                    let _lookup_guard = entry.0.lookup_mutex.lock();
                }
                continue;
            }

            let result = match dir_inode.lookup_child(&candidate) {
                Ok(Some(entry)) => {
                    let location = entry.0.location.read();
                    let has_expected_parent = location
                        .parent
                        .as_ref()
                        .is_some_and(|parent| parent.ptr_eq(self));
                    let is_valid =
                        entry.is_really_positive() && has_expected_parent && location.name == name;
                    drop(location);
                    if is_valid {
                        if self.can_cache_children() && entry.can_cache_as_child() {
                            self.replace_lookup_candidate(name, &candidate, &entry);
                        } else {
                            // Non-cacheable entry (e.g. procfs): retract the
                            // transient candidate without ever publishing the
                            // entry into the children map or the dcache, so a
                            // lookup performs no cache-then-uncache churn.
                            self.uncache_lookup_candidate(name, &candidate);
                        }
                        Ok(entry)
                    } else {
                        self.uncache_lookup_candidate(name, &candidate);
                        Err(VfsError::InvalidInput)
                    }
                }
                Ok(None) => {
                    if !self.can_cache_children() {
                        // Non-cacheable parent (e.g. procfs): do not retain
                        // negative dentries either, or every failed name
                        // lookup would accumulate a permanent dcache entry
                        // and poison later lookups of the same name.
                        self.uncache_lookup_candidate(name, &candidate);
                    }
                    Ok(candidate.clone())
                }
                Err(err) => {
                    self.uncache_lookup_candidate(name, &candidate);
                    Err(err)
                }
            };
            candidate.end_parallel_lookup();
            drop(lookup_guard);
            return result;
        }
    }

    fn lookup_positive_locked(&self, dir_inode: &VfsInode, name: &str) -> VfsResult<Dentry> {
        let entry = self.lookup_locked(dir_inode, name)?;
        if entry.is_negative() {
            Err(VfsError::NotFound)
        } else {
            Ok(entry)
        }
    }

    fn with_locked_child<R>(
        &self,
        name: &str,
        operation_fn: impl FnOnce(&Dentry) -> VfsResult<R>,
    ) -> VfsResult<R> {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let dir_inode = self.vfs_inode();
        let _namespace_guard = dir_inode.lock_namespace_exclusive();
        let candidate = self.lookup_locked(&dir_inode, name)?;
        operation_fn(&candidate)
    }

    /// Runs an exclusive child-creation callback after a locked final lookup.
    ///
    /// The callback receives a negative dentry and runs while the parent
    /// directory namespace lock is held. It must instantiate that dentry
    /// before returning success.
    pub(crate) fn create_exclusive_with(
        &self,
        name: &str,
        create_fn: impl FnOnce(&Dentry) -> VfsResult<()>,
    ) -> VfsResult<Dentry> {
        self.with_locked_child(name, |candidate| {
            if candidate.is_really_positive() {
                return Err(VfsError::AlreadyExists);
            }
            create_fn(candidate)?;
            if candidate.is_negative() {
                return Err(VfsError::InvalidInput);
            }
            Ok(candidate.clone())
        })
    }

    pub(crate) fn lookup_or_create_with<F>(
        &self,
        name: &str,
        exclusive: bool,
        create_fn: F,
    ) -> VfsResult<LookupCreateResult>
    where
        F: FnOnce(&Dentry) -> VfsResult<()>,
    {
        self.with_locked_child(name, |candidate| {
            if candidate.is_really_positive() {
                return if exclusive {
                    Err(VfsError::AlreadyExists)
                } else {
                    Ok(LookupCreateResult::Existing(candidate.clone()))
                };
            }

            create_fn(candidate)?;
            if candidate.is_negative() {
                return Err(VfsError::InvalidInput);
            }
            Ok(LookupCreateResult::Created(candidate.clone()))
        })
    }

    /// Unlinks a non-directory child by name.
    #[cfg(unittest)]
    fn unlink(&self, name: &str) -> VfsResult<()> {
        self.unlink_with(name, |_| Ok(()))
    }

    pub(crate) fn unlink_with<F>(&self, name: &str, may_unlink_fn: F) -> VfsResult<()>
    where
        F: FnOnce(&Dentry) -> VfsResult<()>,
    {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let dir_inode = self.vfs_inode();
        let _namespace_guard = dir_inode.lock_namespace_exclusive();
        let entry = self.lookup_positive_locked(&dir_inode, name)?;
        let victim_inode = entry.vfs_inode();
        if Arc::ptr_eq(&victim_inode, &dir_inode) {
            return Err(VfsError::InvalidInput);
        }
        let _victim_guard = victim_inode.lock_namespace_exclusive();
        if entry.is_dir() {
            return Err(VfsError::IsADirectory);
        }
        may_unlink_fn(&entry)?;
        dir_inode.unlink(&entry)?;
        self.forget_cache_entry(name);
        Ok(())
    }

    /// Removes a directory child by name.
    ///
    /// The filesystem `rmdir` callback is authoritative for whether the
    /// directory is empty.
    #[cfg(unittest)]
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        self.rmdir_with(name, |_| Ok(()))
    }

    pub(crate) fn rmdir_with<F>(&self, name: &str, may_rmdir_fn: F) -> VfsResult<()>
    where
        F: FnOnce(&Dentry) -> VfsResult<()>,
    {
        self.as_dir()?;
        Self::verify_child_name(name)?;
        let dir_inode = self.vfs_inode();
        let _namespace_guard = dir_inode.lock_namespace_exclusive();
        let entry = self.lookup_positive_locked(&dir_inode, name)?;
        let victim_inode = entry.vfs_inode();
        if Arc::ptr_eq(&victim_inode, &dir_inode) {
            return Err(VfsError::InvalidInput);
        }
        let _victim_guard = victim_inode.lock_namespace_exclusive();
        if !entry.is_dir() {
            return Err(VfsError::NotADirectory);
        }
        may_rmdir_fn(&entry)?;
        dir_inode.rmdir(&entry)?;
        self.forget_cache_entry(name);
        Ok(())
    }

    pub(crate) fn has_positive_children(&self) -> bool {
        self.0.children.lock().iter().any(|(name, entry)| {
            name != DOT
                && name != DOTDOT
                && entry
                    .upgrade()
                    .map(Dentry)
                    .is_some_and(|entry| entry.is_really_positive())
        })
    }

    /// Renames a child dentry from this directory to `dst_dir`.
    ///
    /// The filesystem callback is authoritative for target-directory
    /// emptiness; the generic namespace layer only validates common topology,
    /// type, and flag rules.
    #[cfg(unittest)]
    fn rename(
        &self,
        src_name: &str,
        dst_dir: &Self,
        dst_name: &str,
        flags: RenameFlags,
    ) -> VfsResult<()> {
        self.rename_with(src_name, dst_dir, dst_name, flags, |_, _| Ok(()))
    }

    pub(crate) fn rename_with<F>(
        &self,
        src_name: &str,
        dst_dir: &Self,
        dst_name: &str,
        flags: RenameFlags,
        may_rename_fn: F,
    ) -> VfsResult<()>
    where
        F: FnOnce(&Dentry, &Dentry) -> VfsResult<()>,
    {
        if flags.has_conflicting_modes() {
            return Err(VfsError::InvalidInput);
        }
        self.as_dir()?;
        dst_dir.as_dir()?;
        Self::verify_child_name(src_name)?;
        Self::verify_child_name(dst_name)?;
        let supported_flags = RenameFlags::NOREPLACE | RenameFlags::EXCHANGE;
        if !supported_flags.contains(flags) {
            return Err(VfsError::InvalidInput);
        }
        RenameData {
            old_parent: self,
            old_name: src_name,
            new_parent: dst_dir,
            new_name: dst_name,
            flags,
        }
        .execute(may_rename_fn)
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
            .filter_map(|(name, entry)| entry.upgrade().map(|entry| (name.clone(), Dentry(entry))))
            .filter(|(_, entry)| entry.is_really_positive())
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
        let children = mem::take(&mut *self.0.children.lock());
        for child in children
            .into_values()
            .filter_map(|entry| entry.upgrade().map(Dentry))
        {
            if child.is_really_positive()
                && let Ok(dir) = child.as_dir()
            {
                dir.forget();
            }
            if let Some(super_block) = child.super_block() {
                super_block.uncache_dentry(&child);
            }
        }
    }
}

impl RenameData<'_> {
    fn execute<F>(self, may_rename_fn: F) -> VfsResult<()>
    where
        F: FnOnce(&Dentry, &Dentry) -> VfsResult<()>,
    {
        let old_dir_inode = self.old_parent.vfs_inode();
        let new_dir_inode = self.new_parent.vfs_inode();

        if self.old_parent.ptr_eq(self.new_parent) {
            let _parent_guard = old_dir_inode.lock_namespace_exclusive();
            return self.execute_with_parents_locked(
                &old_dir_inode,
                &new_dir_inode,
                false,
                None,
                may_rename_fn,
            );
        }

        let old_super_block = self
            .old_parent
            .super_block()
            .ok_or(VfsError::CrossesDevices)?;
        let new_super_block = self
            .new_parent
            .super_block()
            .ok_or(VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&old_super_block, &new_super_block) {
            return Err(VfsError::CrossesDevices);
        }

        let _topology_guard = old_super_block.lock_rename_topology();
        let (old_parent_first, trap) = self.parent_lock_order()?;
        if Arc::ptr_eq(&old_dir_inode, &new_dir_inode) {
            let _parent_guard = old_dir_inode.lock_namespace_exclusive();
            return self.execute_with_parents_locked(
                &old_dir_inode,
                &new_dir_inode,
                true,
                trap.as_ref(),
                may_rename_fn,
            );
        }
        if old_parent_first {
            let _old_parent_guard = old_dir_inode.lock_namespace_exclusive();
            let _new_parent_guard = new_dir_inode.lock_namespace_exclusive();
            self.execute_with_parents_locked(
                &old_dir_inode,
                &new_dir_inode,
                true,
                trap.as_ref(),
                may_rename_fn,
            )
        } else {
            let _new_parent_guard = new_dir_inode.lock_namespace_exclusive();
            let _old_parent_guard = old_dir_inode.lock_namespace_exclusive();
            self.execute_with_parents_locked(
                &old_dir_inode,
                &new_dir_inode,
                true,
                trap.as_ref(),
                may_rename_fn,
            )
        }
    }

    fn parent_lock_order(&self) -> VfsResult<(bool, Option<Dentry>)> {
        let mut old_ancestor = self.old_parent.clone();
        while let Some(parent) = old_ancestor.parent() {
            if parent.ptr_eq(self.new_parent) {
                return Ok((false, Some(old_ancestor)));
            }
            old_ancestor = parent;
        }

        let old_root = old_ancestor;
        let mut new_ancestor = self.new_parent.clone();
        while let Some(parent) = new_ancestor.parent() {
            if parent.ptr_eq(self.old_parent) {
                return Ok((true, Some(new_ancestor)));
            }
            if parent.ptr_eq(&old_root) {
                return Ok((true, None));
            }
            new_ancestor = parent;
        }
        Err(VfsError::CrossesDevices)
    }

    fn execute_with_parents_locked<F>(
        &self,
        old_dir_inode: &Arc<VfsInode>,
        new_dir_inode: &Arc<VfsInode>,
        is_cross_directory: bool,
        trap: Option<&Dentry>,
        may_rename_fn: F,
    ) -> VfsResult<()>
    where
        F: FnOnce(&Dentry, &Dentry) -> VfsResult<()>,
    {
        let source = self
            .old_parent
            .lookup_positive_locked(old_dir_inode, self.old_name)?;
        let target = self
            .new_parent
            .lookup_locked(new_dir_inode, self.new_name)?;
        if target.is_negative() && self.flags.contains(RenameFlags::EXCHANGE) {
            return Err(VfsError::NotFound);
        }
        if target.is_really_positive() && self.flags.contains(RenameFlags::NOREPLACE) {
            return Err(VfsError::AlreadyExists);
        }

        self.validate_topology(&source, &target, trap)?;
        if target.is_really_positive() && source.is_same_inode(&target) {
            return Ok(());
        }

        let is_exchange = self.flags.contains(RenameFlags::EXCHANGE);
        let target_is_positive = target.is_really_positive();
        let target_is_directory = target_is_positive && target.is_dir();
        let source_inode = source.vfs_inode();
        let target_inode = target_is_positive.then(|| target.vfs_inode());
        let is_parent_inode = |inode: &Arc<VfsInode>| {
            Arc::ptr_eq(inode, old_dir_inode) || Arc::ptr_eq(inode, new_dir_inode)
        };
        let (first_participant, second_participant) = match (source.is_dir(), target_is_directory) {
            (true, _) => {
                let source_participant = if is_cross_directory && !is_parent_inode(&source_inode) {
                    Some(source_inode)
                } else {
                    None
                };
                let should_lock_target = target_is_positive
                    && (!target_is_directory || is_cross_directory || !is_exchange);
                let target_participant =
                    target_inode.filter(|inode| should_lock_target && !is_parent_inode(inode));
                (source_participant, target_participant)
            }
            (false, true) => {
                let target_participant = target_inode.filter(|inode| {
                    (is_cross_directory || !is_exchange) && !is_parent_inode(inode)
                });
                (target_participant, Some(source_inode))
            }
            (false, false) => match target_inode {
                None => (Some(source_inode), None),
                Some(target_inode) if Arc::ptr_eq(&source_inode, &target_inode) => {
                    (Some(source_inode), None)
                }
                Some(target_inode) if Arc::as_ptr(&source_inode) < Arc::as_ptr(&target_inode) => {
                    (Some(source_inode), Some(target_inode))
                }
                Some(target_inode) => (Some(target_inode), Some(source_inode)),
            },
        };
        let _first_guard = first_participant
            .as_ref()
            .map(|inode| inode.lock_namespace_exclusive());
        let _second_guard = second_participant
            .as_ref()
            .map(|inode| inode.lock_namespace_exclusive());
        self.execute_locked(
            old_dir_inode,
            new_dir_inode,
            &source,
            &target,
            may_rename_fn,
        )
    }

    fn execute_locked<F>(
        &self,
        old_dir_inode: &Arc<VfsInode>,
        new_dir_inode: &Arc<VfsInode>,
        source: &Dentry,
        target: &Dentry,
        may_rename_fn: F,
    ) -> VfsResult<()>
    where
        F: FnOnce(&Dentry, &Dentry) -> VfsResult<()>,
    {
        self.validate_locked(source, target)?;
        let source_key = source.key();
        let target_key = target.key();
        let destination_name =
            (!self.flags.contains(RenameFlags::EXCHANGE)).then(|| target.name_snapshot());
        may_rename_fn(source, target)?;
        old_dir_inode.rename(source, new_dir_inode, target, self.flags)?;
        if self.flags.contains(RenameFlags::EXCHANGE) {
            self.old_parent.d_exchange(
                self.old_name,
                source,
                self.new_parent,
                self.new_name,
                target,
                (&source_key, &target_key),
            );
        } else {
            self.old_parent.d_move(
                source,
                self.new_parent,
                target,
                destination_name.expect("non-exchange rename has a destination name"),
                (&source_key, &target_key),
            );
        }
        Ok(())
    }

    fn validate_topology(
        &self,
        source: &Dentry,
        target: &Dentry,
        trap: Option<&Dentry>,
    ) -> VfsResult<()> {
        let Some(trap) = trap else {
            return Ok(());
        };
        if source.ptr_eq(trap) {
            return Err(VfsError::InvalidInput);
        }
        if target.ptr_eq(trap) {
            return if self.flags.contains(RenameFlags::EXCHANGE) {
                Err(VfsError::InvalidInput)
            } else {
                Err(VfsError::DirectoryNotEmpty)
            };
        }
        Ok(())
    }

    fn validate_locked(&self, source: &Dentry, target: &Dentry) -> VfsResult<()> {
        if self.flags.contains(RenameFlags::EXCHANGE) {
            return Ok(());
        }
        if !target.is_really_positive() {
            return Ok(());
        }

        match (source.is_dir(), target.is_dir()) {
            (true, false) => return Err(VfsError::NotADirectory),
            (false, true) => return Err(VfsError::IsADirectory),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(unittest)]
mod tests_dentry {
    use alloc::{string::String, sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use ktime_types::SystemTime;
    use unittest::{assert, def_test};

    use super::{Dentry, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive};
    use crate::{
        AddressSpaceOperations, DirContext, FileDirOperations, FileOperations, InodeDirOperations,
        InodeOperations, InodeSymlinkOperations, LockedDentry, Metadata, MetadataUpdate, NodeFlags,
        NodePermission, NodeType, RenameFlags, StatFs, SuperBlock, SuperBlockOperations, Umode,
        VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
    };

    struct MockFilesystem;

    impl SuperBlockOperations for MockFilesystem {
        fn name(&self) -> &str {
            "mockfs"
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
                atime: SystemTime::UNIX_EPOCH,
                mtime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
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
        can_rename: bool,
        can_remove: bool,
        unlink_count: AtomicUsize,
        rmdir_count: AtomicUsize,
        lookup_result: crate::Mutex<Option<Dentry>>,
    }

    impl MockDirOps {
        fn new(_fs: Arc<MockFilesystem>, inode: u64) -> Self {
            Self {
                inode,
                can_rename: false,
                can_remove: false,
                unlink_count: AtomicUsize::new(0),
                rmdir_count: AtomicUsize::new(0),
                lookup_result: crate::Mutex::new(None),
            }
        }

        fn new_renamable(inode: u64) -> Self {
            Self {
                inode,
                can_rename: true,
                can_remove: false,
                unlink_count: AtomicUsize::new(0),
                rmdir_count: AtomicUsize::new(0),
                lookup_result: crate::Mutex::new(None),
            }
        }

        fn new_removable(inode: u64) -> Self {
            Self {
                inode,
                can_rename: false,
                can_remove: true,
                unlink_count: AtomicUsize::new(0),
                rmdir_count: AtomicUsize::new(0),
                lookup_result: crate::Mutex::new(None),
            }
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
                atime: SystemTime::UNIX_EPOCH,
                mtime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
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
            _dentry: &LockedDentry<'_>,
            _flags: crate::InodeLookupFlags,
        ) -> VfsResult<Option<Dentry>> {
            Ok(self.lookup_result.lock().clone())
        }

        fn create(
            &self,
            _idmap: &crate::MountIdmap,
            _dir: &VfsInode,
            dentry: &LockedDentry<'_>,
            mode: crate::Umode,
            _exclusive: bool,
            _cred: &kcred::Cred,
        ) -> VfsResult<()> {
            let operations = Arc::new(MockFileOps::new(
                Arc::new(MockFilesystem),
                self.inode + 1,
                &[],
            ));
            let inode = VfsInode::new_file(operations, VfsInodeInit::new(self.inode + 1, 0, mode));
            dentry.instantiate(inode)
        }

        fn link(
            &self,
            _old_dentry: &Dentry,
            _dir: &VfsInode,
            _new_dentry: &LockedDentry<'_>,
        ) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }

        fn unlink(&self, _dir: &VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
            self.unlink_count.fetch_add(1, Ordering::Relaxed);
            if self.can_remove {
                Ok(())
            } else {
                Err(VfsError::OperationNotSupported)
            }
        }

        fn rmdir(&self, _dir: &VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
            self.rmdir_count.fetch_add(1, Ordering::Relaxed);
            if self.can_remove {
                Ok(())
            } else {
                Err(VfsError::OperationNotSupported)
            }
        }

        fn rename(
            &self,
            _idmap: &crate::MountIdmap,
            _old_dir: &VfsInode,
            _old_dentry: &LockedDentry<'_>,
            _new_dir: &VfsInode,
            _new_dentry: &LockedDentry<'_>,
            _flags: crate::RenameFlags,
        ) -> VfsResult<()> {
            if self.can_rename {
                Ok(())
            } else {
                Err(VfsError::OperationNotSupported)
            }
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

    fn make_renamable_dir_entry(inode: u64, parent: Option<Dentry>, name: &str) -> Dentry {
        let inode = VfsInode::new_openable_dir(
            Arc::new(MockDirOps::new_renamable(inode)),
            inode_init(inode, NodeType::Directory, 0),
        );
        Dentry::new_dir_from_inode(inode, parent, String::from(name))
    }

    fn make_removable_dir_entry(
        inode: u64,
        parent: Option<Dentry>,
        name: &str,
    ) -> (Dentry, Arc<MockDirOps>) {
        let operations = Arc::new(MockDirOps::new_removable(inode));
        let inode = VfsInode::new_openable_dir(
            operations.clone(),
            inode_init(inode, NodeType::Directory, 0),
        );
        (
            Dentry::new_dir_from_inode(inode, parent, String::from(name)),
            operations,
        )
    }

    fn inode_init(inode: u64, node_type: NodeType, size: u64) -> VfsInodeInit {
        VfsInodeInit::new(
            inode,
            size,
            crate::Umode::new(node_type, NodePermission::default()),
        )
        .with_owner_links_and_rdev(0, 0, 1, Default::default())
        .with_stat_data(
            4096,
            1,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
        )
    }

    #[def_test]
    fn test_dentry_file_helpers_and_private_data() {
        let fs = Arc::new(MockFilesystem);
        let (entry, ops) = make_file_entry(fs, 2, None, "leaf");

        assert!(entry.is_file());
        assert!(!entry.is_dir());
        assert_eq!(entry.name_snapshot(), "leaf");
        assert_eq!(entry.node_type(), NodeType::RegularFile);
        assert!(matches!(entry.as_dir(), Err(VfsError::NotADirectory)));
        assert_eq!(entry.inode(), 2);

        let metadata = d_inode(&entry).metadata();
        assert_eq!(metadata.mode.node_type(), NodeType::RegularFile);
        assert_eq!(metadata.size, 7);

        assert_eq!(ops.update_count.load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn test_dentry_backing_metadata_refresh_validates_entry_state() {
        let fs = Arc::new(MockFilesystem);
        let (entry, _) = make_file_entry(fs, 2, None, "leaf");
        let mut updated = entry.metadata();
        updated.nlink = 2;
        updated.size = 11;

        entry
            .update_metadata_after_backing_change(&updated)
            .unwrap();
        assert_eq!(entry.metadata().nlink, 2);
        assert_eq!(entry.metadata().size, 11);

        updated.inode = 3;
        assert_eq!(
            entry.update_metadata_after_backing_change(&updated),
            Err(VfsError::InvalidInput)
        );

        let negative = Dentry::new_negative(None, String::from("missing"));
        assert_eq!(
            negative.update_metadata_after_backing_change(&updated),
            Err(VfsError::NotFound)
        );
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

        root.insert_cache(negative.name_snapshot(), negative.clone());
        assert!(root.lookup_cache("missing").unwrap().ptr_eq(&negative));

        root.forget_cache_entry("missing");
        assert!(root.lookup_cache("missing").is_none());
    }

    #[def_test]
    fn test_create_instantiates_cached_lookup_miss() {
        let fs = Arc::new(MockFilesystem);
        let root = make_dir_entry(fs.clone(), 24, "");
        let _super_block = SuperBlock::new(fs, |_| root.clone());

        assert!(matches!(root.lookup("created"), Err(VfsError::NotFound)));
        let negative = root.lookup_cache("created").unwrap();
        let created = root
            .create_exclusive_with("created", |candidate| {
                root.vfs_inode().create_with_mode(
                    candidate,
                    Umode::new(NodeType::RegularFile, NodePermission::default()),
                    true,
                    &kcred::initial_cred(),
                )
            })
            .unwrap();

        assert!(created.ptr_eq(&negative));
        assert!(created.is_really_positive());
        assert!(root.lookup_cache("created").unwrap().ptr_eq(&created));
    }

    #[def_test]
    fn test_rename_moves_cache_entry_without_changing_inode_identity() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(30, None, "");
        let (source, _) = make_file_entry(fs, 31, Some(root.clone()), "tmp");

        root.insert_cache(String::from("tmp"), source.clone());
        root.rename("tmp", &root, "final", RenameFlags::empty())
            .unwrap();

        assert!(root.lookup_cache("tmp").is_none());
        let moved = root.lookup_cache("final").unwrap();
        assert!(moved.ptr_eq(&source));
        assert!(moved.is_same_inode(&source));
        assert_eq!(moved.name_snapshot(), "final");
        assert_eq!(moved.absolute_path().unwrap().as_str(), "/final");
    }

    #[def_test]
    fn test_rename_replace_keeps_source_inode_identity() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(32, None, "");
        let (source, _) = make_file_entry(fs.clone(), 33, Some(root.clone()), "tmp");
        let (target, _) = make_file_entry(fs, 34, Some(root.clone()), "final");

        root.insert_cache(String::from("tmp"), source.clone());
        root.insert_cache(String::from("final"), target.clone());
        root.rename("tmp", &root, "final", RenameFlags::empty())
            .unwrap();

        let moved = root.lookup_cache("final").unwrap();
        assert!(moved.ptr_eq(&source));
        assert!(!moved.is_same_inode(&target));
    }

    #[def_test]
    fn test_rename_noreplace_rejects_existing_target_before_filesystem_callback() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(35, None, "");
        let (source, _) = make_file_entry(fs.clone(), 36, Some(root.clone()), "tmp");
        let (target, _) = make_file_entry(fs, 37, Some(root.clone()), "final");

        root.insert_cache(String::from("tmp"), source.clone());
        root.insert_cache(String::from("final"), target.clone());

        assert_eq!(
            root.rename("tmp", &root, "final", RenameFlags::NOREPLACE),
            Err(VfsError::AlreadyExists)
        );
        assert!(root.lookup_cache("tmp").unwrap().ptr_eq(&source));
        assert!(root.lookup_cache("final").unwrap().ptr_eq(&target));
    }

    #[def_test]
    fn test_rename_noreplace_rejects_target_with_source_inode() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(69, None, "");
        let (source, _) = make_file_entry(fs, 70, Some(root.clone()), "tmp");
        let target = Dentry::new_file_from_inode(
            source.vfs_inode(),
            Some(root.clone()),
            String::from("final"),
        );

        root.insert_cache(String::from("tmp"), source.clone());
        root.insert_cache(String::from("final"), target.clone());

        assert_eq!(
            root.rename("tmp", &root, "final", RenameFlags::NOREPLACE),
            Err(VfsError::AlreadyExists)
        );
        assert!(root.lookup_cache("tmp").unwrap().ptr_eq(&source));
        assert!(root.lookup_cache("final").unwrap().ptr_eq(&target));
    }

    #[def_test]
    fn test_rename_exchange_swaps_cache_entries_without_changing_inode_identity() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(38, None, "");
        let (left, _) = make_file_entry(fs.clone(), 39, Some(root.clone()), "left");
        let (right, _) = make_file_entry(fs, 40, Some(root.clone()), "right");

        root.insert_cache(String::from("left"), left.clone());
        root.insert_cache(String::from("right"), right.clone());
        root.rename("left", &root, "right", RenameFlags::EXCHANGE)
            .unwrap();

        assert!(root.lookup_cache("left").unwrap().ptr_eq(&right));
        assert!(root.lookup_cache("right").unwrap().ptr_eq(&left));
        assert_eq!(left.name_snapshot(), "right");
        assert_eq!(right.name_snapshot(), "left");
    }

    #[def_test]
    fn test_rename_rejects_noreplace_exchange_combination() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(41, None, "");
        let (left, _) = make_file_entry(fs.clone(), 42, Some(root.clone()), "left");
        let (right, _) = make_file_entry(fs, 43, Some(root.clone()), "right");

        root.insert_cache(String::from("left"), left.clone());
        root.insert_cache(String::from("right"), right.clone());

        assert_eq!(
            root.rename(
                "left",
                &root,
                "right",
                RenameFlags::NOREPLACE | RenameFlags::EXCHANGE,
            ),
            Err(VfsError::InvalidInput)
        );
        assert!(root.lookup_cache("left").unwrap().ptr_eq(&left));
        assert!(root.lookup_cache("right").unwrap().ptr_eq(&right));
    }

    #[def_test]
    fn test_rmdir_uses_directory_removal_callback() {
        let root_operations = Arc::new(MockDirOps::new_removable(44));
        let root_inode = VfsInode::new_openable_dir(
            root_operations.clone(),
            inode_init(44, NodeType::Directory, 0),
        );
        let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
        let (victim, _) = make_removable_dir_entry(45, Some(root.clone()), "victim");
        let (cached_child, _) =
            make_file_entry(Arc::new(MockFilesystem), 46, Some(victim.clone()), "cached");
        victim.insert_cache(String::from("cached"), cached_child.clone());
        root.insert_cache(String::from("victim"), victim.clone());

        root.rmdir("victim").unwrap();

        assert_eq!(root_operations.unlink_count.load(Ordering::Relaxed), 0);
        assert_eq!(root_operations.rmdir_count.load(Ordering::Relaxed), 1);
        assert!(root.lookup_cache("victim").is_none());
    }

    #[def_test]
    fn test_unlink_validator_observes_callback_victim() {
        let root_operations = Arc::new(MockDirOps::new_removable(60));
        let root_inode = VfsInode::new_openable_dir(
            root_operations.clone(),
            inode_init(60, NodeType::Directory, 0),
        );
        let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
        let (victim, _) =
            make_file_entry(Arc::new(MockFilesystem), 61, Some(root.clone()), "victim");
        root.insert_cache(String::from("victim"), victim.clone());

        assert_eq!(
            root.unlink_with("victim", |checked| {
                if !checked.ptr_eq(&victim) {
                    return Err(VfsError::InvalidInput);
                }
                Err(VfsError::OperationNotPermitted)
            }),
            Err(VfsError::OperationNotPermitted)
        );
        assert_eq!(root_operations.unlink_count.load(Ordering::Relaxed), 0);
        assert!(root.lookup_cache("victim").unwrap().ptr_eq(&victim));
    }

    #[def_test]
    fn test_rename_directory_emptiness_is_filesystem_owned() {
        let root = make_renamable_dir_entry(62, None, "");
        let source = make_renamable_dir_entry(63, Some(root.clone()), "source");
        let target = make_renamable_dir_entry(64, Some(root.clone()), "target");
        let (cached_child, _) =
            make_file_entry(Arc::new(MockFilesystem), 65, Some(target.clone()), "cached");
        root.insert_cache(String::from("source"), source.clone());
        root.insert_cache(String::from("target"), target.clone());
        target.insert_cache(String::from("cached"), cached_child.clone());

        root.rename("source", &root, "target", RenameFlags::empty())
            .unwrap();
        assert!(root.lookup_cache("target").unwrap().ptr_eq(&source));
    }

    #[def_test]
    fn test_rename_rejects_directory_move_into_descendant() {
        let root = make_renamable_dir_entry(46, None, "");
        let source = make_renamable_dir_entry(47, Some(root.clone()), "source");
        let child = make_renamable_dir_entry(48, Some(source.clone()), "child");
        root.insert_cache(String::from("source"), source.clone());
        source.insert_cache(String::from("child"), child.clone());
        let _super_block = SuperBlock::new(Arc::new(MockFilesystem), |_| root.clone());

        assert_eq!(
            root.rename("source", &child, "moved", RenameFlags::empty()),
            Err(VfsError::InvalidInput)
        );
        assert!(root.lookup_cache("source").unwrap().ptr_eq(&source));
    }

    #[def_test]
    fn test_rename_target_ancestor_uses_linux_trap_errors() {
        let root = make_renamable_dir_entry(71, None, "");
        let ancestor = make_renamable_dir_entry(72, Some(root.clone()), "ancestor");
        let old_parent = make_renamable_dir_entry(73, Some(ancestor.clone()), "old");
        let (source, _) = make_file_entry(
            Arc::new(MockFilesystem),
            74,
            Some(old_parent.clone()),
            "source",
        );
        root.insert_cache(String::from("ancestor"), ancestor.clone());
        ancestor.insert_cache(String::from("old"), old_parent.clone());
        old_parent.insert_cache(String::from("source"), source.clone());
        let _super_block = SuperBlock::new(Arc::new(MockFilesystem), |_| root.clone());

        assert_eq!(
            old_parent.rename("source", &root, "ancestor", RenameFlags::empty()),
            Err(VfsError::DirectoryNotEmpty)
        );
        assert_eq!(
            old_parent.rename("source", &root, "ancestor", RenameFlags::EXCHANGE),
            Err(VfsError::InvalidInput)
        );
        assert!(old_parent.lookup_cache("source").unwrap().ptr_eq(&source));
        assert!(root.lookup_cache("ancestor").unwrap().ptr_eq(&ancestor));
    }

    #[def_test]
    fn test_cross_directory_rename_moves_source_dentry() {
        let fs = Arc::new(MockFilesystem);
        let root = make_renamable_dir_entry(49, None, "");
        let old_parent = make_renamable_dir_entry(50, Some(root.clone()), "old");
        let new_parent = make_renamable_dir_entry(51, Some(root.clone()), "new");
        let (source, _) = make_file_entry(fs, 52, Some(old_parent.clone()), "source");
        root.insert_cache(String::from("old"), old_parent.clone());
        root.insert_cache(String::from("new"), new_parent.clone());
        old_parent.insert_cache(String::from("source"), source.clone());
        let _super_block = SuperBlock::new(Arc::new(MockFilesystem), |_| root.clone());

        old_parent
            .rename("source", &new_parent, "target", RenameFlags::empty())
            .unwrap();

        assert!(old_parent.lookup_cache("source").is_none());
        assert!(new_parent.lookup_cache("target").unwrap().ptr_eq(&source));
        assert_eq!(source.absolute_path().unwrap().as_str(), "/new/target");
    }

    #[def_test]
    fn test_directory_inode_rejects_second_live_alias() {
        let root = make_renamable_dir_entry(66, None, "");
        let parent_inode = VfsInode::new_openable_dir(
            Arc::new(MockDirOps::new_renamable(67)),
            inode_init(67, NodeType::Directory, 0),
        );
        let parent = Dentry::new_dir_from_inode(
            parent_inode.clone(),
            Some(root.clone()),
            String::from("parent"),
        );
        root.insert_cache(String::from("parent"), parent.clone());
        let candidate = Dentry::new_negative(Some(root), String::from("alias"));

        assert_eq!(
            candidate.instantiate(parent_inode),
            Err(VfsError::InvalidInput)
        );
        assert!(candidate.is_negative());
    }

    #[def_test]
    fn test_directory_constructor_reuses_alias_at_same_location() {
        let root = make_renamable_dir_entry(75, None, "");
        let inode = VfsInode::new_openable_dir(
            Arc::new(MockDirOps::new_renamable(76)),
            inode_init(76, NodeType::Directory, 0),
        );
        let first =
            Dentry::new_dir_from_inode(inode.clone(), Some(root.clone()), String::from("child"));
        let second = Dentry::new_dir_from_inode(inode, Some(root), String::from("child"));

        assert!(first.ptr_eq(&second));
    }

    #[def_test]
    fn test_remove_rejects_parent_inode_as_victim() {
        let root = make_renamable_dir_entry(77, None, "");
        root.insert_cache(String::from("self"), root.clone());

        assert_eq!(root.unlink("self"), Err(VfsError::InvalidInput));
        assert_eq!(root.rmdir("self"), Err(VfsError::InvalidInput));
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
    fn test_superblock_inode_cache_reuses_live_identity() {
        let fs = Arc::new(MockFilesystem);
        let super_block = SuperBlock::new(fs.clone(), |super_block| {
            let root_inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    inode_init(1, NodeType::Directory, 0),
                )
            });
            Dentry::new_dir_from_inode(root_inode, None, String::new())
        });
        let failed = super_block.get_or_try_init_inode(50, || Err::<Arc<VfsInode>, _>(7u8));
        assert!(matches!(failed, Err(7)));
        let first = super_block.get_or_init_inode(50, || {
            VfsInode::new_file_with_address_space_and_flags(
                Arc::new(MockFileOps::new(fs.clone(), 50, b"first")),
                NodeFlags::empty(),
                inode_init(50, NodeType::RegularFile, b"first".len() as u64),
            )
        });
        let second = super_block.get_or_init_inode(50, || panic!("live inode should be reused"));

        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first.super_block().unwrap(), &super_block));
        assert_eq!(first.inode(), 50);
        assert_eq!(super_block.root_dir().inode(), 1);
        assert!(super_block.lookup_inode(1).is_some());
        assert!(super_block.lookup_inode(50).is_some());

        let old_identity = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(old_identity.upgrade().is_none());
        assert!(super_block.lookup_inode(50).is_none());
        let replacement = super_block.get_or_init_inode(50, || {
            VfsInode::new_file_with_address_space_and_flags(
                Arc::new(MockFileOps::new(fs, 50, b"replacement")),
                NodeFlags::empty(),
                inode_init(50, NodeType::RegularFile, b"replacement".len() as u64),
            )
        });
        assert!(!core::ptr::eq(
            old_identity.as_ptr(),
            Arc::as_ptr(&replacement)
        ));
    }

    #[def_test]
    fn test_superblock_inode_cache_isolates_equal_inode_numbers() {
        let fs = Arc::new(MockFilesystem);
        let first_super_block = SuperBlock::new(fs.clone(), |super_block| {
            let root_inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    inode_init(1, NodeType::Directory, 0),
                )
            });
            Dentry::new_dir_from_inode(root_inode, None, String::new())
        });
        let second_super_block = SuperBlock::new(fs.clone(), |super_block| {
            let root_inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    inode_init(1, NodeType::Directory, 0),
                )
            });
            Dentry::new_dir_from_inode(root_inode, None, String::new())
        });

        let first = first_super_block.get_or_init_inode(50, || {
            VfsInode::new_file_with_address_space_and_flags(
                Arc::new(MockFileOps::new(fs.clone(), 50, b"first")),
                NodeFlags::empty(),
                inode_init(50, NodeType::RegularFile, b"first".len() as u64),
            )
        });
        let second = second_super_block.get_or_init_inode(50, || {
            VfsInode::new_file_with_address_space_and_flags(
                Arc::new(MockFileOps::new(fs, 50, b"second")),
                NodeFlags::empty(),
                inode_init(50, NodeType::RegularFile, b"second".len() as u64),
            )
        });

        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &first_super_block.lookup_inode(50).unwrap(),
            &first
        ));
        assert!(Arc::ptr_eq(
            &second_super_block.lookup_inode(50).unwrap(),
            &second
        ));
    }

    #[def_test]
    fn test_failed_root_initialization_drops_nascent_superblock() {
        let mut nascent_super_block = None;
        let result = SuperBlock::try_new_with_flags(
            Arc::new(MockFilesystem),
            crate::SuperBlockFlags::empty(),
            |super_block| {
                nascent_super_block = Some(Arc::downgrade(super_block));
                Err::<Dentry, _>(VfsError::Io)
            },
        );

        assert!(matches!(result, Err(VfsError::Io)));
        assert!(nascent_super_block.unwrap().upgrade().is_none());
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
    fn test_dentry_cache_retains_hashed_children_and_preserves_parent_lifetime() {
        let fs = Arc::new(MockFilesystem);
        let root = make_dir_entry(fs.clone(), 13, "");
        let _super_block = SuperBlock::new(fs.clone(), |_| root.clone());
        let cached = make_dir_entry_with_parent(fs.clone(), 14, Some(root.clone()), "cached");
        let cached_weak = Arc::downgrade(&cached.0);
        root.insert_cache(String::from("cached"), cached.clone());

        assert!(cached_weak.upgrade().is_some());
        assert!(root.lookup_cache("cached").is_some());
        drop(cached);
        assert!(cached_weak.upgrade().is_some());
        assert!(root.lookup_cache("cached").is_some());

        root.forget_cache_entry("cached");
        assert!(cached_weak.upgrade().is_none());

        let parent = make_dir_entry(fs.clone(), 15, "parent");
        let parent_weak = Arc::downgrade(&parent.0);
        let child = make_dir_entry_with_parent(fs, 16, Some(parent.clone()), "child");
        drop(parent);

        assert!(parent_weak.upgrade().is_some());
        assert!(child.parent().is_some());
        assert_eq!(child.absolute_path().unwrap().as_str(), "/parent/child");
    }

    #[def_test]
    fn test_hashed_child_without_external_reference_keeps_directory_nonempty() {
        let fs = Arc::new(MockFilesystem);
        let root = make_dir_entry(fs.clone(), 17, "");
        let _super_block = SuperBlock::new(fs.clone(), |_| root.clone());
        let directory = make_dir_entry_with_parent(fs.clone(), 18, Some(root.clone()), "directory");
        root.insert_cache(String::from("directory"), directory.clone());
        let (child, _) = make_file_entry(fs, 19, Some(directory.clone()), "child");
        directory.insert_cache(String::from("child"), child.clone());

        drop(child);

        assert!(directory.has_positive_children());
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

    fn make_non_cacheable_dir(
        fs: Arc<MockFilesystem>,
        inode: u64,
        parent: Option<Dentry>,
        name: &str,
    ) -> (Dentry, Arc<MockDirOps>) {
        let ops = Arc::new(MockDirOps::new(fs, inode));
        let private_data: Arc<dyn core::any::Any + Send + Sync> = ops.clone();
        let inode = VfsInode::new_dir_with_operations(
            private_data,
            ops.clone(),
            ops.clone(),
            NodeFlags::NON_CACHEABLE,
            inode_init(inode, NodeType::Directory, 0),
        );
        (
            Dentry::new_dir_from_inode(inode, parent, String::from(name)),
            ops,
        )
    }

    #[def_test]
    fn test_lookup_non_cacheable_positive_retracts() {
        let fs = Arc::new(MockFilesystem);
        let (dir, ops) = make_non_cacheable_dir(fs.clone(), 2, None, "dir");
        let sb = SuperBlock::new(fs.clone(), |_| dir.clone());

        let (child, _) = make_file_entry(fs, 3, Some(dir.clone()), "x");
        *ops.lookup_result.lock() = Some(child.clone());

        let found = dir.lookup("x").unwrap();
        assert!(found.ptr_eq(&child));

        // Neither the transient candidate nor the real entry may survive in
        // the children map or the superblock dcache.
        assert!(dir.lookup_cache_entry("x").is_none());
        assert!(!sb.is_dentry_cached(&child));
    }

    #[def_test]
    fn test_lookup_non_cacheable_negative_retracts() {
        let fs = Arc::new(MockFilesystem);
        let (dir, _) = make_non_cacheable_dir(fs.clone(), 2, None, "dir");
        let sb = SuperBlock::new(fs, |_| dir.clone());

        // Default MockDirOps::lookup returns Ok(None).
        assert!(matches!(dir.lookup("missing"), Err(VfsError::NotFound)));

        // The negative candidate must not survive in the children map or the
        // dcache (checked by key), and a repeated lookup must not be poisoned
        // by a stale negative dentry.
        assert!(dir.lookup_cache_entry("missing").is_none());
        let probe = Dentry::new_negative(Some(dir.clone()), String::from("missing"));
        assert!(!sb.is_dentry_cached(&probe));
        assert!(matches!(dir.lookup("missing"), Err(VfsError::NotFound)));
    }

    #[def_test]
    fn test_bind_super_block_skips_non_cacheable_children() {
        let fs = Arc::new(MockFilesystem);
        let (dir, _) = make_non_cacheable_dir(fs.clone(), 2, None, "dir");
        let sb = SuperBlock::new(fs.clone(), |_| dir.clone());
        let (child, _) = make_file_entry(fs.clone(), 3, Some(dir.clone()), "child");
        dir.0
            .children
            .lock()
            .insert("child".into(), Arc::downgrade(&child.0));

        dir.bind_super_block(&sb);
        assert!(!sb.is_dentry_cached(&child));

        // Control: a cacheable directory does publish its children.
        let cacheable = make_dir_entry(fs.clone(), 4, "cacheable");
        let sb2 = SuperBlock::new(fs.clone(), |_| cacheable.clone());
        let (child2, _) = make_file_entry(fs, 5, Some(cacheable.clone()), "child");
        cacheable
            .0
            .children
            .lock()
            .insert("child".into(), Arc::downgrade(&child2.0));

        cacheable.bind_super_block(&sb2);
        assert!(sb2.is_dentry_cached(&child2));
    }

    #[def_test]
    fn test_replace_lookup_candidate_preserves_replacer() {
        let fs = Arc::new(MockFilesystem);
        let dir = make_dir_entry(fs.clone(), 2, "dir");
        let sb = SuperBlock::new(fs.clone(), |_| dir.clone());

        // Simulate the concurrent-replacement window: the children slot
        // already holds another thread's candidate, so a stale replace must
        // not clobber it.
        let other = Dentry::new_negative(Some(dir.clone()), String::from("x"));
        dir.0
            .children
            .lock()
            .insert("x".into(), Arc::downgrade(&other.0));

        let candidate = Dentry::new_negative(Some(dir.clone()), String::from("x"));
        let (entry, _) = make_file_entry(fs, 3, Some(dir.clone()), "x");
        dir.replace_lookup_candidate("x", &candidate, &entry);

        // The children slot keeps the replacer; only the dcache key is
        // (defensively) refreshed with our entry.
        let cached = dir.0.children.lock().get("x").unwrap().upgrade().unwrap();
        assert!(Arc::ptr_eq(&cached, &other.0));
        assert!(sb.is_dentry_cached(&entry));
    }
}
