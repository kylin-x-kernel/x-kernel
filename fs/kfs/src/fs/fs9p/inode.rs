// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtio-9p inode wrapper and node implementations.
use alloc::{string::String, string::ToString, sync::Arc, vec::Vec};
use core::{any::Any, task::Context, time::Duration};

use fs_ng_vfs::{
    DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, FilesystemOps,
    Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, Reference, VfsError,
    VfsResult, WeakDirEntry,
};
use kpoll::{IoEvents, Pollable};

use super::{Fs9pFilesystem, util::into_vfs_err};

/// 9p inode wrapper used to implement VFS nodes.
pub struct Inode {
    fs: Arc<Fs9pFilesystem>,
    ino: u64,
    is_dir: bool,
    is_symlink: bool,
    this: Option<WeakDirEntry>,
    path: String,
}

impl Inode {
    pub(crate) fn new(
        fs: Arc<Fs9pFilesystem>,
        ino: u64,
        is_dir: bool,
        is_symlink: bool,
        this: Option<WeakDirEntry>,
        path: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            ino,
            is_dir,
            is_symlink,
            this,
            path,
        })
    }

    fn dir_path(&self) -> VfsResult<String> {
        if let Some(this) = self.this.as_ref().and_then(WeakDirEntry::upgrade) {
            return Ok(this.absolute_path()?.to_string());
        }
        Ok(self.path.clone())
    }

    fn create_entry(&self, name: String, ino: u64, node_type: NodeType) -> DirEntry {
        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.clone(),
        );
        let path = join_child_path(&self.dir_path().unwrap_or_else(|_| self.path.clone()), &name);
        if node_type == NodeType::Directory {
            DirEntry::new_dir(
                |this| DirNode::new(Inode::new(self.fs.clone(), ino, true, false, Some(this), path)),
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(Inode::new(
                    self.fs.clone(),
                    ino,
                    false,
                    node_type == NodeType::Symlink,
                    None,
                    path,
                )),
                node_type,
                reference,
            )
        }
    }

    fn file_size_with(&self, session: &mut fs9p::Session) -> VfsResult<u64> {
        if self.is_symlink {
            let target = session.read_link(&self.path).map_err(into_vfs_err)?;
            return Ok(target.as_bytes().len() as u64);
        }
        let chunk = session.read_chunk_size().max(256) as usize;
        let mut offset = 0u64;
        loop {
            let data = session
                .read_file(&self.path, offset, chunk as u32)
                .map_err(into_vfs_err)?;
            if data.is_empty() {
                break;
            }
            offset = offset.saturating_add(data.len() as u64);
            if data.len() < chunk {
                break;
            }
        }
        Ok(offset)
    }
}

impl NodeOps for Inode {
    fn inode(&self) -> u64 {
        self.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let size = if self.is_dir {
            0
        } else {
            let mut state = self.fs.lock();
            self.file_size_with(&mut state.session)?
        };
        let node_type = if self.is_dir {
            NodeType::Directory
        } else if self.is_symlink {
            NodeType::Symlink
        } else {
            NodeType::RegularFile
        };
        Ok(Metadata {
            inode: self.ino,
            device: 0,
            nlink: 1,
            mode: NodePermission::default(),
            node_type,
            uid: 0,
            gid: 0,
            size,
            block_size: 4096,
            blocks: size / 4096,
            rdev: DeviceId::default(),
            atime: Duration::default(),
            mtime: Duration::default(),
            ctime: Duration::default(),
        })
    }

    fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn len(&self) -> VfsResult<u64> {
        if self.is_dir {
            return Ok(0);
        }
        let mut state = self.fs.lock();
        self.file_size_with(&mut state.session)
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING | NodeFlags::NON_CACHEABLE
    }
}

impl FileNodeOps for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if self.is_symlink {
            let mut state = self.fs.lock();
            let target = state.session.read_link(&self.path).map_err(into_vfs_err)?;
            let data = target.as_bytes();
            let start = offset as usize;
            if start >= data.len() {
                return Ok(0);
            }
            let end = core::cmp::min(data.len(), start + buf.len());
            let to_copy = end - start;
            buf[..to_copy].copy_from_slice(&data[start..end]);
            return Ok(to_copy);
        }
        let mut state = self.fs.lock();
        let data = state
            .session
            .read_file(&self.path, offset, buf.len() as u32)
            .map_err(into_vfs_err)?;
        let to_copy = data.len().min(buf.len());
        buf[..to_copy].copy_from_slice(&data[..to_copy]);
        Ok(to_copy)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.is_symlink {
            return Err(VfsError::Unsupported);
        }
        let mut state = self.fs.lock();
        state
            .session
            .write_file(&self.path, offset, buf)
            .map_err(into_vfs_err)
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        if self.is_symlink {
            return Err(VfsError::Unsupported);
        }
        let mut state = self.fs.lock();
        let size = self.file_size_with(&mut state.session)?;
        let written = state
            .session
            .write_file(&self.path, size, buf)
            .map_err(into_vfs_err)?;
        Ok((written, size + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        if self.is_dir {
            return Err(VfsError::IsADirectory);
        }
        if self.is_symlink {
            return Err(VfsError::Unsupported);
        }
        let mut state = self.fs.lock();
        let current = self.file_size_with(&mut state.session)?;
        if len == current {
            return Ok(());
        }
        if len < current {
            return Err(VfsError::Unsupported);
        }
        if len == 0 {
            return Ok(());
        }
        let zero = [0u8; 1];
        state
            .session
            .write_file(&self.path, len - 1, &zero)
            .map_err(into_vfs_err)?;
        Ok(())
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        error!(
            "fs9p: set_symlink unsupported path={} target={}",
            self.path.as_str(),
            target
        );
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
        let mut state = self.fs.lock();
        let names = state.session.list_dir(&self.path).map_err(into_vfs_err)?;

        let mut entries: Vec<(String, u64, NodeType)> = Vec::new();
        entries.push((String::from("."), self.ino, NodeType::Directory));
        let parent_ino = self
            .this
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .and_then(|entry| entry.parent())
            .map(|entry| entry.inode())
            .unwrap_or(self.ino);
        entries.push((String::from(".."), parent_ino, NodeType::Directory));

        for name in names {
            let child_path = join_child_path(&self.path, &name);
            let info = state
                .session
                .lookup_path(&child_path)
                .map_err(into_vfs_err)?;
            let node_type = if info.is_dir {
                NodeType::Directory
            } else if info.is_symlink {
                NodeType::Symlink
            } else {
                NodeType::RegularFile
            };
            entries.push((name, info.qid_path, node_type));
        }

        let mut idx = 0u64;
        let mut count = 0usize;
        for (name, ino, node_type) in entries {
            if idx < offset {
                idx += 1;
                continue;
            }
            idx += 1;
            if !sink.accept(&name, ino, node_type, idx) {
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

        let path = join_child_path(&self.dir_path()?, name);
        let mut state = self.fs.lock();
        let info = state.session.lookup_path(&path).map_err(into_vfs_err)?;
        let node_type = if info.is_dir {
            NodeType::Directory
        } else if info.is_symlink {
            NodeType::Symlink
        } else {
            NodeType::RegularFile
        };
        Ok(self.create_entry(name.to_string(), info.qid_path, node_type))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        let dir_path = self.dir_path()?;
        let path = join_child_path(&dir_path, name);
        let mut state = self.fs.lock();

        match node_type {
            NodeType::Directory => {
                state.session.create_dir(&path).map_err(into_vfs_err)?;
            }
            NodeType::RegularFile => {
                state
                    .session
                    .create_file(&path, permission.bits().into())
                    .map_err(into_vfs_err)?;
            }
            _ => return Err(VfsError::Unsupported),
        }

        let info = state.session.lookup_path(&path).map_err(into_vfs_err)?;
        let node_type = if info.is_dir {
            NodeType::Directory
        } else if info.is_symlink {
            NodeType::Symlink
        } else {
            NodeType::RegularFile
        };
        Ok(self.create_entry(name.to_string(), info.qid_path, node_type))
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::Unsupported)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        let dir_path = self.dir_path()?;
        let path = join_child_path(&dir_path, name);
        let mut state = self.fs.lock();
        state.session.remove_path(&path).map_err(into_vfs_err)
    }

    fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }
}

fn join_child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        alloc::format!("/{name}")
    } else {
        alloc::format!("{parent}/{name}")
    }
}

impl Inode {
    pub(crate) fn create_symlink_entry(&self, name: &str, target: &str) -> VfsResult<DirEntry> {
        let dir_path = self.dir_path()?;
        let path = join_child_path(&dir_path, name);
        let mut state = self.fs.lock();
        state
            .session
            .create_symlink(&path, target)
            .map_err(into_vfs_err)?;
        let info = state.session.lookup_path(&path).map_err(into_vfs_err)?;
        Ok(self.create_entry(name.to_string(), info.qid_path, NodeType::Symlink))
    }
}
