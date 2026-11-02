// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P-backed open-file operations.

use alloc::sync::Arc;

use kvfs::{
    AddressSpace, AddressSpaceOperations, FMode, FileDirOperations, FileOperations, NodeType,
    OpenFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult,
};
use memaddr::PAGE_SIZE_4K;
use p9::{P9Fid, P9OpenFlags, P9OpenMode};
use pagecache::FolioGuard;

use super::inode::V9fsInode;

impl V9fsInode {
    pub(super) fn file_fid(file: &VfsFile) -> VfsResult<Arc<P9Fid>> {
        file.private_data_get::<P9Fid>()
            .ok_or(VfsError::InvalidInput)
    }

    fn read_regular_at(file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let data =
            Self::file_fid(file)?.read(offset, u32::try_from(buf.len()).unwrap_or(u32::MAX))?;
        let count = data.len().min(buf.len());
        buf[..count].copy_from_slice(&data[..count]);
        Ok(count)
    }

    fn write_regular_at(file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        Self::file_fid(file)?.write(offset, buf)
    }

    fn read_symlink_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let target = self.fid.read_link()?;
        let target = target.as_bytes();
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= target.len() {
            return Ok(0);
        }
        let count = buf.len().min(target.len() - start);
        buf[..count].copy_from_slice(&target[start..start + count]);
        Ok(count)
    }
}

impl FileOperations for V9fsInode {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        self.fid.qid().is_dir().then_some(self)
    }

    fn open(self: Arc<Self>, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let mode = if file.mode().contains(FMode::WRITE) {
            if file.mode().contains(FMode::READ) {
                P9OpenMode::ReadWrite
            } else {
                P9OpenMode::Write
            }
        } else {
            P9OpenMode::Read
        };
        let fid = self.fid.walk(&[])?;
        fid.open(
            mode,
            p9_open_flags(file.flags(), file.mode().contains(FMode::CREATED)),
        )?;
        file.set_private_data(fid);
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        !self.fid.qid().is_dir() && !self.fid.qid().is_symlink()
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        match file.inode().node_type() {
            NodeType::RegularFile => Self::read_regular_at(file, buf, offset),
            NodeType::Directory => Err(VfsError::IsADirectory),
            NodeType::Symlink => self.read_symlink_at(buf, offset),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if file.inode().node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }
        let written = Self::write_regular_at(file, buf, offset)?;
        if written != 0 {
            let end = offset
                .checked_add(written as u64)
                .ok_or(VfsError::FileTooLarge)?;
            file.inode().grow_size_after_backing_write(end)?;
        }
        Ok(written)
    }

    fn fsync(&self, file: &VfsFile, data_only: bool) -> VfsResult<()> {
        Self::file_fid(file)?.fsync(data_only)
    }
}

/// Byte window of one folio inside a file of `file_size` bytes.
///
/// Returns the file offset the folio starts at and how many bytes the server
/// must return to fill it. `None` means the folio index overflows the offset.
fn folio_window(file_size: u64, folio_index: u64) -> Option<(u64, usize)> {
    let offset = folio_index.checked_mul(PAGE_SIZE_4K as u64)?;
    let expected = file_size.saturating_sub(offset).min(PAGE_SIZE_4K as u64);
    Some((offset, expected as usize))
}

/// Serves page-cache folios for 9P regular files.
///
/// Executables and shared libraries mapped from a 9P mount fault in through
/// this path; without it the mapping has no backing pages and the faulting
/// task takes SIGBUS.
pub(crate) struct V9fsAddressSpaceOperations;

pub(crate) static V9FS_ADDRESS_SPACE_OPERATIONS: V9fsAddressSpaceOperations =
    V9fsAddressSpaceOperations;

impl AddressSpaceOperations for V9fsAddressSpaceOperations {
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut FolioGuard<'_>) -> VfsResult<()> {
        let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let (offset, expected) =
            folio_window(inode.size(), folio.index()).ok_or(VfsError::InvalidInput)?;
        if expected == 0 {
            folio.data().fill(0);
            return Ok(());
        }
        // Faults are concurrent, so each folio reads through its own fid.
        let fid = inode.private::<V9fsInode>()?.fid.walk(&[])?;
        fid.open(P9OpenMode::Read, P9OpenFlags::empty())?;
        // The client shortens a request to the negotiated msize/iounit, which
        // can be smaller than a folio. Keep issuing TREADs until the window is
        // covered; a zero-length reply is end of file and cannot fill it.
        let buf = folio.data();
        let mut filled = 0;
        while filled < expected {
            let want = u32::try_from(expected - filled).map_err(|_| VfsError::InvalidInput)?;
            let data = fid.read(offset + filled as u64, want)?;
            if data.is_empty() {
                return Err(VfsError::Io);
            }
            let take = data.len().min(expected - filled);
            buf[filled..filled + take].copy_from_slice(&data[..take]);
            filled += take;
        }
        buf[expected..].fill(0);
        Ok(())
    }
}

fn p9_open_flags(mut flags: OpenFlags, was_created: bool) -> P9OpenFlags {
    if was_created {
        flags.remove(OpenFlags::CREATE | OpenFlags::EXCLUSIVE);
    }
    let mut mapped = P9OpenFlags::empty();
    for (vfs_flag, p9_flag) in [
        (OpenFlags::CREATE, P9OpenFlags::CREATE),
        (OpenFlags::EXCLUSIVE, P9OpenFlags::EXCLUSIVE),
        (
            OpenFlags::NO_CONTROLLING_TTY,
            P9OpenFlags::NO_CONTROLLING_TTY,
        ),
        (OpenFlags::APPEND, P9OpenFlags::APPEND),
        (OpenFlags::NONBLOCK, P9OpenFlags::NONBLOCK),
        (OpenFlags::DSYNC, P9OpenFlags::DSYNC),
        (OpenFlags::ASYNC, P9OpenFlags::ASYNC),
        (OpenFlags::DIRECT, P9OpenFlags::DIRECT),
        (OpenFlags::LARGE_FILE, P9OpenFlags::LARGE_FILE),
        (OpenFlags::DIRECTORY, P9OpenFlags::DIRECTORY),
        (OpenFlags::NO_FOLLOW, P9OpenFlags::NO_FOLLOW),
        (OpenFlags::NO_ATIME, P9OpenFlags::NO_ATIME),
        (OpenFlags::CLOSE_ON_EXEC, P9OpenFlags::CLOSE_ON_EXEC),
        (OpenFlags::SYNC, P9OpenFlags::SYNC),
    ] {
        if flags.contains(vfs_flag) {
            mapped.insert(p9_flag);
        }
    }
    mapped
}

unittest::path_mod_test!(tests, "../ut-tests/file.rs");
