// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FAT file inode implementation.
use alloc::{sync::Arc, vec};

use fatfs::{Read, Seek, SeekFrom, Write};
use kvfs::{
    Dentry, FileOperations, InodeOperations, Metadata, MetadataUpdate, NodeType, VfsError, VfsFile,
    VfsResult,
};

use super::{
    FsRef, ff,
    fs::FatFilesystem,
    util::{file_metadata, into_vfs_err, update_file_metadata},
};

/// FAT file inode.
pub(crate) struct FatFileInode {
    fs: Arc<FatFilesystem>,
    inner: FsRef<ff::File<'static>>,
    inode: u64,
}

impl FatFileInode {
    /// Construct a file inode from a FAT file handle.
    pub(crate) fn new(
        fs: Arc<FatFilesystem>,
        owner: &FatFilesystem,
        file: ff::File,
        inode: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            // SAFETY: The FAT handle is only accessed while holding the
            // matching filesystem lock, which outlives this node wrapper.
            inner: unsafe { FsRef::from_file_handle(owner, file) },
            inode,
        })
    }
}

fn file_size(file: &mut ff::File) -> u64 {
    let pos = file.seek(SeekFrom::Current(0)).unwrap_or(0);
    let size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    file.seek(SeekFrom::Start(pos)).ok();
    size
}

fn grow_file(block_size: usize, file: &mut ff::File<'static>, len: u64) -> VfsResult<()> {
    // rust-fatfs does not support growing files directly. We need to
    // pad with zeros manually.
    let mut pos = file.seek(SeekFrom::End(0)).map_err(into_vfs_err)?;
    let block = vec![0; block_size];

    while pos < len {
        let write = (block_size - (pos as usize & (block_size - 1))).min((len - pos) as usize);
        file.write(&block[0..write]).map_err(into_vfs_err)?;
        pos += write as u64;
    }
    Ok(())
}

fn set_file_len(block_size: usize, file: &mut ff::File<'static>, len: u64) -> VfsResult<()> {
    if len > u32::MAX as u64 {
        return Err(VfsError::FileTooLarge);
    }

    let current_len = file_size(file);
    if len < current_len {
        file.seek(SeekFrom::Start(len)).map_err(into_vfs_err)?;
        file.truncate().map_err(into_vfs_err)?;
    } else if len > current_len {
        grow_file(block_size, file, len)?;
    }
    Ok(())
}

impl InodeOperations for FatFileInode {
    fn getattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _path: Option<&kvfs::Path>,
        _request_mask: kvfs::GetattrRequestMask,
        _query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut fs = self.fs.lock();
        let block_size = fs.inner.cluster_size() as u64;
        let file = self.inner.borrow_mut(&mut fs);
        Ok(file_metadata(
            block_size,
            self.inode,
            file,
            NodeType::RegularFile,
        ))
    }

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        // FatFS has no ownership & permission

        let mut fs = self.fs.lock();
        let block_size = fs.inner.cluster_size() as usize;
        let file = self.inner.borrow_mut(&mut fs);
        if let Some(size) = update.size {
            set_file_len(block_size, file, size)?;
        }
        Ok(update_file_metadata(file, update))
    }
}

impl FileOperations for FatFileInode {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, mut buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        let file = self.inner.borrow_mut(&mut fs);
        file.seek(SeekFrom::Start(offset)).map_err(into_vfs_err)?;

        let mut read = 0;
        loop {
            let n = file.read(buf).map_err(into_vfs_err)?;
            if n == 0 {
                return Ok(read);
            }
            read += n;
            buf = &mut buf[n..];
        }
    }

    fn write(&self, _file: &VfsFile, mut buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        let block_size = fs.inner.cluster_size() as usize;
        let file = self.inner.borrow_mut(&mut fs);
        if offset > file_size(file) {
            grow_file(block_size, file, offset)?;
        }
        file.seek(SeekFrom::Start(offset)).map_err(into_vfs_err)?;

        let mut written = 0;
        loop {
            let n = file.write(buf).map_err(into_vfs_err)?;
            if n == 0 {
                return Ok(written);
            }
            written += n;
            buf = &buf[n..];
        }
    }
}

impl Drop for FatFileInode {
    fn drop(&mut self) {
        self.fs.lock().release_inode(self.inode);
    }
}
