// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process file descriptor syscalls.

use kerrno::{KError, KResult};
use kprocess::PidFd;
use linux_raw_sys::general::O_RDWR;
use posix_types::{UserConstPtr, k_siginfo};

use super::make_queue_signal_info;

/// Create a process file descriptor for the specified process.
pub fn sys_pidfd_open(pid: u32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_open <= pid: {pid}, flags: {flags}");

    if flags != 0 {
        return Err(KError::InvalidInput);
    }

    let process = kprocess::pidfd::open_target_process(pid)?;
    kprocess::current_resources()
        .add_file(PidFd::new_file(&process, O_RDWR)?, true)
        .map(|fd| fd as _)
}

/// Duplicate a file descriptor from another process using its pidfd.
pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> KResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    let pidfd_file = kprocess::current_resources().get_file(pidfd)?;
    let pidfd = PidFd::from_file(&pidfd_file)?;
    pidfd
        .live_process()?
        .resources()?
        .fd_table()
        .read()
        .get(target_fd as usize)
        .ok_or(KError::BadFileDescriptor)
        .and_then(|fd| {
            let fd = kprocess::current_resources().add_file(fd.file().clone(), true)?;
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

    let pidfd_file = kprocess::current_resources().get_file(pidfd)?;
    let pidfd = PidFd::from_file(&pidfd_file)?;
    let process = pidfd.live_process()?;
    let pid = process.pid();

    let sig = make_queue_signal_info(pid, signo, sig)?;
    kprocess::process_signals::send_to_process_ref(process, sig)?;
    Ok(0)
}
