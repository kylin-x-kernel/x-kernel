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
use kfs::{Directory, FsContext};
use kvfs::{NodePermission, NodeType};
use linux_raw_sys::general::*;
use osvm::VirtPtr;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::with_fs;

/// Changes the current working directory.
pub fn sys_chdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_chdir <= path: {path}");

    let proc_state = kthread::current_process_state();
    let mut fs = proc_state.fs_context().lock();
    let entry = fs.resolve(path)?;
    fs.set_current_dir(entry)?;
    Ok(0)
}

/// Changes the current working directory by file descriptor.
pub fn sys_fchdir(dirfd: i32) -> KResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let entry = with_fs(dirfd, |fs| Ok(fs.current_dir().clone()))?;
    kthread::current_process_state()
        .fs_context()
        .lock()
        .set_current_dir(entry)?;
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

    let proc_state = kthread::current_process_state();
    let mut fs = proc_state.fs_context().lock();
    let loc = fs.resolve(path)?;
    if loc.node_type() != NodeType::Directory {
        return Err(KError::NotADirectory);
    }
    *fs = FsContext::new(loc);
    Ok(0)
}

/// Creates a directory relative to a directory file descriptor.
pub fn sys_mkdirat(dirfd: i32, path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_mkdirat <= dirfd: {dirfd}, path: {path}, mode: {mode}");

    let mode = mode & !kthread::current_thread().process_state().umask();
    let mode = NodePermission::from_bits_truncate(mode as u16);

    with_fs(dirfd, |fs| match fs.create_dir(&path, mode) {
        Ok(_) => Ok(0),
        // `mkdir` on an existing path should report `EEXIST`.
        Err(KError::InvalidInput) if !path.is_empty() && fs.resolve_no_follow(&path).is_ok() => {
            Err(KError::AlreadyExists)
        }
        Err(err) => Err(err),
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
        // `linux_dirent64`.
        unsafe {
            let entry_ptr = self.buf.as_mut_ptr().add(self.offset);
            entry_ptr.cast::<linux_dirent64>().write(linux_dirent64 {
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
    let dir = kthread::current_resources().get_file_like_as::<Directory>(fd)?;
    let mut dir_offset = dir.offset.lock();
    let mut has_remaining = false;

    dir.inner()
        .read_dir(*dir_offset, &mut |name: &str, ino, node_type, offset| {
            has_remaining = true;
            if !buffer.write_entry(ino, offset as _, node_type, name.as_bytes()) {
                return false;
            }
            *dir_offset = offset;
            true
        })?;

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

    let cwd = kthread::current_process_state()
        .fs_context()
        .lock()
        .current_dir()
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
