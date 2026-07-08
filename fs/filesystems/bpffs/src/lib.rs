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

use alloc::{string::String, sync::Arc, vec::Vec};

use hashbrown::HashMap;
use kerrno::KResult;
use ksync::Mutex;
use kvfs::{
    AnonInodeFs, Dentry, DirContext, FMode, FileDirOperations, FileOperations, InodeDirOperations,
    InodeOperations, Metadata, MetadataUpdate, NodeFlags, NodePermission, NodeType, SimpleFs,
    SimpleFsNode, SuperBlock, VfsError, VfsFile, VfsInode, VfsResult,
};

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

    pub fn new_file(insns: Arc<[u8]>) -> VfsResult<Arc<VfsFile>> {
        Arc::new(Self::new(insns)).into_file()
    }

    pub fn into_file(self: Arc<Self>) -> VfsResult<Arc<VfsFile>> {
        let fops: Arc<dyn FileOperations> = self.clone();
        AnonInodeFs::global().get_file("bpf-prog", fops, self, FMode::READ | FMode::WRITE, 0)
    }

    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(VfsError::BadFileDescriptor)
    }
}

impl FileOperations for BpfProgram {}

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

    fn is_dir(&self) -> bool {
        matches!(self.kind, InodeKind::Dir(_))
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
}

impl BpfNode {
    fn new(fs: Arc<SimpleFs>, inode: Arc<Inode>) -> Arc<Self> {
        Arc::new(Self { fs, inode })
    }

    fn new_entry(&self, parent: &Dentry, name: &str, entry: BpfEntry) -> VfsResult<Dentry> {
        let inode = entry.inode();
        let d_parent = Some(parent.clone());
        let d_name = String::from(name);

        Ok(match entry {
            BpfEntry::Dir(_) => BpfNode::new(self.fs.clone(), inode).into_dentry(
                NodeFlags::empty(),
                d_parent,
                d_name,
            ),
            BpfEntry::Program(_) => {
                let node = BpfNode::new(self.fs.clone(), inode);
                node.into_dentry(NodeFlags::NON_CACHEABLE, d_parent, d_name)
            }
        })
    }

    fn into_vfs_inode(self: Arc<Self>, flags: NodeFlags) -> Arc<VfsInode> {
        let init = self.inode.node.inode_init();
        let is_dir = self.inode.is_dir();
        debug_assert_eq!(is_dir, init.node_type() == NodeType::Directory);
        let private_data: Arc<dyn core::any::Any + Send + Sync> = self.clone();
        let inode_operations: Arc<dyn InodeOperations> =
            Arc::new(BpfInodeOperations::new(self.clone()));
        let file_operations: Arc<dyn FileOperations> = Arc::new(BpfFileOperations::new(self));
        if is_dir {
            VfsInode::new_dir_with_operations(
                private_data,
                inode_operations,
                file_operations,
                flags,
                init,
            )
        } else {
            VfsInode::new_file_with_operations(
                private_data,
                inode_operations,
                file_operations,
                flags,
                init,
            )
        }
    }

    fn into_dentry(
        self: Arc<Self>,
        flags: NodeFlags,
        parent: Option<Dentry>,
        name: String,
    ) -> Dentry {
        let is_dir = self.inode.is_dir();
        let inode = self.into_vfs_inode(flags);
        if is_dir {
            Dentry::new_dir_from_inode(inode, parent, name)
        } else {
            Dentry::new_file_from_inode(inode, parent, name)
        }
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

struct BpfInodeOperations {
    node: Arc<BpfNode>,
}

impl BpfInodeOperations {
    fn new(node: Arc<BpfNode>) -> Self {
        Self { node }
    }
}

impl InodeOperations for BpfInodeOperations {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        if self.node.inode.is_dir() {
            Some(self)
        } else {
            None
        }
    }

    fn getattr(
        &self,
        idmap: &kvfs::MountIdmap,
        path: Option<&kvfs::Path>,
        request_mask: kvfs::GetattrRequestMask,
        query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut metadata = self
            .node
            .inode
            .node
            .getattr(idmap, path, request_mask, query_flags)?;
        if let InodeKind::Dir(entries) = &self.node.inode.kind {
            metadata.size = entries.lock().len() as u64;
        }
        Ok(metadata)
    }

    fn setattr(
        &self,
        idmap: &kvfs::MountIdmap,
        dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<()> {
        self.node.inode.node.setattr(idmap, dentry, update)
    }
}

impl InodeDirOperations for BpfInodeOperations {
    fn lookup(
        &self,
        _dir: &kvfs::VfsInode,
        dentry: &Dentry,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Dentry> {
        let dir = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let entry = self
            .node
            .inode
            .as_dir()?
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        self.node.new_entry(&dir, name, entry)
    }

    fn mkdir(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dir: &kvfs::VfsInode,
        dentry: &Dentry,
        _mode: kvfs::Umode,
    ) -> VfsResult<Dentry> {
        let dir = dentry.parent().ok_or(VfsError::InvalidInput)?;
        let name = dentry.name();
        let inode = Inode::new_dir(self.node.fs.clone());
        let entry = BpfEntry::Dir(inode.clone());
        let mut entries = self.node.inode.as_dir()?.lock();
        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        entries.insert(name.into(), entry.clone());
        self.node.new_entry(&dir, name, entry)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &kvfs::VfsInode,
        _new_dentry: &Dentry,
    ) -> VfsResult<Dentry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, _dir: &kvfs::VfsInode, dentry: &Dentry) -> VfsResult<()> {
        let name = dentry.name();
        let mut entries = self.node.inode.as_dir()?.lock();
        let entry = entries.get(name).ok_or(VfsError::NotFound)?;
        if let InodeKind::Dir(children) = &entry.inode().kind
            && !children.lock().is_empty()
        {
            return Err(VfsError::DirectoryNotEmpty);
        }
        entries.remove(name);
        Ok(())
    }

    fn rename(
        &self,
        _idmap: &kvfs::MountIdmap,
        _old_dir: &kvfs::VfsInode,
        _old_dentry: &Dentry,
        _new_dir: &kvfs::VfsInode,
        _new_dentry: &Dentry,
        _flags: kvfs::RenameFlags,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}

struct BpfFileOperations {
    node: Arc<BpfNode>,
}

impl BpfFileOperations {
    fn new(node: Arc<BpfNode>) -> Self {
        Self { node }
    }
}

impl FileOperations for BpfFileOperations {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        if self.node.inode.is_dir() {
            Some(self)
        } else {
            None
        }
    }

    fn supports_read(&self) -> bool {
        self.node.inode.is_dir()
    }

    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if self.node.inode.is_dir() {
            Err(VfsError::IsADirectory)
        } else {
            Err(VfsError::InvalidInput)
        }
    }
}

impl FileDirOperations for BpfFileOperations {
    fn iterate_shared(&self, file: &VfsFile, ctx: &mut DirContext<'_>) -> VfsResult<usize> {
        let children: Vec<(String, Arc<Inode>)> = self
            .node
            .inode
            .as_dir()?
            .lock()
            .iter()
            .map(|(name, entry)| (name.clone(), entry.inode()))
            .collect();

        let current_metadata = file.path().metadata();
        let dotdot = {
            if let Some(parent) = file.path().parent() {
                let metadata = parent.metadata();
                (metadata.inode, metadata.mode.node_type())
            } else {
                (current_metadata.inode, current_metadata.mode.node_type())
            }
        };

        let total = 2 + children.len();
        let offset = ctx.pos();
        let start = offset as usize;
        if start >= total {
            return Ok(0);
        }

        let mut count = 0;
        for i in start..total {
            let (name, ino, node_type) = match i {
                0 => (
                    ".",
                    current_metadata.inode,
                    current_metadata.mode.node_type(),
                ),
                1 => ("..", dotdot.0, dotdot.1),
                j => {
                    let (name, inode) = &children[j - 2];
                    let init = inode.node.inode_init();
                    (name.as_str(), init.inode_number(), init.node_type())
                }
            };
            if !ctx.emit(name, ino, node_type, i as u64 + 1) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
}

/// Creates a bpffs superblock.
pub fn new_bpffs() -> Arc<SuperBlock> {
    SimpleFs::new_with_flags("bpf".into(), BPF_FS_MAGIC, BPF_MOUNT_FLAGS, |fs| {
        let root = Inode::new_dir(fs.clone());
        Arc::new(move || {
            let node = BpfNode::new(fs.clone(), root.clone());
            node.into_vfs_inode(NodeFlags::empty())
        })
    })
}

/// Pins a BPF program in a resolved bpffs directory.
pub fn pin_program(parent: &kvfs::Path, name: &str, program: Arc<BpfProgram>) -> KResult<()> {
    parent.check_writable_mount()?;
    if !parent.is_dir() {
        return Err(VfsError::NotADirectory);
    }
    let dir = parent.downcast_node::<BpfNode>()?;
    dir.pin_program(name, program)?;
    Ok(())
}

/// Returns the BPF program pinned at a resolved bpffs file location.
pub fn program_from_location(location: &kvfs::Path) -> KResult<Arc<BpfProgram>> {
    if !location.is_file() {
        return Err(VfsError::InvalidInput);
    }
    let file = location.downcast_node::<BpfNode>()?;
    file.program()
}
