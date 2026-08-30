// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory and working-directory syscalls.

use alloc::{ffi::CString, format, string::ToString, vec, vec::Vec};
use core::{
    ffi::c_char,
    mem::{align_of, offset_of},
};

use kerrno::{KError, KResult};
use kvfs::{
    DeviceId, DirContext, Filename, LookupFlags, LookupIntent, NodePermission, NodeType,
    Permission, Umode, may_mknod,
};
use linux_raw_sys::general::*;
use osvm::VirtPtr;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::{with_fs, with_fs_at};

/// Changes the current working directory.
pub fn sys_chdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_chdir <= path: {path}");
    let cred = kprocess::current_cred();

    let entry = with_fs(AT_FDCWD, |fs| {
        Filename::new(path.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
            &cred,
        )
    })?;
    entry.permission(Permission::MAY_EXEC | Permission::MAY_CHDIR, &cred)?;
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
    entry.permission(
        Permission::MAY_EXEC | Permission::MAY_CHDIR,
        &kprocess::current_cred(),
    )?;
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
    let cred = kprocess::current_cred();

    let loc = with_fs(AT_FDCWD, |fs| {
        Filename::new(path.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
            &cred,
        )
    })?;
    if !loc.is_dir() {
        return Err(KError::NotADirectory);
    }
    loc.permission(Permission::MAY_EXEC, &cred)?;
    if !cred.is_privileged() {
        return Err(KError::OperationNotPermitted);
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

    let mode = NodePermission::from_bits_truncate(mode as u16);
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    with_fs_at(dirfd, &filename, |fs| {
        filename
            .mkdir_at(fs.root(), fs.pwd(), mode, fs.node_umask(), &cred)
            .map(|_| 0)
    })
}

/// Creates a filesystem node relative to a directory file descriptor.
pub fn sys_mknodat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    mode: u32,
    device: u32,
) -> KResult<isize> {
    let path = path.load_string()?;
    let mode = Umode::from_bits(mode as u16);
    let mode = mode.with_node_type(may_mknod(mode)?);
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());
    debug!(
        "sys_mknodat <= dirfd: {dirfd}, path: {path:?}, mode: {:#o}, device: {device:#x}",
        mode.bits()
    );

    with_fs_at(dirfd, &filename, |fs| {
        filename
            .mknod_at(
                fs.root(),
                fs.pwd(),
                mode,
                DeviceId(device as u64),
                fs.node_umask(),
                &cred,
            )
            .map(|_| 0)
    })
}

#[cfg(target_arch = "x86_64")]
/// Creates a filesystem node at the given path.
pub fn sys_mknod(path: UserConstPtr<c_char>, mode: u32, device: u32) -> KResult<isize> {
    sys_mknodat(AT_FDCWD, path, mode, device)
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
///
/// On success returns the length of the path written to `buf` (including the
/// NUL terminator), matching the Linux `getcwd(2)` convention.  glibc checks
/// the return value against the buffer size and falls back to a manual
/// directory walk (`generic_getcwd`) when the syscall returns a value that
/// exceeds the buffer — an invariant that breaks when we return a raw
/// pointer.
pub fn sys_getcwd(buf: UserPtr<u8>, size: isize) -> KResult<isize> {
    let size: usize = size.try_into().map_err(|_| KError::BadAddress)?;
    if buf.is_null() {
        return Ok(0);
    }

    let fs_context = kprocess::current_user_process().fs_context()?;
    let fs = fs_context.lock();
    let cwd = match fs.pwd().render_from(fs.root())? {
        kvfs::RenderedPath::Reachable(path) => path.to_string(),
        kvfs::RenderedPath::Unreachable(path) => format!("(unreachable){path}"),
    };
    debug!("sys_getcwd => cwd: {cwd}");

    let cwd = CString::new(cwd).map_err(|_| KError::InvalidInput)?;
    let cwd = cwd.as_bytes_with_nul();

    if cwd.len() <= size {
        buf.write_vm_slice(cwd)?;
        // Return the byte count written (including NUL), per Linux ABI.
        Ok(cwd.len() as isize)
    } else {
        Err(KError::OutOfRange)
    }
}
