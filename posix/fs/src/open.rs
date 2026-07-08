// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `open` and `openat` syscall entry points.

use alloc::sync::Arc;
use core::ffi::{c_char, c_int};

use kerrno::KResult;
use kprocess::current_user_process;
use kvfs::{Filename, NodePermission, Path, VfsFile};
use linux_raw_sys::general::*;
use posix_types::UserConstPtr;

use crate::path::with_fs;

fn open_path(
    root: &Path,
    base: &Path,
    path: &str,
    flags: u32,
    mode: __kernel_mode_t,
) -> KResult<Arc<VfsFile>> {
    let permission = NodePermission::from_bits_truncate(mode as _);
    Filename::new(path).open_with_flags_at(root, base, flags, permission)
}

fn add_to_fd(file: Arc<VfsFile>, flags: u32) -> KResult<i32> {
    current_user_process()
        .resources()?
        .add_file(file, flags & O_CLOEXEC != 0)
}

/// Opens a file relative to a directory file descriptor.
pub fn sys_openat(
    dirfd: c_int,
    path: UserConstPtr<c_char>,
    flags: i32,
    mode: __kernel_mode_t,
) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_openat <= {dirfd} {path:?} {flags:#o} {mode:#o}");
    let mode = mode & !kprocess::current_umask();

    with_fs(dirfd, |fs| {
        open_path(fs.root(), fs.pwd(), path.as_str(), flags as u32, mode)
    })
    .and_then(|it| add_to_fd(it, flags as _))
    .map(|fd| fd as isize)
}

/// Opens a file by path.
#[cfg(target_arch = "x86_64")]
pub fn sys_open(path: UserConstPtr<c_char>, flags: i32, mode: __kernel_mode_t) -> KResult<isize> {
    sys_openat(AT_FDCWD as _, path, flags, mode)
}
