// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process file descriptor syscalls.
//!
//! This module implements process file descriptor operations including:
//! - Create pidfd (pidfd_open, etc.)
//! - Pidfd operations (pidfd_getfd, pidfd_send_signal, etc.)
//! - Process monitoring through pidfds

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kservices::file::PidFd;
use ksignal::SignalInfo;
use kthread::{get_process_state, send_signal_to_process};
use posix_signal::make_queue_signal_info;

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

    // Get the process state for the specified PID
    let task = get_process_state(pid)?;
    // Create a new pidfd object wrapping the process
    let fd = PidFd::new(&task);

    // Add the pidfd to the current process's file descriptor table
    kthread::current_resources()
        .add_file_like(Arc::new(fd), true)
        .map(|fd| fd as _)
}

/// Get a duplicate of a file descriptor from another process using its pidfd
///
/// This allows access to a file descriptor in another process by first opening
/// that process with pidfd_open, then using this syscall to duplicate one of its fds.
/// The duplicated fd is added to the current process's file descriptor table.
pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    let pidfd = kthread::current_resources().get_file_like_as::<PidFd>(pidfd)?;
    // Get the process state that this pidfd refers to
    let proc_state = pidfd.process_state()?;
    // Access the target process's file descriptor table within its scope
    proc_state
        .resources
        .fd_table()
        .read()
        // Get the file descriptor at the specified index
        .get(target_fd as usize)
        .ok_or(KError::BadFileDescriptor)
        // Duplicate the file and add it to current process's fd table
        .and_then(|fd| {
            let fd = kthread::current_resources().add_file_like(fd.inner().clone(), true)?;
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

    let pidfd = kthread::current_resources().get_file_like_as::<PidFd>(pidfd)?;
    let pid = pidfd.process_state()?.proc.pid();

    // Create signal info from user-provided data and send the signal
    let sig = make_queue_signal_info(pid, signo, sig)?;
    send_signal_to_process(pid, sig)?;
    Ok(0)
}
