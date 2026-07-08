// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS inode identity and inode cache helpers.

use alloc::{
    borrow::ToOwned,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use bitflags::bitflags;
use hashbrown::HashMap;
use kerrno::LinuxError;
use khal::time::wall_time;
use klazy::Once;

use super::{
    DeviceFileOps,
    dentry::DentryAlias,
    device::{block_device_file_operations, character_device_file_operations},
};
use crate::{
    AddressSpace, AddressSpaceOperations, DelayedCall, Dentry, DeviceId, FileOperations, Metadata,
    MetadataUpdate, MountIdmap, Mutex, NodePermission, NodeType, Path, SuperBlock, Umode, VfsError,
    VfsFileBuilder, VfsResult,
    address_space::{default_address_space_operations, empty_address_space_operations},
};

const IOP_CACHED_LINK: u16 = 0x0040;
const INODE_BYTES_PER_BLOCK: u64 = 512;

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
    }
}

/// lookup flags passed to `InodeDirOperations::lookup`.
pub type InodeLookupFlags = u32;

/// rename flags passed to `InodeDirOperations::rename`.
pub type RenameFlags = u32;
/// `RENAME_NOREPLACE`.
pub const RENAME_NOREPLACE: RenameFlags = 1 << 0;
/// `RENAME_EXCHANGE`.
pub const RENAME_EXCHANGE: RenameFlags = 1 << 1;
/// `RENAME_WHITEOUT`.
pub const RENAME_WHITEOUT: RenameFlags = 1 << 2;

/// request mask passed to `InodeOperations::getattr`.
pub type GetattrRequestMask = u32;

/// query flags passed to `InodeOperations::getattr`.
pub type GetattrQueryFlags = u32;

/// Directory inode operations.
pub trait InodeDirOperations: Send + Sync {
    fn lookup(
        &self,
        _dir: &VfsInode,
        _dentry: &Dentry,
        _flags: InodeLookupFlags,
    ) -> VfsResult<Dentry> {
        Err(VfsError::NotADirectory)
    }

    fn create(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &Dentry,
        _mode: Umode,
        _exclusive: bool,
    ) -> VfsResult<Dentry> {
        Err(VfsError::PermissionDenied)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &VfsInode,
        _new_dentry: &Dentry,
    ) -> VfsResult<Dentry> {
        Err(VfsError::NotADirectory)
    }

    fn unlink(&self, _dir: &VfsInode, _dentry: &Dentry) -> VfsResult<()> {
        Err(VfsError::NotADirectory)
    }

    fn symlink(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &Dentry,
        _symname: &str,
    ) -> VfsResult<Dentry> {
        Err(VfsError::NotADirectory)
    }

    fn mkdir(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &Dentry,
        _mode: Umode,
    ) -> VfsResult<Dentry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rmdir(&self, dir: &VfsInode, dentry: &Dentry) -> VfsResult<()> {
        self.unlink(dir, dentry)
    }

    fn mknod(
        &self,
        _idmap: &MountIdmap,
        _dir: &VfsInode,
        _dentry: &Dentry,
        _mode: Umode,
        _device: DeviceId,
    ) -> VfsResult<Dentry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rename(
        &self,
        _idmap: &MountIdmap,
        _old_dir: &VfsInode,
        _old_dentry: &Dentry,
        _new_dir: &VfsInode,
        _new_dentry: &Dentry,
        _flags: RenameFlags,
    ) -> VfsResult<()> {
        Err(VfsError::NotADirectory)
    }
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

/// Inode operation table installed on VFS inodes.
pub trait InodeOperations: Send + Sync + 'static {
    /// Returns directory operations when this inode supports directory lookup.
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        None
    }

    /// Returns symlink operations when this inode supports link following.
    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
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
}

struct EmptyInodeOperations;

impl InodeOperations for EmptyInodeOperations {}

#[derive(Clone, Copy, Debug)]
struct InodeTimestamp {
    sec: u64,
    nsec: u32,
}

impl InodeTimestamp {
    fn zero() -> Self {
        Self { sec: 0, nsec: 0 }
    }

    fn from_duration(value: Duration) -> Self {
        Self {
            sec: value.as_secs(),
            nsec: value.subsec_nanos(),
        }
    }

    fn as_duration(self) -> Duration {
        Duration::new(self.sec, self.nsec)
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

    fn allocated_bytes(&self) -> u64 {
        self.blocks
            .saturating_mul(INODE_BYTES_PER_BLOCK)
            .saturating_add(u64::from(self.bytes))
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

    fn node_type(&self) -> NodeType {
        self.node_type
    }

    fn set_permission(&self, permission: NodePermission) {
        *self.permission.lock() = permission;
    }
}

struct InodeIdentity {
    super_block: Once<Weak<SuperBlock>>,
    number: u64,
    aliases: Mutex<Vec<DentryAlias>>,
}

impl InodeIdentity {
    fn new(number: u64) -> Self {
        Self {
            super_block: Once::new(),
            number,
            aliases: Mutex::default(),
        }
    }

    fn number(&self) -> u64 {
        self.number
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

    fn add_alias(&self, dentry: &Dentry) {
        let mut aliases = self.aliases.lock();
        aliases.retain(DentryAlias::is_live);
        if aliases.iter().any(|alias| alias.points_to(dentry)) {
            return;
        }
        aliases.push(DentryAlias::new(dentry));
    }
}

struct InodeAttributes {
    mode: InodeMode,
    operation_flags: AtomicU16,
    flags: NodeFlags,
    owner: Mutex<InodeOwner>,
    link_count: Mutex<u64>,
    generation: u32,
    rdev: DeviceId,
    size: Mutex<u64>,
    times: Mutex<InodeTimes>,
    block_accounting: Mutex<InodeBlockAccounting>,
    block_bits: u8,
}

impl InodeAttributes {
    fn new(init: VfsInodeInit, flags: NodeFlags) -> Self {
        Self {
            mode: InodeMode::new(init.mode),
            operation_flags: AtomicU16::new(0),
            flags,
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

    fn node_type(&self) -> NodeType {
        self.mode.node_type()
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

    fn set_accessed_at(&self, value: Duration) -> Duration {
        let mut times = self.times.lock();
        times.accessed_at = InodeTimestamp::from_duration(value);
        value
    }

    fn set_modified_at(&self, value: Duration) -> Duration {
        let mut times = self.times.lock();
        times.modified_at = InodeTimestamp::from_duration(value);
        value
    }

    fn set_changed_at(&self, value: Duration) -> Duration {
        let mut times = self.times.lock();
        times.changed_at = InodeTimestamp::from_duration(value);
        value
    }

    fn block_bits(&self) -> u8 {
        self.block_bits
    }

    fn blocks(&self) -> u64 {
        self.block_accounting.lock().blocks
    }

    fn allocated_bytes(&self) -> u64 {
        self.block_accounting.lock().allocated_bytes()
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        let mut accounting = self.block_accounting.lock();
        let bytes = accounting.allocated_bytes().saturating_add(bytes);
        accounting.set_allocated_bytes(bytes);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        let mut accounting = self.block_accounting.lock();
        let bytes = accounting.allocated_bytes().saturating_sub(bytes);
        accounting.set_allocated_bytes(bytes);
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.block_accounting.lock().set_allocated_bytes(bytes);
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
            atime: times.accessed_at.as_duration(),
            mtime: times.modified_at.as_duration(),
            ctime: times.changed_at.as_duration(),
        }
    }
}

struct InodeAddressSpaces {
    mapping: Arc<AddressSpace>,
}

impl InodeAddressSpaces {
    fn new(
        owner: Weak<VfsInode>,
        ops: Arc<dyn AddressSpaceOperations>,
        flags: NodeFlags,
        size: u64,
    ) -> Self {
        let mapping = AddressSpace::new_default(owner, ops, flags, size);
        Self { mapping }
    }

    fn mapping(&self) -> Arc<AddressSpace> {
        self.mapping.clone()
    }

    fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.mapping.writepages(data_only)
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.mapping.set_len(len)
    }

    fn set_cached_len(&self, len: u64) -> VfsResult<()> {
        self.mapping.set_cached_len(len)
    }

    fn evict(&self) -> VfsResult<()> {
        self.mapping.evict()
    }
}

struct InodeSpecialState {
    character_device: Mutex<Option<Arc<dyn DeviceFileOps>>>,
    cached_link: Once<String>,
}

impl InodeSpecialState {
    fn new() -> Self {
        Self {
            character_device: Mutex::new(None),
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
    inode_operations: Arc<dyn InodeOperations>,
    file_operations: Arc<dyn FileOperations>,
    private_data: Arc<dyn Any + Send + Sync>,
    address_spaces: InodeAddressSpaces,
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
        NodeType::Fifo | NodeType::Socket => (None, DeviceId::default()),
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
        atime: Duration,
        mtime: Duration,
        ctime: Duration,
    ) -> Self {
        self.accessed_at = InodeTimestamp::from_duration(atime);
        self.modified_at = InodeTimestamp::from_duration(mtime);
        self.changed_at = InodeTimestamp::from_duration(ctime);
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
            attributes: InodeAttributes::new(init, flags),
            inode_operations,
            file_operations: file_operations.unwrap_or_else(no_open_file_operations),
            private_data,
            address_spaces: InodeAddressSpaces::new(
                this.clone(),
                address_space_operations,
                flags,
                init.size,
            ),
            special_state: InodeSpecialState::new(),
        })
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
    pub fn update_metadata(&self, dentry: &Dentry, update: MetadataUpdate) -> VfsResult<()> {
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
        if let Some(mode) = mode {
            self.attributes.set_permission(mode);
        }
        if let Some((uid, gid)) = owner {
            self.attributes.set_owner(uid, gid);
        }
        if changed {
            self.set_changed_at_to_now();
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
        self.address_spaces.set_len(len)?;
        self.set_size(len);
        Ok(())
    }

    /// Records a file length already changed by the backing filesystem.
    pub fn update_size_after_backing_change(&self, len: u64) -> VfsResult<()> {
        self.address_spaces.set_cached_len(len)?;
        self.set_size(len);
        Ok(())
    }

    /// Builds a metadata snapshot from cached inode attributes.
    pub fn metadata(&self) -> Metadata {
        self.attributes.fill_metadata(self.inode())
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

    /// Synchronizes the file to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.address_spaces.sync(data_only)
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

    fn cached_link(&self) -> Option<String> {
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
        dir: &Dentry,
        name: &str,
        mode: Umode,
        exclusive: bool,
    ) -> VfsResult<Dentry> {
        let dentry = Dentry::new_negative(Some(dir.clone()), name.to_owned());
        self.require_directory_operations()?
            .create(&MountIdmap, self, &dentry, mode, exclusive)
    }

    /// Look up a child below this directory inode.
    pub fn lookup(&self, dir: &Dentry, name: &str) -> VfsResult<Dentry> {
        let dentry = Dentry::new_negative(Some(dir.clone()), name.to_owned());
        self.require_directory_operations()?
            .lookup(self, &dentry, 0)
    }

    /// Create a regular-file child below this directory inode.
    pub fn create(
        &self,
        dir: &Dentry,
        name: &str,
        permission: NodePermission,
    ) -> VfsResult<Dentry> {
        let mode = Umode::new(NodeType::RegularFile, permission);
        self.create_with_mode(dir, name, mode, false)
    }

    /// Create a directory child below this directory inode.
    pub fn mkdir(&self, dir: &Dentry, name: &str, permission: NodePermission) -> VfsResult<Dentry> {
        let dentry = Dentry::new_negative(Some(dir.clone()), name.to_owned());
        let mode = Umode::new(NodeType::Directory, permission);
        self.require_directory_operations()?
            .mkdir(&MountIdmap, self, &dentry, mode)
    }

    /// Create a special child below this directory inode.
    pub fn mknod(
        &self,
        dir: &Dentry,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
        device: DeviceId,
    ) -> VfsResult<Dentry> {
        let dentry = Dentry::new_negative(Some(dir.clone()), name.to_owned());
        let mode = Umode::new(node_type, permission);
        self.require_directory_operations()?
            .mknod(&MountIdmap, self, &dentry, mode, device)
    }

    /// Create a symbolic link below this directory inode.
    pub fn symlink(&self, dir: &Dentry, name: &str, target: &str) -> VfsResult<Dentry> {
        let dentry = Dentry::new_negative(Some(dir.clone()), name.to_owned());
        self.require_directory_operations()?
            .symlink(&MountIdmap, self, &dentry, target)
    }

    /// Link `source` below this directory inode.
    pub fn link(&self, dir: &Dentry, name: &str, source: &Dentry) -> VfsResult<Dentry> {
        let dentry = Dentry::new_negative(Some(dir.clone()), name.to_owned());
        self.require_directory_operations()?
            .link(source, self, &dentry)
    }

    /// Unlink a child below this directory inode.
    pub fn unlink(&self, dentry: &Dentry) -> VfsResult<()> {
        self.require_directory_operations()?.unlink(self, dentry)
    }

    /// Rename a child from this directory inode to another directory inode.
    pub fn rename(
        &self,
        old_dentry: &Dentry,
        new_dir: &VfsInode,
        new_dentry: &Dentry,
        flags: RenameFlags,
    ) -> VfsResult<()> {
        self.require_directory_operations()?.rename(
            &MountIdmap,
            self,
            old_dentry,
            new_dir,
            new_dentry,
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
        self.address_spaces.mapping()
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
            self.address_spaces.evict()
        }
    }

    pub(crate) fn bind_super_block(self: &Arc<Self>, super_block: &Arc<SuperBlock>) {
        self.identity.bind_super_block(super_block);
        super_block.register_inode(self);
    }

    pub(crate) fn add_dentry_alias(&self, dentry: &Dentry) {
        self.identity.add_alias(dentry);
    }

    /// Sets the access timestamp.
    pub fn set_accessed_at(&self, timestamp: Duration) -> Duration {
        self.attributes.set_accessed_at(timestamp)
    }

    /// Sets the modification timestamp.
    pub fn set_modified_at(&self, timestamp: Duration) -> Duration {
        self.attributes.set_modified_at(timestamp)
    }

    /// Sets the change timestamp.
    pub fn set_changed_at(&self, timestamp: Duration) -> Duration {
        self.attributes.set_changed_at(timestamp)
    }

    /// Sets the change timestamp to the current wall-clock time.
    pub fn set_changed_at_to_now(&self) -> Duration {
        self.set_changed_at(wall_time())
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
        if let Err(err) = self.evict_inode() {
            log::warn!("failed to evict inode {}: {err:?}", self.inode());
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

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{GetattrQueryFlags, GetattrRequestMask, NodePermission};

    struct TestChrdevInode {
        metadata: Metadata,
    }

    impl TestChrdevInode {
        fn new(rdev: DeviceId) -> Self {
            Self {
                metadata: Metadata {
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
                },
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
            Ok(self.metadata.clone())
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
        assert_eq!(inode.metadata().rdev, DeviceId::new(1, 3));
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
}
