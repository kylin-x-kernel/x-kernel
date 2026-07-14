// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ext4 inode operations.
use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
};

use iov_iter::{IovIterDest, IovIterSource};
use kvfs::{
    AddressSpace, AddressSpaceOperations, Dentry, DeviceId, DirContext, FileDirOperations,
    FileOperations, InodeDirOperations, InodeOperations, InodeSymlinkOperations, Kiocb,
    LockedDentry, Metadata, MetadataUpdate, NodeType, ReadaheadControl, VfsError, VfsFile,
    VfsResult, WriteBeginRequest, WriteEndRequest, WritebackControl,
};
use rsext4::{BLOCK_SIZE, Jbd2Dev};

use super::{
    Ext4Disk, Ext4Filesystem,
    util::{
        dir_entry_type_to_vfs, inode_rdev, into_vfs_err, set_inode_rdev, vfs_type_to_dir_entry,
    },
};

/// Ext4 inode wrapper used to implement VFS nodes.
pub(crate) struct Inode {
    fs: Arc<Ext4Filesystem>,
    ino: u32,
    node_type: NodeType,
}

impl Inode {
    /// Create a new inode wrapper.
    pub(crate) fn new(fs: Arc<Ext4Filesystem>, ino: u32, node_type: NodeType) -> Arc<Self> {
        Arc::new(Self { fs, ino, node_type })
    }

    fn create_entry(
        &self,
        parent: &Dentry,
        ino: u32,
        inode: &rsext4::disknode::Ext4Inode,
        name: impl Into<String>,
    ) -> Dentry {
        let name = name.into();
        let inode = Ext4Filesystem::iget_from_disk_inode(&self.fs, ino, inode);
        if inode.is_dir() {
            Dentry::new_dir_from_inode(inode, Some(parent.clone()), name)
        } else {
            Dentry::new_file_from_inode(inode, Some(parent.clone()), name)
        }
    }

    fn lookup_locked(&self, parent: &Dentry, name: &str) -> VfsResult<Dentry> {
        let parent_ino: u32 = parent
            .inode()
            .try_into()
            .map_err(|_| VfsError::InvalidInput)?;
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        let (ino, inode) = rsext4::dir::get_inode_by_name(fs, dev, parent_ino, name)
            .map_err(into_vfs_err)?
            .ok_or(VfsError::NotFound)?;
        Ok(self.create_entry(parent, ino, &inode, name))
    }

    fn update_ctime_with(
        fs: &mut rsext4::Ext4FileSystem,
        dev: &mut Jbd2Dev<Ext4Disk>,
        ino: u32,
    ) -> VfsResult<()> {
        fs.modify_inode(dev, ino, |inode| {
            #[cfg(feature = "times")]
            {
                inode.i_ctime = khal::time::wall_time().as_secs() as u32;
            }
        })
        .map_err(into_vfs_err)
    }
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
        _path: Option<&kvfs::Path>,
        _request_mask: kvfs::GetattrRequestMask,
        _query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        let inode = fs.get_inode_by_num(dev, self.ino).map_err(into_vfs_err)?;
        Ok(Metadata {
            inode: self.ino as _,
            device: 0,
            nlink: inode.i_links_count as _,
            mode: kvfs::Umode::from_bits(inode.i_mode),
            uid: inode.uid(),
            gid: inode.gid(),
            size: inode.size(),
            block_size: fs.superblock.block_size(),
            blocks: inode.blocks_count(),
            rdev: inode_rdev(&inode),
            atime: core::time::Duration::from_secs(inode.i_atime as u64),
            mtime: core::time::Duration::from_secs(inode.i_mtime as u64),
            ctime: core::time::Duration::from_secs(inode.i_ctime as u64),
        })
    }

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<()> {
        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            fs.modify_inode(dev, self.ino, |inode| {
                if let Some(mode) = update.mode {
                    inode.i_mode = kvfs::Umode::from_bits(inode.i_mode)
                        .with_permission(mode)
                        .bits();
                }
                if let Some((uid, gid)) = update.owner {
                    inode.i_uid = (uid & 0xffff) as u16;
                    inode.l_i_uid_high = ((uid >> 16) & 0xffff) as u16;
                    inode.i_gid = (gid & 0xffff) as u16;
                    inode.l_i_gid_high = ((gid >> 16) & 0xffff) as u16;
                }
                if let Some(atime) = update.atime {
                    inode.i_atime = atime.as_secs() as u32;
                }
                if let Some(mtime) = update.mtime {
                    inode.i_mtime = mtime.as_secs() as u32;
                }
                #[cfg(feature = "times")]
                {
                    inode.i_ctime = khal::time::wall_time().as_secs() as u32;
                }
            })
            .map_err(into_vfs_err)?;
        }
        Ok(())
    }
}

impl InodeSymlinkOperations for Inode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &kvfs::VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        let mut inode = fs.get_inode_by_num(dev, self.ino).map_err(into_vfs_err)?;
        if !inode.is_symlink() {
            return Err(VfsError::InvalidData);
        }
        let target =
            rsext4::file::read_symlink_target(dev, fs, &mut inode).map_err(into_vfs_err)?;
        String::from_utf8(target).map_err(|_| VfsError::InvalidData)
    }
}

impl InodeDirOperations for Inode {
    fn lookup(
        &self,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        if name == "." {
            return Ok(parent.clone());
        }
        if name == ".." {
            return parent.parent().ok_or(VfsError::NotFound);
        }
        self.lookup_locked(&parent, name)
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        if mode.node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let dir_path = parent.absolute_path()?.to_string();
        let path = join_child_path(&dir_path, name);
        let (ino, inode) = {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            if rsext4::dir::get_inode_with_num(fs, dev, &path)
                .map_err(into_vfs_err)?
                .is_some()
            {
                return Err(VfsError::AlreadyExists);
            }

            let inode_mode = mode.bits();
            let (ino, _) = rsext4::file::mkfile_with_ino(
                dev,
                fs,
                &path,
                None,
                Some(rsext4::entries::Ext4DirEntry2::EXT4_FT_REG_FILE),
                Some(inode_mode),
            )
            .ok_or(VfsError::InvalidInput)?;
            Self::update_ctime_with(fs, dev, ino)?;
            let inode = fs.get_inode_by_num(dev, ino).map_err(into_vfs_err)?;
            (ino, inode)
        };

        let inode = Ext4Filesystem::iget_from_disk_inode(&self.fs, ino, &inode);
        Ok(Dentry::new_file_from_inode(
            inode,
            Some(parent.clone()),
            name.to_owned(),
        ))
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        if mode.node_type() != NodeType::Directory {
            return Err(VfsError::InvalidInput);
        }
        let dir_path = parent.absolute_path()?.to_string();
        let path = join_child_path(&dir_path, name);
        let (ino, inode) = {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            if rsext4::dir::get_inode_with_num(fs, dev, &path)
                .map_err(into_vfs_err)?
                .is_some()
            {
                return Err(VfsError::AlreadyExists);
            }

            let (ino, _) =
                rsext4::dir::mkdir_with_ino(dev, fs, &path).ok_or(VfsError::InvalidInput)?;
            fs.modify_inode(dev, ino, |node| {
                node.i_mode = mode.bits();
            })
            .map_err(into_vfs_err)?;
            Self::update_ctime_with(fs, dev, ino)?;
            let inode = fs.get_inode_by_num(dev, ino).map_err(into_vfs_err)?;
            (ino, inode)
        };

        let inode = Ext4Filesystem::iget_from_disk_inode(&self.fs, ino, &inode);
        Ok(Dentry::new_dir_from_inode(
            inode,
            Some(parent.clone()),
            name.to_owned(),
        ))
    }

    fn mknod(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let node_type = mode.node_type();
        if matches!(
            node_type,
            NodeType::Directory | NodeType::RegularFile | NodeType::Symlink | NodeType::Unknown
        ) {
            return Err(VfsError::InvalidInput);
        }
        let dir_path = parent.absolute_path()?.to_string();
        let path = join_child_path(&dir_path, name);
        let (ino, inode) = {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            if rsext4::dir::get_inode_with_num(fs, dev, &path)
                .map_err(into_vfs_err)?
                .is_some()
            {
                return Err(VfsError::AlreadyExists);
            }

            let file_type = vfs_type_to_dir_entry(node_type).ok_or(VfsError::InvalidInput)?;
            let inode_mode = mode.bits();
            let (ino, _) = rsext4::file::mkfile_with_ino(
                dev,
                fs,
                &path,
                None,
                Some(file_type),
                Some(inode_mode),
            )
            .ok_or(VfsError::InvalidInput)?;
            fs.modify_inode(dev, ino, |node| {
                node.i_size_lo = 0;
                node.i_size_high = 0;
                node.i_blocks_lo = 0;
                node.l_i_blocks_high = 0;
                node.i_flags &= !rsext4::disknode::Ext4Inode::EXT4_EXTENTS_FL;
                node.i_block = [0; 15];
                if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice) {
                    set_inode_rdev(node, device);
                }
            })
            .map_err(into_vfs_err)?;
            Self::update_ctime_with(fs, dev, ino)?;
            let inode = fs.get_inode_by_num(dev, ino).map_err(into_vfs_err)?;
            (ino, inode)
        };

        let inode = Ext4Filesystem::iget_from_disk_inode(&self.fs, ino, &inode);
        Ok(Dentry::new_file_from_inode(
            inode,
            Some(parent.clone()),
            name.to_owned(),
        ))
    }

    fn symlink(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let dir_path = parent.absolute_path()?.to_string();
        let link_path = join_child_path(&dir_path, name);
        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            rsext4::file::create_symbol_link(dev, fs, target, &link_path).map_err(into_vfs_err)?;
        }
        self.lookup_locked(&parent, name)
    }

    fn link(
        &self,
        node: &Dentry,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<Dentry> {
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let dir_path = parent.absolute_path()?.to_string();
        let link_path = join_child_path(&dir_path, name);
        let target_path = node.absolute_path()?.to_string();
        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();

            if rsext4::dir::get_inode_with_num(fs, dev, &target_path)
                .map_err(into_vfs_err)?
                .is_none()
            {
                return Err(VfsError::NotFound);
            }
            if rsext4::dir::get_inode_with_num(fs, dev, &link_path)
                .map_err(into_vfs_err)?
                .is_some()
            {
                return Err(VfsError::AlreadyExists);
            }

            rsext4::file::link(fs, dev, &link_path, &target_path).map_err(into_vfs_err)?;
            Self::update_ctime_with(fs, dev, node.inode() as u32)?;
        }
        self.lookup_locked(&parent, name)
    }

    fn unlink(&self, _dir: &kvfs::VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let name = dentry.name();
        let parent = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let dir_path = parent.absolute_path()?.to_string();
        let path = join_child_path(&dir_path, name);
        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            let (_, inode) = rsext4::dir::get_inode_with_num(fs, dev, &path)
                .map_err(into_vfs_err)?
                .ok_or(VfsError::NotFound)?;
            if inode.is_dir() {
                rsext4::file::rmdir(fs, dev, &path).map_err(into_vfs_err)?;
            } else {
                rsext4::file::delete_file(fs, dev, &path).map_err(into_vfs_err)?;
            }
        }
        Ok(())
    }

    fn rename(
        &self,
        _idmap: &kvfs::MountIdmap,
        _old_dir: &kvfs::VfsInode,
        old_dentry: &LockedDentry<'_>,
        _new_dir: &kvfs::VfsInode,
        new_dentry: &LockedDentry<'_>,
        flags: kvfs::RenameFlags,
    ) -> VfsResult<()> {
        if flags.contains(kvfs::RenameFlags::EXCHANGE) {
            return Err(VfsError::InvalidInput);
        }
        let dst_dir = new_dentry.parent().ok_or(VfsError::InvalidInput)?;
        let dst_dir: Arc<Self> = dst_dir.downcast().map_err(|_| VfsError::InvalidInput)?;
        let old_name = old_dentry.name();
        let new_name = new_dentry.name();

        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            rsext4::file::rename_child(fs, dev, self.ino, old_name, dst_dir.ino, new_name)
                .map_err(into_vfs_err)?;
        }
        Ok(())
    }
}

impl AddressSpaceOperations for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        rsext4::file::read_file_with_ino(dev, fs, self.ino, offset, buf).map_err(into_vfs_err)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            rsext4::file::write_file_with_ino(dev, fs, self.ino, offset, buf)
                .map_err(into_vfs_err)?;
        }
        Ok(buf.len())
    }

    fn write_begin(&self, _mapping: &AddressSpace, request: WriteBeginRequest) -> VfsResult<()> {
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        rsext4::file::ext4_da_write_begin(dev, fs, self.ino, request.pos(), request.len())
            .map_err(into_vfs_err)
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        rsext4::file::ext4_da_write_end(
            dev,
            fs,
            self.ino,
            request.pos(),
            request.len(),
            request.copied(),
        )
        .map_err(into_vfs_err)?;
        drop(state);

        let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let old_size = inode.size();
        let accepted = request.copied().min(request.len());
        let end = request
            .pos()
            .checked_add(accepted as u64)
            .ok_or(VfsError::InvalidInput)?;
        let new_size = if accepted == 0 {
            old_size
        } else {
            old_size.max(end)
        };
        inode.update_size_after_backing_change(new_size)?;

        Ok(request.copied())
    }

    fn readahead(&self, mapping: &AddressSpace, control: ReadaheadControl) -> VfsResult<()> {
        if control.count() == 0 {
            return Ok(());
        }
        let offset = control
            .start_index()
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(VfsError::InvalidInput)?;
        let len = control
            .count()
            .checked_mul(BLOCK_SIZE)
            .ok_or(VfsError::InvalidInput)?;
        let mut data = vec![0u8; len];
        let read = self.read_at(&mut data, offset)?;
        let mut copied = 0usize;
        while copied < read {
            let page_index = control.start_index() + (copied / BLOCK_SIZE) as u64;
            let page_off = copied % BLOCK_SIZE;
            let step = (read - copied).min(BLOCK_SIZE - page_off);
            mapping.cache_folio_range(page_index, page_off, &data[copied..copied + step])?;
            copied += step;
        }
        Ok(())
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        const MAX_WRITEBACK_BYTES: usize = 128 * 1024;

        mapping.writeback_cached_ranges(control, MAX_WRITEBACK_BYTES, |offset, data| {
            let written = self.write_at(data, offset)?;
            if written == data.len() {
                Ok(())
            } else {
                Err(VfsError::WriteZero)
            }
        })
    }

    fn set_len(&self, _mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        {
            let mut state = self.fs.lock();
            let (fs, dev) = state.split();
            rsext4::file::truncate_with_ino(dev, fs, self.ino, len).map_err(into_vfs_err)?;
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
        kvfs::simple_fsync_noflush(file, data_only)?;
        self.fs.sync_to_disk()
    }

    fn fallocate(&self, file: &VfsFile, mode: u32, offset: u64, len: u64) -> VfsResult<()> {
        const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
        const FALLOC_FL_INSERT_RANGE: u32 = 0x20;

        match mode {
            FALLOC_FL_COLLAPSE_RANGE => {
                let mut state = self.fs.lock();
                let (fs, dev) = state.split();
                rsext4::file::collapse_range_with_ino(dev, fs, self.ino, offset, len)
                    .map_err(into_vfs_err)?;
                let new_size = file.inode().size().saturating_sub(len);
                file.inode().update_size_after_backing_change(new_size)?;
                Ok(())
            }
            FALLOC_FL_INSERT_RANGE => {
                let mut state = self.fs.lock();
                let (fs, dev) = state.split();
                rsext4::file::insert_range_with_ino(dev, fs, self.ino, offset, len)
                    .map_err(into_vfs_err)?;
                let new_size = file
                    .inode()
                    .size()
                    .checked_add(len)
                    .ok_or(VfsError::InvalidInput)?;
                file.inode().update_size_after_backing_change(new_size)?;
                Ok(())
            }
            _ => Err(VfsError::Unsupported),
        }
    }
}

impl FileDirOperations for Inode {
    fn iterate_shared(&self, _file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let mut state = self.fs.lock();
        let (fs, dev) = state.split();
        let mut inode = fs.get_inode_by_num(dev, self.ino).map_err(into_vfs_err)?;

        let blocks = rsext4::loopfile::resolve_inode_block_allextend(fs, dev, &mut inode)
            .map_err(into_vfs_err)?;

        let mut idx = 0u64;
        let mut count = 0usize;
        let offset = ctx.pos();
        for &phys in blocks.values() {
            let cached = fs
                .datablock_cache
                .get_or_load(dev, phys)
                .map_err(into_vfs_err)?;
            let data = &cached.data[..BLOCK_SIZE];
            let iter = rsext4::entries::DirEntryIterator::new(data);
            for (entry, _) in iter {
                if entry.inode == 0 {
                    continue;
                }
                if idx < offset {
                    idx += 1;
                    continue;
                }
                let name = core::str::from_utf8(entry.name)
                    .map_err(|_| VfsError::InvalidData)?
                    .to_owned();
                let node_type = dir_entry_type_to_vfs(entry.file_type);
                idx += 1;
                if !ctx.emit(&name, entry.inode as u64, node_type, idx) {
                    return Ok(count);
                }
                count += 1;
            }
        }

        Ok(count)
    }
}

fn join_child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}
