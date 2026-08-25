// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FAT directory inode implementation.
use alloc::{string::String, sync::Arc};

use ktime_types::SystemTime;
use kvfs::{
    Dentry, DeviceId, DirContext, DirEntrySink, FileDirOperations, FileOperations,
    InodeDirOperations, InodeOperations, LockedDentry, Metadata, MetadataUpdate, NodePermission,
    NodeType, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
};

use super::{
    FsRef, ff,
    file::FatFileInode,
    fs::FatFilesystem,
    util::{file_metadata, into_vfs_err},
};

/// FAT directory inode.
pub(crate) struct FatDirInode {
    fs: Arc<FatFilesystem>,
    pub(crate) inner: FsRef<ff::Dir<'static>>,
    inode: u64,
}

impl FatDirInode {
    /// Construct a directory inode from a FAT directory handle.
    pub(crate) fn new(
        fs: Arc<FatFilesystem>,
        owner: &super::fs::FatFilesystem,
        dir: ff::Dir,
        inode: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            // SAFETY: The FAT handle is only accessed while holding the
            // matching filesystem lock, which outlives this node wrapper.
            inner: unsafe { FsRef::from_dir_handle(owner, dir) },
            inode,
        })
    }

    fn create_inode(
        &self,
        entry: ff::DirEntry<'_>,
        inode_number: u64,
        block_size: u64,
    ) -> Arc<VfsInode> {
        if entry.is_file() {
            let mut file = entry.to_file();
            let init = VfsInodeInit::from_metadata(&file_metadata(
                block_size,
                inode_number,
                &mut file,
                NodeType::RegularFile,
            ));
            VfsInode::new_file(
                FatFileInode::new(self.fs.clone(), self.fs.as_ref(), file, inode_number),
                init,
            )
        } else {
            VfsInode::new_openable_dir(
                FatDirInode::new(
                    self.fs.clone(),
                    self.fs.as_ref(),
                    entry.to_dir(),
                    inode_number,
                ),
                dir_init(inode_number, block_size),
            )
        }
    }

    fn create_entry(
        &self,
        parent: &Dentry,
        entry: ff::DirEntry<'_>,
        name: String,
        inode_number: u64,
        block_size: u64,
    ) -> Dentry {
        let inode = self.create_inode(entry, inode_number, block_size);
        if inode.is_dir() {
            Dentry::new_dir_from_inode(inode, Some(parent.clone()), name)
        } else {
            Dentry::new_file_from_inode(inode, Some(parent.clone()), name)
        }
    }

    fn read_dir_at(
        &self,
        parent: &kvfs::Path,
        offset: u64,
        sink: &mut dyn DirEntrySink,
    ) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        let block_size = fs.inner.cluster_size() as u64;
        let dir = self.inner.borrow(&fs);

        let mut count = 0;
        for entry in dir.iter().skip(offset as usize) {
            let entry = entry.map_err(into_vfs_err)?;
            let name = entry.file_name().to_ascii_lowercase();
            let node_type = if entry.is_file() {
                NodeType::RegularFile
            } else {
                NodeType::Directory
            };
            let inode = parent.cached_child_inode_or_insert_with(&name, |parent, name| {
                let inode = fs.alloc_inode();
                self.create_entry(parent, entry, name, inode, block_size)
            });
            if !sink.accept(&name, inode, node_type, offset + count + 1) {
                break;
            }
            count += 1;
        }
        Ok(count as usize)
    }
}

impl InodeOperations for FatDirInode {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        Some(self)
    }

    fn getattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _path: Option<&kvfs::Path>,
        _request_mask: kvfs::GetattrRequestMask,
        _query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let fs = self.fs.lock();

        let block_size = fs.inner.cluster_size() as u64;
        Ok(Metadata {
            inode: self.inode,
            device: 0,
            nlink: 1,
            mode: kvfs::Umode::new(NodeType::Directory, NodePermission::default()),
            uid: 0,
            gid: 0,
            size: block_size,
            block_size,
            blocks: 1,
            rdev: DeviceId::default(),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        })
    }

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &Dentry,
        _update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        // TODO: update metadata on directory
        Ok(MetadataUpdate::default())
    }
}

impl InodeDirOperations for FatDirInode {
    fn lookup(
        &self,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let name = dentry.name();
        let mut fs = self.fs.lock();
        let block_size = fs.inner.cluster_size() as u64;
        let dir = self.inner.borrow(&fs);
        let entry = dir
            .iter()
            .find_map(|entry| {
                entry
                    .ok()
                    .filter(|it| it.file_name().eq_ignore_ascii_case(name))
            })
            .map(|entry| {
                let inode = fs.alloc_inode();
                self.create_inode(entry, inode, block_size)
            });
        let Some(entry) = entry else {
            return Ok(None);
        };
        drop(fs);
        dentry.instantiate_or_alias(entry)
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
        _cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let mut fs = self.fs.lock();
        let dir = self.inner.borrow(&fs);
        match mode.node_type() {
            NodeType::RegularFile => {
                let mut file = dir.create_file(name).map_err(into_vfs_err)?;
                let inode_number = fs.alloc_inode();
                let block_size = fs.inner.cluster_size() as u64;
                let init = VfsInodeInit::from_metadata(&file_metadata(
                    block_size,
                    inode_number,
                    &mut file,
                    NodeType::RegularFile,
                ));
                let vfs_inode = VfsInode::new_file(
                    FatFileInode::new(self.fs.clone(), self.fs.as_ref(), file, inode_number),
                    init,
                );
                drop(fs);
                dentry.instantiate(vfs_inode)
            }
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        _mode: kvfs::Umode,
        _cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let mut fs = self.fs.lock();
        let dir = self.inner.borrow(&fs);
        let child = dir.create_dir(name).map_err(into_vfs_err)?;
        let inode_number = fs.alloc_inode();
        let block_size = fs.inner.cluster_size() as u64;
        let vfs_inode = VfsInode::new_openable_dir(
            FatDirInode::new(self.fs.clone(), self.fs.as_ref(), child, inode_number),
            dir_init(inode_number, block_size),
        );
        drop(fs);
        dentry.instantiate(vfs_inode)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &kvfs::VfsInode,
        _new_dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        //  EPERM  The filesystem containing oldpath and newpath does not
        //         support the creation of hard links.
        Err(VfsError::PermissionDenied)
    }

    fn unlink(&self, _dir: &kvfs::VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let name = dentry.name();
        let fs = self.fs.lock();
        let dir = self.inner.borrow(&fs);
        dir.remove(name).map_err(into_vfs_err)
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
        let src_name = old_dentry.name();
        let dst_dir = new_dentry.parent().ok_or(VfsError::InvalidInput)?;
        let dst_name = new_dentry.name();
        let fs = self.fs.lock();
        let dst_dir: Arc<Self> = dst_dir.downcast().map_err(|_| VfsError::InvalidInput)?;

        let dir = self.inner.borrow(&fs);

        // The default implementation throws EEXIST if dst exists, so we need to
        // dispatch_irq it
        match dst_dir.inner.borrow(&fs).remove(dst_name) {
            Ok(_) => {}
            Err(fatfs::Error::NotFound) => {}
            Err(err) => return Err(into_vfs_err(err)),
        }

        dir.rename(src_name, dst_dir.inner.borrow(&fs), dst_name)
            .map_err(into_vfs_err)
    }
}

fn dir_init(inode: u64, block_size: u64) -> VfsInodeInit {
    VfsInodeInit::new(
        inode,
        block_size,
        kvfs::Umode::new(NodeType::Directory, NodePermission::default()),
    )
    .with_owner_links_and_rdev(0, 0, 1, Default::default())
    .with_stat_data(
        block_size,
        1,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
    )
}

impl FileOperations for FatDirInode {
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

impl FileDirOperations for FatDirInode {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let start = ctx.pos();
        self.read_dir_at(file.path(), start, ctx)
    }
}

impl Drop for FatDirInode {
    fn drop(&mut self) {
        self.fs.lock().release_inode(self.inode);
    }
}
