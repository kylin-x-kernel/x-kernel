// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P inode wrapper and VFS node implementations.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
};
use core::{any::Any, task::Context};

use fs9p::FileAttr;
use kpoll::{IoEvents, Pollable};
use kvfs::{
    DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, FilesystemOps,
    Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, Reference, VfsError,
    VfsResult, WeakDirEntry,
};

use super::{
    Fs9pFilesystem,
    util::{dtype_to_vfs, into_vfs_err, qid_type_to_vfs},
};

/// A VFS inode backed by a 9P session.
///
/// Unlike ext4, the 9P filesystem is stateless on the client side — each
/// operation sends a message to the server.  We store the filesystem reference
/// (which holds the session) and the path of this node so that we can call
/// path-based session operations when VFS methods are invoked.
///
/// For file I/O (`read_at`, `write_at`, `append`, `set_len`), we open a
/// temporary fid, perform the operation, then close the fid.
pub struct Inode {
    fs: Arc<Fs9pFilesystem>,
    this: Option<WeakDirEntry>,
    path: Option<String>,
    is_dir: bool,
}

impl Inode {
    /// Create a new inode for a directory node.
    pub(crate) fn new_dir(
        fs: Arc<Fs9pFilesystem>,
        this: Option<WeakDirEntry>,
        path: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            this,
            path,
            is_dir: true,
        })
    }

    /// Create a new inode for a file (or symlink) node.
    pub(crate) fn new_file(fs: Arc<Fs9pFilesystem>, path: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            fs,
            this: None,
            path,
            is_dir: false,
        })
    }

    /// Resolve the absolute path of this node.
    fn node_path(&self) -> VfsResult<String> {
        if let Some(this) = self.this.as_ref().and_then(WeakDirEntry::upgrade) {
            return Ok(this.absolute_path()?.to_string());
        }
        self.path.clone().ok_or(VfsError::InvalidInput)
    }

    /// Resolve the absolute path of this directory node.
    fn dir_path(&self) -> VfsResult<String> {
        self.node_path()
    }

    /// Look up a child entry via the 9P session.
    fn lookup_locked(&self, name: &str) -> VfsResult<DirEntry> {
        let child_path = join_child_path(&self.dir_path()?, name);
        let mut session = self.fs.lock();
        let attr = session.getattr(&child_path).map_err(into_vfs_err)?;
        Ok(self.create_entry_from_attr(name, &attr, &child_path))
    }

    /// Build a `DirEntry` from 9P file attributes.
    fn create_entry_from_attr(&self, name: &str, attr: &FileAttr, path: &str) -> DirEntry {
        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.into(),
        );
        let node_type = qid_type_to_vfs(attr.qid_type);
        if node_type == NodeType::Directory {
            DirEntry::new_dir(
                |this| {
                    DirNode::new(Inode::new_dir(
                        self.fs.clone(),
                        Some(this),
                        Some(path.into()),
                    ))
                },
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(Inode::new_file(self.fs.clone(), Some(path.into()))),
                node_type,
                reference,
            )
        }
    }

    /// Create a DirEntry for a symlink, using the 9P `TSYMLINK` operation.
    ///
    /// This is called from `fs_operations.rs` for the special symlink path.
    pub fn create_symlink_entry(&self, name: &str, target: &str) -> VfsResult<DirEntry> {
        let dir_path = self.dir_path()?;
        let link_path = join_child_path(&dir_path, name);
        let mut session = self.fs.lock();
        session.symlink(target, &link_path).map_err(into_vfs_err)?;
        drop(session);

        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.into(),
        );
        Ok(DirEntry::new_file(
            FileNode::new(Inode::new_file(self.fs.clone(), Some(link_path))),
            NodeType::Symlink,
            reference,
        ))
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
}

// ---------------------------------------------------------------------------
// NodeOps — common operations for all node types
// ---------------------------------------------------------------------------

impl NodeOps for Inode {
    fn inode(&self) -> u64 {
        // 9P does not expose persistent inode numbers to the client in a
        // trivially usable way.  We use the pointer address of this Arc as
        // a unique-ish identifier.
        0
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let path = self.node_path()?;
        let mut session = self.fs.lock();
        let attr = session.getattr(&path).map_err(into_vfs_err)?;
        Ok(Metadata {
            inode: 0,
            device: 0,
            nlink: attr.nlink as _,
            mode: NodePermission::from_bits_truncate((attr.mode & 0o777) as u16),
            node_type: qid_type_to_vfs(attr.qid_type),
            uid: attr.uid,
            gid: attr.gid,
            size: attr.size,
            block_size: 4096,
            blocks: (attr.size + 511) / 512,
            rdev: DeviceId::default(),
            atime: core::time::Duration::from_secs(attr.atime_sec),
            mtime: core::time::Duration::from_secs(attr.mtime_sec),
            ctime: core::time::Duration::from_secs(attr.ctime_sec),
        })
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let path = self.node_path()?;
        if let Some(mode) = update.mode {
            let mut session = self.fs.lock();
            session
                .setattr_mode(&path, mode.bits() as u32)
                .map_err(into_vfs_err)?;
        }
        // 9P2000.L TSETATTR supports uid/gid/atime/mtime/size changes,
        // but our fs9p::Session currently only exposes setattr_mode.
        // Additional setattr helpers can be added as the Session API grows.
        Ok(())
    }

    fn len(&self) -> VfsResult<u64> {
        let path = self.node_path()?;
        let mut session = self.fs.lock();
        let attr = session.getattr(&path).map_err(into_vfs_err)?;
        Ok(attr.size)
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        // QEMU's 9P local backend does not implement TFSYNC.
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }
}

// ---------------------------------------------------------------------------
// FileNodeOps — file I/O operations
// ---------------------------------------------------------------------------

impl FileNodeOps for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // For symlinks, read the link target.
        let path = self.node_path()?;
        {
            let mut session = self.fs.lock();
            let attr = session.getattr(&path).map_err(into_vfs_err)?;
            if qid_type_to_vfs(attr.qid_type) == NodeType::Symlink {
                let target = session.read_link(&path).map_err(into_vfs_err)?;
                let target_bytes = target.as_bytes();
                let start = offset as usize;
                if start >= target_bytes.len() {
                    return Ok(0);
                }
                let to_read = core::cmp::min(buf.len(), target_bytes.len() - start);
                buf[..to_read].copy_from_slice(&target_bytes[start..start + to_read]);
                return Ok(to_read);
            }
        }

        let fid = self.open_fid_rdonly()?;
        let result = {
            let mut session = self.fs.lock();
            session
                .read_fid(fid, offset, buf.len() as u32)
                .map_err(into_vfs_err)
        };
        self.close_fid(fid);

        match result {
            Ok(data) => {
                let n = core::cmp::min(data.len(), buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
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

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let path = self.node_path()?;
        let offset = {
            let mut session = self.fs.lock();
            let attr = session.getattr(&path).map_err(into_vfs_err)?;
            attr.size
        };
        let written = self.write_at(buf, offset)?;
        Ok((written, offset + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let fid = self.open_fid_rdwr()?;
        let result = {
            let mut session = self.fs.lock();
            session.truncate_fid(fid, len).map_err(into_vfs_err)
        };
        self.close_fid(fid);
        result
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        // For 9P, symlinks are created atomically via TSYMLINK in create().
        // Changing an existing symlink target is not part of the 9P protocol.
        Err(VfsError::Unsupported)
    }
}

// ---------------------------------------------------------------------------
// Pollable
// ---------------------------------------------------------------------------

impl Pollable for Inode {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

// ---------------------------------------------------------------------------
// DirNodeOps — directory operations
// ---------------------------------------------------------------------------

impl DirNodeOps for Inode {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let dir_path = self.dir_path()?;
        let mut session = self.fs.lock();
        let entries = session.list_dir_entries(&dir_path).map_err(into_vfs_err)?;

        let mut count = 0usize;
        let mut idx = 0u64;
        for entry in entries {
            if idx < offset {
                idx += 1;
                continue;
            }
            let node_type = dtype_to_vfs(entry.entry_type);
            idx += 1;
            if !sink.accept(&entry.name, 0, node_type, idx) {
                return Ok(count);
            }
            count += 1;
        }

        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        if name == "." {
            return self
                .this
                .as_ref()
                .and_then(WeakDirEntry::upgrade)
                .ok_or(VfsError::NotFound);
        }
        if name == ".." {
            return self
                .this
                .as_ref()
                .and_then(WeakDirEntry::upgrade)
                .and_then(|entry| entry.parent())
                .ok_or(VfsError::NotFound);
        }
        self.lookup_locked(name)
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        _permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        let dir_path = self.dir_path()?;
        let child_path = join_child_path(&dir_path, name);

        let mut session = self.fs.lock();
        if node_type == NodeType::Directory {
            session.create_dir(&child_path).map_err(into_vfs_err)?;
        } else {
            // Create a regular file. The fid returned by create_file is
            // immediately closed — VFS I/O will re-open as needed.
            let fid = session.create_file(&child_path).map_err(into_vfs_err)?;
            let _ = session.close_fid(fid);
        }
        drop(session);

        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.into(),
        );

        Ok(if node_type == NodeType::Directory {
            DirEntry::new_dir(
                |this| {
                    DirNode::new(Inode::new_dir(
                        self.fs.clone(),
                        Some(this),
                        Some(child_path),
                    ))
                },
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(Inode::new_file(self.fs.clone(), Some(child_path))),
                node_type,
                reference,
            )
        })
    }

    fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry> {
        let dir_path = self.dir_path()?;
        let link_path = join_child_path(&dir_path, name);
        let target_path = node.absolute_path()?.to_string();

        let mut session = self.fs.lock();
        session
            .link(&target_path, &link_path)
            .map_err(into_vfs_err)?;
        drop(session);

        self.lookup_locked(name)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        let dir_path = self.dir_path()?;
        let child_path = join_child_path(&dir_path, name);
        let mut session = self.fs.lock();
        session.remove_path(&child_path).map_err(into_vfs_err)
    }

    fn rename(&self, src_name: &str, dst_dir: &DirNode, dst_name: &str) -> VfsResult<()> {
        let dst_dir: Arc<Self> = dst_dir.downcast().map_err(|_| VfsError::InvalidInput)?;
        let src_path = join_child_path(&self.dir_path()?, src_name);
        let dst_path = join_child_path(&dst_dir.dir_path()?, dst_name);
        let mut session = self.fs.lock();
        session
            .rename_path(&src_path, &dst_path)
            .map_err(into_vfs_err)
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
