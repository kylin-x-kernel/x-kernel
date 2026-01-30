// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process file descriptor syscalls.
//!
//! This module implements process file descriptor operations including:
//! - Create pidfd (pidfd_open, etc.)
//! - Pidfd operations (pidfd_getfd, pidfd_send_signal, etc.)
//! - Process monitoring through pidfds

use kcore::task::{get_process_data, send_signal_to_process};
use kerrno::{KError, KResult};
use ksignal::SignalInfo;

use crate::{
    file::{FD_TABLE, FileLike, PidFd, add_file_like},
    syscall::signal::make_queue_signal_info,
};

/// Create a process file descriptor (pidfd) for the specified process
///
/// A pidfd is a file descriptor that refers to a process and can be used to perform
/// operations on that process without holding a reference to the process object.
/// Flags must be 0 (no additional options currently supported).
pub fn sys_pidfd_open(pid: u32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_open <= pid: {pid}, flags: {flags}");

    // No flags are currently supported - must be 0
    if flags != 0 {
        return Err(KError::InvalidInput);
    }

    // Get the process data for the specified PID
    let task = get_process_data(pid)?;
    // Create a new pidfd object wrapping the process
    let fd = PidFd::new(&task);

    // Add the pidfd to the current process's file descriptor table
    fd.add_to_fd_table(true).map(|fd| fd as _)
}

/// Get a duplicate of a file descriptor from another process using its pidfd
///
/// This allows access to a file descriptor in another process by first opening
/// that process with pidfd_open, then using this syscall to duplicate one of its fds.
/// The duplicated fd is added to the current process's file descriptor table.
pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    // Get the pidfd object and validate it
    let pidfd = PidFd::from_fd(pidfd)?;
    // Get the process data that this pidfd refers to
    let proc_data = pidfd.process_data()?;
    // Access the target process's file descriptor table within its scope
    FD_TABLE
        .scope(&proc_data.scope.read())
        .read()
        // Get the file descriptor at the specified index
        .get(target_fd as usize)
        .ok_or(KError::BadFileDescriptor)
        // Duplicate the file and add it to current process's fd table
        .and_then(|fd| {
            let fd = add_file_like(fd.inner.clone(), true)?;
            Ok(fd as isize)
        })
}

/// Send a signal to the process referenced by the pidfd
///
/// This allows sending signals to processes using their process file descriptors.
/// The signal can optionally carry additional data via SignalInfo.
/// Flags must be 0 (no additional options currently supported).
pub fn sys_pidfd_send_signal(
    pidfd: i32,
    signo: u32,
    sig: *mut SignalInfo,
    flags: u32,
) -> KResult<isize> {
    // No flags are currently supported - must be 0
    if flags != 0 {
        return Err(KError::InvalidInput);
    }

    // Get the pidfd object and retrieve the process it refers to
    let pidfd = PidFd::from_fd(pidfd)?;
    let pid = pidfd.process_data()?.proc.pid();

    // Create signal info from user-provided data and send the signal
    let sig = make_queue_signal_info(pid, signo, sig)?;
    send_signal_to_process(pid, sig)?;
    Ok(0)
}
