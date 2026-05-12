// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX sleep syscalls.

use kerrno::{KError, KResult};
use khal::time::TimeValue;
use ktask::future::{block_on, interruptible, sleep};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_MONOTONIC, CLOCK_REALTIME, TIMER_ABSTIME, timespec,
};
use posix_types::{TimeValueLike, UserConstPtr, UserPtr};

fn sleep_impl(clock: impl Fn() -> TimeValue, dur: TimeValue) -> TimeValue {
    debug!("sleep_impl <= {dur:?}");

    let start = clock();

    // TODO: currently ignoring concrete clock type.
    // We detect EINTR manually if the slept time is not enough.
    let _ = block_on(interruptible(sleep(dur)));

    clock() - start
}

/// Sleeps for the requested duration.
pub fn sys_nanosleep(req: UserConstPtr<timespec>, rem: UserPtr<timespec>) -> KResult<isize> {
    let req = req.read_vm()?.try_into_time_value()?;
    debug!("sys_nanosleep <= req: {req:?}");

    let actual = sleep_impl(khal::time::monotonic_time, req);

    if let Some(diff) = req.checked_sub(actual) {
        debug!("sys_nanosleep => rem: {diff:?}");
        if let Some(rem) = rem.check_non_null() {
            rem.write_vm(timespec::from_time_value(diff))?;
        }
        Err(KError::Interrupted)
    } else {
        Ok(0)
    }
}

/// Sleeps against a specific clock, optionally using an absolute deadline.
pub fn sys_clock_nanosleep(
    clock_id: __kernel_clockid_t,
    flags: u32,
    req: UserConstPtr<timespec>,
    rem: UserPtr<timespec>,
) -> KResult<isize> {
    let clock = match clock_id as u32 {
        CLOCK_REALTIME => khal::time::wall_time,
        CLOCK_MONOTONIC => khal::time::monotonic_time,
        _ => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(KError::InvalidInput);
        }
    };

    let req = req.read_vm()?.try_into_time_value()?;
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, req: {req:?}");

    let dur = if flags & TIMER_ABSTIME != 0 {
        req.saturating_sub(clock())
    } else {
        req
    };

    let actual = sleep_impl(clock, dur);

    if let Some(diff) = dur.checked_sub(actual) {
        debug!("sys_clock_nanosleep => rem: {diff:?}");
        if let Some(rem) = rem.check_non_null() {
            rem.write_vm(timespec::from_time_value(diff))?;
        }
        Err(KError::Interrupted)
    } else {
        Ok(0)
    }
}
