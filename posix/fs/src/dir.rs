// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory and working-directory syscalls.

use alloc::{ffi::CString, format, string::ToString};
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
///
/// Accepts directory descriptors, including `O_PATH`, but not the `AT_FDCWD`
/// sentinel used by pathname-relative syscalls. The cwd is unchanged on error.
///
/// # Errors
///
/// Returns `KError::BadFileDescriptor` for an invalid fd and
/// `KError::NotADirectory` for a non-directory. Search-permission and
/// filesystem-context errors are propagated before committing the new cwd.
pub fn sys_fchdir(dirfd: i32) -> KResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let file = kprocess::current_files().get_file(dirfd)?;
    if !file.is_dir() {
        return Err(KError::NotADirectory);
    }
    let entry = file.path().clone();
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
    if !cred.has_cap(kcred::CAP_SYS_CHROOT) {
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
        let created = filename.mkdir_at_entry(fs.root(), fs.pwd(), mode, fs.node_umask(), &cred)?;
        // The new entry is a directory, so the creation carries IN_ISDIR; the
        // parent identity and name come from the created entry's own record,
        // captured under the create lock.
        crate::notify::notify_created_entry(
            &created,
            crate::notify::IN_CREATE | crate::notify::IN_ISDIR,
        );
        Ok(0)
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
        let created = filename.mknod_at_entry(
            fs.root(),
            fs.pwd(),
            mode,
            DeviceId(device as u64),
            fs.node_umask(),
            &cred,
        )?;
        crate::notify::notify_created_entry(&created, crate::notify::IN_CREATE);
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
/// Creates a filesystem node at the given path.
pub fn sys_mknod(path: UserConstPtr<c_char>, mode: u32, device: u32) -> KResult<isize> {
    sys_mknodat(AT_FDCWD, path, mode, device)
}

struct DirBuffer {
    buf: UserPtr<u8>,
    remaining: usize,
}

impl DirBuffer {
    fn new(buf: UserPtr<u8>, len: usize) -> Self {
        Self {
            buf,
            remaining: len,
        }
    }

    fn write_entry(&mut self, d_ino: u64, d_off: i64, d_type: NodeType, name: &[u8]) -> KResult {
        const NAME_OFFSET: usize = offset_of!(linux_dirent64, d_name);

        if name.is_empty() || name.len() >= PATH_MAX as usize || name.contains(&b'/') {
            return Err(KError::Io);
        }
        let len = (NAME_OFFSET + name.len() + 1).next_multiple_of(align_of::<linux_dirent64>());
        if self.remaining < len {
            return Err(KError::InvalidInput);
        }
        let start = self.buf.as_ptr() as usize;
        let end = start.checked_add(len).ok_or(KError::BadAddress)?;
        // Copy only ABI fields and the name. Padding is not output, and the
        // caller's capacity must never become a kernel allocation request.
        UserPtr::from(start + offset_of!(linux_dirent64, d_ino)).write_vm(d_ino)?;
        UserPtr::from(start + offset_of!(linux_dirent64, d_off)).write_vm(d_off)?;
        UserPtr::from(start + offset_of!(linux_dirent64, d_reclen)).write_vm(len as u16)?;
        UserPtr::from(start + offset_of!(linux_dirent64, d_type)).write_vm(d_type as u8)?;
        UserPtr::from(start + NAME_OFFSET + name.len()).write_vm(0u8)?;
        UserPtr::from(start + NAME_OFFSET).write_vm_slice(name)?;

        self.buf = UserPtr::from(end);
        self.remaining -= len;
        Ok(())
    }
}

/// Reads directory entries directly into the caller's `linux_dirent64` buffer.
///
/// Capacity is an unsigned 32-bit count, as in Linux. A later short buffer,
/// user-memory fault or iteration error returns already completed records;
/// the failed entry does not advance the directory position.
pub fn sys_getdents64(fd: i32, buf: UserPtr<u8>, len: u32) -> KResult<isize> {
    debug!(
        "sys_getdents64 <= fd: {fd}, buf: {:?}, len: {len}",
        buf.as_ptr()
    );

    let dir = kprocess::current_files().get_file(fd)?;
    let len = len as usize;
    let mut buffer = DirBuffer::new(buf, len);
    let mut error = Ok(());
    let mut sink = |name: &str, ino, node_type, offset| {
        error = buffer.write_entry(ino, offset as _, node_type, name.as_bytes());
        error.is_ok()
    };
    let mut ctx = DirContext::new(dir.position(), &mut sink);
    let iteration = dir.iterate_dir(&mut ctx);
    let written = len - buffer.remaining;
    if written != 0 {
        return Ok(written as isize);
    }
    iteration?;
    error?;
    Ok(0)
}

/// Gets the current working directory path.
///
/// On success returns the length of the path written to `buf` (including the
/// NUL terminator), matching the Linux `getcwd(2)` convention.  glibc checks
/// the return value against the buffer size and falls back to a manual
/// directory walk (`generic_getcwd`) when the syscall returns a value that
/// exceeds the buffer — an invariant that breaks when we return a raw
/// pointer.
///
/// # Errors
///
/// Returns `KError::OutOfRange` if the unsigned buffer size cannot hold the
/// path and terminator. Otherwise an invalid output pointer, including NULL,
/// returns `KError::BadAddress`; the syscall does not allocate a user buffer.
/// Path-rendering errors are propagated before checking the output buffer.
pub fn sys_getcwd(buf: UserPtr<u8>, size: usize) -> KResult<isize> {
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

unittest::path_mod_test!(getcwd_tests, "../ut-tests/getcwd.rs");

unittest::path_mod_test!(getdents_tests, "../ut-tests/getdents.rs");
