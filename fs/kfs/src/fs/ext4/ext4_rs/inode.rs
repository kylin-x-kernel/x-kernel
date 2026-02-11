// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 inode wrapper and node implementations (ext4_rs backend).
use alloc::{string::String, sync::Arc, vec};
use core::{any::Any, cmp::min, task::Context, time::Duration};

use ext4_rs::BLOCK_SIZE;
use fs_ng_vfs::{
    DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, FilesystemOps,
    Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, Reference, VfsError,
    VfsResult, WeakDirEntry,
};
use kpoll::{IoEvents, Pollable};

use super::{
    Ext4Filesystem,
    util::{dir_entry_type_to_vfs, inode_to_vfs_type, into_vfs_err, vfs_type_to_inode},
};

/// Ext4 inode wrapper used to implement VFS nodes.
pub struct Inode {
    fs: Arc<Ext4Filesystem>,
    ino: u32,
    this: Option<WeakDirEntry>,
    path: Option<String>,
}

impl Inode {
    /// Create a new inode wrapper.
    pub(crate) fn new(
        fs: Arc<Ext4Filesystem>,
        ino: u32,
        this: Option<WeakDirEntry>,
        path: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            ino,
            this,
            path,
        })
    }

    fn create_entry(&self, ino: u32, node_type: NodeType, name: impl Into<String>) -> DirEntry {
        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.into(),
        );
        if node_type == NodeType::Directory {
            DirEntry::new_dir(
                |this| DirNode::new(Inode::new(self.fs.clone(), ino, Some(this), None)),
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(Inode::new(self.fs.clone(), ino, None, None)),
                node_type,
                reference,
            )
        }
    }

    fn lookup_locked(&self, name: &str) -> VfsResult<DirEntry> {
        let fs = self.fs.lock();
        let entries = fs.dir_get_entries(self.ino);
        for entry in entries {
            if entry.get_name() == name {
                let node_type = dir_entry_type_to_vfs(entry.get_de_type());
                return Ok(self.create_entry(entry.inode, node_type, name));
            }
        }
        Err(VfsError::NotFound)
    }
}

unsafe impl Send for Inode {}

unsafe impl Sync for Inode {}

impl NodeOps for Inode {
    fn inode(&self) -> u64 {
        self.ino as _
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let fs = self.fs.lock();
        let inode_ref = fs.get_inode_ref(self.ino);
        let inode = inode_ref.inode;
        Ok(Metadata {
            inode: self.ino as _,
            device: 0,
            nlink: inode.links_count() as u64,
            mode: NodePermission::from_bits_truncate(inode.mode & 0o777),
            node_type: inode_to_vfs_type(inode.file_type()),
            uid: inode.uid() as u32,
            gid: inode.gid() as u32,
            size: inode.size(),
            block_size: BLOCK_SIZE as u64,
            blocks: inode.blocks_count(),
            rdev: DeviceId::default(),
            atime: Duration::from_secs(inode.atime() as u64),
            mtime: Duration::from_secs(inode.mtime() as u64),
            ctime: Duration::from_secs(inode.ctime() as u64),
        })
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let fs = self.fs.lock();
        let mut inode_ref = fs.get_inode_ref(self.ino);
        if let Some(mode) = update.mode {
            let file_type = inode_ref.inode.file_type().bits();
            let perm = mode.bits() & 0o7777;
            inode_ref.inode.set_mode(file_type | perm);
        }
        if let Some((uid, gid)) = update.owner {
            inode_ref.inode.set_uid(uid as u16);
            inode_ref.inode.set_gid(gid as u16);
        }
        if let Some(atime) = update.atime {
            inode_ref.inode.set_atime(atime.as_secs() as u32);
        }
        if let Some(mtime) = update.mtime {
            inode_ref.inode.set_mtime(mtime.as_secs() as u32);
        }
        if cfg!(feature = "times") {
            inode_ref
                .inode
                .set_ctime(khal::time::wall_time().as_secs() as u32);
        }
        fs.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn len(&self) -> VfsResult<u64> {
        let fs = self.fs.lock();
        let inode_ref = fs.get_inode_ref(self.ino);
        Ok(inode_ref.inode.size())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }
}

impl FileNodeOps for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let inode_ref = fs.get_inode_ref(self.ino);
        let inode = inode_ref.inode;
        if inode.is_link() {
            let size = inode.size() as usize;
            if size <= 60 {
                if offset as usize >= size {
                    return Ok(0);
                }
                let data: &[u8; 60] = unsafe { core::mem::transmute(&inode.block) };
                let start = offset as usize;
                let end = min(size, start + buf.len());
                let len = end.saturating_sub(start);
                if len == 0 {
                    return Ok(0);
                }
                buf[..len].copy_from_slice(&data[start..start + len]);
                return Ok(len);
            }
        }

        fs.read_at(self.ino, offset as usize, buf)
            .map_err(into_vfs_err)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let fs = self.fs.lock();
        fs.write_at(self.ino, offset as usize, buf)
            .map_err(into_vfs_err)
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let fs = self.fs.lock();
        let inode_ref = fs.get_inode_ref(self.ino);
        let length = inode_ref.inode.size();
        let written = fs
            .write_at(self.ino, length as usize, buf)
            .map_err(into_vfs_err)?;
        Ok((written, length + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let fs = self.fs.lock();
        let mut inode_ref = fs.get_inode_ref(self.ino);
        let current = inode_ref.inode.size();
        if len == current {
            return Ok(());
        }
        if len < current {
            fs.truncate_inode(&mut inode_ref, len)
                .map_err(into_vfs_err)?;
            return Ok(());
        }

        let mut offset = current;
        let mut remaining = len - current;
        let zeros = vec![0u8; BLOCK_SIZE];
        while remaining > 0 {
            let write_len = min(remaining as usize, zeros.len());
            fs.write_at(self.ino, offset as usize, &zeros[..write_len])
                .map_err(into_vfs_err)?;
            offset += write_len as u64;
            remaining -= write_len as u64;
        }
        Ok(())
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }
}

impl Pollable for Inode {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl DirNodeOps for Inode {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let entries = fs.dir_get_entries(self.ino);
        let start = offset as usize;
        let mut count = 0usize;

        for (_idx, entry) in entries.into_iter().enumerate().skip(start) {
            let name = entry.get_name();
            let node_type = dir_entry_type_to_vfs(entry.get_de_type());
            let ino = entry.inode as u64;
            let next_offset = offset + count as u64 + 1;
            if !sink.accept(&name, ino, node_type, next_offset) {
                break;
            }
            count += 1;
        }

        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        self.lookup_locked(name)
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        let inode_type = vfs_type_to_inode(node_type).ok_or(VfsError::InvalidInput)?;
        let fs = self.fs.lock();
        let exists = fs
            .dir_get_entries(self.ino)
            .into_iter()
            .any(|entry| entry.get_name() == name);
        if exists {
            return Err(VfsError::AlreadyExists);
        }

        let mode_bits = permission.bits() & 0o7777;
        let inode_mode = inode_type.bits() | mode_bits;
        let mut inode_ref = fs
            .create(self.ino, name, inode_mode)
            .map_err(into_vfs_err)?;
        inode_ref.inode.set_mode(inode_mode);
        if cfg!(feature = "times") {
            inode_ref
                .inode
                .set_ctime(khal::time::wall_time().as_secs() as u32);
        }
        fs.write_back_inode(&mut inode_ref);

        Ok(self.create_entry(inode_ref.inode_num, node_type, name))
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::PermissionDenied)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        let fs = self.fs.lock();
        let entries = fs.dir_get_entries(self.ino);
        let inode = entries
            .into_iter()
            .find(|entry| entry.get_name() == name)
            .map(|entry| entry.inode)
            .ok_or(VfsError::NotFound)?;
        let mut child_inode_ref = fs.get_inode_ref(inode);
        if child_inode_ref.inode.is_dir() && fs.dir_has_entry(child_inode_ref.inode_num) {
            return Err(VfsError::DirectoryNotEmpty);
        }

        if child_inode_ref.inode.is_file() && child_inode_ref.inode.links_count() == 1 {
            fs.truncate_inode(&mut child_inode_ref, 0)
                .map_err(into_vfs_err)?;
        }

        let mut parent_inode_ref = fs.get_inode_ref(self.ino);
        fs.unlink(&mut parent_inode_ref, &mut child_inode_ref, name)
            .map_err(into_vfs_err)?;
        Ok(())
    }

    fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }
}
