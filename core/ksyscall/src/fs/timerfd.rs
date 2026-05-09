// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timer file descriptor syscalls.
//!
//! This module implements timer notification operations including:
//! - Timer file creation (timerfd_create)
//! - Timer arming/disarming (timerfd_settime)
//! - Timer query (timerfd_gettime)

use kerrno::{KError, KResult};
use kfd::FileLike;
use kservices::file::timerfd::TimerFd;
use linux_raw_sys::general::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_REALTIME, TFD_CLOEXEC, TFD_NONBLOCK, TFD_TIMER_ABSTIME,
    itimerspec, timespec,
};
use osvm::{VirtMutPtr, VirtPtr};
use posix_types::TimeValueLike;

/// Creates a timerfd file descriptor.
pub fn sys_timerfd_create(clock_id: i32, flags: u32) -> KResult<isize> {
    debug!("sys_timerfd_create <= clock_id: {clock_id}, flags: {flags:#x}");

    // Validate clock_id.
    match clock_id as u32 {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME => {}
        _ => return Err(KError::InvalidInput),
    }

    // Validate flags (only TFD_CLOEXEC and TFD_NONBLOCK are valid).
    let unknown = flags & !(TFD_CLOEXEC | TFD_NONBLOCK);
    if unknown != 0 {
        return Err(KError::InvalidInput);
    }

    let tfd = TimerFd::new(clock_id as u32);
    if flags & TFD_NONBLOCK != 0 {
        tfd.set_nonblocking(true)?;
    }

    kthread::current_resources()
        .add_file_like(tfd as _, flags & TFD_CLOEXEC != 0)
        .map(|fd| fd as _)
}

/// Arms or disarms the timer referred to by `fd`.
pub fn sys_timerfd_settime(
    fd: i32,
    flags: u32,
    new_value: *const itimerspec,
    old_value: *mut itimerspec,
) -> KResult<isize> {
    debug!("sys_timerfd_settime <= fd: {fd}, flags: {flags:#x}");

    // Only TFD_TIMER_ABSTIME is valid.
    let unknown = flags & !TFD_TIMER_ABSTIME;
    if unknown != 0 {
        return Err(KError::InvalidInput);
    }

    let absolute = flags & TFD_TIMER_ABSTIME != 0;

    // SAFETY: reading plain-old-data from user space.
    let new = unsafe { new_value.read_uninit()?.assume_init() };
    let value = new.it_value.try_into_time_value()?;
    let interval = new.it_interval.try_into_time_value()?;

    let tfd = kthread::current_resources().get_file_like_as::<TimerFd>(fd)?;
    let (old_interval, old_remaining) = tfd.settime(absolute, value, interval);

    if let Some(old_value) = old_value.check_non_null() {
        old_value.write_vm(itimerspec {
            it_interval: timespec::from_time_value(old_interval),
            it_value: timespec::from_time_value(old_remaining),
        })?;
    }

    Ok(0)
}

/// Returns the current setting of the timer referred to by `fd`.
pub fn sys_timerfd_gettime(fd: i32, curr_value: *mut itimerspec) -> KResult<isize> {
    debug!("sys_timerfd_gettime <= fd: {fd}");

    let tfd = kthread::current_resources().get_file_like_as::<TimerFd>(fd)?;
    let (interval, remaining) = tfd.gettime();

    curr_value.write_vm(itimerspec {
        it_interval: timespec::from_time_value(interval),
        it_value: timespec::from_time_value(remaining),
    })?;

    Ok(0)
}
