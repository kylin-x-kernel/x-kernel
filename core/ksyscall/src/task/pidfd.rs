// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process file descriptor syscalls.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kthread::PidFd;
use posix_types::{UserConstPtr, k_siginfo};

use super::make_queue_signal_info;

/// Create a process file descriptor for the specified process.
pub fn sys_pidfd_open(pid: u32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_open <= pid: {pid}, flags: {flags}");

    if flags != 0 {
        return Err(KError::InvalidInput);
    }

    let task = kthread::get_process_state(pid)?;
    let fd = PidFd::new(&task);

    kthread::current_resources()
        .add_file_like(Arc::new(fd), true)
        .map(|fd| fd as _)
}

/// Duplicate a file descriptor from another process using its pidfd.
pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    let pidfd = kthread::current_resources().get_file_like_as::<PidFd>(pidfd)?;
    let proc_state = pidfd.process_state()?;
    proc_state
        .resources
        .fd_table()
        .read()
        .get(target_fd as usize)
        .ok_or(KError::BadFileDescriptor)
        .and_then(|fd| {
            let fd = kthread::current_resources().add_file_like(fd.inner().clone(), true)?;
            Ok(fd as isize)
        })
}

/// Send a signal to the process referenced by the pidfd.
pub fn sys_pidfd_send_signal(
    pidfd: i32,
    signo: u32,
    sig: UserConstPtr<k_siginfo>,
    flags: u32,
) -> KResult<isize> {
    if flags != 0 {
        return Err(KError::InvalidInput);
    }

    let pidfd = kthread::current_resources().get_file_like_as::<PidFd>(pidfd)?;
    let pid = pidfd.process_state()?.proc.pid();

    let sig = make_queue_signal_info(pid, signo, sig)?;
    kthread::send_signal_to_process(pid, sig)?;
    Ok(0)
}
