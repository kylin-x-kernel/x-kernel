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

use crate::{
    FileOperations, InodeOperations, Kiocb, Metadata, MetadataUpdate, NodeFlags, NodePermission,
    NodeType, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
    simple_fs::{SimpleFs, SimpleFsNode},
};

/// Operations for a simple file.
pub trait SimpleFileOps: Send + Sync + 'static {
    /// Reads all content in the file.
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>>;
    /// Replaces the file's content with `data`.
    fn write_all(&self, data: &[u8]) -> VfsResult<()>;

    /// Writes data using ordinary seekable-file semantics.
    fn write(&self, _file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        let current = self.read_all()?;
        let mut current = current.to_vec();
        let offset = usize::try_from(offset).map_err(|_| VfsError::FileTooLarge)?;
        let end = offset
            .checked_add(data.len())
            .ok_or(VfsError::FileTooLarge)?;
        if end > current.len() {
            current.resize(end, 0);
        }
        current[offset..end].copy_from_slice(data);
        self.write_all(&current)?;
        Ok(data.len())
    }

    /// Returns whether one write request must reach `write` as one buffer.
    fn consumes_write_atomically(&self) -> bool {
        false
    }
}

/// Type representing operation applied to a simple file.
pub enum SimpleFileOperation<'a> {
    /// Reading the file's content
    Read,
    /// Replacing the file's content
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
        (self.0)(SimpleFileOperation::Read).map(|it| Cow::Owned(it.unwrap().into()))
    }

    fn write_all(&self, _data: &[u8]) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }

    fn write(&self, file: &VfsFile, data: &[u8], _offset: u64) -> VfsResult<usize> {
        (self.0)(SimpleFileOperation::Write { file, data })?;
        Ok(data.len())
    }

    fn consumes_write_atomically(&self) -> bool {
        true
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

    fn write_all(&self, _data: &[u8]) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

/// A simple file.
pub struct SimpleFile {
    node: SimpleFsNode,
    ops: Arc<dyn SimpleFileOps>,
}

impl SimpleFile {
    /// Creates a simple file from given file operations.
    pub fn new(fs: Arc<SimpleFs>, ty: NodeType, ops: impl SimpleFileOps) -> Arc<Self> {
        let node = SimpleFsNode::new(fs, ty, NodePermission::default());
        Arc::new(Self {
            node,
            ops: Arc::new(ops),
        })
    }

    /// Creates a simple file from given file operations.
    pub fn new_regular(fs: Arc<SimpleFs>, ops: impl SimpleFileOps) -> Arc<Self> {
        Self::new(fs, NodeType::RegularFile, ops)
    }

    /// Creates a regular file with an explicit permission mode.
    pub fn new_regular_with_permission(
        fs: Arc<SimpleFs>,
        permission: NodePermission,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        let node = SimpleFsNode::new(fs, NodeType::RegularFile, permission);
        Arc::new(Self {
            node,
            ops: Arc::new(ops),
        })
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
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata>;

    fn setattr(
        &self,
        _idmap: &crate::MountIdmap,
        _dentry: &crate::Dentry,
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
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.ops.read_all()?;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let data = &data[offset as usize..];
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        Ok(read)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.ops.write(file, buf, offset)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        if !self.ops.consumes_write_atomically() {
            let mut total = 0;
            let mut chunk = [0; memaddr::PAGE_SIZE_4K];
            while iter.count() != 0 {
                let want = chunk.len().min(iter.count());
                let copied = match iter.copy_from_iter(&mut chunk[..want]) {
                    Ok(copied) => copied,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                };
                if copied == 0 {
                    break;
                }
                let written = self
                    .ops
                    .write(iocb.file(), &chunk[..copied], iocb.ki_pos())?;
                if written == 0 {
                    return Err(VfsError::WriteZero);
                }
                total += written;
                iocb.advance(written);
                iocb.file().update_size_after_write(iocb.ki_pos())?;
                if written < copied {
                    break;
                }
            }
            return Ok(total);
        }

        const MAX_COMMAND_SIZE: usize = 4096;
        let count = iter.count();
        if count == 0 {
            return Ok(0);
        }
        if count > MAX_COMMAND_SIZE {
            return Err(VfsError::FileTooLarge);
        }
        let mut command = alloc::vec![0; count];
        let mut copied = 0;
        while copied < count {
            let read = iter.copy_from_iter(&mut command[copied..])?;
            if read == 0 {
                return Err(VfsError::BadAddress);
            }
            copied += read;
        }
        let written = self.ops.write(iocb.file(), &command, iocb.ki_pos())?;
        iocb.advance(written);
        iocb.file().update_size_after_write(iocb.ki_pos())?;
        Ok(written)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{DirMapping, FileSystemType, Filename, Mount, NodePermission, SimpleDir, SimpleFs};

    static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("simple-file-test");

    #[def_test]
    fn command_file_consumes_each_write_independently_of_offset() {
        let commands = Arc::new(crate::Mutex::new(Vec::<Vec<u8>>::new()));
        let observed = commands.clone();
        let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, move |fs| {
            let mut root = DirMapping::new();
            root.add(
                "command",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o222),
                    CommandFile::new(move |operation| match operation {
                        SimpleFileOperation::Read => Ok(Some(Vec::new())),
                        SimpleFileOperation::Write { data, .. } => {
                            observed.lock().push(data.to_vec());
                            Ok(None)
                        }
                    }),
                ),
            );
            SimpleDir::new_maker(fs, Arc::new(root))
        });
        let root = Mount::new_root(&super_block).root_path();
        let file = Filename::new("/command")
            .open_with_flags_at(
                &root,
                &root,
                linux_raw_sys::general::O_WRONLY,
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();

        assert_eq!(file.write(b"first"), Ok(5));
        assert_eq!(file.position(), 5);
        assert_eq!(file.write(b""), Ok(0));
        assert_eq!(file.write(b"second"), Ok(6));
        let mut positioned_offset = 123;
        assert_eq!(
            file.write_from(b"positioned", &mut positioned_offset),
            Ok(10)
        );
        assert_eq!(positioned_offset, 133);
        assert_eq!(
            *commands.lock(),
            vec![
                b"first".to_vec(),
                b"second".to_vec(),
                b"positioned".to_vec()
            ]
        );
    }
}
