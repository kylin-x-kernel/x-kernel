// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use fs_context::FsStruct;
use kerrno::{KError, KResult};
use klazy::lazy_static;
use kprocess;
use ksync::RwLock;
use kvfs::{Filename, LookupFlags, LookupIntent, NodePermission, VfsError, VfsFile};
use linux_raw_sys::general::*;
use slab::Slab;
use tee_raw_sys::{TEE_ERROR_GENERIC, TEE_ERROR_ITEM_NOT_FOUND};

use crate::{
    file::{validate_tee_path, with_fs},
    tee::TeeResult,
};

pub const FS_MODE_644: u32 = S_IRUSR | S_IWUSR | S_IRGRP | S_IROTH;
pub const FS_OFLAG_DEFAULT: u32 = O_CREAT | O_RDWR | O_SYNC;
pub const FS_OFLAG_RW: u32 = O_RDWR | O_SYNC;

lazy_static! {
    /// Global open-object table for TEE file descriptors.
    static ref TEE_FD_TABLE: RwLock<Slab<Arc<VfsFile>>> = RwLock::new(Slab::new());
}

fn open_path(
    fs: &FsStruct,
    path: &str,
    flags: u32,
    mode: __kernel_mode_t,
) -> KResult<Arc<VfsFile>> {
    let permission = NodePermission::from_bits_truncate(mode as _);
    Filename::new(path).open_with_flags_at(fs.root(), fs.pwd(), flags, permission)
}

#[derive(Debug, Clone, Copy)]
pub struct FileVariant {
    pub fd: isize,
}

impl Default for FileVariant {
    fn default() -> Self {
        Self { fd: -1 }
    }
}

fn add_to_fd(file: Arc<VfsFile>, _flags: u32) -> KResult<isize> {
    if file.is_dir() {
        info!("add_to_fd = error");
        return Err(KError::InvalidInput);
    }

    let fd = TEE_FD_TABLE.write().insert(file);
    Ok(fd as isize)
}

fn with_file<F, R>(file: &FileVariant, f: F) -> TeeResult<R>
where
    F: FnOnce(&Arc<VfsFile>) -> TeeResult<R>,
{
    let file_arc = TEE_FD_TABLE
        .read()
        .get(file.fd as usize)
        .ok_or_else(|| {
            error!("invalid fd {}", file.fd);
            TEE_ERROR_GENERIC
        })?
        .clone();
    f(&file_arc)
}

impl FileVariant {
    pub fn open(path: &str, flags: u32, mode: u32) -> Result<Self, VfsError> {
        tee_debug!(
            "FileVariant::open: path: {}, flags: {}, mode: {}",
            path,
            flags,
            mode
        );
        let path = validate_tee_path(path).map_err(|_| VfsError::InvalidInput)?;
        let mode = mode & !kprocess::current_umask();

        let fd = with_fs(AT_FDCWD, |fs| {
            open_path(fs, &path, flags, mode as __kernel_mode_t)
        })
        .and_then(|it| add_to_fd(it, flags as _))?;

        tee_debug!("FileVariant::open = fd: {}", fd);
        Ok(Self { fd })
    }

    /// remove file
    ///
    /// # Arguments
    /// * `path` - the path of the file to remove
    /// # Returns
    /// * `TeeResult` - the result of the operation
    ///   - `Ok(())` - file successfully removed
    ///   - `Err(TEE_ERROR_ITEM_NOT_FOUND)` - file does not exist
    ///   - `Err(TEE_ERROR_GENERIC)` - other errors
    pub fn remove_file(path: &str) -> TeeResult {
        tee_debug!("FileVariant::remove file with path: {}", path);
        let path = validate_tee_path(path).map_err(|_| TEE_ERROR_GENERIC)?;
        match with_fs(AT_FDCWD, |fs| {
            let entry = Filename::new(path.as_str()).lookup_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Open,
                LookupFlags::no_follow(),
            )?;
            entry
                .parent()
                .ok_or(VfsError::IsADirectory)?
                .unlink(entry.name())
        }) {
            Ok(()) => Ok(()),
            Err(VfsError::NotFound) => {
                tee_debug!("FileVariant::remove_file: file {} not found", path);
                Err(TEE_ERROR_ITEM_NOT_FOUND)
            }
            Err(e) => {
                error!("FileVariant::remove_file failed: {:?}", e);
                Err(TEE_ERROR_GENERIC)
            }
        }
    }

    /// remove directory.
    ///
    /// # Arguments
    /// * `path` - the path of the file to remove
    /// # Returns
    /// * `TeeResult` - the result of the operation
    #[cfg(unittest)]
    pub fn remove_dir(path: &str) -> TeeResult {
        tee_debug!("FileVariant::remove dir with path: {}", path);
        let path = validate_tee_path(path).map_err(|_| TEE_ERROR_GENERIC)?;
        with_fs(AT_FDCWD, |fs| {
            let entry = Filename::new(path.as_str()).lookup_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Open,
                LookupFlags::no_follow(),
            )?;
            entry
                .parent()
                .ok_or(VfsError::ResourceBusy)?
                .rmdir(entry.name())
        })
        .inspect_err(|e| error!("remove dir failed: {:?}", e))
        .map_err(|_| TEE_ERROR_GENERIC)?;

        Ok(())
    }

    /// Create a directory at the given path.
    ///
    /// # Arguments
    /// * `path` - The path of the directory to create
    ///
    /// # Returns
    /// * `Ok(())` - Success (directory created or already exists)
    /// * `Err(TEE_ERROR_GENERIC)` - Error occurred (e.g., parent directory doesn't exist)
    ///
    /// # Note
    /// This function does not create parent directories. If the parent directory
    /// doesn't exist, the function will return an error.
    /// If the directory already exists, the function returns success (idempotent behavior).
    pub fn create_dir(path: &str) -> TeeResult {
        let path = validate_tee_path(path).map_err(|_| TEE_ERROR_GENERIC)?;
        let mode = NodePermission::from_bits_truncate(0o755);
        with_fs(AT_FDCWD, |fs| {
            let (dir, name) = match Filename::new(path.as_str()).create_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Open,
                LookupFlags::DIRECTORY,
            ) {
                Ok(location) => location,
                Err(VfsError::AlreadyExists) => return Ok(()),
                Err(err) => return Err(err),
            };
            match dir.mkdir(&name, mode) {
                Ok(_) => Ok(()),
                Err(VfsError::AlreadyExists) => {
                    // Directory already exists, return success (idempotent)
                    Ok(())
                }
                Err(e) => {
                    error!("create_dir failed for {}: {:?}", path, e);
                    Err(KError::InvalidInput)
                }
            }
        })
        .inspect_err(|e| error!("tee_crate_dir with_fs failed for {}: {:?}", path, e))
        .map_err(|_| TEE_ERROR_GENERIC)
    }
}

impl FileVariant {
    pub fn pread(&self, buf: &mut [u8], offset: usize) -> TeeResult<usize> {
        tee_debug!(
            "FileVariant::pread = fd: {}, offset: 0x{:X?}, buf_len: 0x{:X?}",
            self.fd,
            offset,
            buf.len(),
        );
        with_file(self, |file| {
            let mut pos = offset as u64;
            file.read_from(buf, &mut pos)
                .inspect_err(|e| error!("read_at from file failed: {:?}", e))
                .map_err(|_| TEE_ERROR_GENERIC)
        })
    }

    pub fn pwrite(&self, buf: &[u8], offset: usize) -> TeeResult<usize> {
        with_file(self, |file| {
            let mut pos = offset as u64;
            let len = file
                .write_from(buf, &mut pos)
                .inspect_err(|e| error!("write_at to file failed: {:?}", e))
                .map_err(|_| TEE_ERROR_GENERIC)?;

            file.fsync(true)
                .inspect_err(|e| error!("pwrite: sync file failed: {:?}", e))
                .map_err(|_| TEE_ERROR_GENERIC)?;

            Ok(len)
        })
    }

    pub fn ftruncate(&mut self, len: usize) -> TeeResult<()> {
        with_file(self, |file| {
            file.path()
                .truncate(len as _)
                .inspect_err(|e| error!("set len failed: {:?}", e))
                .map_err(|_| TEE_ERROR_GENERIC)
        })
    }

    pub fn close(&mut self) -> TeeResult<()> {
        if self.fd < 0 {
            return Ok(()); // already closed
        }
        TEE_FD_TABLE
            .write()
            .try_remove(self.fd as usize)
            .ok_or_else(|| {
                error!("remove file from fd table failed: {:?}", self.fd);
                TEE_ERROR_GENERIC
            })?;
        self.fd = -1;
        Ok(())
    }
}

#[unittest::mod_test]
pub mod tests_file_ops {
    use unittest::{assert, assert_eq};

    use super::*;

    fn file_exists(path: &str) -> bool {
        use crate::file::resolve_at;

        let loc = resolve_at(AT_FDCWD, Some(path), AT_EMPTY_PATH);
        matches!(loc, Ok(loc) if loc.stat().is_ok())
    }

    fn remove_dir(path: &str) -> TeeResult {
        FileVariant::remove_dir(path).inspect_err(|e| error!("remove dir failed: {:?}", e))
    }

    fn tee_get_file_size(path: &str) -> TeeResult<usize> {
        use crate::file::resolve_at;

        let loc = resolve_at(AT_FDCWD, Some(path), 0)
            .inspect_err(|e| error!("resolve_at failed: {:?}", e))
            .map_err(|_| TEE_ERROR_GENERIC)?;
        Ok(loc.stat().map_err(|_| TEE_ERROR_GENERIC)?.size as usize)
    }

    #[unittest::def_test(custom)]
    fn test_file_ops_read() {
        let fd = FileVariant::open("/tmp/test.txt", O_RDWR | O_CREAT, 0o644);
        assert!(fd.is_ok());
        let mut fd = fd.unwrap();
        let buf = [0xAA; 8];
        let n = fd.pwrite(&buf, 0).expect("Failed to pwrite file");
        assert_eq!(n, 8);
        let mut buf = [0; 8];
        let n = fd.pread(&mut buf, 0).expect("Failed to pread file");
        assert_eq!(n, 8);
        assert_eq!(buf, [0xAA; 8]);
        let mut buf = [0; 4];
        let n = fd.pread(&mut buf, 4).expect("Failed to pread file");
        assert_eq!(n, 4);
        assert_eq!(buf, [0xAA; 4]);
        let n = fd.pwrite(&[0xBB; 4], 4).expect("Failed to pwrite file");
        assert_eq!(n, 4);
        let mut buf = [0; 4];
        let n = fd.pread(&mut buf, 4).expect("Failed to pread file");
        assert_eq!(n, 4);
        assert_eq!(buf, [0xBB; 4]);
        fd.ftruncate(4).expect("Failed to truncate file");
        let size = tee_get_file_size("/tmp/test.txt").expect("Failed to get file size");
        assert_eq!(size, 4);
    }

    #[unittest::def_test(custom)]
    fn test_file_ops_exists() {
        let path = "/tmp/test.txt.not_exists";
        assert!(!file_exists(path));
        {
            let fd = FileVariant::open(path, O_RDWR | O_CREAT, 0o644);
            assert!(fd.is_ok());
        }
        assert!(file_exists(path));
        {
            let n = FileVariant::remove_file(path);
            assert!(n.is_ok());
        }
        assert!(!file_exists(path));
    }

    #[unittest::def_test(custom)]
    fn test_file_ops_create_dir() {
        let path = "/tmp/test_create_dir/";
        let n = FileVariant::create_dir(path);
        assert!(n.is_ok());
        assert!(file_exists(path));
        let n = FileVariant::create_dir(path);
        assert!(n.is_ok());
        let n = remove_dir(path);
        assert!(n.is_ok());
        assert!(!file_exists(path));
    }
}
