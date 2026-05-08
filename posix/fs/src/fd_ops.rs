// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor operations.
//!
//! This module implements file descriptor manipulation syscalls including:
//! - Closing files (close, close_range, etc.)
//! - File descriptor duplication (dup, dup2, dup3, etc.)
//! - File descriptor flags and control (fcntl, etc.)

use core::{
    ffi::c_int,
    mem,
    ops::{Deref, DerefMut},
};

use bitflags::bitflags;
use kcore::task::AsThread;
use kerrno::{KError, KResult};
use kservices::file::{FD_TABLE, FileLike, Pipe, add_file_like, close_file_like, get_file_like};
use ktask::current;
use linux_raw_sys::general::*;
use osvm::{VirtMutPtr, VirtPtr};
use posix_types::UserPtr;

/// Closes the specified file descriptor.
pub fn sys_close(fd: c_int) -> KResult<isize> {
    debug!("sys_close <= {fd}");
    close_file_like(fd)?;
    Ok(0)
}

bitflags! {
    #[derive(Debug, Clone, Copy)]
    struct CloseRangeFlags: u32 {
        const UNSHARE = 1 << 1;
        const CLOEXEC = 1 << 2;
    }
}

/// Closes a range of file descriptors.
pub fn sys_close_range(first: i32, last: i32, flags: u32) -> KResult<isize> {
    if first < 0 || last < first {
        return Err(KError::InvalidInput);
    }
    let flags = CloseRangeFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_close_range <= fds: [{first}, {last}], flags: {flags:?}");
    if flags.contains(CloseRangeFlags::UNSHARE) {
        // TODO: optimize
        let curr = current();
        let mut scope = curr.as_thread().proc_data.scope.write();
        let mut guard = FD_TABLE.scope_mut(&mut scope);
        let old_files = mem::take(guard.deref_mut());
        old_files.write().clone_from(old_files.read().deref());
    }

    let cloexec = flags.contains(CloseRangeFlags::CLOEXEC);
    let mut fd_table = FD_TABLE.write();
    if let Some(max_index) = fd_table.ids().next_back() {
        for fd in first..=last.min(max_index as i32) {
            if cloexec {
                if let Some(f) = fd_table.get_mut(fd as _) {
                    f.cloexec = true;
                }
            } else {
                fd_table.remove(fd as _);
            }
        }
    }

    Ok(0)
}

/// Duplicates a file descriptor and optionally sets `CLOEXEC`.
fn dup_fd(old_fd: c_int, cloexec: bool) -> KResult<isize> {
    let f = get_file_like(old_fd)?;
    let new_fd = add_file_like(f, cloexec)?;
    Ok(new_fd as _)
}

/// Duplicates a file descriptor.
pub fn sys_dup(old_fd: c_int) -> KResult<isize> {
    debug!("sys_dup <= {old_fd}");
    dup_fd(old_fd, false)
}

#[cfg(target_arch = "x86_64")]
/// Duplicates a file descriptor to a specific target fd.
pub fn sys_dup2(old_fd: c_int, new_fd: c_int) -> KResult<isize> {
    if old_fd == new_fd {
        get_file_like(new_fd)?;
        return Ok(new_fd as _);
    }
    sys_dup3(old_fd, new_fd, 0)
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Dup3Flags: c_int {
        const O_CLOEXEC = O_CLOEXEC as _; // Close on exec
    }
}

/// Duplicates a file descriptor with additional flags.
pub fn sys_dup3(old_fd: c_int, new_fd: c_int, flags: c_int) -> KResult<isize> {
    let flags = Dup3Flags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_dup3 <= old_fd: {old_fd}, new_fd: {new_fd}, flags: {flags:?}");

    if old_fd == new_fd {
        return Err(KError::InvalidInput);
    }

    let mut fd_table = FD_TABLE.write();
    let mut f = fd_table
        .get(old_fd as _)
        .cloned()
        .ok_or(KError::BadFileDescriptor)?;
    f.cloexec = flags.contains(Dup3Flags::O_CLOEXEC);

    fd_table.remove(new_fd as _);
    fd_table
        .add_at(new_fd as _, f)
        .map_err(|_| KError::BadFileDescriptor)?;

    Ok(new_fd as _)
}

/// Performs file descriptor control operations.
pub fn sys_fcntl(fd: c_int, cmd: c_int, arg: usize) -> KResult<isize> {
    debug!("sys_fcntl <= fd: {fd} cmd: {cmd} arg: {arg}");

    match cmd as u32 {
        F_DUPFD => dup_fd(fd, false),
        F_DUPFD_CLOEXEC => dup_fd(fd, true),
        F_SETLK | F_SETLKW => Ok(0),
        F_OFD_SETLK | F_OFD_SETLKW => Ok(0),
        F_GETLK | F_OFD_GETLK => {
            let arg = UserPtr::<flock64>::from(arg);
            let mut lock = unsafe { arg.read_uninit()?.assume_init() };
            lock.l_type = F_UNLCK as _;
            arg.write_vm(lock)?;
            Ok(0)
        }
        F_SETFL => {
            get_file_like(fd)?.set_nonblocking(arg & (O_NONBLOCK as usize) > 0)?;
            Ok(0)
        }
        F_GETFL => {
            let f = get_file_like(fd)?;

            let mut ret = f.open_flags();
            if f.nonblocking() {
                ret |= O_NONBLOCK;
            }

            Ok(ret as _)
        }
        F_GETFD => {
            let cloexec = FD_TABLE
                .read()
                .get(fd as _)
                .ok_or(KError::BadFileDescriptor)?
                .cloexec;
            Ok(if cloexec { FD_CLOEXEC as _ } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & FD_CLOEXEC as usize != 0;
            FD_TABLE
                .write()
                .get_mut(fd as _)
                .ok_or(KError::BadFileDescriptor)?
                .cloexec = cloexec;
            Ok(0)
        }
        F_GETPIPE_SZ => {
            let pipe = Pipe::from_fd(fd)?;
            Ok(pipe.capacity() as _)
        }
        F_SETPIPE_SZ => {
            let pipe = Pipe::from_fd(fd)?;
            pipe.resize(arg)?;
            Ok(0)
        }
        _ => {
            warn!("unsupported fcntl parameters: cmd: {cmd}");
            Ok(0)
        }
    }
}

/// Applies or removes an advisory lock on a file descriptor.
pub fn sys_flock(fd: c_int, operation: c_int) -> KResult<isize> {
    debug!("flock <= fd: {fd}, operation: {operation}");
    // TODO: flock
    Ok(0)
}
