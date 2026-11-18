// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple directory helpers for the in-kernel VFS.

use alloc::{
    borrow::{Cow, ToOwned},
    collections::btree_map::BTreeMap,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::any::Any;

use kalloc_types::KBox;
use kerrno::LinuxError;
use ksync::Mutex;

use crate::{
    Dentry, DeviceId, DirContext, DirEntrySink, FileDirOperations, FileOperations,
    InodeDirOperations, InodeOperations, LockedDentry, Metadata, MetadataUpdate, NodeFlags,
    NodePermission, NodeType, RenameFlags, SeqFileInode, Umode, VfsError, VfsFile, VfsInode,
    VfsResult,
    file::{SEEK_CUR, SEEK_END, SEEK_SET},
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
        self().into_dir_mapping_entry()
    }
}

impl IntoDirMappingEntry for Arc<crate::SimpleFile> {
    fn into_dir_mapping_entry(self) -> SimpleDirEntry {
        self.new_inode(NodeFlags::NON_CACHEABLE)
            .into_dir_mapping_entry()
    }
}

impl IntoDirMappingEntry for Arc<VfsInode> {
    fn into_dir_mapping_entry(self) -> SimpleDirEntry {
        SimpleDirEntry::new(Arc::new(move |parent, name| {
            Ok(if self.is_dir() {
                Dentry::new_dir_from_inode(self.clone(), parent, name.to_owned())
            } else {
                Dentry::new_file_from_inode(self.clone(), parent, name.to_owned())
            })
        }))
    }
}

impl<I> IntoDirMappingEntry for Arc<SeqFileInode<I>>
where
    I: crate::SeqIterator,
{
    fn into_dir_mapping_entry(self) -> SimpleDirEntry {
        let init = self.inode_init();
        VfsInode::new_file_with_flags(self, NodeFlags::NON_CACHEABLE, init).into_dir_mapping_entry()
    }
}

fn make_dir_child(parent: Option<Dentry>, name: &str, maker: DirMaker) -> Dentry {
    Dentry::new_dir_from_inode(maker(), parent, name.to_owned())
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
    /// Validates one directory open after VFS permission checks. The default
    /// preserves existing helper users; implementations that retain a dynamic
    /// backing identity can reject a stale open here.
    fn validate_open(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Get the names of all children in the directory.
    ///
    /// # Execution context
    /// Requires sleepable task context with IRQs and preemption enabled, no
    /// non-sleepable RCU section, and no locks needed by reclaim, IO or filesystem
    /// callbacks. Implementations release snapshot locks before allocating the
    /// iterator with the default KERNEL policy; returned iterators must not
    /// retain those locks. Iteration and destruction run in the caller's context.
    ///
    /// # Errors
    /// Iterator storage allocation failure returns `VfsError::NoMemory`.
    /// Snapshot containers and name formatting retain their own allocation
    /// behavior; this does not make the entire enumeration fallible on OOM.
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>>;
    /// Look up a child directory or file by name.
    ///
    /// Directory reads skip `NotFound` when a child disappears after the name
    /// snapshot. Other lookup errors still propagate to the reader.
    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry>;

    /// Returns an optional sparse enumeration source.
    ///
    /// The default keeps every existing directory on the full
    /// `child_names()` path. A sparse source opts this directory into the
    /// incremental cursor path; `ChainedDirOps` does not forward it in the
    /// first version.
    fn sparse_source(&self) -> Option<&dyn SparseDirSource> {
        None
    }

    /// Samples a dynamic directory link count for each metadata query.
    ///
    /// `None` retains the backing node's link count. Runs in sleepable task
    /// context without the simple node's metadata lock; implementations own
    /// synchronization and may report a vanished backing object as an error.
    /// This describes the whole directory, not just its enumerated children;
    /// `chain()` therefore does not combine component link counts.
    /// Only the returned metadata copy changes; cached node/inode metadata
    /// remains unchanged. Call `getattr()` for this dynamic attribute rather
    /// than the cached `metadata()` accessor. Directory iteration reads cached
    /// inode/type fields and does not consume their link count.
    fn link_count(&self) -> VfsResult<Option<u64>> {
        Ok(None)
    }

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

    /// Creates a persistent regular-file child from VFS-validated context.
    ///
    /// Only filesystems that accept userspace file creation opt in: Linux
    /// devtmpfs is ramfs-backed and its directories accept plain files, while
    /// pseudo filesystems such as procfs and sysfs keep rejecting creation.
    fn create_file(
        &self,
        _dir: &VfsInode,
        _name: &str,
        _mode: Umode,
        _uid: u32,
        _gid: u32,
    ) -> VfsResult<Arc<VfsInode>> {
        Err(VfsError::PermissionDenied)
    }

    /// Creates an additional name for an existing non-directory child.
    ///
    /// Default matches simple pseudo filesystems, which do not accept `link`;
    /// ramfs-backed devtmpfs directories do.
    fn install_link(
        &self,
        _lookup: SimpleDirLookup<'_>,
        _target: &Dentry,
        _name: &str,
    ) -> VfsResult<()> {
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

    /// Removes the VFS-resolved non-directory child from the backing store.
    fn unlink(&self, _dir: &VfsInode, _victim: &LockedDentry<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Removes the VFS-resolved directory child from the backing store.
    fn rmdir(&self, _dir: &VfsInode, _victim: &LockedDentry<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Renames `old` to `new` inside the backing store.
    ///
    /// `new` may name an existing non-directory child, which is replaced. Callers
    /// validate the paths against the VFS first; this callback only moves the
    /// entry it already resolved.
    fn rename(
        &self,
        _old: &LockedDentry<'_>,
        _new: &LockedDentry<'_>,
        _flags: RenameFlags,
    ) -> VfsResult<()> {
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
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let names: Vec<Cow<'a, str>> = self
            .entries
            .lock()
            .keys()
            .map(|name| Cow::Owned(name.clone()))
            .collect();
        Ok(KBox::new(names.into_iter())?)
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

    fn unlink(&self, _dir: &VfsInode, victim: &LockedDentry<'_>) -> VfsResult<()> {
        if self.remove_child(victim.name()) {
            victim.decrement_link_count();
            Ok(())
        } else {
            Err(VfsError::NotFound)
        }
    }

    fn install_link(
        &self,
        _lookup: SimpleDirLookup<'_>,
        target: &Dentry,
        name: &str,
    ) -> VfsResult<()> {
        let mut entries = self.entries.lock();
        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = target.vfs_inode();
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
        target.increment_link_count();
        Ok(())
    }

    fn rename(
        &self,
        old: &LockedDentry<'_>,
        new: &LockedDentry<'_>,
        _flags: RenameFlags,
    ) -> VfsResult<()> {
        self.rename_child(old.name(), new.name())
    }
}

/// A mapping of directory names to entries.
///
/// Static entries retain one inode independently of dentry-cache residency.
/// Dynamic [`Self::add_child`] callbacks own their inode-lifetime policy.
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
        let entry = entry.into_dir_mapping_entry();
        self.entries.lock().insert(name.into(), entry.factory);
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

    /// Removes a child from the mapping.
    pub fn remove_child(&self, name: &str) -> bool {
        self.entries.lock().remove(name).is_some()
    }

    /// Moves `old` to `new`, replacing an existing `new` entry.
    ///
    /// This is the backing-store part of `rename(2)`: the entry is taken out and
    /// reinserted under the new name in one critical section, so a reader never
    /// observes both names or neither.
    pub fn rename_child(&self, old: &str, new: &str) -> VfsResult<()> {
        let mut entries = self.entries.lock();
        let entry = entries.remove(old).ok_or(VfsError::NotFound)?;
        entries.insert(new.to_owned(), entry);
        Ok(())
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

/// Incremental enumeration source for a simple directory.
///
/// `key` is the source's sorted sparse identity, not the VFS cookie and not a
/// `d_ino`. The source generates names and resolves the current object for a
/// key; kvfs owns dot entries, cookies, seeking and short-read continuation.
pub trait SparseDirSource: Send + Sync + 'static {
    /// Validates one directory open. Implementations must not enumerate here.
    fn validate_open(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Returns the first sparse key, or `None` when the source is empty.
    fn first_key(&self) -> VfsResult<Option<u64>>;

    /// Returns the smallest key strictly greater than `after`.
    fn next_key(&self, after: u64) -> VfsResult<Option<u64>>;

    /// Returns the canonical name for `key`.
    fn name_for_key(&self, key: u64) -> VfsResult<String>;

    /// Resolves the current object for `key`, which must currently have `name`.
    fn lookup_key(&self, lookup: SimpleDirLookup<'_>, key: u64, name: &str) -> VfsResult<Dentry>;

    /// Resolves a path name without treating the name as a numeric key.
    fn lookup_name(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry>;
}

struct SparseDirAdapter {
    source: Arc<dyn SparseDirSource>,
}

impl SimpleDirOps for SparseDirAdapter {
    fn validate_open(&self) -> VfsResult<()> {
        self.source.validate_open()
    }

    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        // Sparse readdir must never take the full-name path. Returning a clear
        // error prevents accidental fallback (and documents the chain limit).
        Err(VfsError::OperationNotSupported)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        self.source.lookup_name(lookup, name)
    }

    fn sparse_source(&self) -> Option<&dyn SparseDirSource> {
        Some(self.source.as_ref())
    }

    fn supports_dentry_cache(&self) -> bool {
        // A sparse source has no invalidation contract in this version. Do not
        // let a fresh pathname lookup reuse a dentry after source replacement.
        false
    }
}

/// Directory created by [`SimpleDirOps::chain`].
pub struct ChainedDirOps<A, B>(A, B);

impl<A: SimpleDirOps, B: SimpleDirOps> SimpleDirOps for ChainedDirOps<A, B> {
    fn validate_open(&self) -> VfsResult<()> {
        self.0.validate_open()?;
        self.1.validate_open()
    }

    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(
            self.0.child_names()?.chain(self.1.child_names()?),
        )?)
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

const SPARSE_COOKIE_LIMIT: u64 = i64::MAX as u64;
const SPARSE_DOT_COOKIE: u64 = 1;
const SPARSE_DOTDOT_COOKIE: u64 = 2;

struct SparseDirCursor {
    next_cookie: u64,
    last_key: Option<u64>,
    scan_started: bool,
    pending_key: Option<u64>,
}

impl SparseDirCursor {
    fn new() -> Self {
        Self {
            next_cookie: 0,
            last_key: None,
            scan_started: false,
            pending_key: None,
        }
    }

    fn reset(&mut self) {
        self.next_cookie = 0;
        self.last_key = None;
        self.scan_started = false;
        self.pending_key = None;
    }

    fn next_key(&mut self, source: &dyn SparseDirSource) -> VfsResult<Option<u64>> {
        let key = if self.scan_started {
            match self.last_key {
                Some(after) => source.next_key(after)?,
                None => None,
            }
        } else {
            source.first_key()?
        };
        if key.is_some() {
            self.scan_started = true;
        }
        Ok(key)
    }
}

fn validate_sparse_name(name: &str) -> VfsResult<()> {
    if name.is_empty()
        || name == DOT
        || name == DOTDOT
        || name.contains('/')
        || name.as_bytes().contains(&0)
    {
        return Err(VfsError::InvalidData);
    }
    Ok(())
}

/// Simple directory.
pub struct SimpleDir<O> {
    node: SimpleFsNode,
    ops: Arc<O>,
}

impl SimpleDir<SparseDirAdapter> {
    /// Creates a directory whose readdir path uses a sparse source.
    ///
    /// The first version does not support combining this inode with
    /// `ChainedDirOps`; the adapter's full-name path is intentionally rejected.
    pub(crate) fn new_sparse_inode_with_owner(
        fs: Arc<SimpleFs>,
        source: Arc<dyn SparseDirSource>,
        permission: NodePermission,
        uid: u32,
        gid: u32,
    ) -> Arc<VfsInode> {
        Self::new_inode_with_owner(
            fs,
            Arc::new(SparseDirAdapter { source }),
            permission,
            uid,
            gid,
        )
    }
}

/// Creates a directory whose readdir path uses a sparse source.
///
/// The first version does not support combining this inode with
/// `ChainedDirOps`; the adapter's full-name path is intentionally rejected.
pub fn new_sparse_dir_inode(
    fs: Arc<SimpleFs>,
    source: Arc<dyn SparseDirSource>,
    permission: NodePermission,
    uid: u32,
    gid: u32,
) -> Arc<VfsInode> {
    SimpleDir::<SparseDirAdapter>::new_sparse_inode_with_owner(fs, source, permission, uid, gid)
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

    fn resolve_sparse_candidate(
        &self,
        source: &dyn SparseDirSource,
        dir: &Dentry,
        key: u64,
    ) -> VfsResult<Option<(String, Dentry)>> {
        let name = match source.name_for_key(key) {
            Ok(name) => name,
            Err(VfsError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        validate_sparse_name(&name)?;
        let entry = match source.lookup_key(SimpleDirLookup::new(dir), key, &name) {
            Ok(entry) => entry,
            Err(VfsError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        if entry.name_snapshot() != name {
            return Err(VfsError::InvalidData);
        }
        Ok(Some((name, entry)))
    }

    fn read_sparse_dir_at(
        &self,
        source: &dyn SparseDirSource,
        dir: &Dentry,
        ctx: &mut DirContext<'_>,
        cursor: &mut SparseDirCursor,
    ) -> VfsResult<usize> {
        let position = ctx.pos();
        if position > SPARSE_COOKIE_LIMIT {
            return Err(VfsError::from(LinuxError::EOVERFLOW));
        }

        if position == 0 {
            cursor.reset();
        } else if position < SPARSE_DOTDOT_COOKIE {
            cursor.reset();
            cursor.next_cookie = position;
        } else if position != cursor.next_cookie
            && !self.rebuild_sparse_cursor(source, dir, cursor, position)?
        {
            // The requested position is beyond the current EOF. Keep it as the
            // caller's observable f_pos; a later insertion can make it valid.
            return Ok(0);
        }

        let mut count = 0;
        if ctx.pos() == 0 {
            let metadata = dir.metadata();
            if !ctx.emit(
                DOT,
                metadata.inode,
                metadata.mode.node_type(),
                SPARSE_DOT_COOKIE,
            ) {
                return Ok(count);
            }
            cursor.next_cookie = SPARSE_DOT_COOKIE;
            count += 1;
        }
        if ctx.pos() == 1 {
            let metadata = dir
                .parent()
                .map_or_else(|| dir.metadata(), |parent| parent.metadata());
            if !ctx.emit(
                DOTDOT,
                metadata.inode,
                metadata.mode.node_type(),
                SPARSE_DOTDOT_COOKIE,
            ) {
                return Ok(count);
            }
            cursor.next_cookie = SPARSE_DOTDOT_COOKIE;
            count += 1;
        }

        loop {
            if cursor.pending_key.is_none() {
                let Some(key) = cursor.next_key(source)? else {
                    return Ok(count);
                };
                cursor.pending_key = Some(key);
            }
            let key = cursor.pending_key.expect("sparse pending key");
            let Some((name, entry)) = self.resolve_sparse_candidate(source, dir, key)? else {
                cursor.last_key = Some(key);
                cursor.pending_key = None;
                continue;
            };

            let next_cookie = cursor
                .next_cookie
                .checked_add(1)
                .filter(|cookie| *cookie <= SPARSE_COOKIE_LIMIT)
                .ok_or_else(|| VfsError::from(LinuxError::EOVERFLOW))?;
            let metadata = entry.metadata();
            if !ctx.emit(
                &name,
                metadata.inode,
                metadata.mode.node_type(),
                next_cookie,
            ) {
                cursor.pending_key = Some(key);
                return Ok(count);
            }
            cursor.pending_key = None;
            cursor.last_key = Some(key);
            cursor.next_cookie = next_cookie;
            count += 1;
        }
    }

    fn rebuild_sparse_cursor(
        &self,
        source: &dyn SparseDirSource,
        dir: &Dentry,
        cursor: &mut SparseDirCursor,
        target: u64,
    ) -> VfsResult<bool> {
        cursor.reset();
        cursor.next_cookie = SPARSE_DOTDOT_COOKIE;

        while cursor.next_cookie < target {
            let Some(key) = cursor.next_key(source)? else {
                return Ok(false);
            };
            cursor.pending_key = Some(key);
            let Some(_entry) = self.resolve_sparse_candidate(source, dir, key)? else {
                cursor.last_key = Some(key);
                cursor.pending_key = None;
                continue;
            };

            let next_cookie = cursor
                .next_cookie
                .checked_add(1)
                .filter(|cookie| *cookie <= SPARSE_COOKIE_LIMIT)
                .ok_or_else(|| VfsError::from(LinuxError::EOVERFLOW))?;
            cursor.next_cookie = next_cookie;
            cursor.last_key = Some(key);
            cursor.pending_key = None;
        }
        Ok(true)
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
                    let entry = match dir.lookup(other) {
                        Ok(entry) => entry,
                        Err(VfsError::NotFound) => continue,
                        Err(error) => return Err(error),
                    };
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
        let mut metadata = self
            .dir
            .node
            .getattr(idmap, path, request_mask, query_flags)?;
        if let Some(nlink) = self.dir.ops.link_count()? {
            // This is a query-time attribute, not a cached inode update.
            metadata.nlink = nlink;
        }
        Ok(metadata)
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

    fn create(
        &self,
        _idmap: &crate::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        _exclusive: bool,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        if mode.node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let inode = self
            .dir
            .ops
            .create_file(dir, dentry.name(), mode, uid, gid)?;
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
        old_dentry: &Dentry,
        _dir: &crate::VfsInode,
        new_dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let parent = new_dentry.parent().ok_or(VfsError::InvalidInput)?;
        self.dir
            .ops
            .install_link(SimpleDirLookup::new(&parent), old_dentry, new_dentry.name())?;
        new_dentry.instantiate(old_dentry.vfs_inode())
    }

    fn unlink(&self, dir: &crate::VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.dir.ops.unlink(dir, dentry)
    }

    fn rename(
        &self,
        _idmap: &crate::MountIdmap,
        old_dir: &crate::VfsInode,
        old_dentry: &LockedDentry<'_>,
        new_dir: &crate::VfsInode,
        new_dentry: &LockedDentry<'_>,
        flags: crate::RenameFlags,
    ) -> VfsResult<()> {
        if !core::ptr::eq(old_dir, new_dir) {
            // A simple directory has one backing mapping; a cross-directory
            // rename would update the source map with the destination name.
            return Err(VfsError::OperationNotSupported);
        }
        if !flags.is_empty() {
            // The backing mapping has no atomic exchange/whiteout operation;
            // reject the extended modes instead of silently replacing an entry.
            return Err(VfsError::OperationNotSupported);
        }
        self.dir.ops.rename(old_dentry, new_dentry, flags)
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
    fn open(
        self: Arc<Self>,
        _inode: &crate::VfsInode,
        file: &mut crate::VfsFileBuilder,
    ) -> VfsResult<()> {
        self.dir.ops.validate_open()?;
        if self.dir.ops.sparse_source().is_some() {
            file.set_private_data(Arc::new(Mutex::new(SparseDirCursor::new())));
        }
        Ok(())
    }

    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        Some(self)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        let is_sparse = self.dir.ops.sparse_source().is_some();
        if is_sparse && whence == SEEK_END {
            return Err(VfsError::InvalidInput);
        }

        let mut position = file.position_lock();
        let base = match whence {
            SEEK_SET => 0i128,
            SEEK_CUR => i128::from(*position),
            SEEK_END => i128::from(file.size()),
            _ => return Err(VfsError::InvalidInput),
        };
        let new_offset = base + i128::from(offset);
        if new_offset < 0 {
            return Err(VfsError::InvalidInput);
        }
        if is_sparse && new_offset > i128::from(SPARSE_COOKIE_LIMIT) {
            return Err(VfsError::from(LinuxError::EOVERFLOW));
        }
        if !is_sparse && new_offset > i128::from(i64::MAX) {
            return Err(VfsError::InvalidInput);
        }
        let new_offset =
            u64::try_from(new_offset).map_err(|_| VfsError::from(LinuxError::EOVERFLOW))?;
        *position = new_offset;
        Ok(new_offset)
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        generic_read_dir(buf, offset)
    }

    fn fsync(
        &self,
        _file: &VfsFile,
        _range: core::ops::Range<u64>,
        _data_only: bool,
    ) -> VfsResult<()> {
        noop_fsync()
    }
}

impl<O: SimpleDirOps> FileDirOperations for SimpleDirFileOperations<O> {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        if let Some(source) = self.dir.ops.sparse_source() {
            let cursor = file
                .private_data_get::<Mutex<SparseDirCursor>>()
                .ok_or(VfsError::InvalidInput)?;
            let mut cursor = cursor.lock();
            return self
                .dir
                .read_sparse_dir_at(source, file.path().dentry(), ctx, &mut cursor);
        }
        let start = ctx.pos();
        self.dir.read_dir_at(file.path().dentry(), start, ctx)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/simple_dir.rs");
