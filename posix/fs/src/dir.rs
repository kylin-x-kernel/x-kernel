// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory and working-directory syscalls.

use alloc::{ffi::CString, vec, vec::Vec};
use core::{
    ffi::c_char,
    mem::{align_of, offset_of},
};

use kerrno::{KError, KResult};
use kvfs::{DirContext, Filename, LookupFlags, LookupIntent, NodePermission, NodeType};
use linux_raw_sys::general::*;
use osvm::VirtPtr;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::with_fs;

/// Changes the current working directory.
pub fn sys_chdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_chdir <= path: {path}");

    let entry = with_fs(AT_FDCWD, |fs| {
        Filename::new(path.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
        )
    })?;
    kprocess::current_user_process()
        .fs_context()?
        .lock()
        .set_pwd(entry)?;
    Ok(0)
}

/// Changes the current working directory by file descriptor.
pub fn sys_fchdir(dirfd: i32) -> KResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let entry = with_fs(dirfd, |fs| Ok(fs.pwd().clone()))?;
    kprocess::current_user_process()
        .fs_context()?
        .lock()
        .set_pwd(entry)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_mkdir(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    sys_mkdirat(AT_FDCWD, path, mode)
}

/// Changes the root directory of the calling process.
pub fn sys_chroot(path: UserConstPtr<c_char>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_chroot <= path: {path}");

    let loc = with_fs(AT_FDCWD, |fs| {
        Filename::new(path.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
        )
    })?;
    if !loc.is_dir() {
        return Err(KError::NotADirectory);
    }
    kprocess::current_user_process()
        .fs_context()?
        .lock()
        .set_root(loc)?;
    Ok(0)
}

/// Creates a directory relative to a directory file descriptor.
pub fn sys_mkdirat(dirfd: i32, path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_mkdirat <= dirfd: {dirfd}, path: {path}, mode: {mode}");

    let mode = mode & !kprocess::current_umask();
    let mode = NodePermission::from_bits_truncate(mode as u16);

    with_fs(dirfd, |fs| {
        let path_exists = || {
            Filename::new(path.as_str())
                .lookup_at(
                    fs.root(),
                    fs.pwd(),
                    LookupIntent::Open,
                    LookupFlags::no_follow(),
                )
                .is_ok()
        };
        let (dir, name) = match Filename::new(path.as_str()).create_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::DIRECTORY,
        ) {
            Ok(parent) => parent,
            Err(KError::InvalidInput) if !path.is_empty() && path_exists() => {
                return Err(KError::AlreadyExists);
            }
            Err(err) => return Err(err),
        };
        match dir.mkdir(&name, mode) {
            Ok(_) => Ok(0),
            Err(KError::InvalidInput) if !path.is_empty() && path_exists() => {
                Err(KError::AlreadyExists)
            }
            Err(err) => Err(err),
        }
    })
}

struct DirBuffer {
    buf: Vec<u8>,
    offset: usize,
}

impl DirBuffer {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0; len],
            offset: 0,
        }
    }

    fn remaining_space(&self) -> usize {
        self.buf.len().saturating_sub(self.offset)
    }

    fn write_entry(&mut self, d_ino: u64, d_off: i64, d_type: NodeType, name: &[u8]) -> bool {
        const NAME_OFFSET: usize = offset_of!(linux_dirent64, d_name);

        let len = NAME_OFFSET + name.len() + 1;
        let len = len.next_multiple_of(align_of::<linux_dirent64>());
        if self.remaining_space() < len {
            return false;
        }

        // SAFETY: Bounds were checked above and the entry layout matches
        // `linux_dirent64`. The byte buffer may not satisfy `linux_dirent64`
        // alignment at arbitrary offsets, so the header write must be
        // unaligned.
        unsafe {
            let entry_ptr = self.buf.as_mut_ptr().add(self.offset);
            entry_ptr
                .cast::<linux_dirent64>()
                .write_unaligned(linux_dirent64 {
                    d_ino,
                    d_off,
                    d_reclen: len as _,
                    d_type: d_type as _,
                    d_name: Default::default(),
                });

            let name_ptr = entry_ptr.add(NAME_OFFSET);
            name_ptr.copy_from_nonoverlapping(name.as_ptr(), name.len());
            name_ptr.add(name.len()).write(0);
        }

        self.offset += len;
        true
    }
}

/// Reads directory entries in `linux_dirent64` format.
pub fn sys_getdents64(fd: i32, buf: UserPtr<u8>, len: usize) -> KResult<isize> {
    debug!(
        "sys_getdents64 <= fd: {fd}, buf: {:?}, len: {len}",
        buf.as_ptr()
    );

    let mut buffer = DirBuffer::new(len);
    let dir = kprocess::current_resources().get_file(fd)?;
    let mut has_remaining = false;

    let mut sink = |name: &str, ino, node_type, offset| {
        has_remaining = true;
        if !buffer.write_entry(ino, offset as _, node_type, name.as_bytes()) {
            return false;
        }
        true
    };
    let mut ctx = DirContext::new(dir.position(), &mut sink);
    dir.iterate_dir(&mut ctx)?;

    if has_remaining && buffer.offset == 0 {
        return Err(KError::InvalidInput);
    }

    buf.write_vm_slice(&buffer.buf)?;
    Ok(buffer.offset as _)
}

/// Gets the current working directory path.
pub fn sys_getcwd(buf: UserPtr<u8>, size: isize) -> KResult<isize> {
    let size: usize = size.try_into().map_err(|_| KError::BadAddress)?;
    if buf.is_null() {
        return Ok(0);
    }

    let cwd = kprocess::current_user_process()
        .fs_context()?
        .lock()
        .pwd()
        .absolute_path()?;
    debug!("sys_getcwd => cwd: {cwd}");

    let cwd = CString::new(cwd.as_str()).map_err(|_| KError::InvalidInput)?;
    let cwd = cwd.as_bytes_with_nul();

    if cwd.len() <= size {
        buf.write_vm_slice(cwd)?;
        Ok(buf.as_ptr() as _)
    } else {
        Err(KError::OutOfRange)
    }
}
