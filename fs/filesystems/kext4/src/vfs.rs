// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVFS integration for ext4 mount and inode-private objects.

use alloc::{string::String, sync::Arc, vec, vec::Vec};

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::LinuxError;
use ktime_types::SystemTime;
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FMode, FiemapExtentFlags,
    FiemapExtentInfo, FiemapFlags, FileDirOperations, FileOperations, InodeAttributeOperations,
    InodeDirOperations, InodeFiemapOperations, InodeOperations, InodeSymlinkOperations, Kiocb,
    LockedDentry, Metadata, MetadataUpdate, NodePermission, NodeType, PageMkwriteRequest,
    ReadaheadControl, RenameFlags, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    Umode, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult, WriteBeginRequest,
    WriteEndRequest, WritebackControl, WritebackRangeOutcome, XattrName, XattrNameRef,
    XattrNameSink, XattrSetFlags, default_evict_inode, inode_init_owner,
};

use crate::{
    BlockMapping, BlockMappingFlags, DirectoryFileType, Ext4DeviceId, Ext4DirEntryRef, Ext4DirPos,
    Ext4DirSink, Ext4Error, Ext4Inode, Ext4InodeMetadataUpdate, Ext4InodeStat, Ext4Result,
    Ext4SyncIntent, Ext4Timestamp, Ext4XattrNameRef, Ext4XattrNameSink, Ext4XattrNamespace,
    Ext4XattrSetMode, InodeKind, InodeNumber, LogicalBlock,
    disk::inode::EXT4_ROOT_INO,
    superblock::{Ext4SbInfo, Ext4StatFsMode},
    sync::{self, RwLock},
};

// Bounds phased eviction to at most 1 MiB of block-allocation work per
// transaction when ext4 uses its maximum 4-KiB block size.
const EVICTION_BATCH_BLOCKS: u32 = 256;
const PAGE_SIZE_4K: usize = 4096;
const MAX_WRITEBACK_BYTES: usize = 128 * 1024;
const XATTR_USER_PREFIX: &[u8] = b"user.";
const XATTR_TRUSTED_PREFIX: &[u8] = b"trusted.";
const XATTR_SECURITY_PREFIX: &[u8] = b"security.";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Ext4MountOptions {
    pub(crate) statfs_mode: Option<Ext4StatFsMode>,
}

impl Ext4MountOptions {
    pub(crate) fn parse(data: Option<&[u8]>) -> VfsResult<Self> {
        let mut options = Self::default();
        let data = data.unwrap_or_default();
        let text = &data[..data
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(data.len())];
        for option in text.split(|byte| *byte == b',') {
            match option {
                b"" => {}
                b"minixdf" => options.statfs_mode = Some(Ext4StatFsMode::Minix),
                b"bsddf" => options.statfs_mode = Some(Ext4StatFsMode::Bsd),
                // Linux accepts these explicit positive defaults. KExt4 has no
                // per-mount state to change for either compatibility spelling.
                b"acl" | b"user_xattr" => {}
                _ => return Err(VfsError::InvalidInput),
            }
        }
        Ok(options)
    }

    const fn mount_statfs_mode(self) -> Ext4StatFsMode {
        match self.statfs_mode {
            Some(mode) => mode,
            None => Ext4StatFsMode::Bsd,
        }
    }
}

fn parse_xattr_name(name: &XattrName) -> VfsResult<(Ext4XattrNamespace, &[u8])> {
    let name = name.as_bytes();
    let (namespace, suffix) = if let Some(suffix) = name.strip_prefix(XATTR_USER_PREFIX) {
        (Ext4XattrNamespace::User, suffix)
    } else if let Some(suffix) = name.strip_prefix(XATTR_TRUSTED_PREFIX) {
        (Ext4XattrNamespace::Trusted, suffix)
    } else if let Some(suffix) = name.strip_prefix(XATTR_SECURITY_PREFIX) {
        (Ext4XattrNamespace::Security, suffix)
    } else {
        // POSIX ACLs require permission evaluation, mode synchronization, and
        // inheritance. The core's opaque ACL storage is intentionally not
        // exposed as a raw system.* xattr until that VFS layer exists.
        return Err(VfsError::OperationNotSupported);
    };
    if suffix.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    Ok((namespace, suffix))
}

fn xattr_set_mode(flags: XattrSetFlags) -> Ext4XattrSetMode {
    match (
        flags.contains(XattrSetFlags::CREATE),
        flags.contains(XattrSetFlags::REPLACE),
    ) {
        (false, false) => Ext4XattrSetMode::CreateOrReplace,
        (true, false) => Ext4XattrSetMode::Create,
        (false, true) => Ext4XattrSetMode::Replace,
        (true, true) => Ext4XattrSetMode::CreateAndReplace,
    }
}

fn xattr_namespace_prefix(namespace: Ext4XattrNamespace) -> Option<&'static [u8]> {
    Some(match namespace {
        Ext4XattrNamespace::User => XATTR_USER_PREFIX,
        Ext4XattrNamespace::Trusted => XATTR_TRUSTED_PREFIX,
        Ext4XattrNamespace::Security => XATTR_SECURITY_PREFIX,
        _ => return None,
    })
}

struct VfsXattrNameSink<'a> {
    inner: &'a mut dyn XattrNameSink,
    error: Option<VfsError>,
}

impl VfsXattrNameSink<'_> {
    fn finish(self) -> VfsResult<()> {
        self.error.map_or(Ok(()), Err)
    }
}

impl Ext4XattrNameSink for VfsXattrNameSink<'_> {
    fn emit(&mut self, name: Ext4XattrNameRef<'_>) -> Ext4Result<()> {
        if self.error.is_some() {
            return Ok(());
        }
        let Some(prefix) = xattr_namespace_prefix(name.namespace()) else {
            return Ok(());
        };
        if name.name_bytes().is_empty() {
            self.error = Some(VfsError::InvalidData);
            return Ok(());
        }
        let name = match XattrNameRef::from_parts(prefix, name.name_bytes()) {
            Ok(name) => name,
            Err(_) => {
                self.error = Some(VfsError::InvalidData);
                return Ok(());
            }
        };
        if let Err(error) = self.inner.emit(name) {
            self.error = Some(error);
        }
        Ok(())
    }
}

fn into_xattr_vfs_err(err: Ext4Error) -> VfsError {
    if err == Ext4Error::NotFound {
        VfsError::from(LinuxError::ENODATA)
    } else {
        into_vfs_err(err)
    }
}

fn into_vfs_err(error: Ext4Error) -> VfsError {
    let linux_error = match error {
        Ext4Error::NoSpace => LinuxError::ENOSPC,
        Ext4Error::AlreadyExists => LinuxError::EEXIST,
        Ext4Error::NotFound => LinuxError::ENOENT,
        Ext4Error::DirectoryNotEmpty => LinuxError::ENOTEMPTY,
        Ext4Error::InvalidName
        | Ext4Error::InvalidBufferLength { .. }
        | Ext4Error::InvalidDeviceBlockSize(_)
        | Ext4Error::OutOfBounds
        | Ext4Error::InvalidDirectoryPosition
        | Ext4Error::InvalidMagic(_)
        | Ext4Error::UnsupportedRevision(_) => LinuxError::EINVAL,
        Ext4Error::JournalBusy => LinuxError::EBUSY,
        Ext4Error::UnsupportedFeature { .. }
        | Ext4Error::UnsupportedJournalFeature { .. }
        | Ext4Error::Unsupported(_) => LinuxError::EOPNOTSUPP,
        Ext4Error::Device(_)
        | Ext4Error::InvalidDelayedAllocationState
        | Ext4Error::InvalidInodeState
        | Ext4Error::JournalAborted
        | Ext4Error::InsufficientJournalCredits
        | Ext4Error::InvalidJournalTransaction
        | Ext4Error::Overflow
        | Ext4Error::NeedsRecovery
        | Ext4Error::ChecksumMismatch { .. }
        | Ext4Error::Corrupt(_) => LinuxError::EIO,
    };
    VfsError::from(linux_error).canonicalize()
}

const fn inode_kind_to_vfs(kind: InodeKind) -> NodeType {
    match kind {
        InodeKind::Fifo => NodeType::Fifo,
        InodeKind::CharacterDevice => NodeType::CharacterDevice,
        InodeKind::Directory => NodeType::Directory,
        InodeKind::BlockDevice => NodeType::BlockDevice,
        InodeKind::RegularFile => NodeType::RegularFile,
        InodeKind::Symlink => NodeType::Symlink,
        InodeKind::Socket => NodeType::Socket,
    }
}

const fn vfs_type_to_inode_kind(node_type: NodeType) -> Option<InodeKind> {
    match node_type {
        NodeType::Fifo => Some(InodeKind::Fifo),
        NodeType::CharacterDevice => Some(InodeKind::CharacterDevice),
        NodeType::Directory => Some(InodeKind::Directory),
        NodeType::BlockDevice => Some(InodeKind::BlockDevice),
        NodeType::RegularFile => Some(InodeKind::RegularFile),
        NodeType::Symlink => Some(InodeKind::Symlink),
        NodeType::Socket => Some(InodeKind::Socket),
        NodeType::Unknown => None,
    }
}

const fn dir_entry_type_to_vfs(file_type: DirectoryFileType) -> NodeType {
    match file_type {
        DirectoryFileType::RegularFile => NodeType::RegularFile,
        DirectoryFileType::Directory => NodeType::Directory,
        DirectoryFileType::CharacterDevice => NodeType::CharacterDevice,
        DirectoryFileType::BlockDevice => NodeType::BlockDevice,
        DirectoryFileType::Fifo => NodeType::Fifo,
        DirectoryFileType::Socket => NodeType::Socket,
        DirectoryFileType::Symlink => NodeType::Symlink,
        DirectoryFileType::Unknown => NodeType::Unknown,
    }
}

const fn device_id_to_ext4(device: DeviceId) -> Ext4DeviceId {
    Ext4DeviceId::new(device.major(), device.minor())
}

const fn ext4_device_id_to_vfs(device: Ext4DeviceId) -> DeviceId {
    DeviceId::new(device.major(), device.minor())
}

fn system_time_to_ext4(timestamp: SystemTime) -> Ext4Timestamp {
    Ext4Timestamp::new(timestamp.unix_seconds(), timestamp.subsec_nanos())
}

fn ext4_timestamp_to_system_time(timestamp: Ext4Timestamp) -> SystemTime {
    SystemTime::from_unix_parts(timestamp.seconds(), timestamp.nanos())
        .expect("Ext4Timestamp normalizes sub-second nanoseconds")
}

#[cfg(feature = "times")]
fn current_ext4_timestamp(inode: &VfsInode) -> Ext4Timestamp {
    system_time_to_ext4(inode.current_time())
}

#[cfg(not(feature = "times"))]
fn current_ext4_timestamp(_inode: &VfsInode) -> Ext4Timestamp {
    Ext4Timestamp::new(0, 0)
}

fn node_flags_from_inode(inode: &Ext4Inode) -> kvfs::NodeFlags {
    let (is_immutable, is_append_only) = inode.inode_attr_flags();
    let mut flags = kvfs::NodeFlags::empty();
    flags.set(kvfs::NodeFlags::IMMUTABLE, is_immutable);
    flags.set(kvfs::NodeFlags::APPEND_ONLY, is_append_only);
    flags
}

fn metadata_from_inode(inode: &Ext4Inode, block_size: u64) -> Metadata {
    let stat = inode.stat();
    Metadata {
        device: 0,
        inode: u64::from(inode.number().get()),
        nlink: u64::from(stat.links_count),
        mode: Umode::from_bits(stat.mode),
        uid: stat.uid,
        gid: stat.gid,
        size: stat.size,
        block_size,
        blocks: stat.blocks,
        rdev: stat.rdev.map_or(DeviceId::default(), ext4_device_id_to_vfs),
        atime: ext4_timestamp_to_system_time(stat.atime),
        mtime: ext4_timestamp_to_system_time(stat.mtime),
        ctime: ext4_timestamp_to_system_time(stat.ctime),
    }
}

fn applied_metadata_update(requested: MetadataUpdate, applied: Ext4InodeStat) -> MetadataUpdate {
    MetadataUpdate {
        size: None,
        mode: requested
            .mode
            .map(|_| NodePermission::from_bits_truncate(applied.mode)),
        owner: requested.owner.map(|_| (applied.uid, applied.gid)),
        atime: requested
            .atime
            .map(|_| ext4_timestamp_to_system_time(applied.atime)),
        mtime: requested
            .mtime
            .map(|_| ext4_timestamp_to_system_time(applied.mtime)),
        ctime: requested
            .ctime
            .map(|_| ext4_timestamp_to_system_time(applied.ctime)),
    }
}

type SharedExt4SbInfo = RwLock<Ext4SbInfo>;

fn ext4_private(super_block: &SuperBlock) -> VfsResult<&SharedExt4SbInfo> {
    super_block.private::<SharedExt4SbInfo>()
}

/// Static ext4 superblock operation table.
///
/// Mount-private identity lives only in the `Ext4SbInfo` installed as KVFS
/// superblock private data, matching Linux's separate `s_op` and `s_fs_info`.
pub(crate) struct Ext4SuperOperations;

static EXT4_SUPER_OPERATIONS: Ext4SuperOperations = Ext4SuperOperations;

impl Ext4SuperOperations {
    /// Fills a newly reserved ext4 superblock from its canonical block device.
    pub(crate) fn fill_super(
        super_block: &Arc<SuperBlock>,
        options: Ext4MountOptions,
    ) -> VfsResult<()> {
        let device = super_block
            .block_device()
            .expect("get_tree_bdev must set s_bdev before fill_super")
            .clone();
        let device: Arc<dyn block::BlockDeviceOperations> = device;
        let statfs_mode = options.mount_statfs_mode();
        let is_read_only = super_block.flags().contains(SuperBlockFlags::RDONLY);
        let mount_fn = |device| Ext4SbInfo::prepare_mount_with_statfs_mode(device, statfs_mode);
        let state = match mount_fn(device.clone()) {
            Ok(state) => state,
            Err(Ext4Error::NeedsRecovery) => {
                if let Err(error) = Ext4SbInfo::recover(device.clone()) {
                    error!("KExt4 recovery failed: {error:?}");
                    return Err(into_vfs_err(error));
                }
                match mount_fn(device) {
                    Ok(state) => state,
                    Err(error) => {
                        error!("KExt4 mount after filesystem recovery failed: {error:?}");
                        return Err(into_vfs_err(error));
                    }
                }
            }
            Err(error) => {
                error!("KExt4 core mount failed: {error:?}");
                return Err(into_vfs_err(error));
            }
        };
        let block_size = u64::from(state.layout().block_size());
        let max_file_size = state.extent_max_file_size().map_err(into_vfs_err)?;
        let ext4 = RwLock::new(state);
        super_block.initialize_with_private(
            &EXT4_SUPER_OPERATIONS,
            ext4,
            block_size,
            max_file_size,
            |super_block| {
                let root_inode = Self::iget(super_block, InodeNumber::new(EXT4_ROOT_INO))
                    .inspect_err(|error| {
                        error!("KExt4 root inode VFS initialization failed: {error:?}")
                    })?;
                let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
                if !is_read_only {
                    sync::write_lock(ext4_private(super_block)?)
                        .persist_directory_hash_policy()
                        .map_err(into_vfs_err)?;
                }
                Ok(root)
            },
        )
    }

    fn iget(super_block: &Arc<SuperBlock>, number: InodeNumber) -> VfsResult<Arc<VfsInode>> {
        let ext4 = ext4_private(super_block)?;
        super_block.get_or_try_init_inode(u64::from(number.get()), || {
            let private = sync::read_lock(ext4)
                .load_inode_private(number)
                .map_err(into_vfs_err)?;
            Self::new_vfs_inode(super_block, ext4, private)
        })
    }

    fn iget_from_private(
        super_block: &Arc<SuperBlock>,
        private: Ext4Inode,
    ) -> VfsResult<Arc<VfsInode>> {
        let number = private.number();
        let ext4 = ext4_private(super_block)?;
        super_block.get_or_try_init_inode(u64::from(number.get()), || {
            Self::new_vfs_inode(super_block, ext4, private)
        })
    }

    fn new_vfs_inode(
        super_block: &Arc<SuperBlock>,
        ext4: &SharedExt4SbInfo,
        private: Ext4Inode,
    ) -> VfsResult<Arc<VfsInode>> {
        let node_type = inode_kind_to_vfs(private.kind());
        let flags = node_flags_from_inode(&private);
        let metadata = metadata_from_inode(&private, super_block.block_size());
        let init = VfsInodeInit::new(
            u64::from(private.number().get()),
            metadata.size,
            metadata.mode,
        )
        .with_owner_links_and_rdev(metadata.uid, metadata.gid, metadata.nlink, metadata.rdev)
        .with_generation(private.generation())
        .with_stat_data(
            metadata.block_size,
            metadata.blocks,
            metadata.atime,
            metadata.mtime,
            metadata.ctime,
        );
        let cached_link = if node_type == NodeType::Symlink {
            match sync::read_lock(ext4).fast_symlink_target(&private) {
                Ok(Some(target)) => core::str::from_utf8(&target).ok().map(String::from),
                _ => None,
            }
        } else {
            None
        };
        let inode = VfsInode::new_with_inode_attribute_operations(
            Arc::new(private),
            flags,
            &EXT4_ADDRESS_SPACE_OPERATIONS,
            init,
        );
        if let Some(target) = cached_link {
            inode.set_cached_link(target);
        }
        Ok(inode)
    }

    fn max_file_size(ext4: &SharedExt4SbInfo, super_block: &SuperBlock, inode: &Ext4Inode) -> u64 {
        if inode.has_extents() {
            super_block.max_file_size()
        } else {
            sync::read_lock(ext4).bitmap_max_file_size()
        }
    }
}

impl SuperBlockOperations for Ext4SuperOperations {
    fn timestamp_limits(&self, super_block: &SuperBlock) -> kvfs::TimestampLimits {
        let ext4 = ext4_private(super_block)
            .expect("ext4 fill_super must install private state before timestamp capabilities");
        sync::read_lock(ext4).timestamp_limits()
    }

    fn statfs(&self, super_block: &SuperBlock) -> VfsResult<StatFs> {
        let ext4 = ext4_private(super_block)?;
        let stat = sync::read_lock(ext4).statfs().map_err(into_vfs_err)?;
        Ok(StatFs {
            fs_type: 0xef53,
            block_size: stat.block_size,
            blocks: stat.blocks,
            blocks_free: stat.blocks_free,
            blocks_available: stat.blocks_available,
            file_count: stat.files,
            free_file_count: stat.files_free,
            name_length: stat.max_name_len,
            fragment_size: stat.fragment_size,
        })
    }

    fn sync_fs(&self, super_block: &SuperBlock) -> VfsResult<()> {
        sync::write_lock(ext4_private(super_block)?)
            .sync_filesystem()
            .map_err(into_vfs_err)
    }

    fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        default_evict_inode(inode)?;
        let private = inode.private::<Ext4Inode>()?;
        let super_block = inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        sync::read_lock(ext4)
            .release_all_delalloc(private)
            .map_err(into_vfs_err)?;
        if inode.link_count() != 0 {
            return Ok(());
        }
        let timestamp = current_ext4_timestamp(inode);
        sync::write_lock(ext4)
            .eviction_prepare(private)
            .map_err(into_vfs_err)?;
        loop {
            let (_, is_done) = sync::write_lock(ext4)
                .eviction_release_batch(private, EVICTION_BATCH_BLOCKS)
                .map_err(into_vfs_err)?;
            if is_done {
                break;
            }
        }
        sync::write_lock(ext4)
            .eviction_finish(private, timestamp)
            .map_err(into_vfs_err)
    }
}

impl InodeAttributeOperations for Ext4Inode {
    fn fill_metadata(&self, inode_number: u64, block_size: u64) -> Metadata {
        debug_assert_eq!(inode_number, u64::from(self.number().get()));
        metadata_from_inode(self, block_size)
    }

    fn mode(&self) -> Umode {
        Umode::from_bits(self.mode())
    }

    fn owner(&self) -> (u32, u32) {
        self.owner()
    }

    fn link_count(&self) -> u64 {
        u64::from(self.links_count())
    }

    fn generation(&self) -> u32 {
        self.generation()
    }

    fn rdev(&self) -> DeviceId {
        self.device_id()
            .map_or(DeviceId::default(), ext4_device_id_to_vfs)
    }

    fn size(&self) -> u64 {
        self.size()
    }

    fn blocks(&self) -> u64 {
        self.blocks()
    }

    fn set_permission(&self, permission: NodePermission) {
        self.set_permission(permission.bits());
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.set_owner(uid, gid);
    }

    fn set_link_count(&self, link_count: u64) {
        self.set_links_count(link_count);
    }

    fn increment_link_count(&self) {
        self.increment_links_count();
    }

    fn decrement_link_count(&self) {
        self.decrement_links_count();
    }

    fn set_size(&self, size: u64) {
        self.set_size(size);
    }

    fn set_accessed_at(&self, value: SystemTime) {
        self.set_atime(system_time_to_ext4(value));
    }

    fn set_modified_at(&self, value: SystemTime) {
        self.set_mtime(system_time_to_ext4(value));
    }

    fn set_changed_at(&self, value: SystemTime) {
        self.set_ctime(system_time_to_ext4(value));
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.set_allocated_bytes(bytes);
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        self.add_allocated_bytes(bytes);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        self.subtract_allocated_bytes(bytes);
    }
}

fn logical_block_range(pos: u64, len: usize, block_size: u64) -> VfsResult<Option<(u64, u64)>> {
    let Some((first, last)) = logical_block_bounds(pos, len, block_size)? else {
        return Ok(None);
    };
    let block_count = last
        .checked_sub(first)
        .and_then(|count| count.checked_add(1))
        .ok_or(VfsError::InvalidInput)?;
    Ok(Some((first, block_count)))
}

fn logical_block_bounds(pos: u64, len: usize, block_size: u64) -> VfsResult<Option<(u64, u64)>> {
    if len == 0 {
        return Ok(None);
    }
    let len = u64::try_from(len).map_err(|_| VfsError::InvalidInput)?;
    let end = pos.checked_add(len).ok_or(VfsError::InvalidInput)?;
    let first = pos / block_size;
    let last = (end - 1) / block_size;
    Ok(Some((first, last)))
}

fn first_logical_block_after_len(len: u64, block_size: u64) -> u64 {
    if len == 0 {
        0
    } else {
        ((len - 1) / block_size) + 1
    }
}

fn mapping_block_count(mapping: BlockMapping) -> VfsResult<u64> {
    let count = match mapping {
        BlockMapping::Hole { len, .. }
        | BlockMapping::Mapped { len, .. }
        | BlockMapping::Unwritten { len, .. } => u64::from(len.get()),
    };
    if count == 0 {
        Err(VfsError::InvalidData)
    } else {
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingExtent {
    logical: u64,
    physical: u64,
    length: u64,
    flags: FiemapExtentFlags,
}

impl PendingExtent {
    fn from_blocks(
        logical_block: u64,
        block_count: u64,
        physical_block: u64,
        block_size: u64,
        flags: FiemapExtentFlags,
    ) -> VfsResult<Self> {
        let logical = logical_block
            .checked_mul(block_size)
            .ok_or(VfsError::InvalidInput)?;
        let physical = physical_block
            .checked_mul(block_size)
            .ok_or(VfsError::InvalidInput)?;
        let length = block_count
            .checked_mul(block_size)
            .ok_or(VfsError::InvalidInput)?;
        if length == 0 {
            return Err(VfsError::InvalidData);
        }
        Ok(Self {
            logical,
            physical,
            length,
            flags,
        })
    }

    fn with_added_flags(mut self, flags: FiemapExtentFlags) -> Self {
        self.flags.insert(flags);
        self
    }
}

fn try_merge_extent(left: PendingExtent, right: PendingExtent) -> VfsResult<Option<PendingExtent>> {
    if !left.flags.contains(FiemapExtentFlags::MERGED)
        || left.flags != right.flags
        || left.logical.checked_add(left.length) != Some(right.logical)
        || left.physical.checked_add(left.length) != Some(right.physical)
    {
        return Ok(None);
    }
    let length = left
        .length
        .checked_add(right.length)
        .ok_or(VfsError::InvalidInput)?;
    Ok(Some(PendingExtent { length, ..left }))
}

fn queue_extent(
    pending: &mut Option<PendingExtent>,
    extent: PendingExtent,
    info: &mut FiemapExtentInfo<'_>,
) -> VfsResult<bool> {
    let Some(previous) = pending.take() else {
        *pending = Some(extent);
        return Ok(true);
    };
    if let Some(merged) = try_merge_extent(previous, extent)? {
        *pending = Some(merged);
        return Ok(true);
    }
    if !info.fill_next_extent(
        previous.logical,
        previous.physical,
        previous.length,
        previous.flags,
    )? {
        // A full sink is a partial mapping result, not proof that this is the
        // final extent in the requested range, so LAST must remain clear.
        return Ok(false);
    }
    *pending = Some(extent);
    Ok(true)
}

struct ExtentWalkQuery {
    start_block: u64,
    end_block: u64,
    block_size: u64,
}

fn walk_file_extents(
    query: ExtentWalkQuery,
    mut map_blocks: impl FnMut(u64) -> VfsResult<BlockMapping>,
    info: &mut FiemapExtentInfo<'_>,
) -> VfsResult<()> {
    let mut logical = query.start_block;
    let mut pending = None;
    while logical < query.end_block {
        let mapping = map_blocks(logical)?;
        let mapping_end = logical
            .checked_add(mapping_block_count(mapping)?)
            .ok_or(VfsError::InvalidInput)?
            .min(query.end_block);
        if mapping_end <= logical {
            return Err(VfsError::InvalidData);
        }

        let extent = match mapping {
            BlockMapping::Hole { flags, .. } => {
                if flags.contains(BlockMappingFlags::DELAYED) {
                    PendingExtent::from_blocks(
                        logical,
                        mapping_end - logical,
                        0,
                        query.block_size,
                        FiemapExtentFlags::DELALLOC,
                    )?
                } else {
                    logical = mapping_end;
                    continue;
                }
            }
            BlockMapping::Mapped {
                physical, flags, ..
            } => {
                let flags = if flags.contains(BlockMappingFlags::MERGED) {
                    FiemapExtentFlags::MERGED
                } else {
                    FiemapExtentFlags::empty()
                };
                PendingExtent::from_blocks(
                    logical,
                    mapping_end - logical,
                    physical.get(),
                    query.block_size,
                    flags,
                )?
            }
            BlockMapping::Unwritten {
                physical, flags, ..
            } => {
                let mut extent_flags = FiemapExtentFlags::UNWRITTEN;
                if flags.contains(BlockMappingFlags::MERGED) {
                    extent_flags.insert(FiemapExtentFlags::MERGED);
                }
                PendingExtent::from_blocks(
                    logical,
                    mapping_end - logical,
                    physical.get(),
                    query.block_size,
                    extent_flags,
                )?
            }
        };

        if !queue_extent(&mut pending, extent, info)? {
            return Ok(());
        }
        logical = mapping_end;
    }

    if let Some(last) = pending {
        let last = last.with_added_flags(FiemapExtentFlags::LAST);
        let _ = info.fill_next_extent(last.logical, last.physical, last.length, last.flags)?;
    }
    Ok(())
}

fn supports_rename_flags(flags: RenameFlags) -> bool {
    flags.is_empty() || flags == RenameFlags::NOREPLACE
}

fn ext4_super_block(inode: &VfsInode) -> VfsResult<Arc<SuperBlock>> {
    inode.super_block()
}

fn ensure_same_super_block(left: &Arc<SuperBlock>, right: &Arc<SuperBlock>) -> VfsResult<()> {
    if Arc::ptr_eq(left, right) {
        Ok(())
    } else {
        Err(VfsError::from(LinuxError::EXDEV))
    }
}

fn check_write_limit(
    ext4: &SharedExt4SbInfo,
    super_block: &SuperBlock,
    inode: &Ext4Inode,
    pos: u64,
    count: &mut usize,
) -> VfsResult<()> {
    if *count == 0 {
        return Ok(());
    }
    let max_file_size = Ext4SuperOperations::max_file_size(ext4, super_block, inode);
    if pos >= max_file_size {
        return Err(VfsError::FileTooLarge);
    }
    let remaining = max_file_size - pos;
    let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
    *count = usize::try_from(count_u64.min(remaining)).map_err(|_| VfsError::InvalidInput)?;
    Ok(())
}

fn reserve_delalloc_range(
    ext4: &SharedExt4SbInfo,
    inode: &Ext4Inode,
    pos: u64,
    len: usize,
    block_size: u64,
) -> VfsResult<()> {
    let Some((first, block_count)) = logical_block_range(pos, len, block_size)? else {
        return Ok(());
    };
    sync::read_lock(ext4)
        .reserve_delalloc_range(inode, LogicalBlock::new(first), block_count)
        .map_err(into_vfs_err)
}

fn release_delalloc_range(
    ext4: &SharedExt4SbInfo,
    inode: &Ext4Inode,
    pos: u64,
    len: usize,
    block_size: u64,
) -> VfsResult<()> {
    let Some((first, block_count)) = logical_block_range(pos, len, block_size)? else {
        return Ok(());
    };
    sync::read_lock(ext4)
        .release_delalloc_range(inode, LogicalBlock::new(first), block_count)
        .map_err(into_vfs_err)
}

fn finish_delalloc_write(
    ext4: &SharedExt4SbInfo,
    inode: &Ext4Inode,
    request: WriteEndRequest,
    accepted: usize,
    block_size: u64,
) -> VfsResult<()> {
    if accepted >= request.len() {
        return Ok(());
    }
    let Some((first, requested_blocks)) =
        logical_block_range(request.pos(), request.len(), block_size)?
    else {
        return Ok(());
    };
    let requested_end = first
        .checked_add(requested_blocks)
        .ok_or(VfsError::InvalidInput)?;
    let release_start = if accepted == 0 {
        first
    } else {
        let accepted_end = request
            .pos()
            .checked_add(accepted as u64)
            .ok_or(VfsError::InvalidInput)?;
        first_logical_block_after_len(accepted_end, block_size)
    };
    if release_start >= requested_end {
        return Ok(());
    }
    sync::read_lock(ext4)
        .release_delalloc_range(
            inode,
            LogicalBlock::new(release_start),
            requested_end - release_start,
        )
        .map_err(into_vfs_err)
}

impl InodeOperations for Ext4Inode {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        if self.kind() == InodeKind::Directory {
            Some(self)
        } else {
            None
        }
    }

    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        if self.kind() == InodeKind::Symlink {
            Some(self)
        } else {
            None
        }
    }

    fn fiemap_operations(&self) -> Option<&dyn InodeFiemapOperations> {
        matches!(self.kind(), InodeKind::RegularFile | InodeKind::Directory).then_some(self)
    }

    fn getattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        path: Option<&kvfs::Path>,
        _request_mask: kvfs::GetattrRequestMask,
        _query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        path.map(kvfs::Path::inode)
            .map(|inode| inode.metadata())
            .ok_or(VfsError::InvalidInput)
    }

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        if update.size.is_some() {
            return Err(VfsError::OperationNotSupported);
        }
        let super_block = dentry.super_block().ok_or(VfsError::InvalidInput)?;
        let ext4 = ext4_private(&super_block)?;
        let mut state = sync::write_lock(ext4);

        let ctime = update
            .ctime
            .map(system_time_to_ext4)
            .unwrap_or_else(|| self.ctime());
        let mut metadata = Ext4InodeMetadataUpdate::new(ctime);
        if let Some(mode) = update.mode {
            metadata = metadata.with_mode(mode.bits());
        }
        if let Some((uid, gid)) = update.owner {
            metadata = metadata.with_owner(uid, gid);
        }
        if let Some(atime) = update.atime {
            metadata = metadata.with_atime(system_time_to_ext4(atime));
        }
        if let Some(mtime) = update.mtime {
            metadata = metadata.with_mtime(system_time_to_ext4(mtime));
        }
        state
            .update_inode_metadata(self, metadata)
            .map_err(into_vfs_err)?;
        Ok(applied_metadata_update(update, self.stat()))
    }

    fn get_xattr(
        &self,
        _dentry: &Dentry,
        inode: &VfsInode,
        name: &XattrName,
    ) -> VfsResult<Vec<u8>> {
        let (namespace, suffix) = parse_xattr_name(name)?;
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        sync::read_lock(ext4)
            .get_xattr(self, namespace, suffix)
            .map_err(into_xattr_vfs_err)?
            .ok_or_else(|| VfsError::from(LinuxError::ENODATA))
    }

    fn list_xattrs(
        &self,
        _dentry: &Dentry,
        inode: &VfsInode,
        sink: &mut dyn XattrNameSink,
    ) -> VfsResult<()> {
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        let mut sink = VfsXattrNameSink {
            inner: sink,
            error: None,
        };
        sync::read_lock(ext4)
            .list_xattrs(self, &mut sink)
            .map_err(into_xattr_vfs_err)?;
        sink.finish()
    }

    fn set_xattr(
        &self,
        _dentry: &Dentry,
        inode: &VfsInode,
        name: &XattrName,
        value: &[u8],
        flags: XattrSetFlags,
    ) -> VfsResult<()> {
        let (namespace, suffix) = parse_xattr_name(name)?;
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        let mode = xattr_set_mode(flags);
        sync::write_lock(ext4)
            .set_xattr_with_mode(
                self,
                namespace,
                suffix,
                value,
                mode,
                current_ext4_timestamp(inode),
            )
            .map_err(into_xattr_vfs_err)
    }

    fn remove_xattr(&self, _dentry: &Dentry, inode: &VfsInode, name: &XattrName) -> VfsResult<()> {
        let (namespace, suffix) = parse_xattr_name(name)?;
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        sync::write_lock(ext4)
            .remove_xattr(self, namespace, suffix, current_ext4_timestamp(inode))
            .map_err(into_xattr_vfs_err)
    }
}

impl InodeFiemapOperations for Ext4Inode {
    fn fiemap(
        &self,
        vfs_inode: &VfsInode,
        info: &mut FiemapExtentInfo<'_>,
        start: u64,
        mut length: u64,
    ) -> VfsResult<()> {
        let super_block = ext4_super_block(vfs_inode)?;
        let ext4 = ext4_private(&super_block)?;
        info.prepare(
            vfs_inode,
            start,
            &mut length,
            Ext4SuperOperations::max_file_size(ext4, &super_block, self),
            FiemapFlags::empty(),
        )?;

        let block_size = super_block.block_size();
        let end = start.checked_add(length).ok_or(VfsError::InvalidInput)?;
        let start_block = start / block_size;
        let end_block = end.div_ceil(block_size);
        let _writeback_guard =
            (self.kind() == InodeKind::RegularFile).then(|| self.lock_writeback());
        walk_file_extents(
            ExtentWalkQuery {
                start_block,
                end_block,
                block_size,
            },
            |logical| {
                sync::read_lock(ext4)
                    .report_mapping(self, LogicalBlock::new(logical))
                    .map_err(into_vfs_err)
            },
            info,
        )
    }
}

impl InodeSymlinkOperations for Ext4Inode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        inode: &VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        let mut target = vec![0; usize::try_from(self.size()).map_err(|_| VfsError::InvalidInput)?];
        let read = sync::read_lock(ext4)
            .read_link_at(self, 0, &mut target)
            .map_err(into_vfs_err)?;
        target.truncate(read);
        String::from_utf8(target).map_err(|_| VfsError::InvalidData)
    }
}

impl InodeDirOperations for Ext4Inode {
    fn lookup(
        &self,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let super_block = dir.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let name = dentry.name();
        let entry = match sync::read_lock(ext4)
            .lookup(self, name)
            .map_err(into_vfs_err)?
        {
            Some(entry) => entry,
            None => return Ok(None),
        };
        let inode = Ext4SuperOperations::iget(&super_block, entry.inode())?;
        dentry.instantiate_or_alias(inode)
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let name = dentry.name();
        if mode.node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let ext4 = ext4_private(&super_block)?;
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = {
            let mut state = sync::write_lock(ext4);
            state
                .create_regular_file(
                    self,
                    name.as_bytes(),
                    mode.permission().bits(),
                    uid,
                    gid,
                    current_ext4_timestamp(dir),
                )
                .map_err(into_vfs_err)?
        };
        let inode = Ext4SuperOperations::iget_from_private(&super_block, child)?;
        dentry.instantiate(inode)
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let name = dentry.name();
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = {
            let mut state = sync::write_lock(ext4);
            state
                .create_directory(
                    self,
                    name.as_bytes(),
                    mode.permission().bits(),
                    uid,
                    gid,
                    current_ext4_timestamp(dir),
                )
                .map_err(into_vfs_err)?
        };
        let inode = Ext4SuperOperations::iget_from_private(&super_block, child)?;
        dentry.instantiate(inode)
    }

    fn mknod(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let name = dentry.name();
        let kind = vfs_type_to_inode_kind(mode.node_type()).ok_or(VfsError::InvalidInput)?;
        let device = match mode.node_type() {
            NodeType::CharacterDevice | NodeType::BlockDevice => Some(device_id_to_ext4(device)),
            NodeType::Fifo | NodeType::Socket => None,
            _ => return Err(VfsError::InvalidInput),
        };
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = {
            let mut state = sync::write_lock(ext4);
            state
                .create_special_file(
                    self,
                    name.as_bytes(),
                    (kind, device),
                    mode.permission().bits(),
                    uid,
                    gid,
                    current_ext4_timestamp(dir),
                )
                .map_err(into_vfs_err)?
        };
        let inode = Ext4SuperOperations::iget_from_private(&super_block, child)?;
        dentry.instantiate(inode)
    }

    fn symlink(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let name = dentry.name();
        let (_, uid, gid) = inode_init_owner(
            dir,
            kvfs::Umode::new(
                NodeType::Symlink,
                kvfs::NodePermission::from_bits_truncate(0o777),
            ),
            cred,
        );
        let child = {
            let mut state = sync::write_lock(ext4);
            state
                .create_symlink(
                    self,
                    name.as_bytes(),
                    target.as_bytes(),
                    uid,
                    gid,
                    current_ext4_timestamp(dir),
                )
                .map_err(into_vfs_err)?
        };
        let inode = Ext4SuperOperations::iget_from_private(&super_block, child)?;
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        old_dentry: &Dentry,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let old_super_block = old_dentry.super_block().ok_or(VfsError::InvalidInput)?;
        ensure_same_super_block(&super_block, &old_super_block)?;
        let ext4 = ext4_private(&super_block)?;
        let name = dentry.name();
        let target: Arc<Self> = old_dentry.downcast()?;
        {
            let mut state = sync::write_lock(ext4);
            state
                .link(self, name.as_bytes(), &target, current_ext4_timestamp(dir))
                .map_err(into_vfs_err)?
        }
        let inode = Ext4SuperOperations::iget(&super_block, target.number())?;
        dentry.instantiate(inode)
    }

    fn unlink(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let child_super_block = dentry.super_block().ok_or(VfsError::InvalidInput)?;
        ensure_same_super_block(&super_block, &child_super_block)?;
        let ext4 = ext4_private(&super_block)?;
        let name = dentry.name();
        let child: Arc<Self> = dentry.downcast()?;
        {
            let mut state = sync::write_lock(ext4);
            if dentry.is_dir() {
                state
                    .remove_directory(self, name.as_bytes(), &child, current_ext4_timestamp(dir))
                    .map_err(into_vfs_err)?
            } else {
                state
                    .unlink(self, name.as_bytes(), &child, current_ext4_timestamp(dir))
                    .map_err(into_vfs_err)?
            }
        }
        Ok(())
    }

    fn rename(
        &self,
        _idmap: &kvfs::MountIdmap,
        old_dir: &VfsInode,
        old_dentry: &LockedDentry<'_>,
        new_dir: &VfsInode,
        new_dentry: &LockedDentry<'_>,
        flags: RenameFlags,
    ) -> VfsResult<()> {
        if !supports_rename_flags(flags) {
            return Err(VfsError::OperationNotSupported);
        }
        let old_super_block = old_dir.super_block()?;
        let new_super_block = new_dir.super_block()?;
        ensure_same_super_block(&old_super_block, &new_super_block)?;
        let ext4 = ext4_private(&old_super_block)?;
        let new_parent: Arc<Self> = new_dir.downcast()?;
        let moved: Arc<Self> = old_dentry.downcast()?;
        let replaced: Option<Arc<Self>> = if new_dentry.is_really_positive() {
            Some(new_dentry.downcast()?)
        } else {
            None
        };
        {
            let mut state = sync::write_lock(ext4);
            state
                .rename(
                    self,
                    old_dentry.name().as_bytes(),
                    &moved,
                    &new_parent,
                    new_dentry.name().as_bytes(),
                    replaced.as_deref(),
                    current_ext4_timestamp(old_dir),
                )
                .map_err(into_vfs_err)?
        }
        Ok(())
    }
}

struct Ext4AddressSpaceOperations;

static EXT4_ADDRESS_SPACE_OPERATIONS: Ext4AddressSpaceOperations = Ext4AddressSpaceOperations;

impl AddressSpaceOperations for Ext4AddressSpaceOperations {
    fn read_at(&self, mapping: &AddressSpace, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        match inode.kind() {
            InodeKind::RegularFile => sync::read_lock(ext4)
                .read_at(inode, offset, buf)
                .map_err(into_vfs_err),
            InodeKind::Symlink => sync::read_lock(ext4)
                .read_link_at(inode, offset, buf)
                .map_err(into_vfs_err),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn page_mkwrite(&self, mapping: &AddressSpace, request: PageMkwriteRequest) -> VfsResult<()> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        if inode.kind() != InodeKind::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let mut len = request.len();
        check_write_limit(ext4, &super_block, inode, request.pos(), &mut len)?;
        reserve_delalloc_range(ext4, inode, request.pos(), len, super_block.block_size())
    }

    fn write_begin(&self, mapping: &AddressSpace, request: WriteBeginRequest) -> VfsResult<()> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        if inode.kind() != InodeKind::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let block_size = super_block.block_size();
        reserve_delalloc_range(ext4, inode, request.pos(), request.len(), block_size)
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let block_size = super_block.block_size();
        let accepted = request.copied();
        if accepted != 0 {
            let end = match request.pos().checked_add(accepted as u64) {
                Some(end) => end,
                None => {
                    release_delalloc_range(ext4, inode, request.pos(), request.len(), block_size)?;
                    return Err(VfsError::InvalidInput);
                }
            };
            if let Err(error) = mapping.write_end_set_size(end) {
                release_delalloc_range(ext4, inode, request.pos(), request.len(), block_size)?;
                return Err(error);
            }
        }
        finish_delalloc_write(ext4, inode, request, accepted, block_size)?;
        Ok(accepted)
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        let _writeback_guard = inode.lock_writeback();
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let intent = Ext4SyncIntent::from_data_only(control.is_data_only());
        let timestamp = current_ext4_timestamp(&vfs_inode);

        // A cache miss may enter `read_at()` while the PageCache mapping lock is
        // held. Keep the core mutex out of PageCache traversal so writeback
        // cannot acquire the same locks in the opposite order.
        mapping.writeback_cached_ranges(control, MAX_WRITEBACK_BYTES, |offset, data| {
            // A filesystem-wide sync may reach this inode while another task
            // is still extending it, so sample the visible size per batch.
            let visible_size = vfs_inode.size();
            let disk_size = inode.disk_size();
            let write_end = offset.saturating_add(data.len() as u64);
            let outcome = {
                let mut state = sync::write_lock(ext4);
                match state.writeback_ordered_at(
                    inode,
                    offset,
                    data,
                    visible_size,
                    timestamp,
                    intent,
                ) {
                    Ok(completed_bytes) => WritebackRangeOutcome::complete(completed_bytes),
                    Err(failure) => {
                        error!(
                            "KExt4 inode {} writeback at offset {offset} for {} bytes failed: \
                             completed {} bytes, visible size {visible_size}, disk size \
                             {disk_size}, write end {write_end}: {:?}",
                            inode.number().get(),
                            data.len(),
                            failure.completed_bytes(),
                            failure.error()
                        );
                        WritebackRangeOutcome::failed(
                            failure.completed_bytes(),
                            into_vfs_err(failure.error()),
                        )
                    }
                }
            };
            Ok(outcome)
        })?;
        Ok(())
    }

    fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        if len > Ext4SuperOperations::max_file_size(ext4, &super_block, inode) {
            return Err(VfsError::FileTooLarge);
        }
        let (visible_size, disk_size) = inode.sizes();
        let is_visible_shrink = len < visible_size;
        let is_disk_shrink = len < disk_size;
        {
            let mut state = sync::write_lock(ext4);
            state
                .prepare_regular_inode_truncate(inode, len, current_ext4_timestamp(&vfs_inode))
                .map_err(into_vfs_err)?
        }
        mapping.truncate_setsize(len)?;
        let mut state = sync::write_lock(ext4);
        if is_visible_shrink {
            let first_unneeded = first_logical_block_after_len(len, super_block.block_size());
            state
                .truncate_delalloc_range(inode, LogicalBlock::new(first_unneeded))
                .map_err(into_vfs_err)?;
        }
        if is_disk_shrink {
            state
                .finish_regular_inode_shrink(inode, len)
                .map_err(into_vfs_err)?;
        }
        Ok(())
    }

    fn readahead(&self, mapping: &AddressSpace, control: ReadaheadControl) -> VfsResult<()> {
        if control.count() == 0 {
            return Ok(());
        }

        let offset = control
            .start_index()
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)?;
        let len = control
            .count()
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(VfsError::InvalidInput)?;
        let mut data = vec![0u8; len];
        let read = self.read_at(mapping, &mut data, offset)?;
        let mut copied = 0usize;
        while copied < read {
            let page_index = control.start_index() + (copied / PAGE_SIZE_4K) as u64;
            let step = (read - copied).min(PAGE_SIZE_4K);
            control.complete_folio(page_index, 0, &data[copied..copied + step])?;
            copied += step;
        }
        Ok(())
    }
}

impl FileOperations for Ext4Inode {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        if self.kind() == InodeKind::Directory {
            Some(self)
        } else {
            None
        }
    }

    fn supports_read(&self) -> bool {
        matches!(self.kind(), InodeKind::RegularFile | InodeKind::Directory)
    }

    fn supports_write(&self) -> bool {
        self.kind() == InodeKind::RegularFile
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        match self.kind() {
            InodeKind::RegularFile => iocb.generic_file_read_iter(iter),
            InodeKind::Directory => Err(VfsError::IsADirectory),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        if self.kind() == InodeKind::RegularFile {
            let super_block = ext4_super_block(iocb.file().inode())?;
            let ext4 = ext4_private(&super_block)?;
            iocb.generic_file_write_iter_with_checks(iter, |pos, count| {
                check_write_limit(ext4, &super_block, self, pos, count)
            })
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    fn fsync(&self, file: &VfsFile, data_only: bool) -> VfsResult<()> {
        kvfs::libfs::simple_fsync_noflush(file, data_only)?;
        let super_block = ext4_super_block(file.inode())?;
        let ext4 = ext4_private(&super_block)?;
        sync::write_lock(ext4)
            .sync_inode(self, Ext4SyncIntent::from_data_only(data_only))
            .map_err(into_vfs_err)
    }

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        if self.kind() != InodeKind::RegularFile || !file.mode().contains(FMode::WRITE) {
            return Ok(());
        }
        if inode.write_count() != 1 {
            return Ok(());
        }
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        sync::write_lock(ext4)
            .discard_regular_inode_preallocations(self)
            .inspect_err(|error| {
                error!(
                    "KExt4 inode {} preallocation discard failed: {error:?}",
                    self.number().get()
                );
            })
            .map_err(into_vfs_err)?;
        Ok(())
    }
}

impl FileDirOperations for Ext4Inode {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let super_block = ext4_super_block(file.inode())?;
        let ext4 = ext4_private(&super_block)?;
        let mut sink = KvfsDirSink { ctx, count: 0 };
        sync::read_lock(ext4)
            .read_dir_from(self, Ext4DirPos::new(sink.ctx.pos()), &mut sink)
            .map_err(into_vfs_err)?;
        Ok(sink.count)
    }
}

struct KvfsDirSink<'a, 'b> {
    ctx: &'a mut DirContext<'b>,
    count: usize,
}

impl Ext4DirSink for KvfsDirSink<'_, '_> {
    fn emit(&mut self, entry: Ext4DirEntryRef<'_>, next_pos: Ext4DirPos) -> Ext4Result<bool> {
        let Some(name) = core::str::from_utf8(entry.name_bytes()).ok() else {
            warn!(
                "KExt4 skipping non-UTF-8 directory entry in inode {}",
                entry.inode().get()
            );
            self.ctx.set_pos(next_pos.get());
            return Ok(true);
        };
        let node_type = dir_entry_type_to_vfs(entry.file_type());
        let accepted = self.ctx.emit(
            name,
            u64::from(entry.inode().get()),
            node_type,
            next_pos.get(),
        );
        if accepted {
            self.count += 1;
        }
        Ok(accepted)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec::Vec;

    use kerrno::LinuxError;
    use ktime_types::SystemTime;
    use kvfs::{
        FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags, MetadataUpdate,
        NodePermission, RenameFlags, VfsError, VfsResult, XattrName, XattrSetFlags,
    };
    use unittest::def_test;

    use super::{
        Ext4MountOptions, ExtentWalkQuery, applied_metadata_update, parse_xattr_name,
        supports_rename_flags, walk_file_extents, xattr_set_mode,
    };
    use crate::{
        BlockCount, BlockMapping, BlockMappingFlags, Ext4InodeStat, Ext4Timestamp,
        Ext4XattrNamespace, Ext4XattrSetMode, PhysicalBlock, superblock::Ext4StatFsMode,
    };

    #[def_test]
    fn setattr_result_uses_post_encoding_inode_values() {
        let requested_timestamp = SystemTime::from_unix_parts(2_147_483_648, 123_456_789).unwrap();
        let applied_timestamp = Ext4Timestamp::new(i32::MAX as i64, 0);
        let requested = MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o777)),
            owner: Some((1000, 1001)),
            atime: Some(requested_timestamp),
            mtime: Some(requested_timestamp),
            ctime: Some(requested_timestamp),
            ..Default::default()
        };
        let applied = applied_metadata_update(
            requested,
            Ext4InodeStat {
                mode: 0o100640,
                uid: 2000,
                gid: 2001,
                size: 0,
                blocks: 0,
                rdev: None,
                links_count: 1,
                atime: applied_timestamp,
                ctime: applied_timestamp,
                mtime: applied_timestamp,
            },
        );
        let applied_system_time = SystemTime::from_unix_seconds(i32::MAX as i64);

        assert_eq!(applied.mode.map(|mode| mode.bits()), Some(0o640));
        assert_eq!(applied.owner, Some((2000, 2001)));
        assert_eq!(applied.atime, Some(applied_system_time));
        assert_eq!(applied.mtime, Some(applied_system_time));
        assert_eq!(applied.ctime, Some(applied_system_time));
    }

    #[def_test]
    fn statfs_mount_options_default_to_bsddf() {
        assert_eq!(Ext4MountOptions::parse(None).unwrap().statfs_mode, None);
        assert_eq!(
            Ext4MountOptions::parse(None).unwrap().mount_statfs_mode(),
            Ext4StatFsMode::Bsd
        );
        assert_eq!(
            Ext4MountOptions::parse(Some(b"\0"))
                .unwrap()
                .mount_statfs_mode(),
            Ext4StatFsMode::Bsd
        );
    }

    #[def_test]
    fn last_statfs_mount_option_wins() {
        assert_eq!(
            Ext4MountOptions::parse(Some(b"bsddf,minixdf\0"))
                .unwrap()
                .mount_statfs_mode(),
            Ext4StatFsMode::Minix
        );
        assert_eq!(
            Ext4MountOptions::parse(Some(b"minixdf,,bsddf\0"))
                .unwrap()
                .mount_statfs_mode(),
            Ext4StatFsMode::Bsd
        );
    }

    #[def_test]
    fn linux_default_acl_and_user_xattr_options_are_accepted() {
        for option in [
            &b"acl\0"[..],
            &b"user_xattr\0"[..],
            &b"acl,user_xattr\0"[..],
        ] {
            assert_eq!(
                Ext4MountOptions::parse(Some(option)).unwrap(),
                Ext4MountOptions::default()
            );
        }
        assert_eq!(
            Ext4MountOptions::parse(Some(b"acl,minixdf,user_xattr\0"))
                .unwrap()
                .mount_statfs_mode(),
            Ext4StatFsMode::Minix
        );
    }

    #[def_test]
    fn unsupported_mount_options_are_rejected() {
        for option in [&b"garbage\0"[..], &b"discard\0"[..], &b"nodelalloc\0"[..]] {
            assert_eq!(
                Ext4MountOptions::parse(Some(option)).unwrap_err(),
                VfsError::InvalidInput
            );
        }
    }

    #[def_test]
    fn bytes_after_mount_data_terminator_are_ignored() {
        assert_eq!(
            Ext4MountOptions::parse(Some(b"minixdf\0\xff"))
                .unwrap()
                .mount_statfs_mode(),
            Ext4StatFsMode::Minix
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct CollectedExtent {
        logical: u64,
        physical: u64,
        length: u64,
        flags: FiemapExtentFlags,
    }

    #[derive(Default)]
    struct CollectingWriter {
        extents: Vec<CollectedExtent>,
    }

    impl FiemapExtentWriter for CollectingWriter {
        fn write_extent(
            &mut self,
            index: u32,
            logical: u64,
            physical: u64,
            length: u64,
            flags: FiemapExtentFlags,
        ) -> VfsResult<()> {
            assert_eq!(usize::try_from(index).ok(), Some(self.extents.len()));
            self.extents.push(CollectedExtent {
                logical,
                physical,
                length,
                flags,
            });
            Ok(())
        }
    }

    fn collect_extents(
        query: ExtentWalkQuery,
        capacity: u32,
        map_blocks: impl FnMut(u64) -> VfsResult<BlockMapping>,
    ) -> VfsResult<Vec<CollectedExtent>> {
        let mut writer = CollectingWriter::default();
        {
            let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), capacity, &mut writer);
            walk_file_extents(query, map_blocks, &mut info)?;
        }
        Ok(writer.extents)
    }
    #[def_test]
    fn rename_support_is_limited_to_move_and_noreplace() {
        assert!(supports_rename_flags(RenameFlags::empty()));
        assert!(supports_rename_flags(RenameFlags::NOREPLACE));
        assert!(!supports_rename_flags(RenameFlags::EXCHANGE));
        assert!(!supports_rename_flags(RenameFlags::WHITEOUT));
    }

    #[def_test]
    fn fiemap_reports_mapped_and_unwritten_extents_but_skips_holes() {
        let extents = collect_extents(
            ExtentWalkQuery {
                start_block: 0,
                end_block: 6,
                block_size: 4096,
            },
            u32::MAX,
            |logical| match logical {
                0 => Ok(BlockMapping::Mapped {
                    physical: PhysicalBlock::new(10),
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::empty(),
                }),
                2 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::empty(),
                }),
                4 => Ok(BlockMapping::Unwritten {
                    physical: PhysicalBlock::new(20),
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::empty(),
                }),
                _ => unreachable!("the mapping walk should advance by complete runs"),
            },
        )
        .expect("fiemap walk should succeed");

        assert_eq!(extents.len(), 2);
        assert_eq!(extents[0].logical, 0);
        assert_eq!(extents[0].physical, 10 * 4096);
        assert_eq!(extents[0].length, 2 * 4096);
        assert!(extents[0].flags.is_empty());
        assert_eq!(extents[1].logical, 4 * 4096);
        assert!(extents[1].flags.contains(FiemapExtentFlags::UNWRITTEN));
        assert!(extents[1].flags.contains(FiemapExtentFlags::LAST));
    }

    #[def_test]
    fn fiemap_reports_delayed_allocation_with_unknown_location() {
        let extents = collect_extents(
            ExtentWalkQuery {
                start_block: 0,
                end_block: 6,
                block_size: 4096,
            },
            u32::MAX,
            |logical| match logical {
                0 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::empty(),
                }),
                2 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::DELAYED,
                }),
                4 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::empty(),
                }),
                _ => unreachable!("the mapping walk should advance by complete runs"),
            },
        )
        .expect("delayed mapping walk should succeed");

        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].logical, 2 * 4096);
        assert_eq!(extents[0].length, 2 * 4096);
        assert!(
            extents[0]
                .flags
                .contains(FiemapExtentFlags::DELALLOC | FiemapExtentFlags::UNKNOWN)
        );
        assert!(extents[0].flags.contains(FiemapExtentFlags::LAST));
    }

    #[def_test]
    fn fiemap_reports_separate_delayed_runs() {
        let extents = collect_extents(
            ExtentWalkQuery {
                start_block: 0,
                end_block: 4,
                block_size: 4096,
            },
            u32::MAX,
            |logical| match logical {
                0 | 2 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::empty(),
                }),
                1 | 3 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::DELAYED,
                }),
                _ => unreachable!("the mapping walk should advance by complete runs"),
            },
        )
        .expect("delayed mappings inside one hole should succeed");

        assert_eq!(extents.len(), 2);
        assert_eq!(extents[0].logical, 4096);
        assert_eq!(extents[1].logical, 3 * 4096);
    }

    #[def_test]
    fn fiemap_does_not_mark_a_full_partial_result_as_last() {
        let extents = collect_extents(
            ExtentWalkQuery {
                start_block: 0,
                end_block: 3,
                block_size: 4096,
            },
            1,
            |logical| match logical {
                0 => Ok(BlockMapping::Mapped {
                    physical: PhysicalBlock::new(10),
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::empty(),
                }),
                1 => Ok(BlockMapping::Hole {
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::empty(),
                }),
                2 => Ok(BlockMapping::Mapped {
                    physical: PhysicalBlock::new(20),
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::empty(),
                }),
                _ => unreachable!("the mapping walk should stop at the query end"),
            },
        )
        .expect("bounded fiemap walk should succeed");

        assert_eq!(extents.len(), 1);
        assert!(!extents[0].flags.contains(FiemapExtentFlags::LAST));
    }

    #[def_test]
    fn fiemap_merges_contiguous_legacy_block_runs() {
        let extents = collect_extents(
            ExtentWalkQuery {
                start_block: 0,
                end_block: 4,
                block_size: 4096,
            },
            u32::MAX,
            |logical| match logical {
                0 => Ok(BlockMapping::Mapped {
                    physical: PhysicalBlock::new(10),
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::MERGED,
                }),
                2 => Ok(BlockMapping::Mapped {
                    physical: PhysicalBlock::new(12),
                    len: BlockCount::new(2),
                    flags: BlockMappingFlags::MERGED,
                }),
                _ => unreachable!("the mapping walk should advance by complete runs"),
            },
        )
        .expect("legacy fiemap walk should succeed");

        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].length, 4 * 4096);
        assert!(
            extents[0]
                .flags
                .contains(FiemapExtentFlags::MERGED | FiemapExtentFlags::LAST)
        );
    }

    #[def_test]
    fn fiemap_clips_mapping_runs_to_the_query_end() {
        let extents = collect_extents(
            ExtentWalkQuery {
                start_block: 0,
                end_block: 2,
                block_size: 4096,
            },
            u32::MAX,
            |_| {
                Ok(BlockMapping::Mapped {
                    physical: PhysicalBlock::new(10),
                    len: BlockCount::new(8),
                    flags: BlockMappingFlags::empty(),
                })
            },
        )
        .expect("bounded fiemap walk should succeed");

        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].length, 2 * 4096);
        assert!(extents[0].flags.contains(FiemapExtentFlags::LAST));
    }

    #[def_test]
    fn xattr_name_mapping_accepts_public_ext4_namespaces() {
        for (name, namespace) in [
            (&b"user.key"[..], Ext4XattrNamespace::User),
            (&b"trusted.key"[..], Ext4XattrNamespace::Trusted),
            (&b"security.key"[..], Ext4XattrNamespace::Security),
        ] {
            let name = XattrName::new(name.to_vec()).unwrap();
            assert_eq!(parse_xattr_name(&name), Ok((namespace, &b"key"[..])));
        }

        for name in [&b"user."[..], &b"trusted."[..], &b"security."[..]] {
            let name = XattrName::new(name.to_vec()).unwrap();
            assert!(matches!(
                parse_xattr_name(&name),
                Err(err) if LinuxError::from(err) == LinuxError::EINVAL
            ));
        }

        let acl = XattrName::new(b"system.posix_acl_access".to_vec()).unwrap();
        assert!(matches!(
            parse_xattr_name(&acl),
            Err(err) if LinuxError::from(err) == LinuxError::EOPNOTSUPP
        ));
    }

    #[def_test]
    fn xattr_set_flags_preserve_all_four_combinations() {
        assert_eq!(
            xattr_set_mode(XattrSetFlags::empty()),
            Ext4XattrSetMode::CreateOrReplace
        );
        assert_eq!(
            xattr_set_mode(XattrSetFlags::CREATE),
            Ext4XattrSetMode::Create
        );
        assert_eq!(
            xattr_set_mode(XattrSetFlags::REPLACE),
            Ext4XattrSetMode::Replace
        );
        assert_eq!(
            xattr_set_mode(XattrSetFlags::CREATE | XattrSetFlags::REPLACE),
            Ext4XattrSetMode::CreateAndReplace
        );
    }
}
