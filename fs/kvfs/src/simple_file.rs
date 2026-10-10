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

    /// Handles one scalar write; read-only files reject it by default.
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
        if count == 0 {
            return Ok(0);
        }
        if count > MAX_COMMAND_SIZE {
            return Err(VfsError::FileTooLarge);
        }
        let mut command = alloc::vec![0; count];
        let mut copied = 0;
        while copied < count {
            let read = match iter.copy_from_iter(&mut command[copied..]) {
                Ok(read) => read,
                Err(error) => {
                    iter.revert(copied)?;
                    return Err(error);
                }
            };
            if read == 0 {
                iter.revert(copied)?;
                return Err(VfsError::BadAddress);
            }
            copied += read;
        }
        let written = match self.write(iocb.file(), &command, iocb.ki_pos()) {
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
        Arc::new(Self {
            node: SimpleFsNode::new(fs, ty, permission),
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

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.file_operations.write(file, buf, offset)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        self.file_operations.write_iter(iocb, iter)
    }
}

impl FileOperations for GeneratedFileOperations {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
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
            file.set_private_data(Arc::new(SimpleFileSnapshot(
                self.0.read_all()?.into_owned(),
            )));
        }
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
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

fn read_generated_bytes(data: &[u8], buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    if offset >= data.len() as u64 {
        return Ok(0);
    }
    let data = &data[offset as usize..];
    let read = data.len().min(buf.len());
    buf[..read].copy_from_slice(&data[..read]);
    Ok(read)
}

#[unittest::mod_test]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{DirMapping, FileSystemType, Filename, Mount, NodePermission, SimpleDir, SimpleFs};

    static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("simple-file-test");

    #[def_test]
    fn command_file_read_rejects_missing_content() {
        let missing = CommandFile::new(|_| Ok(None::<Vec<u8>>));
        assert_eq!(missing.read_all(), Err(VfsError::InvalidData));
        let failed = CommandFile::new(|_| Err::<Option<Vec<u8>>, _>(VfsError::PermissionDenied));
        assert_eq!(failed.read_all(), Err(VfsError::PermissionDenied));
        let empty = CommandFile::new(|_| Ok(Some(Vec::<u8>::new())));
        assert_eq!(empty.read_all(), Ok(Cow::Owned(Vec::new())));
    }

    #[def_test]
    fn snapshot_partial_reads_are_stable_across_source_changes() {
        let source = Arc::new(crate::Mutex::new(b"abcdefgh".to_vec()));
        let observed = source.clone();
        let generations = Arc::new(crate::Mutex::new(0usize));
        let generated = generations.clone();
        let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, move |fs| {
            let mut root = DirMapping::new();
            root.add(
                "snapshot",
                SimpleFile::new_snapshot(fs.clone(), move || {
                    *generated.lock() += 1;
                    Ok(observed.lock().clone())
                }),
            );
            SimpleDir::new_maker(fs, Arc::new(root))
        });
        let root = Mount::new_root(&super_block).root_path();
        let open = |flags| {
            Filename::new("/snapshot")
                .open_with_flags_at(
                    &root,
                    &root,
                    flags,
                    NodePermission::empty(),
                    NodePermission::empty(),
                    kcred::initial_cred(),
                )
                .unwrap()
        };
        let first = open(linux_raw_sys::general::O_RDONLY);
        let mut prefix = [0; 3];
        assert_eq!(first.read(&mut prefix), Ok(3));
        assert_eq!(&prefix, b"abc");
        *source.lock() = b"CHANGED-LONG".to_vec();
        let mut tail = [0; 5];
        assert_eq!(first.read(&mut tail), Ok(5));
        assert_eq!(&tail, b"defgh");
        let mut next = [0; 12];
        assert_eq!(
            open(linux_raw_sys::general::O_RDONLY).read(&mut next),
            Ok(12)
        );
        assert_eq!(&next, b"CHANGED-LONG");
        assert_eq!(first.llseek(0, crate::file::SEEK_END), Ok(8));
        assert_eq!(first.read(&mut next), Ok(0));
        assert_eq!(first.llseek(-2, crate::file::SEEK_END), Ok(6));
        assert_eq!(first.read(&mut next), Ok(2));
        assert_eq!(&next[..2], b"gh");
        assert_eq!(
            first.llseek(-9, crate::file::SEEK_END),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(first.llseek(0, crate::file::SEEK_CUR), Ok(8));
        assert_eq!(
            first.llseek(i64::MAX, crate::file::SEEK_END),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(first.llseek(0, -1), Err(VfsError::InvalidInput));
        assert_eq!(first.llseek(0, crate::file::SEEK_CUR), Ok(8));
        assert_eq!(first.llseek(0, crate::file::SEEK_SET), Ok(0));
        assert_eq!(
            open(linux_raw_sys::general::O_RDONLY).llseek(0, crate::file::SEEK_END),
            Ok(12)
        );
        assert_eq!(first.write(b"x"), Err(VfsError::BadFileDescriptor));
        assert_eq!(*generations.lock(), 3);
        let write_opened = open(linux_raw_sys::general::O_WRONLY);
        assert_eq!(*generations.lock(), 3);
        assert_eq!(
            write_opened
                .private_data_get::<SimpleFileSnapshot>()
                .is_none(),
            true
        );
        assert_eq!(write_opened.llseek(3, crate::file::SEEK_SET), Ok(3));
        assert_eq!(write_opened.llseek(2, crate::file::SEEK_CUR), Ok(5));
        assert_eq!(write_opened.llseek(-1, crate::file::SEEK_CUR), Ok(4));
        assert_eq!(
            write_opened.llseek(-5, crate::file::SEEK_CUR),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            write_opened.llseek(0, crate::file::SEEK_END),
            Err(VfsError::InvalidData)
        );
        assert_eq!(write_opened.llseek(0, crate::file::SEEK_CUR), Ok(4));
        assert_eq!(*generations.lock(), 3);
        assert_eq!(write_opened.write(b"x"), Err(VfsError::InvalidInput));
        assert_eq!(*source.lock(), b"CHANGED-LONG".to_vec());
        let read_write = open(linux_raw_sys::general::O_RDWR);
        assert_eq!(*generations.lock(), 4);
        assert_eq!(read_write.read(&mut next), Ok(12));
        assert_eq!(&next, b"CHANGED-LONG");
        assert_eq!(read_write.write(b"x"), Err(VfsError::InvalidInput));
        source.lock().clear();
        let empty = open(linux_raw_sys::general::O_RDONLY);
        assert_eq!(empty.llseek(0, crate::file::SEEK_END), Ok(0));
        assert_eq!(empty.read(&mut next), Ok(0));
        assert_eq!(empty.llseek(2, crate::file::SEEK_SET), Ok(2));
        assert_eq!(empty.read(&mut next), Ok(0));
        assert_eq!(first.llseek(0, crate::file::SEEK_END), Ok(8));
    }

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
        assert_eq!(file.inode().size(), 0);
        assert_eq!(file.write(b""), Ok(0));
        assert_eq!(file.write(b"second"), Ok(6));
        let mut positioned_offset = 123;
        assert_eq!(
            file.write_from(b"positioned", &mut positioned_offset),
            Ok(10)
        );
        assert_eq!(positioned_offset, 133);
        assert_eq!(file.inode().size(), 0);
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
