// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor operations.
//!
//! This module implements file descriptor manipulation syscalls including:
//! - Closing files (close, close_range, etc.)
//! - File descriptor duplication (dup, dup2, dup3, etc.)
//! - File descriptor flags and control (fcntl, etc.)

use core::ffi::c_int;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use linux_raw_sys::general::*;
use posix_types::UserPtr;

use crate::file::current_pipe_endpoint;

/// Closes the specified file descriptor.
pub fn sys_close(fd: c_int) -> KResult<isize> {
    debug!("sys_close <= {fd}");
    kthread::current_resources().close_file_like(fd)?;
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

    let proc_state = kthread::current_process_state();
    if flags.contains(CloseRangeFlags::UNSHARE) {
        proc_state.resources.unshare_fd_table();
    }

    if flags.contains(CloseRangeFlags::CLOEXEC) {
        proc_state.resources.set_cloexec_range(first, last);
    } else {
        proc_state.resources.close_range(first, last);
    }

    Ok(0)
}

/// Duplicates a file descriptor and optionally sets `CLOEXEC`.
fn dup_fd(old_fd: c_int, cloexec: bool) -> KResult<isize> {
    let proc_state = kthread::current_process_state();
    let new_fd = proc_state.resources.duplicate_file_like(old_fd, cloexec)?;
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
        kthread::current_resources().get_file_like(new_fd)?;
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

    kthread::current_process_state()
        .resources
        .duplicate_file_like_to(old_fd, new_fd, flags.contains(Dup3Flags::O_CLOEXEC))
        .map(|fd| fd as _)
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
            let mut lock = arg.read_vm()?;
            lock.l_type = F_UNLCK as _;
            arg.write_vm(lock)?;
            Ok(0)
        }
        F_SETFL => {
            kthread::current_resources()
                .get_file_like(fd)?
                .set_nonblocking(arg & (O_NONBLOCK as usize) > 0)?;
            Ok(0)
        }
        F_GETFL => {
            let f = kthread::current_resources().get_file_like(fd)?;

            let mut ret = f.open_flags();
            if f.nonblocking() {
                ret |= O_NONBLOCK;
            }

            Ok(ret as _)
        }
        F_GETFD => {
            let cloexec = kthread::current_process_state().resources.cloexec(fd)?;
            Ok(if cloexec { FD_CLOEXEC as _ } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & FD_CLOEXEC as usize != 0;
            kthread::current_process_state()
                .resources
                .set_cloexec(fd, cloexec)?;
            Ok(0)
        }
        F_GETPIPE_SZ => {
            let pipe = current_pipe_endpoint(fd)?;
            Ok(pipe.capacity() as _)
        }
        F_SETPIPE_SZ => {
            let pipe = current_pipe_endpoint(fd)?;
            pipe.resize(arg)?;
            Ok(pipe.capacity() as _)
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
