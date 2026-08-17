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
use kvfs::{FMode, OpenFlags, pipe::PipeObject};
use linux_raw_sys::general::*;
use memfs::shmem;
use posix_types::UserPtr;

const SETFL_MASK: OpenFlags = OpenFlags::APPEND
    .union(OpenFlags::NONBLOCK)
    .union(OpenFlags::DIRECT)
    .union(OpenFlags::NO_ATIME);

/// Closes the specified file descriptor.
pub fn sys_close(fd: c_int) -> KResult<isize> {
    debug!("sys_close <= {fd}");
    kprocess::current_resources().close_file(fd)?;
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

    let resources = kprocess::current_user_process().resources()?;
    if flags.contains(CloseRangeFlags::UNSHARE) {
        resources.unshare_fd_table()?;
    }

    if flags.contains(CloseRangeFlags::CLOEXEC) {
        resources.set_cloexec_range(first, last)?;
    } else {
        resources.close_range(first, last)?;
    }

    Ok(0)
}

/// Duplicates a file descriptor and optionally sets `CLOEXEC`.
fn dup_fd(old_fd: c_int, cloexec: bool) -> KResult<isize> {
    let resources = kprocess::current_user_process().resources()?;
    let new_fd = resources.duplicate_file(old_fd, cloexec)?;
    Ok(new_fd as _)
}

fn seal_bits_from_fcntl_arg(arg: usize) -> KResult<u32> {
    u32::try_from(arg).map_err(|_| KError::InvalidInput)
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
        kprocess::current_resources().get_file(new_fd)?;
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

    kprocess::current_user_process()
        .resources()?
        .duplicate_file_to(old_fd, new_fd, flags.contains(Dup3Flags::O_CLOEXEC))
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
            let mut flags = u32::try_from(arg).map_err(|_| KError::InvalidInput)?;
            if O_NONBLOCK != O_NDELAY && flags & O_NDELAY != 0 {
                flags |= O_NONBLOCK;
            }
            kprocess::current_resources()
                .get_file(fd)?
                .replace_flags(SETFL_MASK, OpenFlags::from_bits_retain(flags));
            Ok(0)
        }
        F_GETFL => Ok(kprocess::current_resources().get_file(fd)?.flags().bits() as _),
        F_GETFD => {
            let cloexec = kprocess::current_user_process().resources()?.cloexec(fd)?;
            Ok(if cloexec { FD_CLOEXEC as _ } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & FD_CLOEXEC as usize != 0;
            kprocess::current_user_process()
                .resources()?
                .set_cloexec(fd, cloexec)?;
            Ok(0)
        }
        F_GETPIPE_SZ => {
            let file = kprocess::current_resources().get_file(fd)?;
            let pipe = PipeObject::from_file(&file)?;
            Ok(pipe.capacity() as _)
        }
        F_SETPIPE_SZ => {
            let file = kprocess::current_resources().get_file(fd)?;
            let pipe = PipeObject::from_file(&file)?;
            pipe.resize(arg)?;
            Ok(pipe.capacity() as _)
        }
        F_GET_SEALS => {
            let file = kprocess::current_resources().get_file(fd)?;
            Ok(shmem::seal_bits_for_location(file.path())? as _)
        }
        F_ADD_SEALS => {
            let file = kprocess::current_resources().get_file(fd)?;
            file.verify_mode(FMode::WRITE)?;
            shmem::add_seals_for_location(file.path(), seal_bits_from_fcntl_arg(arg)?)?;
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
