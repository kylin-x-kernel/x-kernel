// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KExt4 inode operations.

use alloc::{string::String, sync::Arc, vec, vec::Vec};

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::LinuxError;
use kext4::{
    BlockMapping, BlockMappingFlags, Ext4DirEntryRef, Ext4DirPos, Ext4DirSink, Ext4Inode,
    Ext4InodeMetadataUpdate, Ext4SyncIntent, Ext4XattrNameRef, Ext4XattrNameSink,
    Ext4XattrNamespace, Ext4XattrSetMode, LogicalBlock,
};
use ksync::Mutex;
use ktime_types::SystemTime;
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FMode, FiemapExtentFlags,
    FiemapExtentInfo, FiemapFlags, FileDirOperations, FileOperations, InodeAttributeOperations,
    InodeDirOperations, InodeFiemapOperations, InodeOperations, InodeSymlinkOperations, Kiocb,
    LockedDentry, Metadata, MetadataUpdate, NodePermission, NodeType, PageMkwriteRequest,
    ReadaheadControl, RenameFlags, Umode, VfsError, VfsFile, VfsInode, VfsResult,
    WriteBeginRequest, WriteEndRequest, WritebackControl, XattrName, XattrNameRef, XattrNameSink,
    XattrSetFlags, inode_init_owner,
};

use super::{
    fs::Ext4Filesystem,
    util::{
        current_ext4_timestamp, device_id_to_ext4, dir_entry_type_to_vfs, ext4_device_id_to_vfs,
        into_vfs_err, system_time_to_ext4, vfs_type_to_inode_kind,
    },
};

const PAGE_SIZE_4K: usize = 4096;
const MAX_WRITEBACK_BYTES: usize = 128 * 1024;
const XATTR_USER_PREFIX: &[u8] = b"user.";
const XATTR_TRUSTED_PREFIX: &[u8] = b"trusted.";
const XATTR_SECURITY_PREFIX: &[u8] = b"security.";

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
    fn emit(&mut self, name: Ext4XattrNameRef<'_>) -> kext4::Ext4Result<()> {
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

fn into_xattr_vfs_err(err: kext4::Ext4Error) -> VfsError {
    if err == kext4::Ext4Error::NotFound {
        VfsError::from(LinuxError::ENODATA)
    } else {
        into_vfs_err(err)
    }
}

/// VFS inode wrapper for KExt4 nodes.
pub(crate) struct Inode {
    fs: Arc<Ext4Filesystem>,
    core_inode: Ext4Inode,
    node_type: NodeType,
    writeback_lock: Mutex<()>,
}

impl Inode {
    pub(crate) fn new(
        fs: Arc<Ext4Filesystem>,
        core_inode: Ext4Inode,
        node_type: NodeType,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            core_inode,
            node_type,
            writeback_lock: Mutex::new(()),
        })
    }

    pub(crate) fn core_inode(&self) -> &Ext4Inode {
        &self.core_inode
    }

    fn ensure_same_filesystem(&self, other: &Self) -> VfsResult<()> {
        if Arc::ptr_eq(&self.fs, &other.fs) {
            Ok(())
        } else {
            Err(VfsError::Io)
        }
    }

    fn lookup_child(&self, dir: &VfsInode, name: &str) -> VfsResult<Arc<VfsInode>> {
        let super_block = dir.super_block()?;
        let entry = {
            let fs = self.fs.read_lock();
            fs.lookup(&self.core_inode, name).map_err(into_vfs_err)?
        }
        .ok_or(VfsError::NotFound)?;
        Ext4Filesystem::iget(&super_block, &self.fs, entry.inode())
    }

    fn block_size(&self) -> u64 {
        self.fs.block_size()
    }

    fn max_file_size(&self) -> u64 {
        self.fs
            .max_file_size_for_format(self.core_inode.has_extents())
    }

    fn check_write_limit(&self, pos: u64, count: &mut usize) -> VfsResult<()> {
        if *count == 0 {
            return Ok(());
        }
        let max_file_size = self.max_file_size();
        if pos >= max_file_size {
            return Err(VfsError::FileTooLarge);
        }
        let remaining = max_file_size - pos;
        let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
        *count = usize::try_from(count_u64.min(remaining)).map_err(|_| VfsError::InvalidInput)?;
        Ok(())
    }

    fn reserve_delalloc_range(&self, pos: u64, len: usize) -> VfsResult<()> {
        let Some((first, block_count)) = logical_block_range(pos, len, self.block_size())? else {
            return Ok(());
        };
        self.fs
            .lock()
            .reserve_delalloc_range(&self.core_inode, LogicalBlock::new(first), block_count)
            .map_err(into_vfs_err)
    }

    fn release_delalloc_range(&self, pos: u64, len: usize, block_size: u64) -> VfsResult<()> {
        let Some((first, block_count)) = logical_block_range(pos, len, block_size)? else {
            return Ok(());
        };
        self.fs
            .lock()
            .release_delalloc_range(&self.core_inode, LogicalBlock::new(first), block_count)
            .map_err(into_vfs_err)
    }

    fn finish_delalloc_write(&self, request: WriteEndRequest, accepted: usize) -> VfsResult<()> {
        if accepted >= request.len() {
            return Ok(());
        }

        let block_size = self.block_size();
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
        self.fs
            .lock()
            .release_delalloc_range(
                &self.core_inode,
                LogicalBlock::new(release_start),
                requested_end - release_start,
            )
            .map_err(into_vfs_err)
    }
}

impl InodeAttributeOperations for Inode {
    fn fill_metadata(&self, inode_number: u64) -> Metadata {
        debug_assert_eq!(inode_number, u64::from(self.core_inode.number().get()));
        self.fs.metadata_from_core_inode(&self.core_inode)
    }

    fn mode(&self) -> Umode {
        Umode::from_bits(self.core_inode.mode())
    }

    fn owner(&self) -> (u32, u32) {
        self.core_inode.owner()
    }

    fn link_count(&self) -> u64 {
        u64::from(self.core_inode.links_count())
    }

    fn generation(&self) -> u32 {
        self.core_inode.generation()
    }

    fn rdev(&self) -> DeviceId {
        self.core_inode
            .device_id()
            .map_or(DeviceId::default(), ext4_device_id_to_vfs)
    }

    fn size(&self) -> u64 {
        self.core_inode.size()
    }

    fn block_size(&self) -> u64 {
        self.block_size()
    }

    fn blocks(&self) -> u64 {
        self.core_inode.blocks()
    }

    fn set_permission(&self, permission: NodePermission) {
        self.core_inode.set_permission(permission.bits());
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.core_inode.set_owner(uid, gid);
    }

    fn set_link_count(&self, link_count: u64) {
        self.core_inode.set_links_count(link_count);
    }

    fn increment_link_count(&self) {
        self.core_inode.increment_links_count();
    }

    fn decrement_link_count(&self) {
        self.core_inode.decrement_links_count();
    }

    fn set_size(&self, size: u64) {
        self.core_inode.set_size(size);
    }

    fn set_accessed_at(&self, value: SystemTime) {
        self.core_inode.set_atime(system_time_to_ext4(value));
    }

    fn set_modified_at(&self, value: SystemTime) {
        self.core_inode.set_mtime(system_time_to_ext4(value));
    }

    fn set_changed_at(&self, value: SystemTime) {
        self.core_inode.set_ctime(system_time_to_ext4(value));
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.core_inode.set_allocated_bytes(bytes);
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        self.core_inode.add_allocated_bytes(bytes);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        self.core_inode.subtract_allocated_bytes(bytes);
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

impl InodeOperations for Inode {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        if self.node_type == NodeType::Directory {
            Some(self)
        } else {
            None
        }
    }

    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        if self.node_type == NodeType::Symlink {
            Some(self)
        } else {
            None
        }
    }

    fn fiemap_operations(&self) -> Option<&dyn InodeFiemapOperations> {
        matches!(self.node_type, NodeType::RegularFile | NodeType::Directory).then_some(self)
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
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<()> {
        if update.size.is_some() {
            return Err(VfsError::OperationNotSupported);
        }
        let mut fs = self.fs.lock();

        let ctime = update
            .ctime
            .map(system_time_to_ext4)
            .unwrap_or_else(|| self.core_inode.ctime());
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
        fs.update_inode_metadata(&self.core_inode, metadata)
            .map_err(into_vfs_err)?;
        Ok(())
    }

    fn get_xattr(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        name: &XattrName,
    ) -> VfsResult<Vec<u8>> {
        let (namespace, suffix) = parse_xattr_name(name)?;
        let fs = self.fs.read_lock();
        fs.get_xattr(&self.core_inode, namespace, suffix)
            .map_err(into_xattr_vfs_err)?
            .ok_or_else(|| VfsError::from(LinuxError::ENODATA))
    }

    fn list_xattrs(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        sink: &mut dyn XattrNameSink,
    ) -> VfsResult<()> {
        let fs = self.fs.read_lock();
        let mut sink = VfsXattrNameSink {
            inner: sink,
            error: None,
        };
        fs.list_xattrs(&self.core_inode, &mut sink)
            .map_err(into_xattr_vfs_err)?;
        sink.finish()
    }

    fn set_xattr(
        &self,
        _dentry: &Dentry,
        _inode: &VfsInode,
        name: &XattrName,
        value: &[u8],
        flags: XattrSetFlags,
    ) -> VfsResult<()> {
        let (namespace, suffix) = parse_xattr_name(name)?;
        let mut fs = self.fs.lock();
        let mode = xattr_set_mode(flags);
        fs.set_xattr_with_mode(
            &self.core_inode,
            namespace,
            suffix,
            value,
            mode,
            current_ext4_timestamp(),
        )
        .map_err(into_xattr_vfs_err)
    }

    fn remove_xattr(&self, _dentry: &Dentry, _inode: &VfsInode, name: &XattrName) -> VfsResult<()> {
        let (namespace, suffix) = parse_xattr_name(name)?;
        let mut fs = self.fs.lock();
        fs.remove_xattr(
            &self.core_inode,
            namespace,
            suffix,
            current_ext4_timestamp(),
        )
        .map_err(into_xattr_vfs_err)
    }
}

impl InodeFiemapOperations for Inode {
    fn fiemap(
        &self,
        vfs_inode: &VfsInode,
        info: &mut FiemapExtentInfo<'_>,
        start: u64,
        mut length: u64,
    ) -> VfsResult<()> {
        info.prepare(
            vfs_inode,
            start,
            &mut length,
            self.max_file_size(),
            FiemapFlags::empty(),
        )?;

        let block_size = self.block_size();
        let end = start.checked_add(length).ok_or(VfsError::InvalidInput)?;
        let start_block = start / block_size;
        let end_block = end.div_ceil(block_size);
        let _writeback_guard =
            (self.node_type == NodeType::RegularFile).then(|| self.writeback_lock.lock());
        walk_file_extents(
            ExtentWalkQuery {
                start_block,
                end_block,
                block_size,
            },
            |logical| {
                let fs = self.fs.read_lock();
                fs.report_mapping(&self.core_inode, LogicalBlock::new(logical))
                    .map_err(into_vfs_err)
            },
            info,
        )
    }
}

impl InodeSymlinkOperations for Inode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        let fs = self.fs.read_lock();
        let mut target =
            vec![0; usize::try_from(self.core_inode.size()).map_err(|_| VfsError::InvalidInput)?];
        let read = fs
            .read_link_at(&self.core_inode, 0, &mut target)
            .map_err(into_vfs_err)?;
        target.truncate(read);
        String::from_utf8(target).map_err(|_| VfsError::InvalidData)
    }
}

impl InodeDirOperations for Inode {
    fn lookup(
        &self,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let name = dentry.name();
        let inode = match self.lookup_child(dir, name) {
            Ok(inode) => inode,
            Err(err) if err.canonicalize() == VfsError::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
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
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = {
            let mut fs = self.fs.lock();
            fs.create_regular_file(
                &self.core_inode,
                name.as_bytes(),
                mode.permission().bits(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        let inode = Ext4Filesystem::iget_from_core_inode(&super_block, &self.fs, child)?;
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
        let name = dentry.name();
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = {
            let mut fs = self.fs.lock();
            fs.create_directory(
                &self.core_inode,
                name.as_bytes(),
                mode.permission().bits(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        let inode = Ext4Filesystem::iget_from_core_inode(&super_block, &self.fs, child)?;
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
        let name = dentry.name();
        let kind = vfs_type_to_inode_kind(mode.node_type()).ok_or(VfsError::InvalidInput)?;
        let device = match mode.node_type() {
            NodeType::CharacterDevice | NodeType::BlockDevice => Some(device_id_to_ext4(device)),
            NodeType::Fifo | NodeType::Socket => None,
            _ => return Err(VfsError::InvalidInput),
        };
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = {
            let mut fs = self.fs.lock();
            fs.create_special_file(
                &self.core_inode,
                name.as_bytes(),
                (kind, device),
                mode.permission().bits(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        let inode = Ext4Filesystem::iget_from_core_inode(&super_block, &self.fs, child)?;
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
            let mut fs = self.fs.lock();
            fs.create_symlink(
                &self.core_inode,
                name.as_bytes(),
                target.as_bytes(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        let inode = Ext4Filesystem::iget_from_core_inode(&super_block, &self.fs, child)?;
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        old_dentry: &Dentry,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let super_block = dir.super_block()?;
        let name = dentry.name();
        let target: Arc<Self> = old_dentry.downcast()?;
        self.ensure_same_filesystem(&target)?;
        {
            let mut fs = self.fs.lock();
            fs.link(
                &self.core_inode,
                name.as_bytes(),
                &target.core_inode,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        }
        let inode = Ext4Filesystem::iget(&super_block, &self.fs, target.core_inode.number())?;
        dentry.instantiate(inode)
    }

    fn unlink(&self, _dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let name = dentry.name();
        let child: Arc<Self> = dentry.downcast()?;
        self.ensure_same_filesystem(&child)?;
        {
            let mut fs = self.fs.lock();
            if dentry.is_dir() {
                fs.remove_directory(
                    &self.core_inode,
                    name.as_bytes(),
                    &child.core_inode,
                    current_ext4_timestamp(),
                )
                .map_err(into_vfs_err)?
            } else {
                fs.unlink(
                    &self.core_inode,
                    name.as_bytes(),
                    &child.core_inode,
                    current_ext4_timestamp(),
                )
                .map_err(into_vfs_err)?
            }
        }
        Ok(())
    }

    fn rename(
        &self,
        _idmap: &kvfs::MountIdmap,
        _old_dir: &VfsInode,
        old_dentry: &LockedDentry<'_>,
        new_dir: &VfsInode,
        new_dentry: &LockedDentry<'_>,
        flags: RenameFlags,
    ) -> VfsResult<()> {
        if !supports_rename_flags(flags) {
            return Err(VfsError::OperationNotSupported);
        }
        let new_parent: Arc<Self> = new_dir.downcast()?;
        let moved: Arc<Self> = old_dentry.downcast()?;
        let replaced: Option<Arc<Self>> = if new_dentry.is_really_positive() {
            Some(new_dentry.downcast()?)
        } else {
            None
        };
        self.ensure_same_filesystem(&new_parent)?;
        self.ensure_same_filesystem(&moved)?;
        if let Some(replaced) = &replaced {
            self.ensure_same_filesystem(replaced)?;
        }
        {
            let mut fs = self.fs.lock();
            fs.rename(
                &self.core_inode,
                old_dentry.name().as_bytes(),
                &moved.core_inode,
                &new_parent.core_inode,
                new_dentry.name().as_bytes(),
                replaced.as_ref().map(|inode| &inode.core_inode),
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        }
        Ok(())
    }
}

impl AddressSpaceOperations for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let fs = self.fs.read_lock();
        match self.node_type {
            NodeType::RegularFile => fs
                .read_at(&self.core_inode, offset, buf)
                .map_err(into_vfs_err),
            NodeType::Symlink => fs
                .read_link_at(&self.core_inode, offset, buf)
                .map_err(into_vfs_err),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn page_mkwrite(&self, _mapping: &AddressSpace, request: PageMkwriteRequest) -> VfsResult<()> {
        if self.node_type != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let mut len = request.len();
        self.check_write_limit(request.pos(), &mut len)?;
        self.reserve_delalloc_range(request.pos(), len)
    }

    fn write_begin(&self, _mapping: &AddressSpace, request: WriteBeginRequest) -> VfsResult<()> {
        if self.node_type != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        self.reserve_delalloc_range(request.pos(), request.len())
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let accepted = request.copied();
        if accepted != 0 {
            let end = match request.pos().checked_add(accepted as u64) {
                Some(end) => end,
                None => {
                    self.release_delalloc_range(request.pos(), request.len(), self.block_size())?;
                    return Err(VfsError::InvalidInput);
                }
            };
            if let Err(error) = mapping.write_end_set_size(end) {
                self.release_delalloc_range(request.pos(), request.len(), self.block_size())?;
                return Err(error);
            }
        }
        self.finish_delalloc_write(request, accepted)?;
        Ok(accepted)
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        let _writeback_guard = self.writeback_lock.lock();
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let intent = Ext4SyncIntent::from_data_only(control.is_data_only());
        let timestamp = current_ext4_timestamp();

        // A cache miss may enter `read_at()` while the PageCache mapping lock is
        // held. Keep the core mutex out of PageCache traversal so writeback
        // cannot acquire the same locks in the opposite order.
        mapping.writeback_cached_ranges(control, MAX_WRITEBACK_BYTES, |offset, data| {
            // A filesystem-wide sync may reach this inode while another task
            // is still extending it, so sample the visible size per batch.
            let visible_size = vfs_inode.size();
            let disk_size = self.core_inode.disk_size();
            let write_end = offset.saturating_add(data.len() as u64);
            {
                let mut fs = self.fs.lock();
                fs.writeback_ordered_at(
                    &self.core_inode,
                    offset,
                    data,
                    visible_size,
                    timestamp,
                    intent,
                )
                .inspect_err(|error| {
                    error!(
                        "KExt4 inode {} writeback at offset {offset} for {} bytes failed: visible \
                         size {visible_size}, disk size {disk_size}, write end {write_end}: \
                         {error:?}",
                        self.core_inode.number().get(),
                        data.len()
                    );
                })
                .map_err(into_vfs_err)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        if len > self.max_file_size() {
            return Err(VfsError::FileTooLarge);
        }
        let (visible_size, disk_size) = self.core_inode.sizes();
        let is_visible_shrink = len < visible_size;
        let is_disk_shrink = len < disk_size;
        {
            let mut fs = self.fs.lock();
            fs.prepare_regular_inode_truncate(&self.core_inode, len, current_ext4_timestamp())
                .map_err(into_vfs_err)?
        }
        mapping.truncate_setsize(len)?;
        let mut fs = self.fs.lock();
        if is_visible_shrink {
            let first_unneeded = first_logical_block_after_len(len, self.block_size());
            fs.truncate_delalloc_range(&self.core_inode, LogicalBlock::new(first_unneeded))
                .map_err(into_vfs_err)?;
        }
        if is_disk_shrink {
            fs.finish_regular_inode_shrink(&self.core_inode, len)
                .map_err(into_vfs_err)?;
        }
        Ok(())
    }

    fn readahead(&self, _mapping: &AddressSpace, control: ReadaheadControl) -> VfsResult<()> {
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
        let read = self.read_at(&mut data, offset)?;
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

impl FileOperations for Inode {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        if self.node_type == NodeType::Directory {
            Some(self)
        } else {
            None
        }
    }

    fn supports_read(&self) -> bool {
        matches!(self.node_type, NodeType::RegularFile | NodeType::Directory)
    }

    fn supports_write(&self) -> bool {
        self.node_type == NodeType::RegularFile
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        match self.node_type {
            NodeType::RegularFile => iocb.generic_file_read_iter(iter),
            NodeType::Directory => Err(VfsError::IsADirectory),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        if self.node_type == NodeType::RegularFile {
            iocb.generic_file_write_iter_with_checks(iter, |pos, count| {
                self.check_write_limit(pos, count)
            })
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    fn fsync(&self, file: &VfsFile, data_only: bool) -> VfsResult<()> {
        kvfs::libfs::simple_fsync_noflush(file, data_only)?;
        self.fs
            .sync_inode_to_disk(&self.core_inode, Ext4SyncIntent::from_data_only(data_only))
    }

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        if self.node_type != NodeType::RegularFile || !file.mode().contains(FMode::WRITE) {
            return Ok(());
        }
        if inode.write_count() != 1 {
            return Ok(());
        }
        let mut fs = self.fs.lock();
        fs.discard_regular_inode_preallocations(&self.core_inode)
            .inspect_err(|error| {
                error!(
                    "KExt4 inode {} preallocation discard failed: {error:?}",
                    self.core_inode.number().get()
                );
            })
            .map_err(into_vfs_err)?;
        Ok(())
    }
}

impl FileDirOperations for Inode {
    fn iterate_shared(&self, _file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let fs = self.fs.read_lock();
        let mut sink = KvfsDirSink { ctx, count: 0 };
        fs.read_dir_from(&self.core_inode, Ext4DirPos::new(sink.ctx.pos()), &mut sink)
            .map_err(into_vfs_err)?;
        Ok(sink.count)
    }
}

struct KvfsDirSink<'a, 'b> {
    ctx: &'a mut DirContext<'b>,
    count: usize,
}

impl Ext4DirSink for KvfsDirSink<'_, '_> {
    fn emit(
        &mut self,
        entry: Ext4DirEntryRef<'_>,
        next_pos: Ext4DirPos,
    ) -> kext4::Ext4Result<bool> {
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
    use kext4::{
        BlockCount, BlockMapping, BlockMappingFlags, Ext4XattrNamespace, Ext4XattrSetMode,
        PhysicalBlock,
    };
    use kvfs::{
        FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags, RenameFlags,
        VfsResult, XattrName, XattrSetFlags,
    };
    use unittest::def_test;

    use super::{
        ExtentWalkQuery, parse_xattr_name, supports_rename_flags, walk_file_extents, xattr_set_mode,
    };

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
