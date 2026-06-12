// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal BPF filesystem (`bpffs`) support.
//!
//! This crate currently supports pinning loaded BPF programs and reopening them
//! through `BPF_OBJ_GET`. Maps and links are intentionally left for later BPF
//! subsystem work.

#![no_std]

extern crate alloc;

use alloc::{borrow::Cow, string::String, sync::Arc, vec::Vec};
use core::{any::Any, task::Context};

use hashbrown::HashMap;
use kerrno::KResult;
use kfd::FileLike;
use kpoll::{IoEvents, Pollable};
use ksync::Mutex;
use kvfs::{
    DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, Filesystem, FilesystemOps,
    Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, Reference, VfsError,
    VfsResult, WeakDirEntry,
};
use kvfs_simple::{SimpleFs, SimpleFsNode};

/// Linux `BPF_FS_MAGIC`.
const BPF_FS_MAGIC: u32 = 0xcafe4a11;

const BPF_MOUNT_FLAGS: u32 = kvfs::ST_NOSUID | kvfs::ST_NODEV | kvfs::ST_NOEXEC | kvfs::ST_RELATIME;
const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);
const PIN_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o600);

/// Loaded BPF program object shared by anonymous fds and bpffs pins.
pub struct BpfProgram {
    #[allow(dead_code)]
    insns: Arc<[u8]>,
}

impl BpfProgram {
    /// Creates a loaded BPF program object from validated instruction bytes.
    pub fn new(insns: Arc<[u8]>) -> Self {
        Self { insns }
    }
}

impl FileLike for BpfProgram {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:bpf_prog".into()
    }
}

impl Pollable for BpfProgram {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

#[derive(Clone)]
enum BpfEntry {
    Dir(Arc<Inode>),
    Program(Arc<Inode>),
}

impl BpfEntry {
    fn inode(&self) -> Arc<Inode> {
        match self {
            Self::Dir(inode) | Self::Program(inode) => inode.clone(),
        }
    }
}

enum InodeKind {
    Dir(Mutex<HashMap<String, BpfEntry>>),
    Program(Arc<BpfProgram>),
}

struct Inode {
    node: SimpleFsNode,
    kind: InodeKind,
}

impl Inode {
    fn new_dir(fs: Arc<SimpleFs>) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(fs, NodeType::Directory, DIR_PERMISSION),
            kind: InodeKind::Dir(Mutex::new(HashMap::new())),
        })
    }

    fn new_program(fs: Arc<SimpleFs>, program: Arc<BpfProgram>) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(fs, NodeType::RegularFile, PIN_PERMISSION),
            kind: InodeKind::Program(program),
        })
    }

    fn as_dir(&self) -> VfsResult<&Mutex<HashMap<String, BpfEntry>>> {
        match &self.kind {
            InodeKind::Dir(entries) => Ok(entries),
            InodeKind::Program(_) => Err(VfsError::NotADirectory),
        }
    }

    fn as_program(&self) -> VfsResult<Arc<BpfProgram>> {
        match &self.kind {
            InodeKind::Program(program) => Ok(program.clone()),
            InodeKind::Dir(_) => Err(VfsError::IsADirectory),
        }
    }
}

/// A bpffs directory or pinned object node.
pub struct BpfNode {
    fs: Arc<SimpleFs>,
    inode: Arc<Inode>,
    this: Option<WeakDirEntry>,
}

impl BpfNode {
    fn new(fs: Arc<SimpleFs>, inode: Arc<Inode>, this: Option<WeakDirEntry>) -> Arc<Self> {
        Arc::new(Self { fs, inode, this })
    }

    fn new_entry(&self, name: &str, entry: BpfEntry) -> VfsResult<DirEntry> {
        let inode = entry.inode();
        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.into(),
        );

        Ok(match entry {
            BpfEntry::Dir(_) => DirEntry::new_dir(
                |this| DirNode::new(Self::new(self.fs.clone(), inode, Some(this))),
                reference,
            ),
            BpfEntry::Program(_) => DirEntry::new_file(
                FileNode::new(Self::new(self.fs.clone(), inode, None)),
                NodeType::RegularFile,
                reference,
            ),
        })
    }

    fn pin_program(&self, name: &str, program: Arc<BpfProgram>) -> VfsResult<()> {
        let inode = Inode::new_program(self.fs.clone(), program);
        let mut entries = self.inode.as_dir()?.lock();
        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        entries.insert(name.into(), BpfEntry::Program(inode));
        Ok(())
    }

    fn program(&self) -> VfsResult<Arc<BpfProgram>> {
        self.inode.as_program()
    }
}

impl NodeOps for BpfNode {
    fn inode(&self) -> u64 {
        self.inode.node.inode()
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.inode.node.metadata()?;
        if let InodeKind::Dir(entries) = &self.inode.kind {
            metadata.size = entries.lock().len() as u64;
        }
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.inode.node.update_metadata(update)
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.inode.node.filesystem()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

impl FileNodeOps for BpfNode {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }

    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::InvalidInput)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}

impl Pollable for BpfNode {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl DirNodeOps for BpfNode {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let children: Vec<(String, Arc<Inode>)> = self
            .inode
            .as_dir()?
            .lock()
            .iter()
            .map(|(name, entry)| (name.clone(), entry.inode()))
            .collect();

        let dotdot = {
            let parent = self
                .this
                .as_ref()
                .and_then(WeakDirEntry::upgrade)
                .and_then(|entry| entry.parent());
            if let Some(parent) = parent {
                let metadata = parent.metadata()?;
                (metadata.inode, metadata.node_type)
            } else {
                (self.inode(), NodeType::Directory)
            }
        };

        let total = 2 + children.len();
        let start = offset as usize;
        if start >= total {
            return Ok(0);
        }

        let mut count = 0;
        for i in start..total {
            let (name, ino, node_type) = match i {
                0 => (".", self.inode(), NodeType::Directory),
                1 => ("..", dotdot.0, dotdot.1),
                j => {
                    let (name, inode) = &children[j - 2];
                    let metadata = inode.node.metadata()?;
                    (name.as_str(), metadata.inode, metadata.node_type)
                }
            };
            if !sink.accept(name, ino, node_type, i as u64 + 1) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        let entry = self
            .inode
            .as_dir()?
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        self.new_entry(name, entry)
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        _permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        if node_type != NodeType::Directory {
            return Err(VfsError::OperationNotPermitted);
        }
        let inode = Inode::new_dir(self.fs.clone());
        let entry = BpfEntry::Dir(inode.clone());
        let mut entries = self.inode.as_dir()?.lock();
        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        entries.insert(name.into(), entry.clone());
        self.new_entry(name, entry)
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        let mut entries = self.inode.as_dir()?.lock();
        let entry = entries.get(name).ok_or(VfsError::NotFound)?;
        if let InodeKind::Dir(children) = &entry.inode().kind
            && !children.lock().is_empty()
        {
            return Err(VfsError::DirectoryNotEmpty);
        }
        entries.remove(name);
        Ok(())
    }

    fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}

/// Creates a new bpffs filesystem instance.
pub fn new_bpffs() -> Filesystem {
    SimpleFs::new_with_flags("bpf".into(), BPF_FS_MAGIC, BPF_MOUNT_FLAGS, |fs| {
        let root = Inode::new_dir(fs.clone());
        Arc::new(move |this| BpfNode::new(fs.clone(), root.clone(), Some(this)))
    })
}

/// Pins a BPF program in a resolved bpffs directory.
pub fn pin_program(parent: &kvfs::Location, name: &str, program: Arc<BpfProgram>) -> KResult<()> {
    parent.check_writable_mount()?;
    parent.check_is_dir()?;
    let dir = parent.entry().downcast::<BpfNode>()?;
    dir.pin_program(name, program)?;
    Ok(())
}

/// Returns the BPF program pinned at a resolved bpffs file location.
pub fn program_from_location(location: &kvfs::Location) -> KResult<Arc<BpfProgram>> {
    location.check_is_file()?;
    let file = location.entry().downcast::<BpfNode>()?;
    file.program()
}
