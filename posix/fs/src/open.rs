// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `open` and `openat` syscall entry points.

use alloc::sync::Arc;
use core::ffi::{c_char, c_int};

use kerrno::KResult;
use kvfs::{Filename, NodePermission, Path, VfsFile};
use linux_raw_sys::general::*;
use posix_types::UserConstPtr;

use crate::path::with_fs_at;

fn open_path(
    root: &Path,
    base: &Path,
    filename: &Filename,
    flags: u32,
    mode: __kernel_mode_t,
    umask: NodePermission,
    cred: Arc<kcred::Cred>,
) -> KResult<Arc<VfsFile>> {
    let permission = NodePermission::from_bits_truncate(mode as _);
    filename.open_with_flags_at(root, base, flags, permission, umask, cred)
}

fn add_to_fd(file: Arc<VfsFile>, flags: u32) -> KResult<i32> {
    kprocess::current_files().add_file(file, flags & O_CLOEXEC != 0)
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
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    with_fs_at(dirfd, &filename, |fs| {
        open_path(
            fs.root(),
            fs.pwd(),
            &filename,
            flags as u32,
            mode,
            fs.node_umask(),
            cred,
        )
        .inspect(|file| {
            // Report IN_CREATE only when this call created the entry: O_CREAT on
            // an existing file is an ordinary open and must not report a
            // creation the filesystem never performed. The watch probe gates
            // the *reporting*; the creation identity itself is captured by the
            // open path unconditionally (`VfsFile::created_under`), because a
            // watch can be registered concurrently with this open and a
            // capture that had been skipped then would never be recovered -- a
            // removed or renamed object would keep a watch nothing retires.
            if crate::notify::entry_notify_wanted()
                && flags as u32 & O_CREAT != 0
                && file.was_created()
            {
                // Attribution comes from the created entry itself, not from the caller's
                // argument: when the last component is a symbolic link, creation
                // happens where the link points, and `entry_parts_at` (which
                // does not follow the final component) would report the event in
                // the link's directory for a name that was never created.
                crate::notify::notify_created(file, crate::notify::IN_CREATE);
            }
        })
    })
    .and_then(|it| add_to_fd(it, flags as _))
    .map(|fd| fd as isize)
}

/// Opens a file by path.
#[cfg(target_arch = "x86_64")]
pub fn sys_open(path: UserConstPtr<c_char>, flags: i32, mode: __kernel_mode_t) -> KResult<isize> {
    sys_openat(AT_FDCWD as _, path, flags, mode)
}
