// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS inode identity and inode cache helpers.

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicU16, AtomicUsize, Ordering},
};

use bitflags::bitflags;
use hashbrown::HashMap;
use kcred::Cred;
use kerrno::LinuxError;
use klazy::Once;
use ktask::WaitQueue;
use ktime_types::SystemTime;

use super::{
    DeviceFileOps,
    dentry::DentryAlias,
    device::{block_device_file_operations, character_device_file_operations},
};
use crate::{
    AddressSpace, AddressSpaceOperations, DelayedCall, Dentry, DeviceId, FiemapExtentInfo,
    FileOperations, LockedDentry, Metadata, MetadataUpdate, MountIdmap, Mutex, NodePermission,
    NodeType, Path, Permission, RwLock, RwLockReadGuard, RwLockWriteGuard, SuperBlock, Umode,
    VfsError, VfsFileBuilder, VfsResult, XattrName, XattrNameSink, XattrSetFlags,
    address_space::{default_address_space_operations, empty_address_space_operations},
    generic_permission,
    pipe::{PipeObject, fifo_file_operations},
};

const IOP_CACHED_LINK: u16 = 0x0040;
const INODE_BYTES_PER_BLOCK: u64 = 512;

/// Timestamp operation requested by an automatic VFS time update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeUpdateTime {
    /// Update the access time.
    Access,
    /// Update the modification and status-change times.
    ChangeAndModification,
}

bitflags! {
    /// Flags describing special inode behaviors.
    #[derive(Debug, Clone, Copy)]
    pub struct NodeFlags: u32 {
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

        /// Inode is private to internal kernel users.
        const PRIVATE = 0x0010;

        /// Inode is an anonymous inode.
        const ANON_INODE = 0x0020;

        /// Inode contents and metadata cannot be modified.
        const IMMUTABLE = 0x0040;

        /// Inode contents may only be extended by append operations.
        const APPEND_ONLY = 0x0080;
    }
}

bitflags! {
    /// Lookup semantics passed to `InodeDirOperations::lookup`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct InodeLookupFlags: u32 {
    }
}

bitflags! {
    /// Rename semantics passed through the VFS namespace operation.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct RenameFlags: u32 {
        /// Fail if the destination already exists.
        const NOREPLACE = 1 << 0;
        /// Atomically exchange the source and destination.
        const EXCHANGE = 1 << 1;
        /// Leave a whiteout at the source location.
        const WHITEOUT = 1 << 2;
    }
}

impl RenameFlags {
    /// Returns whether the requested rename modes cannot be combined.
    pub const fn has_conflicting_modes(self) -> bool {
        self.contains(Self::EXCHANGE) && self.intersects(Self::NOREPLACE.union(Self::WHITEOUT))
    }
}

bitflags! {
    /// Attribute fields requested from `InodeOperations::getattr`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct GetattrRequestMask: u32 {
    }

    /// Query semantics passed to `InodeOperations::getattr`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct GetattrQueryFlags: u32 {
    }
}

/// Directory inode operations.
///
/// Namespace entry points are called by the VFS while holding the locks needed
/// for the operation. Slow lookup holds the parent lock shared. Mutation holds
/// the parent lock exclusive and also locks source or victim inodes when Linux
/// directory-locking rules require it. Dentry arguments that carry operation
/// names are passed as [`LockedDentry`], allowing callbacks to borrow
/// `dentry.name()` without cloning.
///
/// Lookup follows Linux `->lookup`: a miss leaves the supplied dentry negative
/// and returns `Ok(None)`, while a found inode normally instantiates that same
/// dentry and also returns `Ok(None)`. `Ok(Some(_))` is reserved for an existing
/// directory alias, matching `d_splice_alias()`. Create-like callbacks must
/// instantiate the supplied dentry before returning success.
///
/// Implementations may sleep, but must not re-enter namespace operations on the
/// same VFS objects while these locks are held.
pub trait InodeDirOperations: Send + Sync {
    fn lookup(
        &self,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _flags: InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        Err(VfsError::NotADirectory)
    }

    fn create(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _mode: Umode,
        _exclusive: bool,
        _cred: &Cred,
    ) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &VfsInode,
        _new_dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        Err(VfsError::NotADirectory)
    }

    fn unlink(&self, _dir: &VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
        Err(VfsError::NotADirectory)
    }

    fn symlink(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _symname: &str,
        _cred: &Cred,
    ) -> VfsResult<()> {
        Err(VfsError::NotADirectory)
    }

    fn mkdir(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _mode: Umode,
        _cred: &Cred,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rmdir(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.unlink(dir, dentry)
    }

    fn mknod(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _mode: Umode,
        _device: DeviceId,
        _cred: &Cred,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rename(
        &self,
        _idmap: &MountIdmap,
        _old_dir: &VfsInode,
        _old_dentry: &LockedDentry<'_>,
        _new_dir: &VfsInode,
        _new_dentry: &LockedDentry<'_>,
        _flags: RenameFlags,
    ) -> VfsResult<()> {
        Err(VfsError::NotADirectory)
    }
}

/// Derives the initial owner and mode for a newly allocated inode.
///
/// The filesystem UID comes from the subjective credential. A set-group-ID
/// parent directory contributes its GID to every child and propagates the
/// set-group-ID bit to newly created subdirectories, matching
/// `inode_init_owner()` in Linux.
pub fn inode_init_owner(dir: &VfsInode, mode: Umode, cred: &Cred) -> (Umode, u32, u32) {
    let (parent_permission, parent_gid) = dir.attributes.creation_mode_context();
    let mut permission = mode.permission();
    let gid = if parent_permission.contains(NodePermission::SET_GID) {
        if mode.node_type() == NodeType::Directory {
            permission.insert(NodePermission::SET_GID);
        }
        parent_gid
    } else {
        cred.fsgid()
    };
    (mode.with_permission(permission), cred.fsuid(), gid)
}

/// Symbolic-link inode operations.
pub trait InodeSymlinkOperations: Send + Sync {
    fn get_link(
        &self,
        dentry: Option<&Dentry>,
        inode: &VfsInode,
        done: &mut DelayedCall,
    ) -> VfsResult<String>;
}

/// Inode-scoped FIEMAP operation.
pub trait InodeFiemapOperations: Send + Sync {
    /// Reports allocated extents intersecting the requested byte range.
    fn fiemap(
        &self,
        inode: &VfsInode,
        info: &mut FiemapExtentInfo<'_>,
        start: u64,
        length: u64,
    ) -> VfsResult<()>;
}

/// Optional FIEMAP capability borrowed from a VFS inode.
pub struct FiemapCapability<'a> {
    inode: &'a VfsInode,
    operations: &'a dyn InodeFiemapOperations,
}

impl FiemapCapability<'_> {
    /// Runs FIEMAP under the inode lock that serializes mapping changes.
    pub fn map(&self, info: &mut FiemapExtentInfo<'_>, start: u64, length: u64) -> VfsResult<()> {
        match self.inode.node_type() {
            NodeType::RegularFile => {
                let _data_guard = self.inode.data_lock.read();
                self.operations.fiemap(self.inode, info, start, length)
            }
            NodeType::Directory => {
                let _namespace_guard = self.inode.lock_namespace_shared();
                self.operations.fiemap(self.inode, info, start, length)
            }
            _ => Err(VfsError::OperationNotSupported),
        }
    }
}

/// Inode operation table installed on VFS inodes.
pub trait InodeOperations: Send + Sync + 'static {
    /// Checks access to this inode for the supplied task credentials.
    fn permission(
        &self,
        _idmap: &MountIdmap,
        inode: &VfsInode,
        permission: Permission,
        cred: &Cred,
    ) -> VfsResult<()> {
        generic_permission(inode, permission, cred)
    }

    /// Returns directory operations when this inode supports directory lookup.
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        None
    }

    /// Returns symlink operations when this inode supports link following.
    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        None
    }

    /// Returns FIEMAP operations when this inode exposes extent mappings.
    fn fiemap_operations(&self) -> Option<&dyn InodeFiemapOperations> {
        None
    }

    /// Updates inode metadata.
    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        _attr: MetadataUpdate,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    /// Applies an automatic VFS timestamp update.
    fn update_time(
        &self,
        idmap: &MountIdmap,
        dentry: &Dentry,
        timestamp: SystemTime,
        update: InodeUpdateTime,
    ) -> VfsResult<()> {
        let (atime, mtime, ctime) = match update {
            InodeUpdateTime::Access => (Some(timestamp), None, None),
            InodeUpdateTime::ChangeAndModification => (None, Some(timestamp), Some(timestamp)),
        };
        self.setattr(
            idmap,
            dentry,
            MetadataUpdate {
                atime,
                mtime,
                ctime,
                ..Default::default()
            },
        )
    }

    /// Returns metadata for this inode.
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        path: Option<&Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let path = path.ok_or(VfsError::InvalidInput)?;
        Ok(path.metadata())
    }

    /// Reads an extended attribute from this inode.
    fn get_xattr(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        _name: &XattrName,
    ) -> VfsResult<Vec<u8>> {
        Err(VfsError::OperationNotSupported)
    }

    /// Streams complete extended-attribute names stored on this inode.
    fn list_xattrs(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        _sink: &mut dyn XattrNameSink,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    /// Creates or replaces an extended attribute on this inode.
    fn set_xattr(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        _name: &XattrName,
        _value: &[u8],
        _flags: XattrSetFlags,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    /// Removes an extended attribute from this inode.
    fn remove_xattr(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        _name: &XattrName,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

struct EmptyInodeOperations;

impl InodeOperations for EmptyInodeOperations {}

#[derive(Clone, Copy, Debug)]
struct InodeTimestamp {
    sec: i64,
    nsec: u32,
}

impl InodeTimestamp {
    fn zero() -> Self {
        Self { sec: 0, nsec: 0 }
    }

    fn from_system_time(value: SystemTime) -> Self {
        Self {
            sec: value.unix_seconds(),
            nsec: value.subsec_nanos(),
        }
    }

    fn as_system_time(self) -> SystemTime {
        SystemTime::from_unix_parts(self.sec, self.nsec)
            .expect("cached inode timestamp is normalized")
    }
}

#[derive(Clone, Copy, Debug)]
struct InodeBlockAccounting {
    bytes: u16,
    blocks: u64,
}

impl InodeBlockAccounting {
    fn new(blocks: u64, bytes: u16) -> Self {
        Self { bytes, blocks }
    }

    fn set_allocated_bytes(&mut self, bytes: u64) {
        self.blocks = bytes >> 9;
        self.bytes = (bytes & (INODE_BYTES_PER_BLOCK - 1)) as u16;
    }
}

#[derive(Clone, Copy)]
struct InodeOwner {
    uid: u32,
    gid: u32,
}

impl InodeOwner {
    fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

#[derive(Clone, Copy)]
struct InodeTimes {
    accessed_at: InodeTimestamp,
    modified_at: InodeTimestamp,
    changed_at: InodeTimestamp,
}

impl InodeTimes {
    fn new(
        accessed_at: InodeTimestamp,
        modified_at: InodeTimestamp,
        changed_at: InodeTimestamp,
    ) -> Self {
        Self {
            accessed_at,
            modified_at,
            changed_at,
        }
    }
}

struct InodeMode {
    node_type: NodeType,
    permission: Mutex<NodePermission>,
}

impl InodeMode {
    fn new(mode: Umode) -> Self {
        Self {
            node_type: mode.node_type(),
            permission: Mutex::new(mode.permission()),
        }
    }

    fn mode(&self) -> Umode {
        Umode::new(self.node_type, *self.permission.lock())
    }

    fn set_permission(&self, permission: NodePermission) {
        *self.permission.lock() = permission;
    }
}

struct InodeIdentity {
    inode_cache: Once<Weak<InodeCacheInner>>,
    super_block: Once<Weak<SuperBlock>>,
    number: u64,
    aliases: Mutex<Vec<DentryAlias>>,
}

impl InodeIdentity {
    fn new(number: u64) -> Self {
        Self {
            inode_cache: Once::new(),
            super_block: Once::new(),
            number,
            aliases: Mutex::default(),
        }
    }

    fn number(&self) -> u64 {
        self.number
    }

    fn bind_inode_cache(&self, inode_cache: &Arc<InodeCacheInner>) {
        let bound = self.inode_cache.call_once(|| Arc::downgrade(inode_cache));
        if let Some(bound) = bound.upgrade() {
            assert!(
                Arc::ptr_eq(&bound, inode_cache),
                "one VFS inode identity must not belong to multiple inode caches"
            );
        }
    }

    fn inode_cache(&self) -> Option<Arc<InodeCacheInner>> {
        self.inode_cache.get().and_then(Weak::upgrade)
    }

    fn bind_super_block(&self, super_block: &Arc<SuperBlock>) {
        let bound = self.super_block.call_once(|| Arc::downgrade(super_block));
        if let Some(bound) = bound.upgrade() {
            debug_assert!(Arc::ptr_eq(&bound, super_block));
        }
    }

    fn super_block(&self) -> Option<Arc<SuperBlock>> {
        self.super_block.get().and_then(Weak::upgrade)
    }

    fn add_alias(&self, dentry: &Dentry, is_directory: bool) -> bool {
        let mut aliases = self.aliases.lock();
        aliases.retain(DentryAlias::is_live);
        if aliases.iter().any(|alias| alias.points_to(dentry)) {
            return true;
        }
        if is_directory && !aliases.is_empty() {
            return false;
        }
        aliases.push(DentryAlias::new(dentry));
        true
    }

    fn directory_alias(&self) -> Option<Dentry> {
        let mut aliases = self.aliases.lock();
        aliases.retain(DentryAlias::is_live);
        aliases.first().and_then(DentryAlias::upgrade)
    }
}

/// Filesystem-owned storage for the generic attributes of one VFS inode.
///
/// Filesystems whose private inode component already contains the generic
/// inode fields can implement this interface so KVFS and the filesystem use
/// one attribute state. This is the object-oriented equivalent of embedding
/// Linux `struct inode` in a filesystem-private inode structure.
pub trait InodeAttributeOperations: Send + Sync {
    /// Builds a snapshot using the inode number owned by the VFS identity.
    fn fill_metadata(&self, inode_number: u64) -> Metadata;

    /// Returns the inode mode without requiring a full metadata snapshot.
    fn mode(&self) -> Umode;

    /// Returns the inode owner without requiring a full metadata snapshot.
    fn owner(&self) -> (u32, u32);

    /// Returns the inode link count without requiring a full metadata snapshot.
    fn link_count(&self) -> u64;

    /// Returns the inode generation number.
    fn generation(&self) -> u32;

    /// Returns the inode device number without requiring a full metadata snapshot.
    fn rdev(&self) -> DeviceId;

    /// Returns the visible inode size without requiring a full metadata snapshot.
    fn size(&self) -> u64;

    /// Returns the preferred I/O block size.
    fn block_size(&self) -> u64;

    /// Returns the allocated 512-byte block count.
    fn blocks(&self) -> u64;

    /// Updates permission bits without changing the inode type.
    fn set_permission(&self, permission: NodePermission);

    /// Updates the inode owner.
    fn set_owner(&self, uid: u32, gid: u32);

    /// Updates the link count.
    fn set_link_count(&self, link_count: u64);

    /// Increments the link count.
    fn increment_link_count(&self);

    /// Decrements the link count.
    fn decrement_link_count(&self);

    /// Updates the visible inode size.
    fn set_size(&self, size: u64);

    /// Updates the last-access timestamp.
    fn set_accessed_at(&self, value: SystemTime);

    /// Updates the last-modification timestamp.
    fn set_modified_at(&self, value: SystemTime);

    /// Updates the last-status-change timestamp.
    fn set_changed_at(&self, value: SystemTime);

    /// Updates the number of allocated bytes.
    fn set_allocated_bytes(&self, bytes: u64);

    /// Adds to allocated-byte accounting.
    fn add_allocated_bytes(&self, bytes: u64);

    /// Subtracts from allocated-byte accounting.
    fn subtract_allocated_bytes(&self, bytes: u64);
}

struct OwnedInodeAttributes {
    mode: InodeMode,
    owner: Mutex<InodeOwner>,
    link_count: Mutex<u64>,
    generation: u32,
    rdev: DeviceId,
    size: Mutex<u64>,
    times: Mutex<InodeTimes>,
    block_accounting: Mutex<InodeBlockAccounting>,
    block_bits: u8,
}

impl OwnedInodeAttributes {
    fn new(init: VfsInodeInit) -> Self {
        Self {
            mode: InodeMode::new(init.mode),
            owner: Mutex::new(InodeOwner::new(init.uid, init.gid)),
            link_count: Mutex::new(init.link_count),
            generation: init.generation,
            rdev: init.rdev,
            size: Mutex::new(init.size),
            times: Mutex::new(InodeTimes::new(
                init.accessed_at,
                init.modified_at,
                init.changed_at,
            )),
            block_accounting: Mutex::new(InodeBlockAccounting::new(init.blocks, init.block_bytes)),
            block_bits: init.block_bits,
        }
    }

    fn mode(&self) -> Umode {
        self.mode.mode()
    }

    fn set_permission(&self, permission: NodePermission) {
        self.mode.set_permission(permission);
    }

    fn owner(&self) -> (u32, u32) {
        let owner = *self.owner.lock();
        (owner.uid, owner.gid)
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        *self.owner.lock() = InodeOwner::new(uid, gid);
    }

    fn link_count(&self) -> u64 {
        *self.link_count.lock()
    }

    fn set_link_count(&self, link_count: u64) {
        *self.link_count.lock() = link_count;
    }

    fn increment_link_count(&self) {
        let mut link_count = self.link_count.lock();
        *link_count = link_count.saturating_add(1);
    }

    fn decrement_link_count(&self) {
        let mut link_count = self.link_count.lock();
        debug_assert!(*link_count > 0);
        *link_count = link_count.saturating_sub(1);
    }

    fn generation(&self) -> u32 {
        self.generation
    }

    fn rdev(&self) -> DeviceId {
        self.rdev
    }

    fn size(&self) -> u64 {
        *self.size.lock()
    }

    fn set_size(&self, size: u64) {
        *self.size.lock() = size;
    }

    fn set_accessed_at(&self, value: SystemTime) {
        let mut times = self.times.lock();
        times.accessed_at = InodeTimestamp::from_system_time(value);
    }

    fn set_modified_at(&self, value: SystemTime) {
        let mut times = self.times.lock();
        times.modified_at = InodeTimestamp::from_system_time(value);
    }

    fn set_changed_at(&self, value: SystemTime) {
        let mut times = self.times.lock();
        times.changed_at = InodeTimestamp::from_system_time(value);
    }

    fn block_bits(&self) -> u8 {
        self.block_bits
    }

    fn blocks(&self) -> u64 {
        self.block_accounting.lock().blocks
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.block_accounting.lock().set_allocated_bytes(bytes);
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        let mut accounting = self.block_accounting.lock();
        let allocated = accounting
            .blocks
            .saturating_mul(INODE_BYTES_PER_BLOCK)
            .saturating_add(u64::from(accounting.bytes))
            .saturating_add(bytes);
        accounting.set_allocated_bytes(allocated);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        let mut accounting = self.block_accounting.lock();
        let allocated = accounting
            .blocks
            .saturating_mul(INODE_BYTES_PER_BLOCK)
            .saturating_add(u64::from(accounting.bytes))
            .saturating_sub(bytes);
        accounting.set_allocated_bytes(allocated);
    }

    fn fill_metadata(&self, inode_number: u64) -> Metadata {
        let (uid, gid) = self.owner();
        let times = *self.times.lock();
        Metadata {
            device: 0,
            inode: inode_number,
            nlink: self.link_count(),
            mode: self.mode(),
            uid,
            gid,
            size: self.size(),
            block_size: 1_u64 << self.block_bits(),
            blocks: self.blocks(),
            rdev: self.rdev(),
            atime: times.accessed_at.as_system_time(),
            mtime: times.modified_at.as_system_time(),
            ctime: times.changed_at.as_system_time(),
        }
    }
}

impl InodeAttributeOperations for OwnedInodeAttributes {
    fn fill_metadata(&self, inode_number: u64) -> Metadata {
        OwnedInodeAttributes::fill_metadata(self, inode_number)
    }

    fn mode(&self) -> Umode {
        OwnedInodeAttributes::mode(self)
    }

    fn owner(&self) -> (u32, u32) {
        OwnedInodeAttributes::owner(self)
    }

    fn link_count(&self) -> u64 {
        OwnedInodeAttributes::link_count(self)
    }

    fn generation(&self) -> u32 {
        OwnedInodeAttributes::generation(self)
    }

    fn rdev(&self) -> DeviceId {
        OwnedInodeAttributes::rdev(self)
    }

    fn size(&self) -> u64 {
        OwnedInodeAttributes::size(self)
    }

    fn block_size(&self) -> u64 {
        1_u64 << OwnedInodeAttributes::block_bits(self)
    }

    fn blocks(&self) -> u64 {
        OwnedInodeAttributes::blocks(self)
    }

    fn set_permission(&self, permission: NodePermission) {
        OwnedInodeAttributes::set_permission(self, permission);
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        OwnedInodeAttributes::set_owner(self, uid, gid);
    }

    fn set_link_count(&self, link_count: u64) {
        OwnedInodeAttributes::set_link_count(self, link_count);
    }

    fn increment_link_count(&self) {
        OwnedInodeAttributes::increment_link_count(self);
    }

    fn decrement_link_count(&self) {
        OwnedInodeAttributes::decrement_link_count(self);
    }

    fn set_size(&self, size: u64) {
        OwnedInodeAttributes::set_size(self, size);
    }

    fn set_accessed_at(&self, value: SystemTime) {
        OwnedInodeAttributes::set_accessed_at(self, value);
    }

    fn set_modified_at(&self, value: SystemTime) {
        OwnedInodeAttributes::set_modified_at(self, value);
    }

    fn set_changed_at(&self, value: SystemTime) {
        OwnedInodeAttributes::set_changed_at(self, value);
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        OwnedInodeAttributes::set_allocated_bytes(self, bytes);
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        OwnedInodeAttributes::add_allocated_bytes(self, bytes);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        OwnedInodeAttributes::subtract_allocated_bytes(self, bytes);
    }
}

struct InodeAttributes {
    operations: Arc<dyn InodeAttributeOperations>,
    operation_flags: AtomicU16,
    flags: NodeFlags,
}

impl InodeAttributes {
    fn new(init: VfsInodeInit, flags: NodeFlags) -> Self {
        Self::from_operations(Arc::new(OwnedInodeAttributes::new(init)), flags)
    }

    fn from_operations(operations: Arc<dyn InodeAttributeOperations>, flags: NodeFlags) -> Self {
        Self {
            operations,
            operation_flags: AtomicU16::new(0),
            flags,
        }
    }

    fn metadata(&self, inode_number: u64) -> Metadata {
        self.operations.fill_metadata(inode_number)
    }

    fn mode(&self) -> Umode {
        self.operations.mode()
    }

    fn node_type(&self) -> NodeType {
        self.mode().node_type()
    }

    fn flags(&self) -> NodeFlags {
        self.flags
    }

    fn operation_flags(&self) -> u16 {
        self.operation_flags.load(Ordering::Acquire)
    }

    fn insert_operation_flags(&self, flags: u16) {
        let _ = self.operation_flags.fetch_or(flags, Ordering::AcqRel);
    }

    fn set_permission(&self, permission: NodePermission) {
        self.operations.set_permission(permission);
    }

    fn creation_mode_context(&self) -> (NodePermission, u32) {
        let (_, gid) = self.operations.owner();
        (self.operations.mode().permission(), gid)
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.operations.set_owner(uid, gid);
    }

    fn link_count(&self) -> u64 {
        self.operations.link_count()
    }

    fn set_link_count(&self, link_count: u64) {
        self.operations.set_link_count(link_count);
    }

    fn increment_link_count(&self) {
        self.operations.increment_link_count();
    }

    fn decrement_link_count(&self) {
        self.operations.decrement_link_count();
    }

    fn generation(&self) -> u32 {
        self.operations.generation()
    }

    fn rdev(&self) -> DeviceId {
        self.operations.rdev()
    }

    fn size(&self) -> u64 {
        self.operations.size()
    }

    fn set_size(&self, size: u64) {
        self.operations.set_size(size);
    }

    fn set_accessed_at(&self, value: SystemTime) -> SystemTime {
        self.operations.set_accessed_at(value);
        value
    }

    fn set_modified_at(&self, value: SystemTime) -> SystemTime {
        self.operations.set_modified_at(value);
        value
    }

    fn set_changed_at(&self, value: SystemTime) -> SystemTime {
        self.operations.set_changed_at(value);
        value
    }

    fn block_bits(&self) -> u8 {
        inode_blkbits(self.operations.block_size())
    }

    fn allocated_bytes(&self) -> u64 {
        self.operations
            .blocks()
            .saturating_mul(INODE_BYTES_PER_BLOCK)
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        self.operations.add_allocated_bytes(bytes);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        self.operations.subtract_allocated_bytes(bytes);
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.operations.set_allocated_bytes(bytes);
    }

    fn fill_metadata(&self, inode_number: u64) -> Metadata {
        self.metadata(inode_number)
    }
}

struct InodeSpecialState {
    character_device: Mutex<Option<Arc<dyn DeviceFileOps>>>,
    fifo_pipe: Mutex<Option<Arc<PipeObject>>>,
    cached_link: Once<String>,
}

impl InodeSpecialState {
    fn new() -> Self {
        Self {
            character_device: Mutex::new(None),
            fifo_pipe: Mutex::new(None),
            cached_link: Once::new(),
        }
    }

    fn character_device(&self) -> Option<Arc<dyn DeviceFileOps>> {
        self.character_device.lock().clone()
    }

    fn set_character_device(&self, ops: Arc<dyn DeviceFileOps>) {
        *self.character_device.lock() = Some(ops);
    }

    fn clear_character_device(&self, ops: &Arc<dyn DeviceFileOps>) {
        let mut character_device = self.character_device.lock();
        if character_device
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, ops))
        {
            *character_device = None;
        }
    }

    fn set_cached_link(&self, link: String) {
        self.cached_link.call_once(|| link);
    }

    fn cached_link(&self) -> Option<String> {
        self.cached_link.get().cloned()
    }
}

/// VFS inode identity shared by one or more directory entries.
///
/// This is the VFS-level owner for inode-scoped state. Path-specific state
/// belongs on a dentry; filesystem-owned inode state is kept in `private_data`.
pub struct VfsInode {
    identity: InodeIdentity,
    attributes: InodeAttributes,
    write_count: AtomicUsize,
    data_lock: RwLock<()>,
    namespace_lock: RwLock<()>,
    inode_operations: Arc<dyn InodeOperations>,
    file_operations: Arc<dyn FileOperations>,
    private_data: Arc<dyn Any + Send + Sync>,
    mapping: Arc<AddressSpace>,
    special_state: InodeSpecialState,
}

/// Weak reference to a VFS inode identity.
pub type WeakVfsInode = Weak<VfsInode>;

struct NoOpenFileOperations;
impl FileOperations for NoOpenFileOperations {
    fn open(self: Arc<Self>, _inode: &VfsInode, _file: &mut VfsFileBuilder) -> VfsResult<()> {
        Err(VfsError::from(LinuxError::ENXIO))
    }
}

fn no_open_file_operations() -> Arc<dyn FileOperations> {
    Arc::new(NoOpenFileOperations)
}

fn empty_inode_operations() -> Arc<dyn InodeOperations> {
    Arc::new(EmptyInodeOperations)
}

fn init_special_inode(
    node_type: NodeType,
    rdev: DeviceId,
) -> (Option<Arc<dyn FileOperations>>, DeviceId) {
    match node_type {
        NodeType::CharacterDevice => (Some(character_device_file_operations()), rdev),
        NodeType::BlockDevice => (Some(block_device_file_operations()), rdev),
        NodeType::Fifo => (Some(fifo_file_operations()), DeviceId::default()),
        NodeType::Socket => (None, DeviceId::default()),
        _ => (None, DeviceId::default()),
    }
}

/// Inode fields supplied by a filesystem while materializing a VFS inode.
#[derive(Clone, Copy, Debug)]
pub struct VfsInodeInit {
    number: u64,
    mode: Umode,
    size: u64,
    uid: u32,
    gid: u32,
    link_count: u64,
    rdev: DeviceId,
    generation: u32,
    accessed_at: InodeTimestamp,
    modified_at: InodeTimestamp,
    changed_at: InodeTimestamp,
    block_bytes: u16,
    block_bits: u8,
    blocks: u64,
}

impl VfsInodeInit {
    /// Build the required inode fields.
    pub fn new(number: u64, size: u64, mode: Umode) -> Self {
        Self {
            number,
            mode,
            size,
            uid: 0,
            gid: 0,
            link_count: 1,
            rdev: DeviceId::default(),
            generation: 0,
            accessed_at: InodeTimestamp::zero(),
            modified_at: InodeTimestamp::zero(),
            changed_at: InodeTimestamp::zero(),
            block_bytes: 0,
            block_bits: 0,
            blocks: 0,
        }
    }

    /// Build inode fields from a metadata snapshot.
    pub fn from_metadata(metadata: &Metadata) -> Self {
        Self::new(metadata.inode, metadata.size, metadata.mode)
            .with_owner_links_and_rdev(metadata.uid, metadata.gid, metadata.nlink, metadata.rdev)
            .with_stat_data(
                metadata.block_size,
                metadata.blocks,
                metadata.atime,
                metadata.mtime,
                metadata.ctime,
            )
    }

    /// Returns the inode object type encoded in the mode.
    pub fn node_type(&self) -> NodeType {
        self.mode.node_type()
    }

    /// Returns the inode number.
    pub fn inode_number(&self) -> u64 {
        self.number
    }

    /// Override the cached inode size.
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// Set cached owner, link-count, and special-device metadata.
    pub fn with_owner_links_and_rdev(
        mut self,
        uid: u32,
        gid: u32,
        link_count: u64,
        rdev: DeviceId,
    ) -> Self {
        self.uid = uid;
        self.gid = gid;
        self.link_count = link_count;
        self.rdev = rdev;
        self
    }

    /// Set the cached generation number.
    pub fn with_generation(mut self, generation: u32) -> Self {
        self.generation = generation;
        self
    }

    /// Set cached block accounting and timestamps.
    pub fn with_stat_data(
        mut self,
        block_size: u64,
        blocks: u64,
        atime: SystemTime,
        mtime: SystemTime,
        ctime: SystemTime,
    ) -> Self {
        self.accessed_at = InodeTimestamp::from_system_time(atime);
        self.modified_at = InodeTimestamp::from_system_time(mtime);
        self.changed_at = InodeTimestamp::from_system_time(ctime);
        self.block_bits = inode_blkbits(block_size);
        self.blocks = blocks;
        self
    }
}

struct InodeParts {
    private_data: Arc<dyn Any + Send + Sync>,
    inode_operations: Arc<dyn InodeOperations>,
    file_operations: Option<Arc<dyn FileOperations>>,
    flags: NodeFlags,
    address_space_operations: Arc<dyn AddressSpaceOperations>,
    init: VfsInodeInit,
}

impl VfsInode {
    /// Constructs an inode whose filesystem operation object also stores its
    /// generic attributes.
    ///
    /// This keeps filesystem-private and generic inode state in one composed
    /// object instead of maintaining a second VFS attribute copy.
    pub fn new_with_inode_attribute_operations<T>(
        node: Arc<T>,
        flags: NodeFlags,
        mut init: VfsInodeInit,
    ) -> Arc<Self>
    where
        T: Any
            + Send
            + Sync
            + InodeOperations
            + FileOperations
            + AddressSpaceOperations
            + InodeAttributeOperations,
    {
        let node_type = init.node_type();
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let attribute_operations: Arc<dyn InodeAttributeOperations> = node.clone();
        let (file_operations, address_space_operations) = match node_type {
            NodeType::Directory => {
                let file_operations: Arc<dyn FileOperations> = node.clone();
                (Some(file_operations), empty_address_space_operations())
            }
            NodeType::RegularFile | NodeType::Unknown => {
                let file_operations: Arc<dyn FileOperations> = node.clone();
                let address_space_operations: Arc<dyn AddressSpaceOperations> = node.clone();
                (Some(file_operations), address_space_operations)
            }
            NodeType::Symlink => {
                let address_space_operations: Arc<dyn AddressSpaceOperations> = node.clone();
                (None, address_space_operations)
            }
            NodeType::CharacterDevice
            | NodeType::BlockDevice
            | NodeType::Fifo
            | NodeType::Socket => {
                let (file_operations, rdev) = init_special_inode(node_type, init.rdev);
                init.rdev = rdev;
                (file_operations, empty_address_space_operations())
            }
        };
        Self::from_parts_with_attribute_operations(
            InodeParts {
                private_data,
                inode_operations,
                file_operations,
                flags,
                address_space_operations,
                init,
            },
            attribute_operations,
        )
    }

    /// Construct an inode identity for a non-directory node.
    pub fn new_file<T>(node: Arc<T>, init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let file_operations: Arc<dyn FileOperations> = node;
        Self::new_file_with_operations(
            private_data,
            inode_operations,
            file_operations,
            NodeFlags::empty(),
            init,
        )
    }

    /// Construct an inode identity for a non-directory node with explicit flags.
    pub fn new_file_with_flags<T>(node: Arc<T>, flags: NodeFlags, init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let file_operations: Arc<dyn FileOperations> = node;
        Self::new_file_with_operations(private_data, inode_operations, file_operations, flags, init)
    }

    /// Construct a file inode whose address space is implemented by `node`.
    pub fn new_file_with_address_space<T>(node: Arc<T>, init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations + AddressSpaceOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let file_operations: Arc<dyn FileOperations> = node.clone();
        let address_space_operations: Arc<dyn AddressSpaceOperations> = node;
        Self::new_file_with_address_space_operations(
            private_data,
            inode_operations,
            Some(file_operations),
            NodeFlags::empty(),
            address_space_operations,
            init,
        )
    }

    /// Construct an address-space-backed file inode with explicit flags.
    pub fn new_file_with_address_space_and_flags<T>(
        node: Arc<T>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations + AddressSpaceOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let file_operations: Arc<dyn FileOperations> = node.clone();
        let address_space_operations: Arc<dyn AddressSpaceOperations> = node;
        Self::new_file_with_address_space_operations(
            private_data,
            inode_operations,
            Some(file_operations),
            flags,
            address_space_operations,
            init,
        )
    }

    /// Construct a symbolic-link inode whose address space is implemented by `node`.
    pub fn new_symlink_with_address_space_and_flags<T>(
        node: Arc<T>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + AddressSpaceOperations,
    {
        debug_assert_eq!(init.mode.node_type(), NodeType::Symlink);
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let address_space_operations: Arc<dyn AddressSpaceOperations> = node;
        Self::new_file_with_address_space_operations(
            private_data,
            inode_operations,
            None,
            flags,
            address_space_operations,
            init,
        )
    }

    /// Constructs a cached symbolic-link inode with no file or address-space operations.
    pub(crate) fn new_cached_symlink<T>(
        node: Arc<T>,
        flags: NodeFlags,
        init: VfsInodeInit,
        target: String,
    ) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations,
    {
        debug_assert_eq!(init.mode.node_type(), NodeType::Symlink);
        debug_assert!(node.symlink_operations().is_some());
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node;
        let inode =
            Self::new_file_with_operation_tables(private_data, inode_operations, None, flags, init);
        inode.set_cached_link(target);
        inode
    }

    /// Construct a special inode from filesystem-filled inode fields.
    pub fn new_special<T>(node: Arc<T>, flags: NodeFlags, mut init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations,
    {
        let node_type = init.node_type();
        debug_assert!(matches!(
            node_type,
            NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Fifo | NodeType::Socket
        ));
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let (file_operations, rdev) = init_special_inode(node_type, init.rdev);
        init.rdev = rdev;
        Self::from_parts(InodeParts {
            private_data,
            inode_operations,
            file_operations,
            flags,
            address_space_operations: empty_address_space_operations(),
            init,
        })
    }

    /// Construct an inode identity from explicit inode and file operation tables.
    pub fn new_file_with_operations(
        private_data: Arc<dyn Any + Send + Sync>,
        inode_operations: Arc<dyn InodeOperations>,
        file_operations: Arc<dyn FileOperations>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self> {
        Self::new_file_with_operation_tables(
            private_data,
            inode_operations,
            Some(file_operations),
            flags,
            init,
        )
    }

    /// Construct a non-directory inode with no-open file operations.
    pub(crate) fn new_file_with_inode_operations(
        private_data: Arc<dyn Any + Send + Sync>,
        inode_operations: Arc<dyn InodeOperations>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self> {
        Self::new_file_with_operation_tables(private_data, inode_operations, None, flags, init)
    }

    fn new_file_with_operation_tables(
        private_data: Arc<dyn Any + Send + Sync>,
        inode_operations: Arc<dyn InodeOperations>,
        file_operations: Option<Arc<dyn FileOperations>>,
        flags: NodeFlags,
        mut init: VfsInodeInit,
    ) -> Arc<Self> {
        let node_type = init.mode.node_type();
        let is_special = matches!(
            node_type,
            NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Fifo | NodeType::Socket
        );
        let ops = if is_special {
            empty_address_space_operations()
        } else {
            default_address_space_operations(flags, node_type)
        };
        let rdev = if is_special {
            init.rdev
        } else {
            DeviceId::default()
        };
        init.rdev = rdev;
        let flags =
            if node_type == NodeType::RegularFile && !flags.contains(NodeFlags::ALWAYS_CACHE) {
                flags | NodeFlags::NON_CACHEABLE
            } else {
                flags
            };
        Self::from_parts(InodeParts {
            private_data,
            inode_operations,
            file_operations,
            flags,
            address_space_operations: ops,
            init,
        })
    }

    fn new_file_with_address_space_operations(
        private_data: Arc<dyn Any + Send + Sync>,
        inode_operations: Arc<dyn InodeOperations>,
        file_operations: Option<Arc<dyn FileOperations>>,
        flags: NodeFlags,
        ops: Arc<dyn crate::AddressSpaceOperations>,
        init: VfsInodeInit,
    ) -> Arc<Self> {
        let node_type = init.mode.node_type();
        debug_assert_ne!(node_type, NodeType::Directory);
        Self::from_parts(InodeParts {
            private_data,
            inode_operations,
            file_operations,
            flags,
            address_space_operations: ops,
            init,
        })
    }

    fn from_parts(parts: InodeParts) -> Arc<Self> {
        let init = parts.init;
        let attributes = InodeAttributes::new(init, parts.flags);
        Self::from_parts_with_attributes(parts, attributes)
    }

    fn from_parts_with_attribute_operations(
        parts: InodeParts,
        operations: Arc<dyn InodeAttributeOperations>,
    ) -> Arc<Self> {
        let metadata = operations.fill_metadata(parts.init.number);
        assert_eq!(
            metadata.inode, parts.init.number,
            "inode attribute operations must describe the constructed inode"
        );
        assert_eq!(
            metadata.mode.node_type(),
            parts.init.node_type(),
            "inode attribute operations must preserve the constructed inode type"
        );
        let attributes = InodeAttributes::from_operations(operations, parts.flags);
        Self::from_parts_with_attributes(parts, attributes)
    }

    fn from_parts_with_attributes(parts: InodeParts, attributes: InodeAttributes) -> Arc<Self> {
        let InodeParts {
            private_data,
            inode_operations,
            file_operations,
            flags,
            address_space_operations,
            init,
        } = parts;
        Arc::new_cyclic(move |this| Self {
            identity: InodeIdentity::new(init.number),
            attributes,
            write_count: AtomicUsize::new(0),
            data_lock: RwLock::new(()),
            namespace_lock: RwLock::new(()),
            inode_operations,
            file_operations: file_operations.unwrap_or_else(no_open_file_operations),
            private_data,
            mapping: AddressSpace::new_default(this.clone(), address_space_operations, flags),
            special_state: InodeSpecialState::new(),
        })
    }

    pub(crate) fn acquire_write_access(&self) {
        let previous = self.write_count.fetch_add(1, Ordering::AcqRel);
        debug_assert_ne!(previous, usize::MAX);
    }

    pub(crate) fn release_write_access(&self) {
        let previous = self.write_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert_ne!(previous, 0);
    }

    pub(crate) fn acquire_fifo_pipe(&self) -> VfsResult<Arc<PipeObject>> {
        let mut fifo_pipe = self.special_state.fifo_pipe.lock();
        let pipe = fifo_pipe.get_or_insert_with(PipeObject::new_fifo);
        pipe.acquire_file()?;
        Ok(pipe.clone())
    }

    pub(crate) fn release_fifo_pipe(&self, pipe: &Arc<PipeObject>) {
        let mut fifo_pipe = self.special_state.fifo_pipe.lock();
        assert!(
            fifo_pipe
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, pipe)),
            "released FIFO pipe is not attached to its inode"
        );
        if pipe.release_file() {
            *fifo_pipe = None;
        }
    }

    /// Returns the number of open files holding write access to this inode.
    pub fn write_count(&self) -> usize {
        self.write_count.load(Ordering::Acquire)
    }

    /// Construct a directory inode without directory file operations.
    pub fn new_dir<T>(node: Arc<T>, init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node;
        Self::new_dir_with_inode_operations(
            private_data,
            inode_operations,
            NodeFlags::empty(),
            init,
        )
    }

    /// Construct a directory inode without directory file operations.
    pub fn new_dir_with_flags<T>(node: Arc<T>, flags: NodeFlags, init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node;
        Self::new_dir_with_inode_operations(private_data, inode_operations, flags, init)
    }

    /// Construct a directory inode with default operation tables.
    pub fn new_dir_with_defaults(flags: NodeFlags, init: VfsInodeInit) -> Arc<Self> {
        Self::new_dir_with_inode_operations(Arc::new(()), empty_inode_operations(), flags, init)
    }

    fn new_dir_with_inode_operations(
        private_data: Arc<dyn Any + Send + Sync>,
        inode_operations: Arc<dyn InodeOperations>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self> {
        let is_directory = init.mode.node_type() == NodeType::Directory;
        debug_assert!(is_directory);
        Self::from_parts(InodeParts {
            private_data,
            inode_operations,
            flags,
            file_operations: None,
            address_space_operations: empty_address_space_operations(),
            init,
        })
    }

    /// Construct an openable directory inode from a shared operation object.
    pub fn new_openable_dir<T>(node: Arc<T>, init: VfsInodeInit) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations,
    {
        Self::new_openable_dir_with_flags(node, NodeFlags::empty(), init)
    }

    /// Construct an openable directory inode with explicit inode flags.
    pub fn new_openable_dir_with_flags<T>(
        node: Arc<T>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations,
    {
        let private_data: Arc<dyn Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let file_operations: Arc<dyn FileOperations> = node;
        Self::new_dir_with_operations(private_data, inode_operations, file_operations, flags, init)
    }

    /// Construct a directory inode identity from explicit operation tables.
    pub fn new_dir_with_operations(
        private_data: Arc<dyn Any + Send + Sync>,
        inode_operations: Arc<dyn InodeOperations>,
        file_operations: Arc<dyn FileOperations>,
        flags: NodeFlags,
        init: VfsInodeInit,
    ) -> Arc<Self> {
        let is_directory = init.mode.node_type() == NodeType::Directory;
        debug_assert!(is_directory);
        Self::from_parts(InodeParts {
            private_data,
            inode_operations,
            file_operations: Some(file_operations),
            flags,
            address_space_operations: empty_address_space_operations(),
            init,
        })
    }

    /// Gets the inode number of the node.
    pub fn inode(&self) -> u64 {
        self.identity.number()
    }

    /// Updates the metadata of the node.
    pub fn update_metadata(&self, dentry: &Dentry, mut update: MetadataUpdate) -> VfsResult<()> {
        let size = update.size;
        let mode = update.mode;
        let owner = update.owner;
        let atime = update.atime;
        let mtime = update.mtime;
        let changed = size.is_some()
            || mode.is_some()
            || owner.is_some()
            || atime.is_some()
            || mtime.is_some();
        if changed && update.ctime.is_none() {
            update.ctime = Some(ktime::realtime());
        }
        let ctime = update.ctime;
        self.inode_operations
            .as_ref()
            .setattr(&MountIdmap, dentry, update)?;
        if let Some(size) = size {
            self.set_size(size);
        }
        if let Some(atime) = atime {
            self.set_accessed_at(atime);
        }
        if let Some(mtime) = mtime {
            self.set_modified_at(mtime);
        }
        if let Some(ctime) = ctime {
            self.set_changed_at(ctime);
        }
        if let Some(mode) = mode {
            self.attributes.set_permission(mode);
        }
        if let Some((uid, gid)) = owner {
            self.attributes.set_owner(uid, gid);
        }
        Ok(())
    }

    pub(crate) fn update_time(
        &self,
        dentry: &Dentry,
        timestamp: SystemTime,
        update: InodeUpdateTime,
    ) -> VfsResult<()> {
        self.inode_operations
            .update_time(&MountIdmap, dentry, timestamp, update)?;
        match update {
            InodeUpdateTime::Access => {
                self.set_accessed_at(timestamp);
            }
            InodeUpdateTime::ChangeAndModification => {
                self.set_modified_at(timestamp);
                self.set_changed_at(timestamp);
            }
        }
        Ok(())
    }

    /// Gets the size of the node.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64> {
        Ok(self.size())
    }

    /// Returns the cached file size.
    pub fn size(&self) -> u64 {
        self.attributes.size()
    }

    pub(crate) fn set_size(&self, size: u64) {
        self.attributes.set_size(size);
    }

    pub(crate) fn set_len(&self, len: u64) -> VfsResult<()> {
        let _data_guard = self.data_lock.write();
        self.mapping.set_len(len)?;
        if self.size() == len {
            Ok(())
        } else {
            Err(VfsError::InvalidData)
        }
    }

    /// Records a file length already changed by the backing filesystem.
    pub fn update_size_after_backing_change(&self, len: u64) -> VfsResult<()> {
        let _data_guard = self.data_lock.write();
        self.mapping.truncate_setsize_after_backing_change(len)
    }

    pub(crate) fn lock_data(&self) -> RwLockWriteGuard<'_, ()> {
        self.data_lock.write()
    }

    /// Refreshes mutable cached attributes from a validated backing inode.
    ///
    /// This does not change the cached size because truncate ordering may
    /// require page-cache invalidation between backing-filesystem phases. Use
    /// [`Self::update_metadata_after_backing_change`] when the backing size is
    /// already final and no split truncate sequence is in progress.
    pub fn update_attributes_after_backing_change(&self, metadata: &Metadata) -> VfsResult<()> {
        let expected_block_size = 1_u64 << self.attributes.block_bits();
        if metadata.inode != self.inode()
            || metadata.mode.node_type() != self.node_type()
            || metadata.block_size != expected_block_size
            || metadata.rdev != self.attributes.rdev()
        {
            return Err(VfsError::InvalidInput);
        }

        self.attributes.set_permission(metadata.mode.permission());
        self.attributes.set_owner(metadata.uid, metadata.gid);
        self.attributes.set_link_count(metadata.nlink);
        self.attributes.set_accessed_at(metadata.atime);
        self.attributes.set_modified_at(metadata.mtime);
        self.attributes.set_changed_at(metadata.ctime);
        self.attributes
            .set_allocated_bytes(metadata.blocks.saturating_mul(INODE_BYTES_PER_BLOCK));
        Ok(())
    }

    /// Refreshes size and mutable cached attributes from a validated backing
    /// inode after a completed backing-store mutation.
    pub fn update_metadata_after_backing_change(&self, metadata: &Metadata) -> VfsResult<()> {
        self.update_attributes_after_backing_change(metadata)?;
        self.update_size_after_backing_change(metadata.size)
    }

    /// Builds a metadata snapshot from cached inode attributes.
    pub fn metadata(&self) -> Metadata {
        self.attributes.fill_metadata(self.inode())
    }

    /// Checks access through this inode's permission operation.
    pub fn permission(&self, permission: Permission, cred: &Cred) -> VfsResult<()> {
        self.inode_operations
            .permission(&MountIdmap, self, permission, cred)
    }

    /// Returns metadata through this inode's operation family.
    pub fn getattr(
        &self,
        path: &Path,
        request_mask: GetattrRequestMask,
        query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        self.inode_operations
            .as_ref()
            .getattr(&MountIdmap, Some(path), request_mask, query_flags)
    }

    /// Returns this inode's optional FIEMAP capability.
    pub fn fiemap_capability(&self) -> Option<FiemapCapability<'_>> {
        self.inode_operations
            .fiemap_operations()
            .map(|operations| FiemapCapability {
                inode: self,
                operations,
            })
    }

    pub(crate) fn get_xattr(&self, dentry: &Dentry, name: &XattrName) -> VfsResult<Vec<u8>> {
        self.inode_operations.get_xattr(dentry, self, name)
    }

    pub(crate) fn list_xattrs(
        &self,
        dentry: &Dentry,
        sink: &mut dyn XattrNameSink,
    ) -> VfsResult<()> {
        self.inode_operations.list_xattrs(dentry, self, sink)
    }

    pub(crate) fn set_xattr(
        &self,
        dentry: &Dentry,
        name: &XattrName,
        value: &[u8],
        flags: XattrSetFlags,
    ) -> VfsResult<()> {
        self.inode_operations
            .set_xattr(dentry, self, name, value, flags)
    }

    pub(crate) fn remove_xattr(&self, dentry: &Dentry, name: &XattrName) -> VfsResult<()> {
        self.inode_operations.remove_xattr(dentry, self, name)
    }

    /// Synchronizes the file to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.mapping.writepages(data_only)
    }

    /// Returns the flags of the node.
    pub fn flags(&self) -> NodeFlags {
        self.attributes.flags()
    }

    /// Returns operation flags cached on this inode.
    fn operation_flags(&self) -> u16 {
        self.attributes.operation_flags()
    }

    /// Returns the link count.
    pub fn link_count(&self) -> u64 {
        self.attributes.link_count()
    }

    /// Returns the inode generation number.
    pub fn generation(&self) -> u32 {
        self.attributes.generation()
    }

    /// Returns the special-device id.
    pub fn rdev(&self) -> DeviceId {
        self.attributes.rdev()
    }

    pub(crate) fn character_device(&self) -> Option<Arc<dyn DeviceFileOps>> {
        self.special_state.character_device()
    }

    pub(crate) fn set_character_device(&self, ops: Arc<dyn DeviceFileOps>) {
        self.special_state.set_character_device(ops);
    }

    pub(crate) fn clear_character_device(&self, ops: &Arc<dyn DeviceFileOps>) {
        self.special_state.clear_character_device(ops);
    }

    /// Store a cached symbolic-link target.
    pub fn set_cached_link(&self, link: String) {
        self.special_state.set_cached_link(link);
        self.attributes.insert_operation_flags(IOP_CACHED_LINK);
    }

    pub(crate) fn cached_link(&self) -> Option<String> {
        self.special_state.cached_link()
    }

    /// Returns the VFS node type for this inode.
    pub fn node_type(&self) -> NodeType {
        self.attributes.node_type()
    }

    /// Returns `true` if this inode is not a directory.
    pub fn is_file(&self) -> bool {
        self.node_type() != NodeType::Directory
    }

    /// Returns `true` if this inode wraps a directory node.
    pub fn is_dir(&self) -> bool {
        self.node_type() == NodeType::Directory
    }

    pub(crate) fn open_file_operations(&self) -> &Arc<dyn FileOperations> {
        &self.file_operations
    }

    pub(crate) fn magic_link(&self) -> Option<Arc<dyn crate::MagicLinkOps>> {
        self.file_operations.clone().magic_link()
    }

    fn require_directory_operations(&self) -> VfsResult<&dyn InodeDirOperations> {
        self.inode_operations
            .as_ref()
            .directory_operations()
            .ok_or(VfsError::NotADirectory)
    }

    fn require_symlink_operations(&self) -> VfsResult<&dyn InodeSymlinkOperations> {
        self.inode_operations
            .as_ref()
            .symlink_operations()
            .ok_or(VfsError::InvalidData)
    }

    pub(crate) fn lock_namespace_shared(&self) -> RwLockReadGuard<'_, ()> {
        self.namespace_lock.read()
    }

    pub(crate) fn lock_namespace_exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.namespace_lock.write()
    }

    pub(crate) fn supports_directory_operations(&self) -> bool {
        self.inode_operations
            .as_ref()
            .directory_operations()
            .is_some()
    }

    pub(crate) fn supports_symlink_operations(&self) -> bool {
        self.inode_operations
            .as_ref()
            .symlink_operations()
            .is_some()
    }

    pub(crate) fn create_with_mode(
        &self,
        dentry: &Dentry,
        mode: Umode,
        exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?.create(
            &MountIdmap,
            self,
            &dentry,
            mode,
            exclusive,
            cred,
        )
    }

    /// Applies the VFS SGID, umask, permission-mask, and node-type policy.
    pub(crate) fn prepare_create_mode(
        &self,
        mode: Umode,
        umask: NodePermission,
        allowed_permission: NodePermission,
        node_type: NodeType,
        cred: &Cred,
    ) -> Umode {
        let (parent_permission, parent_gid) = self.attributes.creation_mode_context();
        let mut permission = mode.permission();
        // Linux `mode_strip_sgid()` reaches the credential check only for a
        // non-directory SGID+group-executable mode below an SGID directory.
        let should_strip_set_gid = mode.node_type() != NodeType::Directory
            && permission.contains(NodePermission::SET_GID | NodePermission::GROUP_EXEC)
            && parent_permission.contains(NodePermission::SET_GID)
            && !cred.in_group(parent_gid)
            && !cred.is_privileged();
        if should_strip_set_gid {
            permission.remove(NodePermission::SET_GID);
        }
        permission.remove(umask);
        permission &= allowed_permission;
        Umode::new(node_type, permission)
    }

    pub(crate) fn lookup_child(&self, dentry: &Dentry) -> VfsResult<Option<Dentry>> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?
            .lookup(self, &dentry, InodeLookupFlags::empty())
    }

    /// Create a directory child below this directory inode.
    pub(crate) fn mkdir(&self, dentry: &Dentry, mode: Umode, cred: &Cred) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?
            .mkdir(&MountIdmap, self, &dentry, mode, cred)
    }

    pub(crate) fn mknod_with_mode(
        &self,
        dentry: &Dentry,
        mode: Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?
            .mknod(&MountIdmap, self, &dentry, mode, device, cred)
    }

    /// Create a symbolic link below this directory inode.
    pub(crate) fn symlink(&self, dentry: &Dentry, target: &str, cred: &Cred) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?
            .symlink(&MountIdmap, self, &dentry, target, cred)
    }

    /// Link `source` below this directory inode.
    pub(crate) fn link(&self, dentry: &Dentry, source: &Dentry) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?
            .link(source, self, &dentry)
    }

    /// Unlink a child below this directory inode.
    pub(crate) fn unlink(&self, dentry: &Dentry) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?.unlink(self, &dentry)
    }

    /// Remove a directory child below this directory inode.
    pub(crate) fn rmdir(&self, dentry: &Dentry) -> VfsResult<()> {
        let dentry = dentry.lock_location();
        self.require_directory_operations()?.rmdir(self, &dentry)
    }

    /// Rename a child from this directory inode to another directory inode.
    pub(crate) fn rename(
        &self,
        old_dentry: &Dentry,
        new_dir: &VfsInode,
        new_dentry: &Dentry,
        flags: RenameFlags,
    ) -> VfsResult<()> {
        let old_dentry = old_dentry.lock_location();
        let new_dentry = new_dentry.lock_location();
        self.require_directory_operations()?.rename(
            &MountIdmap,
            self,
            &old_dentry,
            new_dir,
            &new_dentry,
            flags,
        )
    }

    /// Attempt to downcast the inode to a concrete node type.
    pub fn downcast<T: Any + Send + Sync>(&self) -> VfsResult<Arc<T>> {
        self.private_data
            .clone()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }

    pub(crate) fn address_space(&self) -> Arc<AddressSpace> {
        self.mapping.clone()
    }

    /// Read a dentry's symlink target as a string.
    pub fn read_link(&self, dentry: &Dentry) -> VfsResult<String> {
        if self.operation_flags() & IOP_CACHED_LINK != 0
            && let Some(link) = self.cached_link()
        {
            return Ok(link);
        }
        let mut done = crate::DelayedCall;
        self.require_symlink_operations()?
            .get_link(Some(dentry), self, &mut done)
    }

    fn evict_inode(&self) -> VfsResult<()> {
        if let Some(super_block) = self.identity.super_block() {
            super_block.evict_inode(self)
        } else {
            self.mapping.evict()
        }
    }

    pub(crate) fn bind_super_block(self: &Arc<Self>, super_block: &Arc<SuperBlock>) {
        self.identity.bind_super_block(super_block);
        super_block.register_inode(self);
    }

    pub(crate) fn add_dentry_alias(&self, dentry: &Dentry) -> bool {
        self.identity.add_alias(dentry, self.is_dir())
    }

    pub(crate) fn directory_alias(&self) -> Option<Dentry> {
        if self.is_dir() {
            self.identity.directory_alias()
        } else {
            None
        }
    }

    /// Sets the access timestamp.
    pub fn set_accessed_at(&self, timestamp: SystemTime) -> SystemTime {
        self.attributes.set_accessed_at(timestamp)
    }

    /// Sets the modification timestamp.
    pub fn set_modified_at(&self, timestamp: SystemTime) -> SystemTime {
        self.attributes.set_modified_at(timestamp)
    }

    /// Sets the change timestamp.
    pub fn set_changed_at(&self, timestamp: SystemTime) -> SystemTime {
        self.attributes.set_changed_at(timestamp)
    }

    /// Sets the change timestamp to the current wall-clock time.
    pub fn set_changed_at_to_now(&self) -> SystemTime {
        self.set_changed_at(ktime::realtime())
    }

    /// Sets the link count.
    pub fn set_link_count(&self, link_count: u64) {
        self.attributes.set_link_count(link_count);
    }

    /// Clears the link count.
    pub fn clear_link_count(&self) {
        self.set_link_count(0);
    }

    /// Increments the link count.
    pub fn increment_link_count(&self) {
        self.attributes.increment_link_count();
    }

    /// Decrements the link count.
    pub fn decrement_link_count(&self) {
        self.attributes.decrement_link_count();
    }

    /// Adds allocated bytes to block accounting.
    pub fn add_allocated_bytes(&self, bytes: u64) {
        self.attributes.add_allocated_bytes(bytes);
    }

    /// Subtracts allocated bytes from block accounting.
    pub fn subtract_allocated_bytes(&self, bytes: u64) {
        self.attributes.subtract_allocated_bytes(bytes);
    }

    /// Returns allocated bytes recorded by block accounting.
    pub fn allocated_bytes(&self) -> u64 {
        self.attributes.allocated_bytes()
    }

    /// Stores allocated bytes into block accounting.
    pub fn set_allocated_bytes(&self, bytes: u64) {
        self.attributes.set_allocated_bytes(bytes);
    }
}

fn inode_blkbits(block_size: u64) -> u8 {
    if block_size.is_power_of_two() {
        block_size.trailing_zeros() as u8
    } else {
        0
    }
}

impl fmt::Debug for VfsInode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfsInode")
            .field("inode", &self.inode())
            .field("node_type", &self.node_type())
            .finish()
    }
}

impl Drop for VfsInode {
    fn drop(&mut self) {
        let inode_cache = self
            .identity
            .inode_cache()
            .filter(|cache| cache.begin_eviction(self));
        let result = self.evict_inode();
        if let Some(inode_cache) = inode_cache {
            inode_cache.finish_eviction(self);
        }
        if let Err(err) = result {
            log::warn!("failed to evict inode {}: {err:?}", self.inode());
        }
    }
}

enum CachedInodeState {
    New,
    Live(WeakVfsInode),
    Freeing(WeakVfsInode),
}

struct CachedInode {
    state: CachedInodeState,
    waiters: Arc<WaitQueue>,
}

impl CachedInode {
    fn new() -> Self {
        Self {
            state: CachedInodeState::New,
            waiters: Arc::new(WaitQueue::new()),
        }
    }
}

#[derive(Default)]
struct InodeCacheState {
    inodes: HashMap<u64, CachedInode>,
}

struct InodeCacheInner {
    state: Mutex<InodeCacheState>,
}

struct NewInodeReservation<'a> {
    cache: &'a InodeCacheInner,
    inode_number: u64,
    is_published: bool,
}

impl<'a> NewInodeReservation<'a> {
    fn new(cache: &'a InodeCacheInner, inode_number: u64) -> Self {
        Self {
            cache,
            inode_number,
            is_published: false,
        }
    }

    fn publish(mut self, inode: &Arc<VfsInode>) {
        let mut state = self.cache.state.lock();
        let entry = state
            .inodes
            .get_mut(&self.inode_number)
            .expect("an inode-cache reservation must retain its slot");
        assert!(
            matches!(entry.state, CachedInodeState::New),
            "an inode-cache reservation must remain I_NEW until publication"
        );
        entry.state = CachedInodeState::Live(Arc::downgrade(inode));
        let waiters = entry.waiters.clone();
        self.is_published = true;
        drop(state);
        waiters.notify_all(false);
    }
}

impl Drop for NewInodeReservation<'_> {
    fn drop(&mut self) {
        if self.is_published {
            return;
        }
        let mut state = self.cache.state.lock();
        let waiters = if matches!(
            state.inodes.get(&self.inode_number),
            Some(entry) if matches!(entry.state, CachedInodeState::New)
        ) {
            state
                .inodes
                .remove(&self.inode_number)
                .map(|entry| entry.waiters)
        } else {
            None
        };
        drop(state);
        if let Some(waiters) = waiters {
            waiters.notify_all(false);
        }
    }
}

impl InodeCacheInner {
    fn new() -> Self {
        Self {
            state: Mutex::default(),
        }
    }

    fn wait_for_transition(&self, inode_number: u64, waiters: &Arc<WaitQueue>) {
        waiters.wait_until(|| {
            let state = self.state.lock();
            match state.inodes.get(&inode_number) {
                None => true,
                Some(entry) if !Arc::ptr_eq(&entry.waiters, waiters) => true,
                Some(entry) => match &entry.state {
                    CachedInodeState::Live(inode) => inode.strong_count() != 0,
                    CachedInodeState::New | CachedInodeState::Freeing(_) => false,
                },
            }
        });
    }

    fn begin_eviction(&self, inode: &VfsInode) -> bool {
        let mut state = self.state.lock();
        let Some(entry) = state.inodes.get_mut(&inode.inode()) else {
            return false;
        };
        let CachedInodeState::Live(cached) = &entry.state else {
            return false;
        };
        if !core::ptr::eq(cached.as_ptr(), inode) {
            return false;
        }
        let cached = cached.clone();
        entry.state = CachedInodeState::Freeing(cached);
        true
    }

    fn finish_eviction(&self, inode: &VfsInode) {
        let mut state = self.state.lock();
        let is_evicting_inode = matches!(
            state.inodes.get(&inode.inode()),
            Some(entry)
                if matches!(
                    &entry.state,
                    CachedInodeState::Freeing(cached)
                        if core::ptr::eq(cached.as_ptr(), inode)
                )
        );
        let waiters = is_evicting_inode
            .then(|| state.inodes.remove(&inode.inode()))
            .flatten()
            .map(|entry| entry.waiters);
        drop(state);
        if let Some(waiters) = waiters {
            waiters.notify_all(false);
        }
    }
}

/// Per-filesystem cache for live VFS inode identities.
///
/// New identities remain private until their filesystem-specific state is
/// initialized. Lookups wait while an inode is being initialized or freed,
/// matching Linux `I_NEW` and `I_FREEING` semantics. The live cache entry is a
/// weak reference, so dentry and open-file lifetimes still decide when final
/// eviction starts.
pub struct InodeCache {
    inner: Arc<InodeCacheInner>,
}

impl Default for InodeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeCache {
    /// Create an empty inode cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InodeCacheInner::new()),
        }
    }

    /// Looks up a live inode by inode number without loading it from disk.
    ///
    /// This method matches Linux `ilookup()`: it may sleep while the matching
    /// identity is being initialized or freed, retries after an unhashed
    /// identity, and returns `None` if no resident identity remains. Callers
    /// that must load an absent inode use [`Self::get_or_try_insert_with`].
    pub fn lookup(&self, inode_number: u64) -> Option<Arc<VfsInode>> {
        loop {
            let waiters = {
                let state = self.inner.state.lock();
                match state.inodes.get(&inode_number) {
                    None => return None,
                    Some(entry) => match &entry.state {
                        CachedInodeState::Live(inode) => match inode.upgrade() {
                            Some(inode) => return Some(inode),
                            None => entry.waiters.clone(),
                        },
                        CachedInodeState::New | CachedInodeState::Freeing(_) => {
                            entry.waiters.clone()
                        }
                    },
                }
            };
            self.inner.wait_for_transition(inode_number, &waiters);
        }
    }

    /// Return the live inode for `inode_number`, or insert a newly created one.
    ///
    /// The cache reserves the inode number before running the constructor, so
    /// exactly one caller initializes filesystem-private state. Concurrent
    /// callers wait for that initialization instead of constructing duplicate
    /// resident identities.
    pub fn get_or_insert_with(
        &self,
        inode_number: u64,
        create_inode_fn: impl FnOnce() -> Arc<VfsInode>,
    ) -> Arc<VfsInode> {
        match self.get_or_try_insert_with(inode_number, || {
            Ok::<_, core::convert::Infallible>(create_inode_fn())
        }) {
            Ok(inode) => inode,
            Err(error) => match error {},
        }
    }

    /// Returns the live inode or fallibly initializes a new resident identity.
    ///
    /// Failed initialization removes the `I_NEW`-equivalent reservation and
    /// wakes waiters, allowing a later lookup to retry from disk.
    ///
    /// # Errors
    ///
    /// Returns the filesystem initializer's error without publishing an inode.
    ///
    /// # Panics
    ///
    /// Panics if the initializer violates the internal VFS contract by
    /// returning a different inode number or an identity already bound to
    /// another inode cache.
    pub fn get_or_try_insert_with<E>(
        &self,
        inode_number: u64,
        create_inode_fn: impl FnOnce() -> Result<Arc<VfsInode>, E>,
    ) -> Result<Arc<VfsInode>, E> {
        loop {
            let waiters = {
                let mut state = self.inner.state.lock();
                match state.inodes.get(&inode_number) {
                    None => {
                        state.inodes.insert(inode_number, CachedInode::new());
                        None
                    }
                    Some(entry) => match &entry.state {
                        CachedInodeState::Live(inode) => match inode.upgrade() {
                            Some(inode) => return Ok(inode),
                            None => Some(entry.waiters.clone()),
                        },
                        CachedInodeState::New | CachedInodeState::Freeing(_) => {
                            Some(entry.waiters.clone())
                        }
                    },
                }
            };
            let Some(waiters) = waiters else {
                break;
            };
            self.inner.wait_for_transition(inode_number, &waiters);
        }

        let reservation = NewInodeReservation::new(&self.inner, inode_number);

        let new_inode = create_inode_fn()?;
        assert_eq!(
            new_inode.inode(),
            inode_number,
            "an inode-cache initializer must return the reserved inode number"
        );
        new_inode.identity.bind_inode_cache(&self.inner);
        reservation.publish(&new_inode);
        Ok(new_inode)
    }

    /// Return or create a symbolic-link inode from filesystem-filled inode fields.
    pub fn get_or_insert_symlink_with_init<T>(
        &self,
        flags: NodeFlags,
        init: VfsInodeInit,
        create_node_fn: impl FnOnce() -> Arc<T>,
    ) -> Arc<VfsInode>
    where
        T: Any + Send + Sync + InodeOperations + AddressSpaceOperations,
    {
        debug_assert_eq!(init.node_type(), NodeType::Symlink);
        self.get_or_insert_with(init.inode_number(), || {
            VfsInode::new_symlink_with_address_space_and_flags(create_node_fn(), flags, init)
        })
    }

    /// Return or create a special inode from filesystem-filled inode fields.
    pub fn get_or_insert_special_with_init<T>(
        &self,
        flags: NodeFlags,
        init: VfsInodeInit,
        create_node_fn: impl FnOnce() -> Arc<T>,
    ) -> Arc<VfsInode>
    where
        T: Any + Send + Sync + InodeOperations,
    {
        let node_type = init.node_type();
        debug_assert!(!matches!(
            node_type,
            NodeType::Directory | NodeType::RegularFile | NodeType::Symlink
        ));
        self.get_or_insert_with(init.inode_number(), || {
            VfsInode::new_special(create_node_fn(), flags, init)
        })
    }

    /// Return or create a regular or unknown file inode from filesystem-filled fields.
    pub fn get_or_insert_file_with_init<T>(
        &self,
        flags: NodeFlags,
        init: VfsInodeInit,
        create_node_fn: impl FnOnce() -> Arc<T>,
    ) -> Arc<VfsInode>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations + AddressSpaceOperations,
    {
        let node_type = init.node_type();
        debug_assert!(matches!(
            node_type,
            NodeType::RegularFile | NodeType::Unknown
        ));
        self.get_or_insert_with(init.inode_number(), || {
            VfsInode::new_file_with_address_space_and_flags(create_node_fn(), flags, init)
        })
    }

    /// Return or create a directory inode from filesystem-filled inode fields.
    pub fn get_or_insert_dir_with_init<T>(
        &self,
        flags: NodeFlags,
        init: VfsInodeInit,
        create_node_fn: impl FnOnce() -> Arc<T>,
    ) -> Arc<VfsInode>
    where
        T: Any + Send + Sync + InodeOperations,
    {
        debug_assert_eq!(init.node_type(), NodeType::Directory);
        self.get_or_insert_with(init.inode_number(), || {
            VfsInode::new_dir_with_flags(create_node_fn(), flags, init)
        })
    }

    /// Return or create an openable directory inode from filesystem-filled fields.
    pub fn get_or_insert_openable_dir_with_init<T>(
        &self,
        flags: NodeFlags,
        init: VfsInodeInit,
        create_node_fn: impl FnOnce() -> Arc<T>,
    ) -> Arc<VfsInode>
    where
        T: Any + Send + Sync + InodeOperations + FileOperations,
    {
        debug_assert_eq!(init.node_type(), NodeType::Directory);
        self.get_or_insert_with(init.inode_number(), || {
            VfsInode::new_openable_dir_with_flags(create_node_fn(), flags, init)
        })
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use ktime_types::SystemTime;
    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{GetattrQueryFlags, GetattrRequestMask, NodePermission};

    struct TestChrdevInode {
        metadata: Mutex<Metadata>,
    }

    impl TestChrdevInode {
        fn new(rdev: DeviceId) -> Self {
            Self {
                metadata: Mutex::new(Metadata {
                    device: 0,
                    inode: 42,
                    nlink: 1,
                    mode: Umode::new(
                        NodeType::CharacterDevice,
                        NodePermission::from_bits_truncate(0o666),
                    ),
                    uid: 0,
                    gid: 0,
                    size: 0,
                    block_size: 0,
                    blocks: 0,
                    rdev,
                    atime: Default::default(),
                    mtime: Default::default(),
                    ctime: Default::default(),
                }),
            }
        }
    }

    impl InodeOperations for TestChrdevInode {
        fn getattr(
            &self,
            _idmap: &MountIdmap,
            _path: Option<&crate::Path>,
            _request_mask: GetattrRequestMask,
            _query_flags: GetattrQueryFlags,
        ) -> VfsResult<Metadata> {
            Ok(self.metadata.lock().clone())
        }

        fn setattr(
            &self,
            _idmap: &MountIdmap,
            _dentry: &Dentry,
            _update: MetadataUpdate,
        ) -> VfsResult<()> {
            Ok(())
        }
    }

    impl FileOperations for TestChrdevInode {}

    impl AddressSpaceOperations for TestChrdevInode {}

    impl InodeAttributeOperations for TestChrdevInode {
        fn fill_metadata(&self, inode_number: u64) -> Metadata {
            let mut metadata = self.metadata.lock().clone();
            metadata.inode = inode_number;
            metadata
        }

        fn mode(&self) -> Umode {
            self.metadata.lock().mode
        }

        fn owner(&self) -> (u32, u32) {
            let metadata = self.metadata.lock();
            (metadata.uid, metadata.gid)
        }

        fn link_count(&self) -> u64 {
            self.metadata.lock().nlink
        }

        fn generation(&self) -> u32 {
            17
        }

        fn rdev(&self) -> DeviceId {
            self.metadata.lock().rdev
        }

        fn size(&self) -> u64 {
            self.metadata.lock().size
        }

        fn block_size(&self) -> u64 {
            self.metadata.lock().block_size
        }

        fn blocks(&self) -> u64 {
            self.metadata.lock().blocks
        }

        fn set_permission(&self, permission: NodePermission) {
            let mut metadata = self.metadata.lock();
            metadata.mode = Umode::new(metadata.mode.node_type(), permission);
        }

        fn set_owner(&self, uid: u32, gid: u32) {
            let mut metadata = self.metadata.lock();
            metadata.uid = uid;
            metadata.gid = gid;
        }

        fn set_link_count(&self, link_count: u64) {
            self.metadata.lock().nlink = link_count;
        }

        fn increment_link_count(&self) {
            let mut metadata = self.metadata.lock();
            metadata.nlink = metadata.nlink.saturating_add(1);
        }

        fn decrement_link_count(&self) {
            let mut metadata = self.metadata.lock();
            metadata.nlink = metadata.nlink.saturating_sub(1);
        }

        fn set_size(&self, size: u64) {
            self.metadata.lock().size = size;
        }

        fn set_accessed_at(&self, value: SystemTime) {
            self.metadata.lock().atime = value;
        }

        fn set_modified_at(&self, value: SystemTime) {
            self.metadata.lock().mtime = value;
        }

        fn set_changed_at(&self, value: SystemTime) {
            self.metadata.lock().ctime = value;
        }

        fn set_allocated_bytes(&self, bytes: u64) {
            self.metadata.lock().blocks = bytes / INODE_BYTES_PER_BLOCK;
        }

        fn add_allocated_bytes(&self, bytes: u64) {
            let mut metadata = self.metadata.lock();
            metadata.blocks = metadata
                .blocks
                .saturating_add(bytes / INODE_BYTES_PER_BLOCK);
        }

        fn subtract_allocated_bytes(&self, bytes: u64) {
            let mut metadata = self.metadata.lock();
            metadata.blocks = metadata
                .blocks
                .saturating_sub(bytes / INODE_BYTES_PER_BLOCK);
        }
    }

    #[def_test]
    fn special_inodes_preserve_rdev() {
        let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
        let init = VfsInodeInit::new(
            7,
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
        let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);
        assert_eq!(inode.metadata().inode, 7);
        assert_eq!(inode.metadata().rdev, DeviceId::new(1, 3));
    }

    #[def_test]
    fn filesystem_attribute_operations_are_the_vfs_inode_state() {
        let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
        let init = VfsInodeInit::new(
            42,
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
        let inode = VfsInode::new_with_inode_attribute_operations(
            node.clone(),
            NodeFlags::NON_CACHEABLE,
            init,
        );

        inode.set_link_count(5);
        inode.set_size(23);
        assert_eq!(node.metadata.lock().nlink, 5);
        assert_eq!(node.metadata.lock().size, 23);

        node.metadata.lock().uid = 1000;
        node.metadata.lock().size = 41;
        assert_eq!(inode.metadata().uid, 1000);
        assert_eq!(inode.size(), 41);
        assert_eq!(inode.generation(), 17);
    }

    #[def_test]
    fn inode_cache_retries_failed_initialization_and_replaces_freed_identity() {
        let cache = InodeCache::new();
        let init = VfsInodeInit::new(
            9,
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o600),
            ),
        );
        let failed = cache.get_or_try_insert_with(9, || Err::<Arc<VfsInode>, _>(7u8));
        assert!(matches!(failed, Err(7)));

        let inode = cache.get_or_insert_with(9, || {
            VfsInode::new_special(
                Arc::new(TestChrdevInode::new(DeviceId::default())),
                NodeFlags::NON_CACHEABLE,
                init,
            )
        });
        assert!(Arc::ptr_eq(&cache.lookup(9).unwrap(), &inode));

        let old_identity = Arc::downgrade(&inode);
        drop(inode);
        assert!(old_identity.upgrade().is_none());
        assert!(cache.lookup(9).is_none());
        let replacement = cache.get_or_insert_with(9, || {
            VfsInode::new_special(
                Arc::new(TestChrdevInode::new(DeviceId::default())),
                NodeFlags::NON_CACHEABLE,
                init,
            )
        });
        assert!(!core::ptr::eq(
            old_identity.as_ptr(),
            Arc::as_ptr(&replacement)
        ));
    }

    #[def_test]
    fn inode_link_count_helpers_update_link_count() {
        let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
        let init = VfsInodeInit::new(
            7,
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
        let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);

        assert_eq!(inode.link_count(), 1);
        inode.increment_link_count();
        assert_eq!(inode.link_count(), 2);
        inode.decrement_link_count();
        assert_eq!(inode.link_count(), 1);
        inode.increment_link_count();
        assert_eq!(inode.link_count(), 2);
        inode.decrement_link_count();
        assert_eq!(inode.link_count(), 1);
        inode.set_link_count(7);
        assert_eq!(inode.link_count(), 7);
        inode.clear_link_count();
        assert_eq!(inode.link_count(), 0);
    }

    #[def_test]
    fn backing_metadata_refresh_updates_mutable_cached_attributes() {
        let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
        let init = VfsInodeInit::new(
            7,
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
        let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);
        let mut updated = inode.metadata();
        updated.nlink = 0;
        updated.mode = updated
            .mode
            .with_permission(NodePermission::from_bits_truncate(0o640));
        updated.uid = 1000;
        updated.gid = 1001;
        updated.blocks = 7;
        updated.atime = SystemTime::from_unix_parts(11, 1).unwrap();
        updated.mtime = SystemTime::from_unix_parts(12, 2).unwrap();
        updated.ctime = SystemTime::from_unix_parts(13, 3).unwrap();

        inode
            .update_attributes_after_backing_change(&updated)
            .unwrap();

        let cached = inode.metadata();
        assert_eq!(cached.nlink, 0);
        assert_eq!(cached.mode.permission().bits(), 0o640);
        assert_eq!((cached.uid, cached.gid), (1000, 1001));
        assert_eq!(cached.blocks, 7);
        assert_eq!(cached.atime, SystemTime::from_unix_parts(11, 1).unwrap());
        assert_eq!(cached.mtime, SystemTime::from_unix_parts(12, 2).unwrap());
        assert_eq!(cached.ctime, SystemTime::from_unix_parts(13, 3).unwrap());

        updated.inode = 8;
        assert_eq!(
            inode.update_attributes_after_backing_change(&updated),
            Err(VfsError::InvalidInput)
        );
    }
}
