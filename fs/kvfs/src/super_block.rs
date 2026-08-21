// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Superblock state and filesystem statistics.
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicU32, Ordering},
};

use hashbrown::HashMap;
use klazy::{Lazy, Once};
use ktask::WaitQueue;

use crate::{
    Dentry, DentryKey, DentryOperations, FileSystemType, Filename, FsContext, FsContextPurpose,
    LookupFlags, LookupIntent, MountFlags, Mutex, MutexGuard, NodeType, Path, VfsError, VfsInode,
    VfsResult, WritebackControl, node,
};

fn has_readonly_mismatch(current: SuperBlockFlags, requested: SuperBlockFlags) -> bool {
    (current ^ requested).contains(SuperBlockFlags::RDONLY)
}

fn validate_block_device_flags(
    block_device: &block::BlockDevice,
    flags: SuperBlockFlags,
) -> VfsResult<()> {
    if block_device.is_read_only() && !flags.contains(SuperBlockFlags::RDONLY) {
        return Err(VfsError::PermissionDenied);
    }
    Ok(())
}

/// Constructs a device-less superblock from a filesystem context.
pub fn get_tree_nodev(
    context: &FsContext<'_>,
    fill_super: impl FnOnce(&'static FileSystemType, SuperBlockFlags) -> VfsResult<Arc<SuperBlock>>,
) -> VfsResult<Arc<SuperBlock>> {
    fill_super(context.fs_type(), context.sb_flags())
}

/// Constructs a block-backed superblock from a filesystem context.
///
/// This is the VFS counterpart of Linux `get_tree_bdev()`: it resolves
/// `fc->source`, validates the resulting block-special inode and mount policy,
/// obtains the canonical `BlockDevice` from the block core, and invokes the
/// filesystem's fill operation only for a newly reserved identity. Existing
/// instances are reused by canonical filesystem type and block device,
/// corresponding to Linux `sget_dev()`. The one-shot closure may capture parsed
/// mount state without exposing filesystem-specific options to the generic VFS.
pub fn get_tree_bdev<F>(
    context: &FsContext<'_>,
    lookup_root: &Path,
    lookup_pwd: &Path,
    fill_super: F,
) -> VfsResult<Arc<SuperBlock>>
where
    F: FnOnce(&Arc<SuperBlock>) -> VfsResult<()>,
{
    let source = context
        .source()
        .filter(|source| !source.is_empty())
        .ok_or(VfsError::InvalidInput)?;
    let path = Filename::new(source).lookup_at(
        lookup_root,
        lookup_pwd,
        LookupIntent::Open,
        LookupFlags::follow(),
        context.cred(),
    )?;
    if path.inode().node_type() != NodeType::BlockDevice {
        return Err(VfsError::from(kerrno::LinuxError::ENOTBLK));
    }
    if path.mount().flags().contains(MountFlags::NODEV) {
        return Err(VfsError::PermissionDenied);
    }

    let device_number = path.inode().rdev();
    if device_number == crate::DeviceId::default() {
        return Err(VfsError::from(kerrno::LinuxError::ENXIO));
    }
    let device = block::lookup_block_device(device_number)
        .ok_or_else(|| VfsError::from(kerrno::LinuxError::ENXIO))?;
    validate_block_device_flags(&device, context.sb_flags())?;
    super_block_registry().get_or_try_init_bdev(
        context.fs_type(),
        device,
        context.sb_flags(),
        fill_super,
    )
}

static SUPER_BLOCK_REGISTRY: Lazy<SuperBlockRegistry> = Lazy::new(SuperBlockRegistry::new);

/// Returns the VFS-wide superblock registry.
pub fn super_block_registry() -> &'static SuperBlockRegistry {
    Lazy::force(&SUPER_BLOCK_REGISTRY)
}

/// Synchronizes all live superblocks known to the VFS.
pub fn sync_filesystems() -> VfsResult<()> {
    super_block_registry().sync_filesystems()
}

#[derive(Debug, Default)]
struct SuperBlockSet {
    super_blocks: Vec<Weak<SuperBlock>>,
}

impl SuperBlockSet {
    fn register(&mut self, super_block: &Arc<SuperBlock>) {
        self.super_blocks.push(Arc::downgrade(super_block));
    }

    fn lookup_bdev(
        &mut self,
        file_system_type: &'static FileSystemType,
        block_device: &Arc<block::BlockDevice>,
    ) -> Option<Arc<SuperBlock>> {
        let mut matching = None;
        self.super_blocks
            .retain(|registered| match registered.upgrade() {
                Some(super_block) => {
                    if matching.is_none()
                        && core::ptr::eq(super_block.file_system_type(), file_system_type)
                        && super_block.block_device().is_some_and(|registered_device| {
                            Arc::ptr_eq(registered_device, block_device)
                        })
                    {
                        matching = Some(super_block);
                    }
                    true
                }
                None => false,
            });
        matching
    }

    fn unregister(&mut self, super_block: &SuperBlock) {
        if let Some(index) = self
            .super_blocks
            .iter()
            .position(|registered| core::ptr::eq(registered.as_ptr(), super_block))
        {
            self.super_blocks.swap_remove(index);
        }
    }

    fn live_super_blocks(&mut self) -> Vec<Arc<SuperBlock>> {
        let mut live = Vec::new();
        self.super_blocks
            .retain(|super_block| match super_block.upgrade() {
                Some(super_block) => {
                    live.push(super_block);
                    true
                }
                None => false,
            });
        live
    }
}

/// VFS-wide owner for live superblocks.
#[derive(Debug, Default)]
pub struct SuperBlockRegistry {
    super_blocks: Mutex<SuperBlockSet>,
}

impl SuperBlockRegistry {
    fn new() -> Self {
        Self::default()
    }

    fn register(&self, super_block: &Arc<SuperBlock>) {
        self.super_blocks.lock().register(super_block);
    }

    fn get_or_try_init_bdev<F>(
        &self,
        file_system_type: &'static FileSystemType,
        block_device: Arc<block::BlockDevice>,
        flags: SuperBlockFlags,
        fill_super: F,
    ) -> VfsResult<Arc<SuperBlock>>
    where
        F: FnOnce(&Arc<SuperBlock>) -> VfsResult<()>,
    {
        let mut fill_super = Some(fill_super);
        loop {
            let (super_block, is_new) = {
                let mut super_blocks = self.super_blocks.lock();
                match super_blocks.lookup_bdev(file_system_type, &block_device) {
                    Some(super_block) => (super_block, false),
                    None => {
                        let claim = block_device.claim_exclusive()?;
                        let candidate = SuperBlock::allocate(file_system_type, Some(claim), flags);
                        super_blocks.register(&candidate);
                        (candidate, true)
                    }
                }
            };
            if is_new {
                let fill_super = fill_super
                    .take()
                    .expect("a fill callback is consumed only by a new superblock");
                match fill_super(&super_block) {
                    Ok(()) => super_block.finish_initialization(),
                    Err(error) => {
                        {
                            let mut super_blocks = self.super_blocks.lock();
                            super_block.fail_initialization();
                            super_blocks.unregister(&super_block);
                        }
                        super_block.lifecycle_waiters.notify_all(false);
                        return Err(error);
                    }
                }
                return Ok(super_block);
            }

            if !super_block.wait_until_available_or_dead() {
                continue;
            }
            if has_readonly_mismatch(super_block.flags(), flags) {
                return Err(VfsError::ResourceBusy);
            }
            return Ok(super_block);
        }
    }

    /// Returns a snapshot of live superblocks and prunes dropped ones.
    pub fn live_super_blocks(&self) -> Vec<Arc<SuperBlock>> {
        self.super_blocks.lock().live_super_blocks()
    }

    /// Synchronizes all live superblocks.
    pub fn sync_filesystems(&self) -> VfsResult<()> {
        for super_block in self.live_super_blocks() {
            super_block.sync_fs()?;
        }
        Ok(())
    }
}

/// Superblock-wide filesystem operations.
///
/// A superblock owns one mounted filesystem instance and the state shared by
/// all inodes in that instance. This boundary must not contain per-open-file
/// state.
pub trait SuperBlockOperations: Send + Sync + 'static {
    /// Returns filesystem statistics.
    fn statfs(&self, super_block: &SuperBlock) -> VfsResult<StatFs>;

    /// Writes back superblock-owned dirty state.
    ///
    /// [`SuperBlock::sync_fs`] calls this hook only after writing back dirty
    /// page-cache state for all live inodes registered on the superblock and
    /// after giving the filesystem a chance to write inode-owned metadata.
    /// Filesystems should use this hook for superblock-wide metadata, journal
    /// checkpoint, and device flush work rather than for discovering ordinary
    /// dirty file data.
    fn sync_fs(&self, _super_block: &SuperBlock) -> VfsResult<()> {
        Ok(())
    }

    /// Writes back filesystem-owned metadata for one inode.
    fn write_inode(&self, _inode: &VfsInode, _control: &mut WritebackControl) -> VfsResult<()> {
        Ok(())
    }

    /// Releases filesystem-owned inode state during final inode teardown.
    ///
    /// KVFS calls this after the hashed inode identity enters its freeing state
    /// and before removing that identity from the inode cache, corresponding to
    /// Linux `I_FREEING` around `super_operations::evict_inode`. Implementations
    /// must not reacquire the same `(superblock, inode number)` identity because
    /// lookup waits for this callback to finish. They may load unrelated inode
    /// identities provided that doing so cannot form a cyclic eviction dependency.
    /// This callback may sleep and perform I/O.
    ///
    /// The default drops the inode page cache without treating final teardown
    /// as ordinary writeback.
    fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        default_evict_inode(inode)
    }
}

/// Releases the VFS-owned state for an inode with no filesystem-specific
/// teardown requirements.
pub fn default_evict_inode(inode: &VfsInode) -> VfsResult<()> {
    inode.address_space().truncate_final()
}

/// Large-file page-cache limit for 64-bit VFS offsets.
pub const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;

bitflags::bitflags! {
    /// VFS superblock flags corresponding to Linux `super_block::s_flags`.
    ///
    /// Per-mount policy belongs to [`crate::MountFlags`] instead. In
    /// particular, `RELATIME` is a mount flag rather than a superblock flag.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct SuperBlockFlags: u32 {
        /// Filesystem is read-only.
        const RDONLY = 1 << 0;
        /// Do not update access times.
        const NOATIME = 1 << 10;
        /// Do not update directory access times.
        const NODIRATIME = 1 << 11;
    }
}

/// Atomic storage that preserves [`SuperBlockFlags`] as the semantic interface.
///
/// Relaxed ordering is sufficient because the flags neither publish nor guard
/// other superblock state.
#[derive(Debug)]
struct AtomicSuperBlockFlags(AtomicU32);

impl AtomicSuperBlockFlags {
    fn new(flags: SuperBlockFlags) -> Self {
        Self(AtomicU32::new(flags.bits()))
    }

    fn load(&self) -> SuperBlockFlags {
        SuperBlockFlags::from_bits_truncate(self.0.load(Ordering::Relaxed))
    }

    fn store(&self, flags: SuperBlockFlags) {
        self.0.store(flags.bits(), Ordering::Relaxed);
    }
}

bitflags::bitflags! {
    /// User-visible `statfs(2)` `f_flags` (`ST_*`) bits.
    ///
    /// This is an ABI output type. It must not be used to configure
    /// superblock or mount state; VFS derives it from [`SuperBlockFlags`] and
    /// [`crate::MountFlags`] when exporting `statfs`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct StatFsFlags: u32 {
        /// Filesystem is read-only.
        const RDONLY = 0x1;
        /// Ignore set-user-ID and set-group-ID bits.
        const NOSUID = 0x2;
        /// Disallow access to device special files.
        const NODEV = 0x4;
        /// Disallow program execution.
        const NOEXEC = 0x8;
        /// `f_flags` support is implemented.
        const VALID = 0x20;
        /// Do not update file access times.
        const NOATIME = 0x400;
        /// Do not update directory access times.
        const NODIRATIME = 0x800;
        /// Update access time relative to mtime/ctime.
        const RELATIME = 0x1000;
        /// Do not follow symlinks.
        const NOSYMFOLLOW = 0x2000;
    }
}

/// Filesystem statistics returned by [`SuperBlockOperations::statfs`].
///
/// VFS derives user-visible `statfs(2)` flags from the containing mount and
/// superblock after obtaining these filesystem-owned values.
pub struct StatFs {
    /// Filesystem type identifier.
    pub fs_type: u32,
    /// Fundamental block size (bytes).
    pub block_size: u32,
    /// Total data blocks in the filesystem.
    pub blocks: u64,
    /// Free blocks in the filesystem.
    pub blocks_free: u64,
    /// Blocks available to unprivileged users.
    pub blocks_available: u64,

    /// Total file count.
    pub file_count: u64,
    /// Free file count.
    pub free_file_count: u64,

    /// Maximum filename length.
    pub name_length: u32,
    /// Fragment size (bytes).
    pub fragment_size: u32,
}

#[derive(Debug)]
enum SuperBlockLifecycle {
    Nascent,
    Available { active_mounts: usize },
    Dying,
    Dead,
}

impl SuperBlockLifecycle {
    fn try_acquire_mount(&mut self) -> bool {
        let Self::Available { active_mounts } = self else {
            return false;
        };
        *active_mounts = active_mounts
            .checked_add(1)
            .expect("active mount count must not overflow");
        true
    }

    fn release_unless_last(&mut self) -> bool {
        let Self::Available { active_mounts } = self else {
            panic!("every active mount release must match an acquisition");
        };
        match *active_mounts {
            0 => panic!("every active mount release must match an acquisition"),
            1 => true,
            _ => {
                *active_mounts -= 1;
                false
            }
        }
    }

    fn begin_shutdown(&mut self) -> bool {
        if !self.release_unless_last() {
            return false;
        }
        *self = Self::Dying;
        true
    }

    fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    fn is_initializing_or_dying(&self) -> bool {
        matches!(self, Self::Nascent | Self::Dying)
    }
}

/// VFS superblock object.
///
/// A superblock owns one filesystem instance and the live inode set attached to
/// that instance. Inodes own their address spaces; superblock-wide writeback
/// reaches page cache state through those inodes, matching Linux's
/// `super_block` -> `inode` -> `address_space` layering. It also retains hashed
/// dentries until namespace eviction, matching Linux dcache lifetime semantics.
/// Cross-directory rename also uses the superblock's topology mutex,
/// corresponding to Linux `s_vfs_rename_mutex`; same-directory rename does not
/// take that mutex.
pub struct SuperBlock {
    /// Canonical filesystem type corresponding to Linux `s_type`.
    file_system_type: &'static FileSystemType,
    /// Exclusive claim on the canonical device corresponding to Linux
    /// `super_block::s_bdev` and its block-device holder.
    block_device: Option<block::BlockDeviceClaim>,
    /// Shared filesystem operation table corresponding to Linux `s_op`.
    ops: Once<&'static dyn SuperBlockOperations>,
    /// Default dentry operation table corresponding to Linux `s_d_op`.
    default_dentry_operations: Once<&'static dyn DentryOperations>,
    /// Filesystem magic corresponding to Linux `s_magic`.
    magic: Once<u32>,
    /// Filesystem-private mount state corresponding to Linux `s_fs_info`.
    private: Once<Box<dyn Any + Send + Sync>>,
    root: Once<Dentry>,
    /// VFS-wide state corresponding to Linux `super_block::s_flags`.
    flags: AtomicSuperBlockFlags,
    /// Active mount references and shutdown state, corresponding to Linux
    /// `super_block::s_active` together with `SB_DYING` and `SB_DEAD`.
    lifecycle: Mutex<SuperBlockLifecycle>,
    /// Serializes reconfiguration and final shutdown, corresponding to Linux
    /// `super_block::s_umount`.
    umount_lock: Mutex<()>,
    /// Wakes `sget_dev` callers after nascent initialization or final shutdown.
    lifecycle_waiters: WaitQueue,
    dentry_cache: Mutex<HashMap<DentryKey, Dentry>>,
    block_size: Once<u64>,
    max_file_size: Once<u64>,
    rename_mutex: Mutex<()>,
    inodes: Mutex<Vec<Weak<VfsInode>>>,
}

impl SuperBlock {
    /// Returns the filesystem type name.
    pub const fn name(&self) -> &'static str {
        self.file_system_type.name()
    }

    /// Returns the canonical filesystem type that constructed this superblock.
    pub const fn file_system_type(&self) -> &'static FileSystemType {
        self.file_system_type
    }

    /// Returns the canonical block device backing this superblock, if any.
    pub fn block_device(&self) -> Option<&Arc<block::BlockDevice>> {
        self.block_device
            .as_ref()
            .map(block::BlockDeviceClaim::device)
    }

    /// Returns the root dentry for this superblock.
    pub fn root_dir(self: &Arc<Self>) -> Dentry {
        self.root
            .get()
            .expect("a published superblock must have a root dentry")
            .clone()
    }

    /// Returns filesystem statistics for this superblock.
    pub fn stat(&self) -> VfsResult<StatFs> {
        self.ops().statfs(self)
    }

    /// Returns whether this superblock is read-only without issuing `statfs`.
    pub(crate) fn is_readonly(&self) -> bool {
        self.flags().contains(SuperBlockFlags::RDONLY)
    }

    /// Returns VFS superblock flags without issuing `statfs`.
    pub fn flags(&self) -> SuperBlockFlags {
        self.flags.load()
    }

    pub(crate) fn is_available(&self) -> bool {
        self.lifecycle.lock().is_available()
    }

    /// Creates a superblock and initializes its root after allocation.
    ///
    /// The initializer receives the nascent superblock so the root inode can be
    /// obtained through [`Self::get_or_init_inode`]. The superblock is published
    /// to the global registry only after the initializer returns its root.
    pub fn new(
        file_system_type: &'static FileSystemType,
        ops: &'static dyn SuperBlockOperations,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Dentry,
    ) -> Arc<Self> {
        Self::new_with_flags(
            file_system_type,
            ops,
            SuperBlockFlags::empty(),
            init_root_fn,
        )
    }

    /// Creates a superblock with initial VFS-wide flags and then initializes its root.
    pub fn new_with_flags(
        file_system_type: &'static FileSystemType,
        ops: &'static dyn SuperBlockOperations,
        flags: SuperBlockFlags,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Dentry,
    ) -> Arc<Self> {
        match Self::try_new_with_flags(file_system_type, ops, flags, |super_block| {
            Ok::<_, core::convert::Infallible>(init_root_fn(super_block))
        }) {
            Ok(super_block) => super_block,
            Err(error) => match error {},
        }
    }

    /// Creates a superblock with a shared default dentry operation table.
    ///
    /// Dentries allocated on this superblock inherit `dentry_operations`,
    /// corresponding to Linux `set_default_d_op()` assigning `s_d_op` during
    /// `fill_super()`.
    pub fn new_with_dentry_operations(
        file_system_type: &'static FileSystemType,
        ops: &'static dyn SuperBlockOperations,
        dentry_operations: &'static dyn DentryOperations,
        magic: u32,
        block_size: u64,
        max_file_size: u64,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Dentry,
    ) -> Arc<Self> {
        let super_block = Self::allocate(file_system_type, None, SuperBlockFlags::empty());
        super_block.magic.call_once(|| magic);
        match super_block.initialize_inner(
            ops,
            Some(dentry_operations),
            None,
            block_size,
            max_file_size,
            |super_block| Ok::<_, core::convert::Infallible>(init_root_fn(super_block)),
        ) {
            Ok(()) => {}
            Err(error) => match error {},
        }
        super_block.finish_initialization();
        super_block_registry().register(&super_block);
        super_block
    }

    /// Creates a superblock whose root initialization can fail.
    ///
    /// A failed initializer leaves neither a root dentry nor a global
    /// superblock-registry entry behind.
    ///
    /// # Errors
    ///
    /// Returns the root initializer's error without publishing the nascent
    /// superblock.
    pub fn try_new_with_flags<E>(
        file_system_type: &'static FileSystemType,
        ops: &'static dyn SuperBlockOperations,
        flags: SuperBlockFlags,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Result<Dentry, E>,
    ) -> Result<Arc<Self>, E> {
        let super_block = Self::allocate(file_system_type, None, flags);
        super_block.initialize(ops, 1, MAX_LFS_FILESIZE, init_root_fn)?;
        super_block.finish_initialization();
        super_block_registry().register(&super_block);
        Ok(super_block)
    }

    /// Creates a superblock with distinct shared operations and mount-private state.
    pub fn new_with_flags_and_private<T>(
        file_system_type: &'static FileSystemType,
        ops: &'static dyn SuperBlockOperations,
        private: T,
        flags: SuperBlockFlags,
        block_size: u64,
        max_file_size: u64,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Dentry,
    ) -> Arc<Self>
    where
        T: Any + Send + Sync + 'static,
    {
        match Self::try_new_with_flags_and_private(
            file_system_type,
            ops,
            private,
            flags,
            block_size,
            max_file_size,
            |super_block| Ok::<_, core::convert::Infallible>(init_root_fn(super_block)),
        ) {
            Ok(super_block) => super_block,
            Err(error) => match error {},
        }
    }

    /// Creates a superblock with distinct filesystem operations and private state.
    ///
    /// This is the object-model counterpart of Linux `fill_super()` installing
    /// static `s_op`, filesystem-owned `s_fs_info`, and generic `s_blocksize`
    /// and `s_maxbytes` fields on the same nascent superblock.
    ///
    /// # Errors
    ///
    /// Returns the root initializer's error without publishing the nascent
    /// superblock.
    pub fn try_new_with_flags_and_private<T, E>(
        file_system_type: &'static FileSystemType,
        ops: &'static dyn SuperBlockOperations,
        private: T,
        flags: SuperBlockFlags,
        block_size: u64,
        max_file_size: u64,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Result<Dentry, E>,
    ) -> Result<Arc<Self>, E>
    where
        T: Any + Send + Sync + 'static,
    {
        let super_block = Self::allocate(file_system_type, None, flags);
        super_block.initialize_with_private(
            ops,
            private,
            block_size,
            max_file_size,
            init_root_fn,
        )?;
        super_block.finish_initialization();
        super_block_registry().register(&super_block);
        Ok(super_block)
    }

    /// Installs static operations and a root on a nascent superblock.
    ///
    /// This is the object-oriented `fill_super()` operation. Block-backed
    /// filesystems receive the identity reserved by [`get_tree_bdev`] and fill
    /// that object instead of allocating a second superblock.
    pub fn initialize<E>(
        self: &Arc<Self>,
        ops: &'static dyn SuperBlockOperations,
        block_size: u64,
        max_file_size: u64,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Result<Dentry, E>,
    ) -> Result<(), E> {
        self.initialize_inner(ops, None, None, block_size, max_file_size, init_root_fn)
    }

    /// Installs static operations, mount-private state, and a root on a
    /// nascent superblock.
    pub fn initialize_with_private<T, E>(
        self: &Arc<Self>,
        ops: &'static dyn SuperBlockOperations,
        private: T,
        block_size: u64,
        max_file_size: u64,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Result<Dentry, E>,
    ) -> Result<(), E>
    where
        T: Any + Send + Sync + 'static,
    {
        self.initialize_inner(
            ops,
            None,
            Some(Box::new(private)),
            block_size,
            max_file_size,
            init_root_fn,
        )
    }

    fn initialize_inner<E>(
        self: &Arc<Self>,
        ops: &'static dyn SuperBlockOperations,
        default_dentry_operations: Option<&'static dyn DentryOperations>,
        private: Option<Box<dyn Any + Send + Sync>>,
        block_size: u64,
        max_file_size: u64,
        init_root_fn: impl FnOnce(&Arc<Self>) -> Result<Dentry, E>,
    ) -> Result<(), E> {
        assert!(
            matches!(*self.lifecycle.lock(), SuperBlockLifecycle::Nascent),
            "only a nascent superblock can be initialized"
        );
        assert!(
            self.ops.get().is_none()
                && self.default_dentry_operations.get().is_none()
                && self.private.get().is_none()
                && self.root.get().is_none()
                && self.block_size.get().is_none()
                && self.max_file_size.get().is_none(),
            "a superblock can only be initialized once"
        );
        assert!(
            block_size.is_power_of_two(),
            "a superblock block size must be a non-zero power of two"
        );
        self.ops.call_once(|| ops);
        if let Some(default_dentry_operations) = default_dentry_operations {
            self.default_dentry_operations
                .call_once(|| default_dentry_operations);
        }
        if let Some(private) = private {
            self.private.call_once(|| private);
        }
        self.block_size.call_once(|| block_size);
        self.max_file_size
            .call_once(|| max_file_size.min(MAX_LFS_FILESIZE));
        let root = init_root_fn(self)?;
        Self::publish_root(self, root);
        Ok(())
    }

    fn allocate(
        file_system_type: &'static FileSystemType,
        block_device: Option<block::BlockDeviceClaim>,
        flags: SuperBlockFlags,
    ) -> Arc<Self> {
        Arc::new(Self {
            file_system_type,
            block_device,
            ops: Once::new(),
            default_dentry_operations: Once::new(),
            magic: Once::new(),
            private: Once::new(),
            root: Once::new(),
            flags: AtomicSuperBlockFlags::new(flags),
            lifecycle: Mutex::new(SuperBlockLifecycle::Nascent),
            umount_lock: Mutex::default(),
            lifecycle_waiters: WaitQueue::new(),
            dentry_cache: Mutex::default(),
            block_size: Once::new(),
            max_file_size: Once::new(),
            rename_mutex: Mutex::default(),
            inodes: Mutex::default(),
        })
    }

    fn ops(&self) -> &'static dyn SuperBlockOperations {
        *self
            .ops
            .get()
            .expect("a published superblock must have filesystem operations")
    }

    pub(crate) fn default_dentry_operations(&self) -> Option<&'static dyn DentryOperations> {
        self.default_dentry_operations.get().copied()
    }

    /// Returns the filesystem magic installed during superblock initialization.
    pub fn magic(&self) -> u32 {
        self.magic.get().copied().unwrap_or_default()
    }

    /// Returns the filesystem block size in bytes.
    pub fn block_size(&self) -> u64 {
        *self
            .block_size
            .get()
            .expect("a published superblock must have a block size")
    }

    /// Returns this filesystem's mount-private state.
    ///
    /// This is the object-oriented counterpart of Linux helpers such as
    /// `EXT4_SB()`, which recover `s_fs_info` independently of the static
    /// superblock operation table.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] when `T` is not the private object
    /// installed by the filesystem's fill operation.
    pub fn private<T>(&self) -> VfsResult<&T>
    where
        T: Any + Send + Sync + 'static,
    {
        self.private
            .get()
            .map(Box::as_ref)
            .ok_or(VfsError::InvalidInput)?
            .downcast_ref::<T>()
            .ok_or(VfsError::InvalidInput)
    }

    fn publish_root(super_block: &Arc<Self>, root: Dentry) {
        let root = super_block.root.call_once(|| root);
        root.bind_super_block(super_block);
    }

    fn finish_initialization(&self) {
        assert!(
            self.ops.get().is_some()
                && self.root.get().is_some()
                && self.block_size.get().is_some()
                && self.max_file_size.get().is_some(),
            "fill_super must install operations, sizes, and a root"
        );
        let mut lifecycle = self.lifecycle.lock();
        assert!(
            matches!(*lifecycle, SuperBlockLifecycle::Nascent),
            "only a nascent superblock can finish initialization"
        );
        *lifecycle = SuperBlockLifecycle::Available { active_mounts: 0 };
        drop(lifecycle);
        self.lifecycle_waiters.notify_all(false);
    }

    fn fail_initialization(&self) {
        let mut lifecycle = self.lifecycle.lock();
        assert!(
            matches!(*lifecycle, SuperBlockLifecycle::Nascent),
            "only a nascent superblock can fail initialization"
        );
        *lifecycle = SuperBlockLifecycle::Dead;
        drop(lifecycle);
        self.release_block_device();
    }

    fn release_block_device(&self) {
        if let Some(block_device) = &self.block_device {
            block_device.release();
        }
    }

    fn wait_until_available_or_dead(&self) -> bool {
        loop {
            self.lifecycle_waiters
                .wait_until(|| !self.lifecycle.lock().is_initializing_or_dying());
            match &*self.lifecycle.lock() {
                SuperBlockLifecycle::Available { .. } => return true,
                SuperBlockLifecycle::Dead => return false,
                SuperBlockLifecycle::Nascent | SuperBlockLifecycle::Dying => {}
            }
        }
    }

    /// Looks up a live inode identity without invoking a filesystem loader.
    ///
    /// This is the superblock-owned counterpart of Linux `ilookup()`. It only
    /// finds hashed identities; pseudo or otherwise unhashed inodes are not
    /// returned.
    pub fn lookup_inode(self: &Arc<Self>, inode_number: u64) -> Option<Arc<VfsInode>> {
        node::lookup_inode(self, inode_number)
    }

    /// Returns the live inode identity or initializes it exactly once.
    ///
    /// The new inode is attached to this superblock before concurrent lookups
    /// can observe it. This entry is for hashed identities; Linux-style pseudo
    /// inodes constructed outside the hash must remain unhashed.
    pub fn get_or_init_inode(
        self: &Arc<Self>,
        inode_number: u64,
        init_inode_fn: impl FnOnce() -> Arc<VfsInode>,
    ) -> Arc<VfsInode> {
        match self.get_or_try_init_inode(inode_number, || {
            Ok::<_, core::convert::Infallible>(init_inode_fn())
        }) {
            Ok(inode) => inode,
            Err(error) => match error {},
        }
    }

    /// Returns the live inode identity or fallibly initializes it exactly once.
    ///
    /// This provides the identity and initialization semantics of Linux
    /// `iget_locked()` followed by `unlock_new_inode()`. Failed initialization
    /// removes the reservation so a later lookup can retry.
    /// Filesystems must use this entry consistently for every lookup of a
    /// hashed identity; Linux-style pseudo inodes constructed outside the hash
    /// must remain unhashed.
    ///
    /// # Errors
    ///
    /// Returns the inode initializer's error without publishing a new identity.
    ///
    /// # Panics
    ///
    /// Panics if the initializer returns a different inode number or an inode
    /// identity already owned by another superblock.
    pub fn get_or_try_init_inode<E>(
        self: &Arc<Self>,
        inode_number: u64,
        init_inode_fn: impl FnOnce() -> Result<Arc<VfsInode>, E>,
    ) -> Result<Arc<VfsInode>, E> {
        node::get_or_try_init_inode(self, inode_number, init_inode_fn)
    }

    /// Acquires the active superblock reference owned by one `VfsMount`.
    pub(crate) fn activate_mount(&self) {
        assert!(
            self.try_activate_mount(None)
                .expect("unchecked mount activation cannot reject flags"),
            "a dying or dead superblock must not acquire an active mount"
        );
    }

    /// Attempts to acquire one active mount reference.
    ///
    /// Activation is serialized with final shutdown so a mount either becomes
    /// active before the last old mount is released or observes the instance
    /// as unavailable and can repeat `get_tree`.
    pub(crate) fn try_activate_mount(
        &self,
        requested_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<bool> {
        let _umount_guard = self.umount_lock.lock();
        let mut lifecycle = self.lifecycle.lock();
        if !lifecycle.is_available() {
            return Ok(false);
        }
        if requested_flags
            .is_some_and(|requested_flags| has_readonly_mismatch(self.flags(), requested_flags))
        {
            return Err(VfsError::ResourceBusy);
        }
        Ok(lifecycle.try_acquire_mount())
    }

    /// Releases one active mount reference and shuts down the last active mount.
    ///
    /// This is the object-lifetime counterpart of Linux
    /// `cleanup_mnt() -> deactivate_super() -> generic_shutdown_super()`.
    /// Shutdown errors cannot be returned from `Drop`; they are logged, while
    /// topology detach remains committed as it does for Linux `umount(2)`.
    pub(crate) fn deactivate_mount(&self) {
        // Like Linux `deactivate_super()`, do not consume the final active
        // reference until final shutdown is serialized by `s_umount`.
        if !self.lifecycle.lock().release_unless_last() {
            return;
        }

        let _umount_guard = self.umount_lock.lock();
        // A concurrent activation may have made this reference non-final while
        // the umount lock was being acquired.
        if !self.lifecycle.lock().begin_shutdown() {
            return;
        }

        let shutdown_result = self.shutdown();
        // A new `sget` must observe either the old identity and holder or
        // neither of them.
        {
            let mut super_blocks = super_block_registry().super_blocks.lock();
            *self.lifecycle.lock() = SuperBlockLifecycle::Dead;
            self.release_block_device();
            super_blocks.unregister(self);
        }
        self.lifecycle_waiters.notify_all(false);
        if let Err(err) = shutdown_result {
            log::warn!(
                "failed to shut down {} after its last active mount: {err:?}",
                self.name()
            );
        }
    }

    /// Performs the filesystem-independent part of final active-mount shutdown.
    fn shutdown(&self) -> VfsResult<()> {
        // X-Kernel does not retain dirty inodes independently of dentries yet,
        // so write back before dropping dcache ownership. Eviction can publish
        // additional filesystem metadata, which requires the second sync.
        let writeback_result = self.sync_fs_locked();
        if let Some(root) = self.root.get()
            && let Ok(root) = root.as_dir()
        {
            root.forget();
        }
        let flush_result = self.sync_fs_locked();
        writeback_result.and(flush_result)
    }

    /// Applies a parsed filesystem-context transaction to this superblock.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] if the context is not a reconfigure
    /// transaction or targets another superblock. Filesystem-specific
    /// validation errors are returned without changing the superblock flags.
    pub fn reconfigure(&self, context: &mut FsContext<'_>) -> VfsResult<()> {
        let _umount_guard = self.umount_lock.lock();
        if context.purpose() != FsContextPurpose::Reconfigure
            || !core::ptr::eq(self, context.super_block()?)
        {
            return Err(VfsError::InvalidInput);
        }
        let mask = context.sb_flags_mask();
        let flags = (self.flags() & !mask) | (context.sb_flags() & mask);
        if let Some(block_device) = self.block_device() {
            validate_block_device_flags(block_device, flags)?;
        }
        context.reconfigure()?;
        self.flags.store(flags);
        Ok(())
    }

    /// Retains a hashed dentry until it is explicitly evicted from the dcache.
    pub(crate) fn cache_dentry(&self, dentry: Dentry) {
        let key = dentry.key();
        let replaced = self.dentry_cache.lock().insert(key, dentry);
        drop(replaced);
    }

    /// Removes a dentry from the dcache without affecting external references.
    pub(crate) fn uncache_dentry(&self, dentry: &Dentry) {
        let key = dentry.key();
        let removed = self.dentry_cache.lock().remove(&key);
        drop(removed);
    }

    /// Returns whether `dentry` is currently retained by the dcache.
    #[cfg(unittest)]
    pub(crate) fn is_dentry_cached(&self, dentry: &Dentry) -> bool {
        self.dentry_cache.lock().contains_key(&dentry.key())
    }

    pub(crate) fn move_cached_dentry(
        &self,
        old_key: &DentryKey,
        new_key: &DentryKey,
        source: &Dentry,
    ) {
        let mut cache = self.dentry_cache.lock();
        let removed = cache.remove(old_key);
        if let Some(target_slot) = cache.get_mut(new_key) {
            *target_slot = source.clone();
        }
        drop(cache);
        drop(removed);
    }

    pub(crate) fn exchange_cached_dentries(
        &self,
        old_key: &DentryKey,
        source: &Dentry,
        new_key: &DentryKey,
        target: &Dentry,
    ) {
        let mut cache = self.dentry_cache.lock();
        if cache.contains_key(old_key) && cache.contains_key(new_key) {
            *cache.get_mut(old_key).expect("checked cache entry") = target.clone();
            *cache.get_mut(new_key).expect("checked cache entry") = source.clone();
        } else {
            let old = cache.remove(old_key);
            let new = cache.remove(new_key);
            drop(cache);
            drop(old);
            drop(new);
        }
    }

    /// Returns this superblock's maximum regular-file size.
    pub fn max_file_size(&self) -> u64 {
        *self
            .max_file_size
            .get()
            .expect("a published superblock must have a maximum file size")
    }

    /// Serializes directory-tree topology changes across different parents.
    pub(crate) fn lock_rename_topology(&self) -> MutexGuard<'_, ()> {
        self.rename_mutex.lock()
    }

    /// Tracks an inode attached to this superblock.
    pub(crate) fn register_inode(&self, inode: &Arc<VfsInode>) {
        let mut inodes = self.inodes.lock();
        inodes.retain(|existing| match existing.upgrade() {
            Some(existing) => !Arc::ptr_eq(&existing, inode),
            None => false,
        });
        inodes.push(Arc::downgrade(inode));
    }

    fn live_inodes(&self) -> Vec<Arc<VfsInode>> {
        let mut live = Vec::new();
        self.inodes.lock().retain(|inode| match inode.upgrade() {
            Some(inode) => {
                live.push(inode);
                true
            }
            None => false,
        });
        live
    }

    /// Starts writeback for dirty inode page-cache state on this superblock.
    fn writeback_inodes(inodes: &[Arc<VfsInode>], data_only: bool) -> VfsResult<()> {
        for inode in inodes {
            inode.sync(data_only)?;
        }
        Ok(())
    }

    /// Writes back filesystem-owned metadata for live inodes on this superblock.
    fn writeback_inode_metadata(&self, inodes: &[Arc<VfsInode>], data_only: bool) -> VfsResult<()> {
        let mut control = WritebackControl::all(data_only);
        for inode in inodes {
            self.ops().write_inode(inode.as_ref(), &mut control)?;
        }
        Ok(())
    }

    /// Synchronizes inode page-cache state and then filesystem-owned state.
    ///
    /// The operation is serialized with reconfiguration and final shutdown by
    /// the superblock umount lock.
    pub fn sync_fs(&self) -> VfsResult<()> {
        let _umount_guard = self.umount_lock.lock();
        if !self.lifecycle.lock().is_available() {
            return Ok(());
        }
        self.sync_fs_locked()
    }

    fn sync_fs_locked(&self) -> VfsResult<()> {
        let inodes = self.live_inodes();
        Self::writeback_inodes(&inodes, false)?;
        self.writeback_inode_metadata(&inodes, false)?;
        self.ops().sync_fs(self)
    }

    /// Releases filesystem-owned inode state during final inode teardown.
    pub(crate) fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        self.ops().evict_inode(inode)
    }
}

impl core::fmt::Debug for SuperBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SuperBlock")
            .field("name", &self.name())
            .finish()
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, string::String, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::{
        MAX_LFS_FILESIZE, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations,
        super_block_registry,
    };
    use crate::{
        Dentry, FileSystemType, FsContext, FsContextOperations, NodeFlags, NodePermission,
        NodeType, Path, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult,
    };

    struct TestSuperOperations;

    static TEST_SUPER_OPERATIONS: TestSuperOperations = TestSuperOperations;
    static TEST_FILE_SYSTEM_TYPE: crate::FileSystemType =
        crate::FileSystemType::internal("private-test");

    struct TestPrivate(u64);

    impl SuperBlockOperations for TestSuperOperations {
        fn statfs(&self, super_block: &SuperBlock) -> crate::VfsResult<StatFs> {
            if super_block.private::<TestPrivate>()?.0 != 0x5f5f_494e_464f {
                return Err(crate::VfsError::InvalidInput);
            }
            Ok(StatFs {
                fs_type: 0,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 0,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 4096,
            })
        }
    }

    #[def_test]
    fn superblock_separates_operations_from_private_state() {
        let super_block = SuperBlock::try_new_with_flags_and_private(
            &TEST_FILE_SYSTEM_TYPE,
            &TEST_SUPER_OPERATIONS,
            TestPrivate(0x5f5f_494e_464f),
            SuperBlockFlags::empty(),
            4096,
            1 << 30,
            |_| {
                let inode = VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    VfsInodeInit::new(
                        2,
                        0,
                        Umode::new(NodeType::Directory, NodePermission::default()),
                    ),
                );
                Ok::<_, crate::VfsError>(Dentry::new_dir_from_inode(inode, None, String::new()))
            },
        )
        .expect("construct superblock with private state");

        assert_eq!(super_block.block_size(), 4096);
        assert_eq!(super_block.max_file_size(), 1 << 30);
        assert!(super_block.private::<TestSuperOperations>().is_err());
        assert!(super_block.stat().is_ok());
    }

    static FILL_COUNT: AtomicUsize = AtomicUsize::new(0);
    static TRANSITION_STAGE: AtomicUsize = AtomicUsize::new(0);
    static TRANSITION_WAITER_STARTED: AtomicUsize = AtomicUsize::new(0);
    static TRANSITION_WAITER_DONE: AtomicUsize = AtomicUsize::new(0);
    static TRANSITION_WAITERS: ktask::WaitQueue = ktask::WaitQueue::new();

    fn bdev_test_get_tree(
        _context: &mut FsContext<'_>,
        _lookup_root: &Path,
        _lookup_pwd: &Path,
    ) -> VfsResult<Arc<SuperBlock>> {
        unreachable!("the registry test invokes its fill operation directly")
    }

    static BDEV_TEST_CONTEXT_OPERATIONS: FsContextOperations =
        FsContextOperations::new(bdev_test_get_tree);

    fn init_bdev_test_context(context: &mut FsContext<'_>) -> VfsResult<()> {
        context.set_operations(&BDEV_TEST_CONTEXT_OPERATIONS);
        Ok(())
    }

    static BDEV_TEST_FILE_SYSTEM_TYPE: FileSystemType =
        FileSystemType::device_backed("bdev-test", init_bdev_test_context);
    static BDEV_ALT_FILE_SYSTEM_TYPE: FileSystemType =
        FileSystemType::device_backed("bdev-alt-test", init_bdev_test_context);

    struct BdevTestSuperOperations;

    static BDEV_TEST_SUPER_OPERATIONS: BdevTestSuperOperations = BdevTestSuperOperations;

    fn test_statfs() -> StatFs {
        StatFs {
            fs_type: 0,
            block_size: 512,
            blocks: 1,
            blocks_free: 1,
            blocks_available: 1,
            file_count: 1,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 512,
        }
    }

    impl SuperBlockOperations for BdevTestSuperOperations {
        fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
            Ok(test_statfs())
        }
    }

    struct BlockingShutdownOperations;

    static BLOCKING_SHUTDOWN_OPERATIONS: BlockingShutdownOperations = BlockingShutdownOperations;

    impl SuperBlockOperations for BlockingShutdownOperations {
        fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
            Ok(test_statfs())
        }

        fn sync_fs(&self, _super_block: &SuperBlock) -> VfsResult<()> {
            if TRANSITION_STAGE
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                TRANSITION_WAITERS.notify_all(false);
                TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) >= 2);
            }
            Ok(())
        }
    }

    struct TestBlockDevice;

    impl block::BlockDeviceOperations for TestBlockDevice {
        fn num_blocks(&self) -> u64 {
            1
        }

        fn block_size(&self) -> usize {
            512
        }

        fn read_block(&self, _block_id: u64, _buf: &mut [u8]) -> block::DriverResult {
            Ok(())
        }

        fn write_block(&self, _block_id: u64, _buf: &[u8]) -> block::DriverResult {
            Ok(())
        }

        fn flush(&self) -> block::DriverResult {
            Ok(())
        }
    }

    fn initialize_test_bdev(
        super_block: &Arc<SuperBlock>,
        operations: &'static dyn SuperBlockOperations,
    ) -> VfsResult<()> {
        super_block.initialize(operations, 512, MAX_LFS_FILESIZE, |super_block| {
            let inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    VfsInodeInit::new(
                        1,
                        0,
                        Umode::new(NodeType::Directory, NodePermission::default()),
                    ),
                )
            });
            Ok(Dentry::new_dir_from_inode(inode, None, String::new()))
        })
    }

    fn fill_test_bdev(super_block: &Arc<SuperBlock>) -> VfsResult<()> {
        FILL_COUNT.fetch_add(1, Ordering::Relaxed);
        initialize_test_bdev(super_block, &BDEV_TEST_SUPER_OPERATIONS)
    }

    fn fail_test_bdev(_super_block: &Arc<SuperBlock>) -> VfsResult<()> {
        FILL_COUNT.fetch_add(1, Ordering::Relaxed);
        TRANSITION_STAGE.store(1, Ordering::Release);
        TRANSITION_WAITERS.notify_all(false);
        TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) >= 2);
        Err(VfsError::InvalidInput)
    }

    fn fill_blocking_shutdown_bdev(super_block: &Arc<SuperBlock>) -> VfsResult<()> {
        FILL_COUNT.fetch_add(1, Ordering::Relaxed);
        initialize_test_bdev(super_block, &BLOCKING_SHUTDOWN_OPERATIONS)
    }

    fn reset_transition_test_state() {
        FILL_COUNT.store(0, Ordering::Relaxed);
        TRANSITION_STAGE.store(0, Ordering::Release);
        TRANSITION_WAITER_STARTED.store(0, Ordering::Release);
        TRANSITION_WAITER_DONE.store(0, Ordering::Release);
    }

    fn spawn_successful_waiting_mount(block_device: Arc<block::BlockDevice>) -> ktask::KtaskRef {
        ktask::spawn(move || {
            TRANSITION_WAITER_STARTED.store(1, Ordering::Release);
            TRANSITION_WAITERS.notify_all(false);
            let super_block = super_block_registry()
                .get_or_try_init_bdev(
                    &BDEV_TEST_FILE_SYSTEM_TYPE,
                    block_device,
                    SuperBlockFlags::empty(),
                    fill_test_bdev,
                )
                .expect("mount retries after the old superblock transition");
            super_block.activate_mount();
            super_block.deactivate_mount();
            TRANSITION_WAITER_DONE.store(1, Ordering::Release);
            TRANSITION_WAITERS.notify_all(false);
        })
    }

    fn add_test_disk(name: &str, major: u32) -> (Arc<block::Gendisk>, Arc<block::BlockDevice>) {
        let disk = Arc::new(
            block::Gendisk::new(String::from(name), major, 0, 1, Box::new(TestBlockDevice))
                .expect("valid superblock test disk"),
        );
        let block_device = block::add_disk(disk.clone()).expect("publish superblock test disk");
        (disk, block_device)
    }

    #[def_test(serial)]
    fn sget_dev_reuses_identity_and_rejects_readonly_mismatch() {
        FILL_COUNT.store(0, Ordering::Relaxed);
        let (disk, block_device) = add_test_disk("kvfs-sget-test", 246);
        let registry = super_block_registry();

        let first = registry
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device.clone(),
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("first mount initializes the device superblock");
        let second = registry
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device.clone(),
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("second mount reuses the device superblock");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 1);
        first.activate_mount();
        assert!(matches!(
            registry.get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device,
                SuperBlockFlags::RDONLY,
                fill_test_bdev,
            ),
            Err(VfsError::ResourceBusy)
        ));
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 1);

        first.deactivate_mount();
        drop(second);
        block::del_gendisk(disk.device_number()).expect("remove sget test disk");
    }

    #[def_test(serial)]
    fn block_device_claim_rejects_a_different_filesystem_until_shutdown() {
        FILL_COUNT.store(0, Ordering::Relaxed);
        let (disk, block_device) = add_test_disk("kvfs-holder-test", 247);
        let registry = super_block_registry();
        let first = registry
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device.clone(),
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("first filesystem claims the device");
        first.activate_mount();

        assert!(matches!(
            registry.get_or_try_init_bdev(
                &BDEV_ALT_FILE_SYSTEM_TYPE,
                block_device.clone(),
                SuperBlockFlags::empty(),
                fill_test_bdev,
            ),
            Err(VfsError::ResourceBusy)
        ));
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 1);

        first.deactivate_mount();
        let replacement = registry
            .get_or_try_init_bdev(
                &BDEV_ALT_FILE_SYSTEM_TYPE,
                block_device,
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("dead superblock releases its block-device claim");
        replacement.activate_mount();
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);
        replacement.deactivate_mount();
        block::del_gendisk(disk.device_number()).expect("remove holder test disk");
    }

    #[def_test(serial)]
    fn sget_dev_uses_canonical_device_object_identity() {
        FILL_COUNT.store(0, Ordering::Relaxed);
        let (old_disk, old_device) = add_test_disk("kvfs-old-device", 248);
        let registry = super_block_registry();
        let old_super_block = registry
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                old_device,
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("mount old device generation");
        old_super_block.activate_mount();
        block::del_gendisk(old_disk.device_number()).expect("unpublish old disk");

        let (new_disk, new_device) = add_test_disk("kvfs-new-device", 248);
        let new_super_block = registry
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                new_device,
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("mount replacement device generation");
        new_super_block.activate_mount();

        assert!(!Arc::ptr_eq(&old_super_block, &new_super_block));
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);

        old_super_block.deactivate_mount();
        new_super_block.deactivate_mount();
        block::del_gendisk(new_disk.device_number()).expect("remove replacement disk");
    }

    #[def_test(serial)]
    fn read_only_block_device_cannot_be_reconfigured_read_write() {
        FILL_COUNT.store(0, Ordering::Relaxed);
        let (disk, block_device) = add_test_disk("kvfs-read-only-remount", 249);
        block_device
            .set_disk_read_only(true)
            .expect("make backing device read-only");
        let super_block = super_block_registry()
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device,
                SuperBlockFlags::RDONLY,
                fill_test_bdev,
            )
            .expect("mount read-only device read-only");
        super_block.activate_mount();
        let cred = kcred::initial_cred();
        let mut context = FsContext::new_reconfigure(
            super_block.as_ref(),
            None,
            None,
            SuperBlockFlags::empty(),
            SuperBlockFlags::RDONLY,
            &cred,
        )
        .expect("construct writable reconfigure context");

        assert_eq!(
            super_block.reconfigure(&mut context),
            Err(VfsError::PermissionDenied)
        );
        assert!(super_block.flags().contains(SuperBlockFlags::RDONLY));

        super_block.deactivate_mount();
        block::del_gendisk(disk.device_number()).expect("remove read-only remount disk");
    }

    #[def_test(serial)]
    fn failed_fill_wakes_waiter_and_allows_retry() {
        reset_transition_test_state();
        let (disk, block_device) = add_test_disk("kvfs-failed-fill", 250);
        let owner_device = block_device.clone();
        let owner = ktask::spawn(move || {
            match super_block_registry().get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                owner_device,
                SuperBlockFlags::empty(),
                fail_test_bdev,
            ) {
                Err(VfsError::InvalidInput) => {}
                result => panic!("failed fill returned an unexpected result: {result:?}"),
            }
        });
        TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) == 1);

        let waiter = spawn_successful_waiting_mount(block_device);
        TRANSITION_WAITERS.wait_until(|| TRANSITION_WAITER_STARTED.load(Ordering::Acquire) == 1);
        ktask::yield_now();
        assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 0);

        TRANSITION_STAGE.store(2, Ordering::Release);
        TRANSITION_WAITERS.notify_all(true);
        owner.join();
        waiter.join();

        assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 1);
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);
        block::del_gendisk(disk.device_number()).expect("remove failed-fill test disk");
    }

    #[def_test(serial)]
    fn dying_superblock_wakes_waiter_after_shutdown() {
        reset_transition_test_state();
        let (disk, block_device) = add_test_disk("kvfs-dying-wait", 251);
        let super_block = super_block_registry()
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device.clone(),
                SuperBlockFlags::empty(),
                fill_blocking_shutdown_bdev,
            )
            .expect("mount device before blocking shutdown");
        super_block.activate_mount();

        let shutting_down = super_block.clone();
        let shutdown = ktask::spawn(move || shutting_down.deactivate_mount());
        TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) == 1);

        let waiter = spawn_successful_waiting_mount(block_device);
        TRANSITION_WAITERS.wait_until(|| TRANSITION_WAITER_STARTED.load(Ordering::Acquire) == 1);
        ktask::yield_now();
        assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 0);

        TRANSITION_STAGE.store(2, Ordering::Release);
        TRANSITION_WAITERS.notify_all(true);
        shutdown.join();
        waiter.join();

        assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 1);
        assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);
        block::del_gendisk(disk.device_number()).expect("remove dying-wait test disk");
    }
}
