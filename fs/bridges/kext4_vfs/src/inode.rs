// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KExt4 inode operations.

use alloc::{collections::BTreeSet, string::String, sync::Arc, vec, vec::Vec};

use iov_iter::{IovIterDest, IovIterSource};
use kext4::{
    BlockMapping, Ext4DirEntryRef, Ext4DirPos, Ext4DirSink, Ext4Inode, Ext4InodeMetadataUpdate,
    Ext4SyncIntent, InodeNumber, LogicalBlock,
};
use ksync::Mutex;
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FMode, FileDirOperations,
    FileOperations, InodeDirOperations, InodeOperations, InodeSymlinkOperations, Kiocb,
    LockedDentry, Metadata, MetadataUpdate, NodeType, ReadaheadControl, RenameFlags, VfsError,
    VfsFile, VfsInode, VfsResult, WriteBeginRequest, WriteEndRequest, WritebackControl,
    inode_init_owner,
};

use super::{
    fs::Ext4Filesystem,
    util::{
        current_ext4_timestamp, device_id_to_ext4, dir_entry_type_to_vfs, duration_to_ext4,
        ext4_timestamp_to_duration, into_vfs_err, vfs_type_to_inode_kind,
    },
};

const PAGE_SIZE_4K: usize = 4096;
const MAX_WRITEBACK_BYTES: usize = 128 * 1024;

/// VFS inode wrapper for KExt4 nodes.
pub(crate) struct Inode {
    fs: Arc<Ext4Filesystem>,
    number: InodeNumber,
    node_type: NodeType,
    delayed_blocks: Mutex<BTreeSet<u64>>,
    writeback_lock: Mutex<()>,
}

impl Inode {
    pub(crate) fn new(
        fs: Arc<Ext4Filesystem>,
        number: InodeNumber,
        node_type: NodeType,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            number,
            node_type,
            delayed_blocks: Mutex::new(BTreeSet::new()),
            writeback_lock: Mutex::new(()),
        })
    }

    pub(crate) const fn number(&self) -> InodeNumber {
        self.number
    }

    fn lookup_child(&self, name: &str) -> VfsResult<Arc<VfsInode>> {
        let entry = {
            let fs = self.fs.lock();
            let directory = fs.inode(self.number).map_err(into_vfs_err)?;
            fs.lookup(&directory, name).map_err(into_vfs_err)?
        }
        .ok_or(VfsError::NotFound)?;
        let inode = self.fs.load_inode(entry.inode())?;
        Ext4Filesystem::iget_from_core_inode(&self.fs, inode)
    }

    fn block_size(&self) -> u64 {
        self.fs.block_size()
    }

    fn collect_hole_blocks(&self, pos: u64, len: usize) -> VfsResult<Vec<u64>> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let block_size = self.block_size();
        let (first, last) =
            logical_block_bounds(pos, len, block_size)?.ok_or(VfsError::InvalidInput)?;
        let candidates = {
            let delayed_blocks = self.delayed_blocks.lock();
            (first..=last)
                .filter(|block| !delayed_blocks.contains(block))
                .collect::<Vec<_>>()
        };
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let fs = self.fs.lock();
        let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;
        let mut holes = Vec::new();
        for block in candidates {
            if matches!(
                fs.map_blocks(&inode, LogicalBlock::new(block))
                    .map_err(into_vfs_err)?,
                BlockMapping::Hole { .. }
            ) {
                holes.push(block);
            }
        }
        Ok(holes)
    }

    fn reserve_delalloc_blocks(&self, blocks: Vec<u64>) -> VfsResult<()> {
        if blocks.is_empty() {
            return Ok(());
        }

        let candidates = {
            let delayed_blocks = self.delayed_blocks.lock();
            blocks
                .into_iter()
                .filter(|block| !delayed_blocks.contains(block))
                .collect::<Vec<_>>()
        };
        if candidates.is_empty() {
            return Ok(());
        }

        let candidate_count = candidates.len();
        self.fs.reserve_delalloc_blocks(candidate_count as u64)?;
        let inserted = {
            let mut delayed_blocks = self.delayed_blocks.lock();
            candidates
                .into_iter()
                .filter(|block| delayed_blocks.insert(*block))
                .count()
        };
        self.fs
            .release_delalloc_blocks(candidate_count.saturating_sub(inserted) as u64);
        Ok(())
    }

    fn release_delalloc_blocks(&self, blocks: Vec<u64>) {
        if blocks.is_empty() {
            return;
        }
        let released = {
            let mut delayed_blocks = self.delayed_blocks.lock();
            blocks
                .into_iter()
                .filter(|block| delayed_blocks.remove(block))
                .count()
        };
        self.fs.release_delalloc_blocks(released as u64);
    }

    fn release_delalloc_range(&self, pos: u64, len: usize, block_size: u64) -> VfsResult<()> {
        self.release_delalloc_blocks(logical_blocks_for_range(pos, len, block_size)?);
        Ok(())
    }

    fn release_delalloc_tail(&self, len: u64, block_size: u64) {
        let first_unneeded = first_logical_block_after_len(len, block_size);
        let released = {
            let mut delayed_blocks = self.delayed_blocks.lock();
            let tail = delayed_blocks
                .range(first_unneeded..)
                .copied()
                .collect::<Vec<_>>();
            for block in &tail {
                delayed_blocks.remove(block);
            }
            tail.len()
        };
        self.fs.release_delalloc_blocks(released as u64);
    }

    pub(crate) fn release_delalloc_for_eviction(&self) {
        let released = {
            let mut delayed_blocks = self.delayed_blocks.lock();
            let released = delayed_blocks.len();
            delayed_blocks.clear();
            released
        };
        self.fs.release_delalloc_blocks(released as u64);
    }

    fn finish_delalloc_write(&self, request: WriteEndRequest, accepted: usize) -> VfsResult<()> {
        if accepted >= request.len() {
            return Ok(());
        }

        let block_size = self.block_size();
        let keep = logical_blocks_for_range(request.pos(), accepted, block_size)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let release = logical_blocks_for_range(request.pos(), request.len(), block_size)?
            .into_iter()
            .filter(|block| !keep.contains(block))
            .collect::<Vec<_>>();
        self.release_delalloc_blocks(release);
        Ok(())
    }
}

fn logical_blocks_for_range(pos: u64, len: usize, block_size: u64) -> VfsResult<Vec<u64>> {
    let Some((first, last)) = logical_block_bounds(pos, len, block_size)? else {
        return Ok(Vec::new());
    };
    Ok((first..=last).collect())
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

fn supports_rename_flags(flags: RenameFlags) -> bool {
    flags.is_empty() || flags == RenameFlags::NOREPLACE
}

fn validate_live_inode_number(live_number: u64, core_inode: &Ext4Inode) -> VfsResult<()> {
    if live_number == u64::from(core_inode.number().get()) {
        Ok(())
    } else {
        Err(VfsError::InvalidInput)
    }
}

fn sync_dentry_ctime(dentry: &Dentry, core_inode: &Ext4Inode) -> VfsResult<()> {
    validate_live_inode_number(dentry.inode(), core_inode)?;
    dentry.set_changed_at(ext4_timestamp_to_duration(core_inode.ctime()));
    Ok(())
}

fn sync_dentry_link_state(dentry: &Dentry, core_inode: &Ext4Inode) -> VfsResult<()> {
    validate_live_inode_number(dentry.inode(), core_inode)?;
    dentry.set_link_count(u64::from(core_inode.links_count()));
    dentry.set_changed_at(ext4_timestamp_to_duration(core_inode.ctime()));
    Ok(())
}

fn sync_vfs_inode_writeback_state(vfs_inode: &VfsInode, core_inode: &Ext4Inode) -> VfsResult<()> {
    validate_live_inode_number(vfs_inode.inode(), core_inode)?;
    vfs_inode.set_allocated_bytes(core_inode.blocks().saturating_mul(512));
    vfs_inode.set_modified_at(ext4_timestamp_to_duration(core_inode.mtime()));
    vfs_inode.set_changed_at(ext4_timestamp_to_duration(core_inode.ctime()));
    Ok(())
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
        let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;

        let mut metadata = Ext4InodeMetadataUpdate::new(current_ext4_timestamp());
        if let Some(mode) = update.mode {
            metadata = metadata.with_mode(mode.bits());
        }
        if let Some((uid, gid)) = update.owner {
            metadata = metadata.with_owner(uid, gid);
        }
        if let Some(atime) = update.atime {
            metadata = metadata.with_atime(duration_to_ext4(atime));
        }
        if let Some(mtime) = update.mtime {
            metadata = metadata.with_mtime(duration_to_ext4(mtime));
        }
        fs.update_inode_metadata(&inode, metadata)
            .map_err(into_vfs_err)?;
        Ok(())
    }
}

impl InodeSymlinkOperations for Inode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        let fs = self.fs.lock();
        let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;
        let mut target =
            vec![0; usize::try_from(inode.size()).map_err(|_| VfsError::InvalidInput)?];
        let read = fs
            .read_link_at(&inode, 0, &mut target)
            .map_err(into_vfs_err)?;
        target.truncate(read);
        String::from_utf8(target).map_err(|_| VfsError::InvalidData)
    }
}

impl InodeDirOperations for Inode {
    fn lookup(
        &self,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let name = dentry.name();
        let inode = match self.lookup_child(name) {
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
        let name = dentry.name();
        if mode.node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let created = {
            let mut fs = self.fs.lock();
            let parent_inode = fs.inode(self.number).map_err(into_vfs_err)?;
            fs.create_regular_file(
                &parent_inode,
                name.as_bytes(),
                mode.permission().bits(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        self.fs.sync_vfs_directory(dir, created.parent())?;
        let inode = Ext4Filesystem::iget_from_core_inode(&self.fs, created.child().clone())?;
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
        let name = dentry.name();
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let created = {
            let mut fs = self.fs.lock();
            let parent_inode = fs.inode(self.number).map_err(into_vfs_err)?;
            fs.create_directory(
                &parent_inode,
                name.as_bytes(),
                mode.permission().bits(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        self.fs.sync_vfs_directory(dir, created.parent())?;
        let inode = Ext4Filesystem::iget_from_core_inode(&self.fs, created.child().clone())?;
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
        let name = dentry.name();
        let kind = vfs_type_to_inode_kind(mode.node_type()).ok_or(VfsError::InvalidInput)?;
        let device = match mode.node_type() {
            NodeType::CharacterDevice | NodeType::BlockDevice => Some(device_id_to_ext4(device)),
            NodeType::Fifo | NodeType::Socket => None,
            _ => return Err(VfsError::InvalidInput),
        };
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let created = {
            let mut fs = self.fs.lock();
            let parent_inode = fs.inode(self.number).map_err(into_vfs_err)?;
            fs.create_special_file(
                &parent_inode,
                name.as_bytes(),
                (kind, device),
                mode.permission().bits(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        self.fs.sync_vfs_directory(dir, created.parent())?;
        let inode = Ext4Filesystem::iget_from_core_inode(&self.fs, created.child().clone())?;
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
        let name = dentry.name();
        let (_, uid, gid) = inode_init_owner(
            dir,
            kvfs::Umode::new(
                NodeType::Symlink,
                kvfs::NodePermission::from_bits_truncate(0o777),
            ),
            cred,
        );
        let created = {
            let mut fs = self.fs.lock();
            let parent_inode = fs.inode(self.number).map_err(into_vfs_err)?;
            fs.create_symlink(
                &parent_inode,
                name.as_bytes(),
                target.as_bytes(),
                uid,
                gid,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        self.fs.sync_vfs_directory(dir, created.parent())?;
        let inode = Ext4Filesystem::iget_from_core_inode(&self.fs, created.child().clone())?;
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        old_dentry: &Dentry,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let target: Arc<Self> = old_dentry.downcast()?;
        let linked = {
            let mut fs = self.fs.lock();
            let parent_inode = fs.inode(self.number).map_err(into_vfs_err)?;
            let target_inode = fs.inode(target.number).map_err(into_vfs_err)?;
            fs.link(
                &parent_inode,
                name.as_bytes(),
                &target_inode,
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        self.fs.sync_vfs_directory(dir, linked.parent())?;
        sync_dentry_link_state(old_dentry, linked.target())?;
        let inode = Ext4Filesystem::iget_from_core_inode(&self.fs, linked.target().clone())?;
        dentry.instantiate(inode)
    }

    fn unlink(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let name = dentry.name();
        let removed = {
            let mut fs = self.fs.lock();
            let parent_inode = fs.inode(self.number).map_err(into_vfs_err)?;
            if dentry.is_dir() {
                fs.remove_directory(&parent_inode, name.as_bytes(), current_ext4_timestamp())
                    .map_err(into_vfs_err)?
            } else {
                fs.unlink(&parent_inode, name.as_bytes(), current_ext4_timestamp())
                    .map_err(into_vfs_err)?
            }
        };
        self.fs.sync_vfs_directory(dir, removed.parent())?;
        sync_dentry_link_state(dentry, removed.removed())?;
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
        let new_parent: Arc<Self> = new_dir.downcast()?;
        let renamed = {
            let mut fs = self.fs.lock();
            let source_parent = fs.inode(self.number).map_err(into_vfs_err)?;
            let target_parent = fs.inode(new_parent.number).map_err(into_vfs_err)?;
            fs.rename(
                &source_parent,
                old_dentry.name().as_bytes(),
                &target_parent,
                new_dentry.name().as_bytes(),
                current_ext4_timestamp(),
            )
            .map_err(into_vfs_err)?
        };
        self.fs
            .sync_vfs_directory(old_dir, renamed.source_parent())?;
        self.fs
            .sync_vfs_directory(new_dir, renamed.target_parent())?;
        sync_dentry_ctime(old_dentry, renamed.moved())?;
        if new_dentry.is_really_positive()
            && let Some(core_inode) = renamed.replaced()
        {
            sync_dentry_link_state(new_dentry, core_inode)?;
        }
        Ok(())
    }
}

impl AddressSpaceOperations for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;
        match self.node_type {
            NodeType::RegularFile => fs.read_at(&inode, offset, buf).map_err(into_vfs_err),
            NodeType::Symlink => fs.read_link_at(&inode, offset, buf).map_err(into_vfs_err),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn write_begin(&self, _mapping: &AddressSpace, request: WriteBeginRequest) -> VfsResult<()> {
        if self.node_type != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let holes = self.collect_hole_blocks(request.pos(), request.len())?;
        self.reserve_delalloc_blocks(holes)
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
        let block_size = self.block_size();
        let mut inode = self
            .fs
            .lock()
            .referenced_inode(self.number)
            .map_err(into_vfs_err)?;

        // A cache miss may enter `read_at()` while the PageCache mapping lock is
        // held. Keep the core mutex out of PageCache traversal so writeback
        // cannot acquire the same locks in the opposite order.
        mapping.writeback_cached_ranges(control, MAX_WRITEBACK_BYTES, |offset, data| {
            // A filesystem-wide sync may reach this inode while another task
            // is still extending it, so sample the visible size per batch.
            let visible_size = vfs_inode.size();
            let disk_size = inode.disk_size();
            let write_end = offset.saturating_add(data.len() as u64);
            inode = {
                let mut fs = self.fs.lock();
                fs.writeback_ordered_at(&inode, offset, data, visible_size, timestamp, intent)
                    .inspect_err(|error| {
                        error!(
                            "KExt4 inode {} writeback at offset {offset} for {} bytes failed: \
                             visible size {visible_size}, disk size {disk_size}, write end \
                             {write_end}: {error:?}",
                            self.number.get(),
                            data.len()
                        );
                    })
                    .map_err(into_vfs_err)?
            };
            self.release_delalloc_range(offset, data.len(), block_size)?;
            Ok(())
        })?;
        sync_vfs_inode_writeback_state(&vfs_inode, &inode)
    }

    fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        let block_size = self.block_size();
        let prepared = {
            let mut fs = self.fs.lock();
            let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;
            fs.prepare_regular_inode_truncate(&inode, len, current_ext4_timestamp())
                .map_err(into_vfs_err)?
        };
        mapping.truncate_setsize(len)?;
        let inode = self
            .fs
            .lock()
            .finish_regular_inode_truncate(prepared)
            .map_err(into_vfs_err)?;
        self.release_delalloc_tail(len, block_size);
        let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        self.fs.sync_vfs_inode_attributes(&vfs_inode, &inode)
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
            iocb.generic_file_write_iter(iter)
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    fn fsync(&self, file: &VfsFile, data_only: bool) -> VfsResult<()> {
        kvfs::libfs::simple_fsync_noflush(file, data_only)?;
        self.fs.sync_to_disk()
    }

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        if self.node_type != NodeType::RegularFile || !file.mode().contains(FMode::WRITE) {
            return Ok(());
        }
        if inode.write_count() != 1 {
            return Ok(());
        }
        if !self.delayed_blocks.lock().is_empty() {
            return Ok(());
        }
        let mut fs = self.fs.lock();
        let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;
        fs.discard_regular_inode_preallocations(&inode)
            .inspect_err(|error| {
                error!(
                    "KExt4 inode {} preallocation discard failed: {error:?}",
                    self.number.get()
                );
            })
            .map_err(into_vfs_err)?;
        Ok(())
    }
}

impl FileDirOperations for Inode {
    fn iterate_shared(&self, _file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let inode = fs.referenced_inode(self.number).map_err(into_vfs_err)?;
        let mut sink = KvfsDirSink { ctx, count: 0 };
        fs.read_dir_from(&inode, Ext4DirPos::new(sink.ctx.pos()), &mut sink)
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
    use kvfs::RenameFlags;
    use unittest::def_test;

    use super::supports_rename_flags;

    #[def_test]
    fn rename_support_is_limited_to_move_and_noreplace() {
        assert!(supports_rename_flags(RenameFlags::empty()));
        assert!(supports_rename_flags(RenameFlags::NOREPLACE));
        assert!(!supports_rename_flags(RenameFlags::EXCHANGE));
        assert!(!supports_rename_flags(RenameFlags::WHITEOUT));
    }
}
