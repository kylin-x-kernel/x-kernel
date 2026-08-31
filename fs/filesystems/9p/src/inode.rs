// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete 9P inode and VFS node implementations.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
};

use kcred::Cred;
use ktime_types::SystemTime;
use kvfs::{
    Dentry, DeviceId, DirContext, FileDirOperations, FileOperations, InodeDirOperations,
    InodeOperations, InodeSymlinkOperations, InodeUpdateTime, LockedDentry, Metadata,
    MetadataUpdate, NodeFlags, NodePermission, NodeType, Umode, VfsError, VfsFile, VfsInode,
    VfsInodeInit, VfsResult, inode_init_owner,
};
use p9::FileAttr;

use super::{
    Fs9pFilesystem,
    util::{dotl_decode_dev, dtype_to_vfs, into_vfs_err},
};

pub(crate) fn inode_init_from_attr(attr: &FileAttr) -> VfsInodeInit {
    let block_size = if attr.blksize == 0 {
        4096
    } else {
        attr.blksize
    };
    VfsInodeInit::new(0, attr.size, kvfs::Umode::from_bits(attr.mode as u16))
        .with_owner_links_and_rdev(attr.uid, attr.gid, attr.nlink, dotl_decode_dev(attr.rdev))
        .with_stat_data(
            block_size,
            attr.blocks,
            system_time_from_9p(attr.atime_sec),
            system_time_from_9p(attr.mtime_sec),
            system_time_from_9p(attr.ctime_sec),
        )
}

fn system_time_from_9p(seconds: u64) -> SystemTime {
    SystemTime::from_unix_seconds(i64::try_from(seconds).unwrap_or(i64::MAX))
}

/// A VFS inode backed by a 9P session.
///
/// Unlike ext4, the 9P filesystem is stateless on the client side — each
/// operation sends a message to the server.  We store the filesystem reference
/// (which holds the session) and the path of this node so that we can call
/// path-based session operations when VFS methods are invoked.
///
/// For file I/O (`read_at`, `write_at`, `append`, `set_len`), we open a
/// temporary fid, perform the operation, then close the fid.
pub(crate) struct Inode {
    fs: Arc<Fs9pFilesystem>,
    node_type: NodeType,
    path: Option<String>,
}

impl Inode {
    /// Create a new inode for a directory node.
    pub(crate) fn new_dir(fs: Arc<Fs9pFilesystem>, path: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            fs,
            node_type: NodeType::Directory,
            path,
        })
    }

    /// Create a new inode for a file (or symlink) node.
    pub(crate) fn new_file(
        fs: Arc<Fs9pFilesystem>,
        node_type: NodeType,
        path: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            node_type,
            path,
        })
    }

    fn into_vfs_inode(self: Arc<Self>, flags: NodeFlags, init: VfsInodeInit) -> Arc<VfsInode> {
        let node_type = self.node_type;
        debug_assert_eq!(node_type, init.node_type());
        match node_type {
            NodeType::Directory => VfsInode::new_openable_dir_with_flags(self, flags, init),
            NodeType::CharacterDevice
            | NodeType::BlockDevice
            | NodeType::Fifo
            | NodeType::Socket => VfsInode::new_special(self, flags, init),
            NodeType::RegularFile | NodeType::Symlink | NodeType::Unknown => {
                VfsInode::new_file_with_flags(self, flags, init)
            }
        }
    }

    pub(crate) fn into_dentry(
        self: Arc<Self>,
        init: VfsInodeInit,
        flags: NodeFlags,
        parent: Option<Dentry>,
        name: String,
    ) -> Dentry {
        let is_directory = self.node_type == NodeType::Directory;
        let inode = self.into_vfs_inode(flags, init);
        if is_directory {
            Dentry::new_dir_from_inode(inode, parent, name)
        } else {
            Dentry::new_file_from_inode(inode, parent, name)
        }
    }

    /// Resolve the absolute path of this node.
    fn node_path(&self) -> VfsResult<String> {
        self.path.clone().ok_or(VfsError::InvalidInput)
    }

    /// Resolve the absolute path of this directory node.
    fn dir_path(&self) -> VfsResult<String> {
        self.node_path()
    }

    /// Look up a child entry via the 9P session.
    fn lookup_locked(&self, name: &str) -> VfsResult<Arc<VfsInode>> {
        let child_path = join_child_path(&self.dir_path()?, name);
        let mut session = self.fs.lock();
        let attr = session.getattr(&child_path).map_err(into_vfs_err)?;
        Ok(self.create_inode_from_attr(&attr, &child_path))
    }

    fn create_inode_from_attr(&self, attr: &FileAttr, path: &str) -> Arc<VfsInode> {
        let node_type = kvfs::Umode::from_bits(attr.mode as u16).node_type();
        let node = if node_type == NodeType::Directory {
            Inode::new_dir(self.fs.clone(), Some(path.into()))
        } else {
            Inode::new_file(self.fs.clone(), node_type, Some(path.into()))
        };
        let flags = if node_type == NodeType::RegularFile {
            NodeFlags::NON_CACHEABLE
        } else {
            NodeFlags::empty()
        };
        node.into_vfs_inode(flags, inode_init_from_attr(attr))
    }

    /// Create a Dentry for a symlink, using the 9P `TSYMLINK` operation.
    ///
    /// This is called from KFS path handling for the special symlink path.
    fn create_symlink_inode(&self, name: &str, target: &str, gid: u32) -> VfsResult<Arc<VfsInode>> {
        let dir_path = self.dir_path()?;
        let link_path = join_child_path(&dir_path, name);
        let mut session = self.fs.lock();
        session
            .symlink_with_gid(target, &link_path, gid)
            .map_err(into_vfs_err)?;
        let attr = session.getattr(&link_path).map_err(into_vfs_err)?;
        drop(session);

        Ok(
            Inode::new_file(self.fs.clone(), NodeType::Symlink, Some(link_path))
                .into_vfs_inode(NodeFlags::empty(), inode_init_from_attr(&attr)),
        )
    }

    /// Open a temporary fid for file I/O on this node's path.
    fn open_fid_rdonly(&self) -> VfsResult<u32> {
        let path = self.node_path()?;
        let mut session = self.fs.lock();
        session
            .open_path_with_flags(&path, 0, 0) // OREAD / P9_DOTL_RDONLY
            .map_err(into_vfs_err)
    }

    /// Open a temporary fid for read-write file I/O.
    fn open_fid_rdwr(&self) -> VfsResult<u32> {
        let path = self.node_path()?;
        let mut session = self.fs.lock();
        session
            .open_path_with_flags(&path, 2, 2) // ORDWR / P9_DOTL_RDWR
            .map_err(into_vfs_err)
    }

    fn close_fid(&self, fid: u32) {
        let mut session = self.fs.lock();
        let _ = session.close_fid(fid);
    }

    fn read_regular_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let fid = self.open_fid_rdonly()?;
        let result = {
            let mut session = self.fs.lock();
            session
                .read_fid(fid, offset, buf.len() as u32)
                .map_err(into_vfs_err)
        };
        self.close_fid(fid);

        let data = result?;
        let n = core::cmp::min(data.len(), buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    fn write_regular_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let fid = self.open_fid_rdwr()?;
        let result = {
            let mut session = self.fs.lock();
            session.write_fid(fid, offset, buf).map_err(into_vfs_err)
        };
        self.close_fid(fid);
        result
    }

    fn set_regular_len(&self, len: u64) -> VfsResult<()> {
        let fid = self.open_fid_rdwr()?;
        let result = {
            let mut session = self.fs.lock();
            session.truncate_fid(fid, len).map_err(into_vfs_err)
        };
        self.close_fid(fid);
        result
    }

    fn read_symlink_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let path = self.node_path()?;
        let target = self.fs.lock().read_link(&path).map_err(into_vfs_err)?;
        let target_bytes = target.as_bytes();
        let start = offset as usize;
        if start >= target_bytes.len() {
            return Ok(0);
        }
        let to_read = core::cmp::min(buf.len(), target_bytes.len() - start);
        buf[..to_read].copy_from_slice(&target_bytes[start..start + to_read]);
        Ok(to_read)
    }
}

// ---------------------------------------------------------------------------
// InodeOperations — common operations for all node types
// ---------------------------------------------------------------------------

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
        let path = self.node_path()?;
        let mut session = self.fs.lock();
        let attr = session.getattr(&path).map_err(into_vfs_err)?;
        Ok(Metadata {
            inode: 0,
            device: 0,
            nlink: attr.nlink as _,
            mode: kvfs::Umode::from_bits(attr.mode as u16),
            uid: attr.uid,
            gid: attr.gid,
            size: attr.size,
            block_size: if attr.blksize == 0 {
                4096
            } else {
                attr.blksize
            },
            blocks: attr.blocks,
            rdev: dotl_decode_dev(attr.rdev),
            atime: system_time_from_9p(attr.atime_sec),
            mtime: system_time_from_9p(attr.mtime_sec),
            ctime: system_time_from_9p(attr.ctime_sec),
        })
    }

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        if update.owner.is_some() || update.atime.is_some() || update.mtime.is_some() {
            return Err(VfsError::OperationNotSupported);
        }
        let path = self.node_path()?;
        if let Some(mode) = update.mode {
            let mut session = self.fs.lock();
            session
                .setattr_mode(&path, mode.bits() as u32)
                .map_err(into_vfs_err)?;
        }
        if let Some(size) = update.size {
            self.set_regular_len(size)?;
        }
        Ok(MetadataUpdate {
            size: update.size,
            mode: update.mode,
            ..Default::default()
        })
    }

    fn update_time(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &Dentry,
        _timestamp: SystemTime,
        _update: InodeUpdateTime,
    ) -> VfsResult<MetadataUpdate> {
        // Automatic updates remain best-effort until the session exposes the
        // 9P2000.L timestamp form of TSETATTR. Returning no applied fields
        // prevents KVFS from publishing a value that was never sent.
        Ok(MetadataUpdate::default())
    }
}

impl InodeSymlinkOperations for Inode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &kvfs::VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        let path = self.node_path()?;
        self.fs.lock().read_link(&path).map_err(into_vfs_err)
    }
}

impl InodeDirOperations for Inode {
    fn lookup(
        &self,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let name = dentry.name();
        let inode = match self.lookup_locked(name) {
            Ok(inode) => inode,
            Err(err) if err.canonicalize() == VfsError::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        dentry.instantiate_or_alias(inode)
    }

    fn create(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        _exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, _, gid) = inode_init_owner(dir, mode, cred);
        let name = dentry.name();
        if mode.node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let dir_path = self.dir_path()?;
        let child_path = join_child_path(&dir_path, name);
        let mode_bits = u32::from(mode.bits());

        let mut session = self.fs.lock();
        let fid = session
            .create_file_with_mode_and_gid(&child_path, mode_bits, gid)
            .map_err(into_vfs_err)?;
        let _ = session.close_fid(fid);
        let attr = session.getattr(&child_path).map_err(into_vfs_err)?;
        drop(session);

        let inode = Inode::new_file(self.fs.clone(), NodeType::RegularFile, Some(child_path))
            .into_vfs_inode(NodeFlags::NON_CACHEABLE, inode_init_from_attr(&attr));
        dentry.instantiate(inode)
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, _, gid) = inode_init_owner(dir, mode, cred);
        let name = dentry.name();
        if mode.node_type() != NodeType::Directory {
            return Err(VfsError::InvalidInput);
        }
        let dir_path = self.dir_path()?;
        let child_path = join_child_path(&dir_path, name);

        let mut session = self.fs.lock();
        session
            .create_dir_with_mode_and_gid(&child_path, mode.permission().bits() as u32, gid)
            .map_err(into_vfs_err)?;
        let attr = session.getattr(&child_path).map_err(into_vfs_err)?;
        drop(session);

        let inode = Inode::new_dir(self.fs.clone(), Some(child_path))
            .into_vfs_inode(NodeFlags::empty(), inode_init_from_attr(&attr));
        dentry.instantiate(inode)
    }

    fn mknod(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        mode: kvfs::Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, _, gid) = inode_init_owner(dir, mode, cred);
        let name = dentry.name();
        let node_type = mode.node_type();
        if matches!(
            node_type,
            NodeType::Directory | NodeType::RegularFile | NodeType::Symlink | NodeType::Unknown
        ) {
            return Err(VfsError::InvalidInput);
        }
        let dir_path = self.dir_path()?;
        let child_path = join_child_path(&dir_path, name);
        let mode_bits = u32::from(mode.bits());

        let mut session = self.fs.lock();
        session
            .mknod_dotl(&child_path, mode_bits, device.major(), device.minor(), gid)
            .map_err(into_vfs_err)?;
        let attr = session.getattr(&child_path).map_err(into_vfs_err)?;
        drop(session);

        let inode = Inode::new_file(self.fs.clone(), node_type, Some(child_path))
            .into_vfs_inode(NodeFlags::empty(), inode_init_from_attr(&attr));
        dentry.instantiate(inode)
    }

    fn symlink(
        &self,
        _idmap: &kvfs::MountIdmap,
        dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let (_, _, gid) = inode_init_owner(
            dir,
            Umode::new(NodeType::Symlink, NodePermission::from_bits_truncate(0o777)),
            cred,
        );
        let inode = self.create_symlink_inode(name, target, gid)?;
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        node: &Dentry,
        _dir: &kvfs::VfsInode,
        dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        let name = dentry.name();
        let dir_path = self.dir_path()?;
        let link_path = join_child_path(&dir_path, name);
        let target_path = node.absolute_path()?.to_string();

        let mut session = self.fs.lock();
        session
            .link(&target_path, &link_path)
            .map_err(into_vfs_err)?;
        drop(session);

        let inode = self.lookup_locked(name)?;
        dentry.instantiate(inode)
    }

    fn unlink(&self, _dir: &kvfs::VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let dir_path = self.dir_path()?;
        let child_path = join_child_path(&dir_path, dentry.name());
        let mut session = self.fs.lock();
        session.remove_path(&child_path).map_err(into_vfs_err)
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
        let src_path = join_child_path(&self.dir_path()?, old_dentry.name());
        let dst_path = join_child_path(&dst_dir.dir_path()?, new_dentry.name());
        let mut session = self.fs.lock();
        session
            .rename_path(&src_path, &dst_path)
            .map_err(into_vfs_err)
    }
}

// ---------------------------------------------------------------------------
// FileOperations — file I/O operations
// ---------------------------------------------------------------------------

impl FileOperations for Inode {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        if self.node_type == NodeType::Directory {
            Some(self)
        } else {
            None
        }
    }

    fn supports_read(&self) -> bool {
        matches!(
            self.node_type,
            NodeType::RegularFile | NodeType::Directory | NodeType::Symlink
        )
    }

    fn supports_write(&self) -> bool {
        self.node_type == NodeType::RegularFile
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        match self.node_type {
            NodeType::RegularFile => self.read_regular_at(buf, offset),
            NodeType::Directory => Err(VfsError::IsADirectory),
            NodeType::Symlink => self.read_symlink_at(buf, offset),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.node_type == NodeType::RegularFile {
            self.write_regular_at(buf, offset)
        } else {
            Err(VfsError::InvalidInput)
        }
    }
}

impl FileDirOperations for Inode {
    fn iterate_shared(&self, _file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let dir_path = self.dir_path()?;
        let mut session = self.fs.lock();
        let entries = session.list_dir_entries(&dir_path).map_err(into_vfs_err)?;

        let mut count = 0usize;
        let mut idx = 0u64;
        let offset = ctx.pos();
        for entry in entries {
            if idx < offset {
                idx += 1;
                continue;
            }
            let node_type = dtype_to_vfs(entry.entry_type);
            idx += 1;
            if !ctx.emit(&entry.name, 0, node_type, idx) {
                return Ok(count);
            }
            count += 1;
        }

        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn join_child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}
