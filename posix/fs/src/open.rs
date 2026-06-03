// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux open/openat compatibility entry points.

use alloc::{format, string::ToString, sync::Arc};
use core::ffi::{c_char, c_int};

use devfs::DeviceFile;
use kerrno::{KError, KResult};
use kfd::FileLike;
use kfs::{Directory, FileBackend, OpenOptions, OpenResult};
use kthread::current_process_state;
use ktty::tty;
use kvfs::{DirEntry, FileNode, Location, NodeType, Reference};
use linux_raw_sys::general::*;
use posix_types::UserConstPtr;

use crate::path::{PathSource, resolve_open_path_source, with_fs};

fn current_effective_ids() -> (u32, u32) {
    (0, 0)
}

/// Converts Linux open flags into internal `OpenOptions`.
fn flags_to_options(flags: c_int, mode: __kernel_mode_t, (uid, gid): (u32, u32)) -> OpenOptions {
    let flags = flags as u32;
    let mut options = OpenOptions::new();
    options.mode(mode).user(uid, gid).open_flags(flags);

    match flags & 0b11 {
        O_RDONLY => options.read(true),
        O_WRONLY => options.write(true),
        _ => options.read(true).write(true),
    };

    if flags & O_APPEND != 0 {
        options.append(true);
    }
    if flags & O_TRUNC != 0 {
        options.truncate(true);
    }
    if flags & O_CREAT != 0 {
        options.create(true);
    }
    if flags & O_PATH != 0 {
        options.path(true);
    }
    if flags & O_EXCL != 0 {
        options.create_new(true);
    }
    if flags & O_DIRECTORY != 0 {
        options.directory(true);
    }
    if flags & O_NOFOLLOW != 0 {
        options.no_follow(true);
    }
    if flags & O_DIRECT != 0 {
        options.direct(true);
    }
    options
}

fn add_to_fd(result: OpenResult, flags: u32) -> KResult<i32> {
    let f: Arc<dyn FileLike> = match result {
        OpenResult::File(mut file) => {
            if let Ok(device) = file.location().entry().downcast::<DeviceFile>() {
                let inner = device.inner().as_any();
                if let Some(ptmx) = inner.downcast_ref::<devfs::Ptmx>() {
                    let (master, pty_number) = ptmx.create_pty()?;
                    let pts = current_process_state()
                        .fs_context()
                        .lock()
                        .resolve("/dev/pts")?;
                    let entry = DirEntry::new_file(
                        FileNode::new(master),
                        NodeType::CharacterDevice,
                        Reference::new(Some(pts.entry().clone()), pty_number.to_string()),
                    );
                    let loc = Location::new(file.location().mountpoint().clone(), entry);
                    file = kfs::File::with_open_flags(
                        FileBackend::Direct(loc),
                        file.flags(),
                        file.open_flags(),
                    );
                } else if inner.is::<tty::CurrentTty>() {
                    let term = kthread::current_thread()
                        .process_state()
                        .proc
                        .group()
                        .session()
                        .terminal()
                        .ok_or(KError::NotFound)?;
                    let path = if term.is::<tty::NTtyDriver>() {
                        "/dev/console".to_string()
                    } else if let Some(pts) = term.downcast_ref::<tty::PtyDriver>() {
                        format!("/dev/pts/{}", pts.pty_number())
                    } else {
                        return Err(KError::OperationNotSupported);
                    };
                    let loc = kthread::current_process_fs_context()
                        .lock()
                        .resolve(&path)?;
                    file = kfs::File::with_open_flags(
                        FileBackend::Direct(loc),
                        file.flags(),
                        file.open_flags(),
                    );
                }
            }
            Arc::new(file)
        }
        OpenResult::Dir(dir) => Arc::new(Directory::new(dir)),
    };
    if flags & O_NONBLOCK != 0 {
        f.set_nonblocking(true)?;
    }
    current_process_state()
        .resources
        .add_file_like(f, flags & O_CLOEXEC != 0)
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
    let raw_flags = flags as u32;

    let mode = mode & !kthread::current_thread().process_state().umask();
    let options = flags_to_options(flags, mode, current_effective_ids());

    if let PathSource::Resolved(path) = resolve_open_path_source(dirfd, Some(&path), raw_flags)?
        && path.is_procfd()
    {
        return options
            .open_loc(path.location().ok_or(KError::InvalidInput)?)
            .and_then(|it| add_to_fd(it, flags as _))
            .map(|fd| fd as isize);
    }
    with_fs(dirfd, |fs| options.open(fs, path))
        .and_then(|it| add_to_fd(it, flags as _))
        .map(|fd| fd as isize)
}

/// Opens a file by path.
#[cfg(target_arch = "x86_64")]
pub fn sys_open(path: UserConstPtr<c_char>, flags: i32, mode: __kernel_mode_t) -> KResult<isize> {
    sys_openat(AT_FDCWD as _, path, flags, mode)
}
