// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVFS integration for ext4 mount and inode-private objects.

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::ops::Range;

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::LinuxError;
use ktime_types::SystemTime;
use kvfs::{
    AddressSpace, AddressSpaceOperations, BatchWriteRunTail, BatchedWriteOperations, Dentry,
    DeviceId, DirContext, FMode, FallocateFlags, FiemapExtentFlags, FiemapExtentInfo, FiemapFlags,
    FileDirOperations, FileMappingOperations, FileOperations, InodeAttributeOperations,
    InodeDirOperations, InodeFiemapOperations, InodeOperations, InodeSymlinkOperations, InodeTimes,
    InodeUpdateTime, Kiocb, LockedDentry, Metadata, MetadataUpdate, NodePermission, NodeType,
    ReadaheadControl, RenameFlags, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    Umode, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsInodeInit, VfsResult, WriteBeginRequest,
    WriteEndRequest, WriteReservation, WritebackControl, XattrName, XattrNameRef, XattrNameSink,
    XattrSetFlags, default_evict_inode, inode_init_owner,
};
use pagecache::FolioRead;

use crate::{
    BlockMapping, BlockMappingFlags, DirectoryFileType, Ext4DeviceId, Ext4DirEntryRef, Ext4DirPos,
    Ext4DirSink, Ext4Error, Ext4Inode, Ext4InodeMetadataUpdate, Ext4InodeStat, Ext4Result,
    Ext4SyncIntent, Ext4Timestamp, Ext4XattrNameRef, Ext4XattrNameSink, Ext4XattrNamespace,
    Ext4XattrSetMode, InodeKind, InodeNumber, LogicalBlock,
    disk::inode::EXT4_ROOT_INO,
    superblock::{Ext4SbInfo, Ext4StatFsMode},
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
    pub(crate) quota_types: Option<u8>,
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
                b"usrquota" | b"quota" => *options.quota_types.get_or_insert(0) |= 1,
                b"grpquota" => *options.quota_types.get_or_insert(0) |= 2,
                b"prjquota" => *options.quota_types.get_or_insert(0) |= 4,
                b"noquota" => options.quota_types = Some(0),
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

pub(super) fn into_vfs_err(error: Ext4Error) -> VfsError {
    let linux_error = match error {
        Ext4Error::Allocation(error) => error.into(),
        Ext4Error::Quota(error) => error,
        Ext4Error::NoSpace | Ext4Error::InsufficientJournalCredits => LinuxError::ENOSPC,
        Ext4Error::JournalAborted => LinuxError::EROFS,
        Ext4Error::AlreadyExists => LinuxError::EEXIST,
        Ext4Error::NotFound => LinuxError::ENOENT,
        Ext4Error::CrossProject => LinuxError::EXDEV,
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
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Ext4Timestamp constructors normalize \
                  nanoseconds below one billion"
    )]
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

mod mount;
mod quota;
pub(crate) use quota::can_bypass_quota_limits;
mod read;

mod writeback;
pub(crate) use mount::Ext4Mount;

fn ext4_private(super_block: &SuperBlock) -> VfsResult<&Ext4Mount> {
    super_block.private::<Ext4Mount>()
}

/// Static ext4 superblock operation table.
///
/// Mount-private identity lives in the `Ext4Mount` installed as KVFS superblock
/// private data. The operation table remains shared across mounts.
pub(crate) struct Ext4SuperOperations;

pub(crate) static EXT4_SUPER_OPERATIONS: Ext4SuperOperations = Ext4SuperOperations;

impl Ext4SuperOperations {
    /// Fills a newly reserved ext4 superblock from its canonical block device.
    ///
    /// Requires sleepable mount context. The prepared private owner uses NOWAIT
    /// while holding the nascent device reservation, not an ext4 state lock.
    /// Replay determines mount capabilities before private-owner installation.
    /// Orphan cleanup then uses that owner before the root is published.
    /// Owner allocation failure returns NoMemory before orphan cleanup; prior
    /// replay and nested allocations retain their existing contracts.
    pub(crate) fn fill_super(
        super_block: &Arc<SuperBlock>,
        options: Ext4MountOptions,
    ) -> VfsResult<()> {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: get_tree_bdev installs the block \
                      device before invoking fill_super"
        )]
        let device = super_block
            .block_device()
            .expect("get_tree_bdev must set s_bdev before fill_super")
            .clone();
        let statfs_mode = options.mount_statfs_mode();
        let is_read_only = super_block.flags().contains(SuperBlockFlags::RDONLY);
        let mut state = Ext4SbInfo::open(device, true).map_err(into_vfs_err)?;
        state.set_statfs_mode(statfs_mode);
        state.quota_mount_options = options.quota_types.unwrap_or(0);
        if state.quota_mount_options & 4 != 0 && !state.superblock().features().has_project() {
            return Err(VfsError::InvalidInput);
        }
        state.vfs_superblock = Arc::downgrade(super_block);
        if !is_read_only {
            state.ensure_writable().map_err(into_vfs_err)?;
        }
        let replay = state.prepare_recovery().map_err(into_vfs_err)?;
        let block_size = u64::from(state.layout().block_size());
        let max_file_size = state.extent_max_file_size().map_err(into_vfs_err)?;
        let ext4 = Ext4Mount::new(state);
        super_block.initialize_with_private(
            &EXT4_SUPER_OPERATIONS,
            kalloc_types::KBox::new_nowait(ext4)?,
            block_size,
            max_file_size,
            |super_block| {
                let needs_quota_recovery = {
                    let state = ext4_private(super_block)?.read();
                    state.ensure_quota_feature_supported().is_ok()
                        && state.quota_recovery_needed().map_err(into_vfs_err)?
                };
                if !is_read_only || needs_quota_recovery {
                    Self::activate_internal_quotas(super_block)?;
                }
                ext4_private(super_block)?
                    .write(super_block)
                    .finish_recovery(replay)
                    .map_err(into_vfs_err)?;
                if is_read_only {
                    Self::deactivate_internal_quotas(super_block)?;
                    if needs_quota_recovery {
                        ext4_private(super_block)?
                            .write(super_block)
                            .sync_filesystem()
                            .map_err(into_vfs_err)?;
                    }
                }
                let root_inode = Self::iget(super_block, InodeNumber::new(EXT4_ROOT_INO))
                    .inspect_err(|error| {
                        error!("KExt4 root inode VFS initialization failed: {error:?}")
                    })?;
                let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
                if !is_read_only {
                    ext4_private(super_block)?
                        .write(super_block)
                        .persist_directory_hash_policy()
                        .map_err(into_vfs_err)?;
                }
                Ok(root)
            },
        )
    }

    pub(crate) fn deactivate_internal_quotas(super_block: &SuperBlock) -> VfsResult<()> {
        let ext4 = ext4_private(super_block)?;
        // Completion captures can outlive failed writeback. Drain them before
        // quota ownership is removed, without holding mount mutation protection.
        super_block.for_each_inode(|inode| {
            let private = inode.private::<Ext4Inode>()?;
            private.writeback_io().drain();
            private
                .writeback_io()
                .flush_completions()
                .map_err(into_vfs_err)
        })?;
        let mut result = Ok(());
        for kind in kquota::QuotaType::ALL {
            let release = ext4.disable_internal_quota(super_block, kind);
            result = result.and(release.map_err(into_vfs_err));
        }
        result
    }

    pub(crate) fn activate_internal_quotas(super_block: &Arc<SuperBlock>) -> VfsResult<()> {
        let ext4 = ext4_private(super_block)?;
        let mut files = Vec::new();
        {
            let state = ext4.read();
            if !state.has_quota_feature() || state.ensure_quota_feature_supported().is_err() {
                return Ok(());
            }
            state
                .validate_internal_quota_inodes()
                .map_err(into_vfs_err)?;
            for kind in kquota::QuotaType::ALL {
                if super_block.quota_info().info(kind).is_some() {
                    continue;
                }
                let number = state.superblock().quota_inode(kind);
                if number == 0 {
                    continue;
                }
                let number = InodeNumber::new(number);
                if number.get() == EXT4_ROOT_INO
                    || state
                        .superblock()
                        .journal()
                        .inode()
                        .is_some_and(|journal| journal.get() == number.get())
                    || state
                        .superblock()
                        .orphan_file_inode()
                        .is_some_and(|orphan| orphan.get() == number.get())
                    || !state.is_inode_allocated(number).map_err(into_vfs_err)?
                {
                    return Err(VfsError::InvalidData);
                }
                let private = state.special_iget(number).map_err(into_vfs_err)?;
                if private.kind() != InodeKind::RegularFile {
                    return Err(VfsError::InvalidData);
                }
                files.push((kind, private));
            }
        }
        let mut enabled = 0_u8;
        for (kind, private) in files {
            let number = private.number();
            let file = super_block.get_or_try_init_inode(u64::from(number.get()), || {
                Self::new_vfs_inode(super_block, ext4, private)
            })?;
            file.private::<Ext4Inode>()?.set_noquota(true);
            let result = ext4
                .write(super_block)
                .enable_internal_quota(super_block, kind, &file)
                .map_err(into_vfs_err);
            if result.is_err() {
                for loaded in kquota::QuotaType::ALL {
                    if enabled & (1 << loaded as u8) == 0 {
                        continue;
                    }
                    let release = ext4.disable_internal_quota(super_block, loaded);
                    if let Err(error) = release {
                        error!("KExt4 quota activation cleanup failed: {error:?}");
                    }
                }
            }
            result?;
            enabled |= 1 << kind as u8;
        }
        Ok(())
    }

    fn iget(super_block: &Arc<SuperBlock>, number: InodeNumber) -> VfsResult<Arc<VfsInode>> {
        let ext4 = ext4_private(super_block)?;
        if !ext4.read().is_public_inode_number(number) {
            return Err(VfsError::OperationNotSupported);
        }
        super_block.get_or_try_init_inode(u64::from(number.get()), || {
            let private = ext4
                .read()
                .load_inode_private(number)
                .map_err(into_vfs_err)?;
            Self::new_vfs_inode(super_block, ext4, private)
        })
    }

    /// Builds the VFS inode for an already-loaded ext4 inode.
    ///
    /// - `super_block`: mounted ext4 superblock.
    /// - `private`: ext4 inode payload handed to the new VFS inode.
    ///
    /// Returns: the cached VFS inode for `private.number()`, initialized on
    /// first use.
    ///
    /// Errors: inode construction errors propagate unchanged.
    ///
    /// Locking: none; may sleep while the VFS inode is initialized.
    pub(crate) fn iget_from_private(
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
        ext4: &Ext4Mount,
        private: Ext4Inode,
    ) -> VfsResult<Arc<VfsInode>> {
        let node_type = inode_kind_to_vfs(private.kind());
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
            match ext4.read().fast_symlink_target(&private) {
                Ok(Some(target)) => core::str::from_utf8(&target).ok().map(String::from),
                _ => None,
            }
        } else {
            None
        };
        let inode = VfsInode::new_with_inode_attribute_operations(
            Arc::new(private),
            &EXT4_ADDRESS_SPACE_OPERATIONS,
            init,
        );
        if let Some(target) = cached_link {
            inode.set_cached_link(target);
        }
        Ok(inode)
    }

    fn max_file_size(ext4: &Ext4Mount, super_block: &SuperBlock, inode: &Ext4Inode) -> u64 {
        crate::inode::inode_max_file_size(inode.flags(), super_block.max_file_size(), || {
            ext4.bitmap_max_file_size()
        })
    }
}

impl SuperBlockOperations for Ext4SuperOperations {
    fn show_options(&self, super_block: &SuperBlock) -> String {
        let mut options = String::new();
        if let Ok(mount) = ext4_private(super_block) {
            let types = mount.read().quota_mount_options;
            if types & 1 != 0 {
                options.push_str(",usrquota");
            }
            if types & 2 != 0 {
                options.push_str(",grpquota");
            }
            if types & 4 != 0 {
                options.push_str(",prjquota");
            }
        }
        options
    }

    fn quota_operations(&self) -> Option<&dyn kvfs::QuotaOperations> {
        Some(self)
    }

    fn put_super(&self, super_block: &SuperBlock) -> VfsResult<()> {
        let quota_result = Self::deactivate_internal_quotas(super_block);
        let filesystem_result = ext4_private(super_block)?
            .write(super_block)
            .sync_filesystem()
            .map_err(into_vfs_err);
        quota_result.and(filesystem_result)
    }

    fn timestamp_limits(&self, super_block: &SuperBlock) -> kvfs::TimestampLimits {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Successful ext4 fill_super installs \
                      filesystem-private state before publishing timestamp operations"
        )]
        let ext4 = ext4_private(super_block)
            .expect("ext4 fill_super must install private state before timestamp capabilities");
        ext4.timestamp_limits()
    }

    fn statfs(&self, super_block: &SuperBlock, dentry: &kvfs::Dentry) -> VfsResult<StatFs> {
        let ext4 = ext4_private(super_block)?;
        let stat = ext4.read().statfs().map_err(into_vfs_err)?;
        let mut result = StatFs {
            fs_type: 0xef53,
            block_size: stat.block_size,
            blocks: stat.blocks,
            blocks_free: stat.blocks_free,
            blocks_available: stat.blocks_available,
            file_count: stat.files,
            free_file_count: stat.files_free,
            name_length: stat.max_name_len,
            fragment_size: stat.fragment_size,
        };
        let inode = dentry.inode_ref();
        let private = inode.private::<Ext4Inode>()?;
        // Keep the filesystem-wide statistics if the project record is unavailable.
        let _ = quota::apply_project_statfs(super_block, private, &mut result);
        Ok(result)
    }

    fn sync_fs(&self, super_block: &SuperBlock) -> VfsResult<()> {
        let mut state = ext4_private(super_block)?.write(super_block);
        let quota_result = state.sync_quotas(super_block);
        let filesystem_result = state.sync_filesystem().map_err(into_vfs_err);
        quota_result.and(filesystem_result)
    }

    fn write_inode(&self, inode: &VfsInode, _control: &mut WritebackControl) -> VfsResult<()> {
        let private = inode.private::<Ext4Inode>()?;
        let super_block = inode.super_block()?;
        ext4_private(&super_block)?
            .write(&super_block)
            .write_inode_timestamps(private)
            .map_err(into_vfs_err)
    }

    fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        let private = inode.private::<Ext4Inode>()?;
        let result = (|| {
            default_evict_inode(inode)?;
            let _writeback_guard = private.lock_writeback();
            let super_block = inode.super_block()?;
            let ext4 = ext4_private(&super_block)?;
            let state = ext4.read();
            if state.is_quota_inode(private.number()) {
                return Ok(());
            }
            // Drop the spaces on every eviction, link count included: unlink comes
            // through here too, and only this call releases the group twin's strong
            // owner reference. Must run before release_all_delalloc: the latter can
            // fail, and a space must never outlive its owner. Linux:
            // `ext4_discard_preallocations` at its eviction sites.
            //
            // Unconditional on purpose: this is the last guarantee that a twin
            // cannot outlive its owner, and a "no inode-side entry" short-circuit
            // would trust an invariant that drift only logs in release builds.
            state.release_preallocs(private);
            state.release_all_delalloc(private).map_err(into_vfs_err)?;
            drop(state);
            if inode.link_count() != 0 {
                ext4.write(&super_block)
                    .write_inode_timestamps(private)
                    .map_err(into_vfs_err)?;
                return Ok(());
            }
            let timestamp = current_ext4_timestamp(inode);
            ext4.write(&super_block)
                .eviction_prepare(private)
                .map_err(into_vfs_err)?;
            loop {
                let (_, is_done) = ext4
                    .write(&super_block)
                    .eviction_release_batch(private, EVICTION_BATCH_BLOCKS)
                    .map_err(into_vfs_err)?;
                if is_done {
                    break;
                }
            }
            ext4.write(&super_block)
                .eviction_finish(private, timestamp)
                .map_err(into_vfs_err)
        })();
        private.drop_quota_records(None);
        result
    }
}

impl InodeAttributeOperations for Ext4Inode {
    fn flags(&self) -> kvfs::NodeFlags {
        self.node_flags()
    }

    fn fill_metadata(&self, inode_number: u64, block_size: u64) -> Metadata {
        debug_assert_eq!(inode_number, u64::from(self.number().get()));
        metadata_from_inode(self, block_size)
    }

    fn times(&self) -> InodeTimes {
        let stat = self.stat();
        InodeTimes {
            accessed_at: ext4_timestamp_to_system_time(stat.atime),
            modified_at: ext4_timestamp_to_system_time(stat.mtime),
            changed_at: ext4_timestamp_to_system_time(stat.ctime),
        }
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
    ext4: &Ext4Mount,
    super_block: &SuperBlock,
    inode: &Ext4Inode,
    pos: u64,
    count: &mut usize,
) -> VfsResult<()> {
    if *count == 0 {
        return Ok(());
    }
    ext4.read().ensure_writable().map_err(into_vfs_err)?;
    let max_file_size = Ext4SuperOperations::max_file_size(ext4, super_block, inode);
    if pos >= max_file_size {
        return Err(VfsError::FileTooLarge);
    }
    let remaining = max_file_size - pos;
    let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
    *count = usize::try_from(count_u64.min(remaining)).map_err(|_| VfsError::InvalidInput)?;
    Ok(())
}

/// Reserves delayed-allocation space for one buffered write.
///
/// - `ext4`: mount holding the delayed-allocation accounting.
/// - `inode`: regular file receiving the write.
/// - `pos`: file byte offset of the write range.
/// - `len`: byte length of the write range.
/// - `block_size`: filesystem block size in bytes.
///
/// Returns: a reservation listing the half-open byte ranges newly reserved here.
/// Ranges already covered by an existing reservation, or a write proven to fall
/// inside the initialized extent prefix, yield an empty reservation.
///
/// Errors: `ReadOnly` for a read-only mount, quota/ENOSPC admission failures and
/// corruption propagate unchanged; `InvalidInput` on byte/block overflow.
///
/// Locking: takes the mount read guard and the inode-state lock internally.
fn reserve_delalloc_range(
    ext4: &Ext4Mount,
    inode: &Ext4Inode,
    pos: u64,
    len: usize,
    block_size: u64,
) -> VfsResult<WriteReservation> {
    let Some((first, block_count)) = logical_block_range(pos, len, block_size)? else {
        return Ok(WriteReservation::new());
    };
    ext4.ensure_writable().map_err(into_vfs_err)?;
    if inode
        .check_initialized_overwrite(
            LogicalBlock::new(first),
            block_count,
            block_size as u32,
            ext4.has_huge_file(),
        )
        .map_err(into_vfs_err)?
    {
        return Ok(WriteReservation::new());
    }
    // Cache misses still need mapping and accounting validation under mount
    // protection. Scan-to-insert atomicity comes from the inode-state lock held across
    // Ext4Inode::reserve_delalloc_range, so reserves on different inodes never
    // queue behind each other. Holders of the mount write lock (writeback,
    // fsync, set_len) still exclude this shared guard.
    let reserved = ext4
        .read()
        .reserve_delalloc_range_into(inode, LogicalBlock::new(first), block_count)
        .map_err(into_vfs_err)?;
    let mut ranges = Vec::with_capacity(reserved.len());
    for (start, end) in reserved {
        let start_byte = start
            .checked_mul(block_size)
            .ok_or(VfsError::InvalidInput)?;
        let end_byte = end.checked_mul(block_size).ok_or(VfsError::InvalidInput)?;
        ranges.push((start_byte, end_byte));
    }
    Ok(WriteReservation::from_ranges(ranges))
}

/// Releases a buffered write's unaccepted tail: the reservation this write
/// still owns beyond the accepted prefix.
///
/// - `mapping`: address space whose inode owns the reservation; its filesystem
///   context is resolved only when the accepted prefix leaves a tail to settle.
/// - `pos`: write or run start byte offset.
/// - `len`: reserved byte count of the write or run.
/// - `accepted`: bytes accepted (per-folio) or published (batched), at most
///   `len`.
/// - `reservation`: byte ranges the matching admission newly reserved; ranges
///   it does not list belong to earlier writes and stay reserved.
///
/// Errors: accounting-invariant and overflow errors propagate unchanged.
///
/// Locking: takes the mount read guard and the inode-state lock per release.
fn finish_delalloc_write(
    mapping: &AddressSpace,
    pos: u64,
    len: usize,
    accepted: usize,
    reservation: &WriteReservation,
) -> VfsResult<()> {
    if accepted >= len {
        return Ok(());
    }
    let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
    let inode = vfs_inode.private::<Ext4Inode>()?;
    let super_block = vfs_inode.super_block()?;
    let ext4 = ext4_private(&super_block)?;
    let block_size = super_block.block_size();
    let Some((first, requested_blocks)) = logical_block_range(pos, len, block_size)? else {
        return Ok(());
    };
    let requested_end = first
        .checked_add(requested_blocks)
        .ok_or(VfsError::InvalidInput)?;
    let release_start = if accepted == 0 {
        first
    } else {
        let accepted_end = pos + accepted as u64;
        first_logical_block_after_len(accepted_end, block_size)
    };
    if release_start >= requested_end {
        return Ok(());
    }
    for (range_start, range_end) in reservation.ranges() {
        let Some((extent_first, extent_count)) =
            logical_block_range(*range_start, (range_end - range_start) as usize, block_size)?
        else {
            continue;
        };
        let extent_end = extent_first
            .checked_add(extent_count)
            .ok_or(VfsError::InvalidInput)?;
        let owned_start = extent_first.max(release_start);
        let owned_end = extent_end.min(requested_end);
        if owned_start >= owned_end {
            continue;
        }
        ext4.read()
            .release_delalloc_range(
                inode,
                LogicalBlock::new(owned_start),
                owned_end - owned_start,
            )
            .map_err(into_vfs_err)?;
    }
    Ok(())
}

impl InodeOperations for Ext4Inode {
    fn fileattr_get(&self, _inode: &VfsInode) -> VfsResult<kvfs::FileAttr> {
        const VISIBLE_FLAGS: u32 = 0x725b_dfff;
        let mut flags = self.flags() & VISIBLE_FLAGS;
        if self.kind() == InodeKind::RegularFile {
            flags &= !crate::disk::inode::EXT4_PROJINHERIT_FL;
        }
        Ok(kvfs::FileAttr {
            flags,
            project_id: self.project_id(),
            flags_valid: true,
        })
    }

    fn fileattr_set(&self, inode: &VfsInode, attributes: kvfs::FileAttr) -> VfsResult<()> {
        use crate::disk::inode::{EXT4_IMMUTABLE_FL, EXT4_NOATIME_FL, EXT4_PROJINHERIT_FL};

        const VISIBLE_FLAGS: u32 = 0x725b_dfff;
        const MODIFIABLE_FLAGS: u32 = 0x624b_c0ff;
        const SUPPORTED_FLAGS: u32 = EXT4_NOATIME_FL | 0x0000_0040 | EXT4_PROJINHERIT_FL;
        if attributes.flags & !VISIBLE_FLAGS != 0 {
            return Err(VfsError::OperationNotSupported);
        }
        let old = self.flags();
        let requested_flags = attributes.flags & MODIFIABLE_FLAGS;
        let flags = (old & !SUPPORTED_FLAGS) | (attributes.flags & SUPPORTED_FLAGS);
        if self.kind().mask_flags(requested_flags) != requested_flags {
            return Err(VfsError::OperationNotSupported);
        }
        let super_block = inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        if old & requested_flags & EXT4_IMMUTABLE_FL != 0
            && ((old ^ requested_flags) & !EXT4_IMMUTABLE_FL != 0
                || ext4.read().superblock().features().has_project()
                    && self.project_id() != attributes.project_id)
        {
            return Err(VfsError::OperationNotPermitted);
        }
        if (old ^ requested_flags) & MODIFIABLE_FLAGS & !SUPPORTED_FLAGS != 0 {
            return Err(VfsError::OperationNotSupported);
        }
        let mut state = ext4.write(&super_block);
        let timestamp = current_ext4_timestamp(inode);
        state
            .set_inode_user_flags(self, flags, timestamp)
            .map_err(into_vfs_err)?;
        state
            .set_inode_project_id(self, attributes.project_id, timestamp)
            .map_err(into_vfs_err)
    }

    fn update_time(
        &self,
        idmap: &kvfs::MountIdmap,
        dentry: &Dentry,
        inode: &VfsInode,
        timestamp: SystemTime,
        update: InodeUpdateTime,
    ) -> VfsResult<()> {
        if update == InodeUpdateTime::ChangeAndModification {
            let super_block = dentry.super_block().ok_or(VfsError::InvalidInput)?;
            ext4_private(&super_block)?
                .read()
                .ensure_writable()
                .map_err(into_vfs_err)?;
            // The resident state owns both values and their dirty bits, so no
            // VFS snapshot or separate attribute-setter locks are needed.
            if self.update_regular_write_timestamps(system_time_to_ext4(timestamp)) {
                return Ok(());
            }
        }
        kvfs::generic_update_time(self, idmap, dentry, inode, timestamp, update)
    }

    fn set_len(&self, vfs_inode: &VfsInode, len: u64) -> VfsResult<()> {
        let _writeback_guard = self.lock_writeback();
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        if len > Ext4SuperOperations::max_file_size(ext4, &super_block, self) {
            return Err(VfsError::FileTooLarge);
        }
        let (visible_size, disk_size) = self.sizes();
        let is_visible_shrink = len < visible_size;
        let is_disk_shrink = len < disk_size;
        {
            let mut state = ext4.write(&super_block);
            state
                .prepare_regular_inode_truncate(self, len, current_ext4_timestamp(vfs_inode))
                .map_err(into_vfs_err)?
        }
        vfs_inode.truncate_setsize(len)?;
        let mut state = ext4.write(&super_block);
        if is_visible_shrink {
            let first_unneeded = first_logical_block_after_len(len, super_block.block_size());
            state
                .truncate_delalloc_range(self, LogicalBlock::new(first_unneeded))
                .map_err(into_vfs_err)?;
            // A space at or past the new EOF can never be written through
            // again, so clip there; the prefix below it stays usable.
            state.clip_preallocs_from_logical(self, first_unneeded);
        }
        if is_disk_shrink {
            state
                .finish_regular_inode_shrink(self, len)
                .map_err(into_vfs_err)?;
        }
        Ok(())
    }

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
        let mut metadata = path
            .map(kvfs::Path::inode)
            .map(|inode| inode.metadata())
            .ok_or(VfsError::InvalidInput)?;
        if self.kind() == InodeKind::RegularFile {
            metadata.blocks = self
                .blocks_with_delalloc(metadata.block_size)
                .map_err(into_vfs_err)?;
        }
        Ok(metadata)
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
        let mut state = ext4.write(&super_block);

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
        ext4.read()
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
        ext4.read()
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
        ext4.write(&super_block)
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
        ext4.write(&super_block)
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
                ext4.read()
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
        let read = ext4
            .read()
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
        let entry = match ext4.read().lookup(self, name).map_err(into_vfs_err)? {
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
            let mut state = ext4.write(&super_block);
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
            let mut state = ext4.write(&super_block);
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
            let mut state = ext4.write(&super_block);
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
            let mut state = ext4.write(&super_block);
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
            let mut state = ext4.write(&super_block);
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
            let mut state = ext4.write(&super_block);
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
            let mut state = ext4.write(&old_super_block);
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
    fn read_folio(&self, mapping: &AddressSpace, folio: FolioRead) -> VfsResult<()> {
        read::submit_folios(mapping, core::iter::once(folio))
    }

    fn write_begin(
        &self,
        mapping: &AddressSpace,
        request: WriteBeginRequest,
    ) -> VfsResult<WriteReservation> {
        <Self as BatchedWriteOperations>::batch_write_begin(self, mapping, request)
    }

    fn write_end(
        &self,
        mapping: &AddressSpace,
        request: WriteEndRequest,
        reservation: &WriteReservation,
    ) -> VfsResult<usize> {
        let accepted = request.copied();
        let accepted_end = request.pos() + accepted as u64;
        finish_delalloc_write(mapping, request.pos(), request.len(), accepted, reservation)?;
        if accepted != 0 {
            // The accepted prefix may still be dirty if size publication fails.
            mapping.write_end_set_size(accepted_end)?;
        }
        Ok(accepted)
    }

    fn batched_write_ops(&self) -> Option<&'static dyn BatchedWriteOperations> {
        Some(&EXT4_ADDRESS_SPACE_OPERATIONS)
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        writeback::writepages(mapping, control)
    }

    fn readahead(&self, control: &mut ReadaheadControl<'_>) -> VfsResult<()> {
        if control.count() == 0 {
            return Ok(());
        }
        let mapping = control.mapping();
        read::submit_folios(mapping, core::iter::from_fn(|| control.next_folio()))
    }
}

impl BatchedWriteOperations for Ext4AddressSpaceOperations {
    fn batch_write_begin(
        &self,
        mapping: &AddressSpace,
        request: WriteBeginRequest,
    ) -> VfsResult<WriteReservation> {
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let inode = vfs_inode.private::<Ext4Inode>()?;
        // The kind check is lock-free; reservation re-validates the same
        // regular-file capability inside its inode-state critical section, except
        // on the proven-prefix fast path, which can only be published after that
        // check already passed.
        if inode.kind() != InodeKind::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let super_block = vfs_inode.super_block()?;
        let ext4 = ext4_private(&super_block)?;
        let block_size = super_block.block_size();
        reserve_delalloc_range(ext4, inode, request.pos(), request.len(), block_size)
    }

    fn release_write_run_tail(
        &self,
        mapping: &AddressSpace,
        tail: BatchWriteRunTail,
        reservation: &WriteReservation,
    ) -> VfsResult<()> {
        // Published bytes are final; only the reservation tail inside this
        // run's token is released, and no size is published (the run never
        // grows past the visible size).
        finish_delalloc_write(
            mapping,
            tail.pos(),
            tail.len_bytes(),
            tail.published(),
            reservation,
        )
    }
}

impl Ext4Inode {
    /// Linux `ext4_generic_write_checks`: rejects immutable files and
    /// out-of-range positions before the journal check.
    ///
    /// - `inode`: VFS inode carrying immutable flags and node policy.
    /// - `pos`: requested write offset in bytes.
    /// - `count`: requested byte count; zero skips the size check.
    /// - `extent_max_size`: extent-mapped size limit in bytes.
    /// - `bitmap_max_size`: bitmap-mapped size limit in bytes.
    ///
    /// Returns: `true` when a nonempty in-range request may proceed to
    /// admission; `false` for an empty request, which the caller finishes
    /// without journal admission or timestamp publication.
    ///
    /// Errors: `OperationNotPermitted` for an immutable inode; `FileTooLarge`
    /// when `pos` is at or beyond the inode's format-selected size limit.
    ///
    /// Locking: lock-free; reads published flags and caller-supplied limits.
    fn ext4_generic_write_checks(
        &self,
        inode: &VfsInode,
        pos: u64,
        count: usize,
        extent_max_size: u64,
        bitmap_max_size: u64,
    ) -> VfsResult<bool> {
        if inode.flags().contains(kvfs::NodeFlags::IMMUTABLE) {
            return Err(VfsError::OperationNotPermitted);
        }
        if count == 0 {
            return Ok(false);
        }
        let max_size =
            crate::inode::inode_max_file_size(self.flags(), extent_max_size, || bitmap_max_size);
        if pos >= max_size {
            return Err(VfsError::FileTooLarge);
        }
        Ok(true)
    }
}

impl FileOperations for Ext4Inode {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        if file.mode().contains(FMode::WRITE) {
            let super_block = inode.super_block()?;
            let mut state = ext4_private(&super_block)?.write(&super_block);
            state.ensure_writable().map_err(into_vfs_err)?;
            state.initialize_inode_quotas(&self).map_err(into_vfs_err)?;
        }
        Ok(())
    }

    fn mapping_operations(&self) -> Option<&dyn FileMappingOperations> {
        (self.kind() == InodeKind::RegularFile).then_some(self)
    }

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
            iocb.generic_file_write_iter_with_prepare(iter, |iocb, count| {
                let file = iocb.file();
                // Linux `ext4_generic_write_checks`: immutable and size limits
                // precede the journal check; the count clamp and timestamps stay
                // in `prepare_buffered_write`'s inode-state critical section.
                if !self.ext4_generic_write_checks(
                    file.inode(),
                    iocb.ki_pos(),
                    *count,
                    super_block.max_file_size(),
                    ext4.bitmap_max_file_size(),
                )? {
                    return Ok(());
                }
                // A journal abort must be rejected before timestamp publication,
                // so this stays ahead of `prepare_buffered_write` even though the
                // per-page reservation repeats it. Mount writability was
                // established at VFS write admission (`lock_writable_mount` plus
                // `verify_write_area`), so it does not repeat here.
                ext4.ensure_writable().map_err(into_vfs_err)?;
                *count = self
                    .prepare_buffered_write(
                        iocb.ki_pos(),
                        *count,
                        super_block.max_file_size(),
                        ext4.bitmap_max_file_size(),
                        // Match file_update_time/page_mkwrite even without
                        // `times`: VFS owns clock sampling and range clipping.
                        system_time_to_ext4(file.inode().current_time()),
                    )
                    .ok_or(VfsError::FileTooLarge)?;
                Ok(())
            })
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    fn fsync(&self, file: &VfsFile, range: Range<u64>, data_only: bool) -> VfsResult<()> {
        // Each caller waits for its data before selecting a metadata target;
        // only the transaction layer can reuse an earlier commit's progress.
        file.mapping().write_and_wait_range(range)?;
        let super_block = ext4_super_block(file.inode())?;
        let ext4 = ext4_private(&super_block)?;
        let mut state = ext4.write(&super_block);
        if !data_only {
            state.write_inode_timestamps(self).map_err(into_vfs_err)?;
        }
        state
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
        let _writeback_guard = self.lock_writeback();
        let super_block = ext4_super_block(inode)?;
        let ext4 = ext4_private(&super_block)?;
        let mut state = ext4.write(&super_block);
        // Spaces survive the close while delalloc reservations still expect to
        // allocate through them; writeback settlement or eviction reclaims
        // whatever remains. Linux: `ext4_release_file` gates the discard on
        // `i_reserved_data_blocks`.
        if !self.has_delalloc_reservations() {
            state.release_preallocs(self);
        }
        state
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

    /// Declares plain allocation plus `KEEP_SIZE`; `UNSHARE_RANGE` needs
    /// reflink, which ext4 lacks.
    ///
    /// Returns: `KEEP_SIZE`.
    fn supported_fallocate_modes(&self) -> FallocateFlags {
        FallocateFlags::KEEP_SIZE
    }

    /// Preallocates unwritten extents for a regular file.
    ///
    /// - `file`: the open file; its inode supplies the VFS and ext4 state.
    /// - `flags`: must be within `supported_fallocate_modes`.
    /// - `offset`: range start in bytes.
    /// - `len`: range length in bytes; `0` is a no-op.
    ///
    /// Returns: `()`; the visible size is published here when the request
    /// grows the file.
    ///
    /// Errors: `EOPNOTSUPP` for non-regular inodes, `EFBIG` past the
    /// filesystem size bound; core errors propagate unchanged.
    ///
    /// Locking: called with the inode data lock and the address-space
    /// invalidate lock held; takes the ext4 write lock and publishes size
    /// through `truncate_setsize`. May sleep on device I/O.
    fn fallocate(
        &self,
        file: &VfsFile,
        flags: FallocateFlags,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        if self.kind() != InodeKind::RegularFile {
            return Err(VfsError::from(LinuxError::EOPNOTSUPP));
        }
        let keep_size = flags.contains(FallocateFlags::KEEP_SIZE);
        if len == 0 {
            return Ok(());
        }
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        let vfs_inode = file.inode();
        let super_block = ext4_super_block(vfs_inode)?;
        let ext4 = ext4_private(&super_block)?;
        if end > Ext4SuperOperations::max_file_size(ext4, &super_block, self) {
            return Err(VfsError::FileTooLarge);
        }
        let timestamp = current_ext4_timestamp(vfs_inode);
        let mut committed_end = 0u64;
        let result = ext4
            .fallocate_regular_inode_preallocate(
                &super_block,
                self,
                offset,
                len,
                keep_size,
                timestamp,
                &mut committed_end,
            )
            .map_err(into_vfs_err);
        // Publish the committed prefix on success and partial failure so the
        // writeback `visible_size < disk_size` guard stays satisfiable; `0`
        // (KEEP_SIZE / first-batch failure) and the `> size` check skip it.
        if committed_end > self.size() {
            // Dispatch holds the data and invalidate locks, so writeback is
            // excluded while `disk_size` already leads the visible size.
            vfs_inode.truncate_setsize(committed_end)?;
        }
        result
    }
}

impl FileMappingOperations for Ext4Inode {
    fn page_mkwrite(&self, file: &VfsFile, pos: u64, mut len: usize) -> VfsResult<()> {
        let super_block = ext4_super_block(file.inode())?;
        let ext4 = ext4_private(&super_block)?;
        check_write_limit(ext4, &super_block, self, pos, &mut len)?;
        // The fault has no write_end settlement step, so the reservation token
        // is not threaded; the blocks remain reserved until writeback consumes
        // them or a later write cancels them.
        reserve_delalloc_range(ext4, self, pos, len, super_block.block_size()).map(|_| ())
    }
}

impl FileDirOperations for Ext4Inode {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let super_block = ext4_super_block(file.inode())?;
        let ext4 = ext4_private(&super_block)?;
        let mut sink = KvfsDirSink { ctx, count: 0 };
        ext4.read()
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
unittest::path_mod_test!(hook_tests, "../ut-tests/vfs/hook_tests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(read_tests, "../ut-tests/vfs/read_tests.rs");

unittest::path_mod_test!(tests, "../ut-tests/vfs/tests.rs");

unittest::path_mod_test!(writeback_tests, "../ut-tests/vfs/writeback_tests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(quota_tests, "../ut-tests/vfs/quota_tests.rs");
