// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux stat and access compatibility entry points.

use core::ffi::{c_char, c_int};

use fs_ng_vfs::{Location, NodePermission};
use kerrno::{KError, KResult};
use kservices::file::File;
use kthread::current_process_state;
use linux_raw_sys::general::{
    __kernel_fsid_t, AT_EMPTY_PATH, R_OK, W_OK, X_OK, stat, statfs, statx,
};
use osvm::VirtMutPtr;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::resolve_at;

/// Get the file metadata by `path` and write into `statbuf`.
#[cfg(target_arch = "x86_64")]
pub fn sys_stat(path: UserConstPtr<c_char>, statbuf: UserPtr<stat>) -> KResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_fstatat(AT_FDCWD, path, statbuf, 0)
}

/// Get file metadata by `fd` and write into `statbuf`.
pub fn sys_fstat(fd: i32, statbuf: UserPtr<stat>) -> KResult<isize> {
    sys_fstatat(fd, UserConstPtr::default(), statbuf, AT_EMPTY_PATH)
}

/// Get the metadata of the symbolic link and write into `buf`.
#[cfg(target_arch = "x86_64")]
pub fn sys_lstat(path: UserConstPtr<c_char>, statbuf: UserPtr<stat>) -> KResult<isize> {
    use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

    sys_fstatat(AT_FDCWD, path, statbuf, AT_SYMLINK_NOFOLLOW)
}

/// Gets file metadata relative to a directory file descriptor.
pub fn sys_fstatat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    statbuf: UserPtr<stat>,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;

    debug!("sys_fstatat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    statbuf.write_vm(loc.stat()?.into())?;

    Ok(0)
}

/// Gets extended file metadata (statx).
pub fn sys_statx(
    dirfd: c_int,
    path: UserConstPtr<c_char>,
    flags: u32,
    _mask: u32,
    statxbuf: UserPtr<statx>,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    debug!("sys_statx <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    statxbuf.write_vm(resolve_at(dirfd, path.as_deref(), flags)?.stat()?.into())?;

    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_access(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_faccessat2(AT_FDCWD, path, mode, 0)
}

/// Checks file accessibility with additional flags.
pub fn sys_faccessat2(
    dirfd: c_int,
    path: UserConstPtr<c_char>,
    mode: u32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    debug!("sys_faccessat2 <= dirfd: {dirfd}, path: {path:?}, mode: {mode}, flags: {flags}");

    let file = resolve_at(dirfd, path.as_deref(), flags)?;

    if mode == 0 {
        return Ok(0);
    }
    let mut required_mode = NodePermission::empty();
    if mode & R_OK != 0 {
        required_mode |= NodePermission::OWNER_READ;
    }
    if mode & W_OK != 0 {
        required_mode |= NodePermission::OWNER_WRITE;
    }
    if mode & X_OK != 0 {
        required_mode |= NodePermission::OWNER_EXEC;
    }
    let required_mode = required_mode.bits();
    if (file.stat()?.mode as u16 & required_mode) != required_mode {
        return Err(KError::PermissionDenied);
    }

    Ok(0)
}

fn statfs(loc: &Location) -> KResult<statfs> {
    let stat = loc.filesystem().stat()?;
    let mut result: statfs = unsafe { core::mem::zeroed() };
    result.f_type = stat.fs_type as _;
    result.f_bsize = stat.block_size as _;
    result.f_blocks = stat.blocks as _;
    result.f_bfree = stat.blocks_free as _;
    result.f_bavail = stat.blocks_available as _;
    result.f_files = stat.file_count as _;
    result.f_ffree = stat.free_file_count as _;
    result.f_fsid = __kernel_fsid_t {
        val: [0, loc.mountpoint().device() as _],
    };
    result.f_namelen = stat.name_length as _;
    result.f_frsize = stat.fragment_size as _;
    result.f_flags = stat.mount_flags as _;
    Ok(result)
}

/// Gets filesystem statistics by path.
pub fn sys_statfs(path: UserConstPtr<c_char>, buf: UserPtr<statfs>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_statfs <= path: {path:?}");

    buf.write_vm(statfs(
        &current_process_state()
            .fs_context()
            .lock()
            .resolve(path)?
            .mountpoint()
            .root_location(),
    )?)?;
    Ok(0)
}

/// Gets filesystem statistics by file descriptor.
pub fn sys_fstatfs(fd: i32, buf: UserPtr<statfs>) -> KResult<isize> {
    debug!("sys_fstatfs <= fd: {fd}");

    let proc_state = current_process_state();
    let file = proc_state.resources.get_file_like_as::<File>(fd)?;
    buf.write_vm(statfs(file.inner().location())?)?;
    Ok(0)
}
