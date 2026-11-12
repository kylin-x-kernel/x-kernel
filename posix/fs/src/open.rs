// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `open` and `openat` syscall entry points.

use alloc::{string::String, sync::Arc};
use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult};
use kvfs::{Filename, NodePermission, Openat2How, Path, ResolveFlags, VfsFile, path::PATH_MAX};
use linux_raw_sys::general::*;
use osvm::VirtPtr;
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

/// Reports the fsnotify effects of an open, shared by `openat` and `openat2`
/// (Linux funnels both through `do_file_open`).
fn report_open_notify(file: &Arc<VfsFile>, flags: u32) {
    // Report IN_CREATE only when this call created the entry: O_CREAT on an
    // existing file is an ordinary open and must not report a creation the
    // filesystem never performed. The watch probe gates the *reporting*; the
    // creation identity itself is captured by the open path unconditionally
    // (`VfsFile::created_under`), because a watch can be registered
    // concurrently with this open and a capture that had been skipped then
    // would never be recovered -- a removed or renamed object would keep a
    // watch nothing retires.
    if crate::notify::entry_notify_wanted() && flags & O_CREAT != 0 && file.was_created() {
        // Attribution comes from the created entry itself, not from the
        // caller's argument: when the last component is a symbolic link,
        // creation happens where the link points.
        crate::notify::notify_created(file, crate::notify::IN_CREATE);
    }
    if flags & O_TRUNC != 0
        && !file.is_path()
        && !file.was_created()
        && file.path().is_regular_file()
    {
        crate::notify::notify_modified(file.path());
    }
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
        .inspect(|file| report_open_notify(file, flags as u32))
    })
    .and_then(|it| add_to_fd(it, flags as _))
    .map(|fd| fd as isize)
}

/// Size of `OPEN_HOW_SIZE_VER0` (`uapi/linux/openat2.h`).
const OPEN_HOW_SIZE_VER0: usize = 24;
/// Reads `struct open_how` the way Linux `copy_struct_from_user()` does.
///
/// A `size` below `OPEN_HOW_SIZE_VER0` is `EINVAL`; a larger `size` is
/// accepted only when the bytes past the fields this kernel knows are zero,
/// otherwise `E2BIG`.
fn read_open_how(how: UserConstPtr<u8>, size: usize) -> KResult<(u64, u64, u64)> {
    if size < OPEN_HOW_SIZE_VER0 {
        return Err(KError::InvalidInput);
    }
    if size > PATH_MAX {
        // Linux reads the whole caller-supplied size byte by byte; cap it so a
        // hostile size cannot pin us in the kernel.
        return Err(KError::ArgumentListTooLong);
    }
    let base = how.as_ptr() as usize;
    let word = |at: usize| {
        UserConstPtr::<u64>::from(base + at)
            .read_vm()
            .map(u64::from_le)
    };
    let (flags, mode, resolve) = (word(0)?, word(8)?, word(16)?);
    // Bytes past `OPEN_HOW_SIZE_VER0` are reserved: Linux `copy_struct_from_user`
    // answers E2BIG when any of them is non-zero.
    for offset in OPEN_HOW_SIZE_VER0..size {
        if UserConstPtr::<u8>::from(base + offset).read_vm()? != 0 {
            return Err(KError::ArgumentListTooLong);
        }
    }
    Ok((flags, mode, resolve))
}

/// Opens a file relative to a directory fd with `openat2(2)` resolution.
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's credentials,
/// filesystem context and fd table. It may sleep (pathname lookup, filesystem
/// callbacks and fd allocation), so it must not be called from IRQ context or
/// while holding a spinlock or IRQ-preemption guard.
pub fn sys_openat2(
    dirfd: c_int,
    path: UserConstPtr<c_char>,
    how: UserConstPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let (flags, mode, resolve) = read_open_how(how, size)?;
    // Validate the whole `open_how` before touching the pathname or the
    // directory fd, so a malformed request reports `EINVAL` rather than
    // `EFAULT`/`EBADF`/`ENOTDIR` from later stages.
    let how = Openat2How {
        flags,
        mode,
        resolve,
    }
    .validate()?;
    // Linux `getname` reports `ENAMETOOLONG` for a pathname that reaches
    // `PATH_MAX`; the bounded loader signals that as `OutOfRange`.
    let bytes = path.load_bytes_with_max_len(PATH_MAX - 1).map_err(|err| {
        if err.canonicalize() == KError::OutOfRange {
            KError::NameTooLong
        } else {
            err
        }
    })?;
    let path = String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)?;
    debug!("sys_openat2 <= {dirfd} {path:?} flags={flags:#x} resolve={resolve:#x}");
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    let resolve_flags = ResolveFlags::from_bits_truncate(resolve);
    let anchored = resolve_flags.intersects(ResolveFlags::IN_ROOT | ResolveFlags::BENEATH);
    // `RESOLVE_BENEATH` rejects an absolute pathname with `EXDEV` before the
    // dirfd is consulted (Linux `path_init`/`nd_jump_root`), so a bad or
    // non-directory dirfd must not surface as `EBADF`/`ENOTDIR` here.
    if resolve_flags.contains(ResolveFlags::BENEATH) && path.as_str().starts_with('/') {
        return Err(KError::CrossesDevices);
    }
    let fs = kprocess::current_user_process_fs_context()
        .lock()
        .snapshot();
    // A plain absolute pathname is independent of the directory fd; only an
    // anchored request (`IN_ROOT`/`BENEATH`) or a relative pathname makes the
    // fd meaningful.
    let base = if !path.as_str().starts_with('/') || anchored {
        if dirfd == AT_FDCWD as c_int {
            fs.pwd().clone()
        } else {
            let file = kprocess::current_files().get_file(dirfd)?;
            if !file.is_dir() {
                return Err(KError::NotADirectory);
            }
            file.path().clone()
        }
    } else {
        fs.root().clone()
    };
    // `RESOLVE_IN_ROOT` and `RESOLVE_BENEATH` anchor the walk at the directory
    // fd; `Nameidata` owns the rebasing/clamping rules.
    let root = if anchored {
        base.clone()
    } else {
        fs.root().clone()
    };

    filename
        .open_with_openat2_at(&root, &base, how, fs.node_umask(), cred)
        .inspect(|file| report_open_notify(file, flags as u32))
        .and_then(|file| add_to_fd(file, flags as u32))
        .map(|fd| fd as isize)
}

/// Opens a file by path.
#[cfg(target_arch = "x86_64")]
pub fn sys_open(path: UserConstPtr<c_char>, flags: i32, mode: __kernel_mode_t) -> KResult<isize> {
    sys_openat(AT_FDCWD as _, path, flags, mode)
}
