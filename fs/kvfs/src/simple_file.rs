// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple file helpers for the in-kernel VFS.

use alloc::{
    borrow::{Cow, ToOwned},
    string::String,
    sync::Arc,
    vec::Vec,
};

use inherit_methods_macro::inherit_methods;
use iov_iter::IovIterSource;
use kpoll::{IoEvents, PollContext, PollRegisterError};

use crate::{
    FileOperations, InodeOperations, Kiocb, Metadata, MetadataUpdate, NodeFlags, NodePermission,
    NodeType, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
    command::{self, CompleteCommandOptions, EmptyCommandPolicy},
    simple_fs::{SimpleFs, SimpleFsNode},
};

/// Operations for a simple file.
pub trait SimpleFileOps: Send + Sync + 'static {
    /// Reads all content in the file.
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>>;

    /// Validates one open after VFS permission checks and before file state is
    /// installed. The default preserves existing helper users.
    fn validate_open(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Validates one open that requests write access.
    ///
    /// The default preserves existing helper users; `SetgroupsFile` uses it
    /// for Linux's open-for-write capability check without denying read opens.
    fn validate_open_for_write(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Handles one scalar write; read-only files reject it by default.
    ///
    /// `CommandFile` invokes this callback after complete command copying and
    /// possibly while the VFS position mutex is held. The callback must not
    /// recursively perform I/O, seek, or another write on the same open
    /// description.
    fn write(&self, _file: &VfsFile, _data: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    /// Handles one iterator write, preserving the operation's own write semantics.
    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        crate::file::scalar_write_iter(iocb, iter, |file, data, offset| {
            self.write(file, data, offset)
        })
    }
}

/// Type representing operation applied to a simple file.
pub enum SimpleFileOperation<'a> {
    /// Reading the file's content
    Read,
    /// Consuming one write request.
    Write {
        /// Open file description whose opener credential governs the command.
        file: &'a VfsFile,
        /// Complete command supplied by this write request.
        data: &'a [u8],
    },
}

/// A simple command file that consumes each write as one complete request.
pub struct CommandFile<F>(F);

impl<F, R> CommandFile<F>
where
    F: Fn(SimpleFileOperation) -> VfsResult<Option<R>> + Send + Sync,
    R: Into<Vec<u8>>,
{
    /// Creates a command file.
    ///
    /// The handler must return `Some(content)` for reads. A successful read
    /// without content is rejected with [`VfsError::InvalidData`].
    pub fn new(imp: F) -> Self {
        Self(imp)
    }
}

impl<F, R> SimpleFileOps for CommandFile<F>
where
    F: Fn(SimpleFileOperation) -> VfsResult<Option<R>> + Send + Sync + 'static,
    R: Into<Vec<u8>>,
{
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let content = (self.0)(SimpleFileOperation::Read)?.ok_or(VfsError::InvalidData)?;
        Ok(Cow::Owned(content.into()))
    }

    fn write(&self, file: &VfsFile, data: &[u8], _offset: u64) -> VfsResult<usize> {
        (self.0)(SimpleFileOperation::Write { file, data })?;
        Ok(data.len())
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        const MAX_COMMAND_SIZE: usize = 4096;
        let count = iter.count();
        let Some(command) = command::copy_complete_command(
            iter,
            CompleteCommandOptions {
                max_bytes: MAX_COMMAND_SIZE,
                empty: EmptyCommandPolicy::NoOp,
            },
        )?
        else {
            return Ok(0);
        };
        let written = match self.write(iocb.file(), &command, 0) {
            Ok(written) => written,
            Err(error) => {
                iter.revert(count)?;
                return Err(error);
            }
        };
        if written > count {
            iter.revert(count)?;
            return Err(VfsError::InvalidData);
        }
        iter.revert(count - written)?;
        if written == 0 {
            return Err(VfsError::WriteZero);
        }
        iocb.advance(written);
        Ok(written)
    }
}

impl<F, R> SimpleFileOps for F
where
    F: Fn() -> VfsResult<R> + Send + Sync + 'static,
    R: Into<Vec<u8>>,
{
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        (self)().map(|it| Cow::Owned(it.into()))
    }
}

/// A simple file.
pub struct SimpleFile {
    node: SimpleFsNode,
    ops: Arc<dyn SimpleFileOps>,
    file_operations: Arc<dyn FileOperations>,
}

struct SimpleFileSnapshot(Vec<u8>);

struct GeneratedFileOperations(Arc<dyn SimpleFileOps>);

struct SnapshotFileOperations(Arc<dyn SimpleFileOps>);

impl SimpleFile {
    fn new_with_options(
        fs: Arc<SimpleFs>,
        ty: NodeType,
        permission: NodePermission,
        ops: Arc<dyn SimpleFileOps>,
        file_operations: Arc<dyn FileOperations>,
    ) -> Arc<Self> {
        Self::new_with_options_owner(fs, ty, permission, 0, 0, ops, file_operations)
    }

    fn new_with_options_owner(
        fs: Arc<SimpleFs>,
        ty: NodeType,
        permission: NodePermission,
        uid: u32,
        gid: u32,
        ops: Arc<dyn SimpleFileOps>,
        file_operations: Arc<dyn FileOperations>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new_with_owner(fs, ty, permission, uid, gid),
            ops,
            file_operations,
        })
    }

    /// Creates a simple file from given file operations.
    pub fn new(fs: Arc<SimpleFs>, ty: NodeType, ops: impl SimpleFileOps) -> Arc<Self> {
        Self::new_regular_with_type(fs, ty, NodePermission::default(), ops)
    }

    /// Creates a simple file from given file operations.
    pub fn new_regular(fs: Arc<SimpleFs>, ops: impl SimpleFileOps) -> Arc<Self> {
        Self::new(fs, NodeType::RegularFile, ops)
    }

    /// Creates a read-only diagnostic file whose content is frozen per open.
    ///
    /// Each read-capable open owns immutable generated bytes. Reads and seeks
    /// use that generation; `SEEK_END` uses its length, independently of other
    /// opens and the inode size. Write-only opens do not generate content.
    /// Writes are rejected by VFS capability checks. Descriptors must be
    /// initialized through the normal open path; reads and `SEEK_END` reject
    /// missing state. `SEEK_SET` and `SEEK_CUR` also work for write-only opens
    /// without generating a snapshot.
    pub fn new_snapshot(fs: Arc<SimpleFs>, ops: impl SimpleFileOps) -> Arc<Self> {
        let ops: Arc<dyn SimpleFileOps> = Arc::new(ops);
        Self::new_with_options(
            fs,
            NodeType::RegularFile,
            NodePermission::default(),
            ops.clone(),
            Arc::new(SnapshotFileOperations(ops)),
        )
    }

    /// Creates a per-open snapshot file with explicit mode and owner.
    pub fn new_snapshot_with_owner(
        fs: Arc<SimpleFs>,
        permission: NodePermission,
        uid: u32,
        gid: u32,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        let ops: Arc<dyn SimpleFileOps> = Arc::new(ops);
        Self::new_with_options_owner(
            fs,
            NodeType::RegularFile,
            permission,
            uid,
            gid,
            ops.clone(),
            Arc::new(SnapshotFileOperations(ops)),
        )
    }

    /// Creates a regular file with explicit mode and global owner.
    pub fn new_regular_with_owner(
        fs: Arc<SimpleFs>,
        permission: NodePermission,
        uid: u32,
        gid: u32,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        let ops: Arc<dyn SimpleFileOps> = Arc::new(ops);
        Self::new_with_options_owner(
            fs,
            NodeType::RegularFile,
            permission,
            uid,
            gid,
            ops.clone(),
            Arc::new(GeneratedFileOperations(ops)),
        )
    }

    /// Creates a regular file with an explicit permission mode.
    pub fn new_regular_with_permission(
        fs: Arc<SimpleFs>,
        permission: NodePermission,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        Self::new_regular_with_type(fs, NodeType::RegularFile, permission, ops)
    }

    fn new_regular_with_type(
        fs: Arc<SimpleFs>,
        ty: NodeType,
        permission: NodePermission,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        let ops: Arc<dyn SimpleFileOps> = Arc::new(ops);
        Self::new_with_options(
            fs,
            ty,
            permission,
            ops.clone(),
            Arc::new(GeneratedFileOperations(ops)),
        )
    }

    /// Returns the inode fields used when materializing this simple file.
    pub fn inode_init(&self) -> VfsInodeInit {
        self.node.inode_init()
    }

    /// Materializes one VFS inode backed by this persistent simple-file node.
    pub fn new_inode(self: &Arc<Self>, flags: NodeFlags) -> Arc<VfsInode> {
        let init = self.inode_init();
        let node_type = init.node_type();
        if matches!(
            node_type,
            NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Fifo | NodeType::Socket
        ) {
            return VfsInode::new_special(self.clone(), flags, init);
        }
        VfsInode::new_file_with_flags(self.clone(), flags, init)
    }
}

#[inherit_methods(from = "self.node")]
impl InodeOperations for SimpleFile {
    fn symlink_operations(&self) -> Option<&dyn crate::InodeSymlinkOperations> {
        if self.node.metadata.lock().mode.node_type() == crate::NodeType::Symlink {
            Some(self)
        } else {
            None
        }
    }

    fn getattr(
        &self,
        idmap: &crate::MountIdmap,
        path: Option<&crate::Path>,
        request_mask: crate::GetattrRequestMask,
        query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata>;

    fn setattr(
        &self,
        idmap: &crate::MountIdmap,
        dentry: &crate::Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate>;
}

impl crate::InodeSymlinkOperations for SimpleFile {
    fn get_link(
        &self,
        _dentry: Option<&crate::Dentry>,
        _inode: &crate::VfsInode,
        _done: &mut crate::DelayedCall,
    ) -> VfsResult<String> {
        let data = self.ops.read_all()?;
        core::str::from_utf8(&data)
            .map(ToOwned::to_owned)
            .map_err(|_| VfsError::InvalidData)
    }
}

impl FileOperations for SimpleFile {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut crate::VfsFileBuilder) -> VfsResult<()> {
        self.ops.validate_open()?;
        if file.mode().contains(crate::FMode::WRITE) {
            self.ops.validate_open_for_write()?;
        }
        self.file_operations.clone().open(inode, file)
    }

    fn supports_read(&self) -> bool {
        self.file_operations.supports_read()
    }

    fn supports_write(&self) -> bool {
        self.file_operations.supports_write()
    }

    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        self.file_operations.llseek(file, offset, whence)
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.file_operations.read(file, buf, offset)
    }

    fn read_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut iov_iter::IovIterDest<'_>,
    ) -> VfsResult<usize> {
        self.file_operations.read_iter(iocb, iter)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.file_operations.write(file, buf, offset)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        self.file_operations.write_iter(iocb, iter)
    }

    /// Forwards the poll operation to the generated/snapshot file operations,
    /// which is where the Linux fallback documented there lives. The inode
    /// carries this wrapper as its fops, so without the forwarding the
    /// capability would exist one level down and never be seen.
    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        self.file_operations.poll(file)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.file_operations.register_poll(file, context, events)
    }
}

impl FileOperations for GeneratedFileOperations {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    /// Implements the poll operation these files' Linux fops always carry.
    ///
    /// The files built here are the attribute/diagnostic files of `/proc`,
    /// `/sys` and cgroup v2. Linux serves all three through an fops that
    /// provides `poll` even when the entry has no handler of its own:
    /// `proc_reg_file_ops` (`proc_reg_poll`) and `kernfs_file_fops`
    /// (`kernfs_fop_poll`) both fall back to `DEFAULT_POLLMASK`. Reporting that
    /// same fallback is what keeps `epoll_ctl` accepting these files, since
    /// `file_can_poll` only asks whether the fops provides a handler at all.
    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(IoEvents::IN | IoEvents::OUT)
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.0.read_all()?;
        read_generated_bytes(&data, buf, offset)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.0.write(file, buf, offset)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        self.0.write_iter(iocb, iter)
    }
}

impl FileOperations for SnapshotFileOperations {
    fn open(self: Arc<Self>, _inode: &VfsInode, file: &mut crate::VfsFileBuilder) -> VfsResult<()> {
        if file.mode().contains(crate::FMode::READ) {
            file.set_private_data(Arc::new(SimpleFileSnapshot(materialize_content(
                self.0.read_all()?,
            )?)));
        }
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    /// Same poll operation as [`GeneratedFileOperations`]: the snapshot form is
    /// the read-only shape of the same `/proc` and `/sys` files, whose Linux
    /// fops provide `poll` and fall back to `DEFAULT_POLLMASK`.
    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(IoEvents::IN | IoEvents::OUT)
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let snapshot = file
            .private_data_get::<SimpleFileSnapshot>()
            .ok_or(VfsError::InvalidData)?;
        read_generated_bytes(&snapshot.0, buf, offset)
    }

    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        let mut position = file.position_lock();
        let base = match whence {
            crate::file::SEEK_SET => 0,
            crate::file::SEEK_CUR => {
                i64::try_from(*position).map_err(|_| VfsError::InvalidInput)?
            }
            crate::file::SEEK_END => {
                let snapshot = file
                    .private_data_get::<SimpleFileSnapshot>()
                    .ok_or(VfsError::InvalidData)?;
                i64::try_from(snapshot.0.len()).map_err(|_| VfsError::InvalidInput)?
            }
            _ => return Err(VfsError::InvalidInput),
        };
        let next = base
            .checked_add(offset)
            .filter(|next| *next >= 0)
            .ok_or(VfsError::InvalidInput)? as u64;
        *position = next;
        Ok(next)
    }
}

fn materialize_content(content: Cow<'_, [u8]>) -> VfsResult<Vec<u8>> {
    match content {
        Cow::Owned(bytes) => Ok(bytes),
        Cow::Borrowed(bytes) => {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(bytes.len())
                .map_err(|_| VfsError::NoMemory)?;
            owned.extend_from_slice(bytes);
            Ok(owned)
        }
    }
}

fn read_generated_bytes(data: &[u8], buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    if offset >= data.len() as u64 {
        return Ok(0);
    }
    let data = &data[offset as usize..];
    let read = data.len().min(buf.len());
    buf[..read].copy_from_slice(&data[..read]);
    Ok(read)
}

unittest::path_mod_test!(tests, "../ut-tests/simple_file.rs");
