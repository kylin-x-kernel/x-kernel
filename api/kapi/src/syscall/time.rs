// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related syscalls.
//!
//! This module implements time and timer operations including:
//! - Clock operations (clock_gettime, clock_settime, clock_getres, etc.)
//! - Time queries (gettimeofday, gettime, etc.)
//! - Timer management (setitimer, getitimer, timer_*, etc.)
//! - Time conversions and utilities
use kcore::{task::AsThread, time::ITimerType};
use kerrno::{KError, KResult};
use khal::time::{TimeValue, monotonic_time, monotonic_time_nanos, ns2t, wall_time};
use ktask::current;
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE,
    CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE,
    CLOCK_THREAD_CPUTIME_ID, itimerval, timespec, timeval,
};
use osvm::{VirtMutPtr, VirtPtr};

use crate::time::TimeValueLike;

/// Get the current time from the specified clock
pub fn sys_clock_gettime(clock_id: __kernel_clockid_t, ts: *mut timespec) -> KResult<isize> {
    let now = match clock_id as u32 {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => wall_time(),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            monotonic_time()
        }
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            let (utime, stime) = current().as_thread().time.borrow().output();
            utime + stime
        }
        _ => {
            warn!("Called sys_clock_gettime for unsupported clock {clock_id}");
            wall_time()
            // return Err(KError::EINVAL);
        }
    };
    ts.write_vm(timespec::from_time_value(now))?;
    Ok(0)
}

/// Get the current time of day
pub fn sys_gettimeofday(ts: *mut timeval) -> KResult<isize> {
    ts.write_vm(timeval::from_time_value(wall_time()))?;
    Ok(0)
}

/// Get the resolution of the specified clock
pub fn sys_clock_getres(clock_id: __kernel_clockid_t, res: *mut timespec) -> KResult<isize> {
    if clock_id as u32 != CLOCK_MONOTONIC && clock_id as u32 != CLOCK_REALTIME {
        warn!("Called sys_clock_getres for unsupported clock {clock_id}");
    }
    if let Some(res) = res.check_non_null() {
        res.write_vm(timespec::from_time_value(TimeValue::from_micros(1)))?;
    }
    Ok(0)
}

#[repr(C)]
pub struct Tms {
    /// user time
    tms_utime: usize,
    /// system time
    tms_stime: usize,
    /// user time of children
    tms_cutime: usize,
    /// system time of children
    tms_cstime: usize,
}

/// Get timing information including user and system CPU time
pub fn sys_times(tms: *mut Tms) -> KResult<isize> {
    let (utime, stime) = current().as_thread().time.borrow().output();
    let utime = utime.as_micros() as usize;
    let stime = stime.as_micros() as usize;
    tms.write_vm(Tms {
        tms_utime: utime,
        tms_stime: stime,
        tms_cutime: utime,
        tms_cstime: stime,
    })?;
    Ok(ns2t(monotonic_time_nanos()) as _)
}

/// Get the current value of a timer
pub fn sys_getitimer(which: i32, value: *mut itimerval) -> KResult<isize> {
    let ty = ITimerType::from_repr(which).ok_or(KError::InvalidInput)?;
    let (it_interval, it_value) = current().as_thread().time.borrow().get_itimer(ty);

    value.write_vm(itimerval {
        it_interval: timeval::from_time_value(it_interval),
        it_value: timeval::from_time_value(it_value),
    })?;
    Ok(0)
}

/// Set a timer to deliver a signal after a specified interval
pub fn sys_setitimer(
    which: i32,
    new_value: *const itimerval,
    old_value: *mut itimerval,
) -> KResult<isize> {
    let ty = ITimerType::from_repr(which).ok_or(KError::InvalidInput)?;
    let curr = current();

    let (interval, remained) = match new_value.check_non_null() {
        Some(new_value) => {
            // FIXME: AnyBitPattern
            let new_value = unsafe { new_value.read_uninit()?.assume_init() };
            (
                new_value.it_interval.try_into_time_value()?.as_nanos() as usize,
                new_value.it_value.try_into_time_value()?.as_nanos() as usize,
            )
        }
        None => (0, 0),
    };

    debug!("sys_setitimer <= type: {ty:?}, interval: {interval:?}, remained: {remained:?}");

    let old = curr
        .as_thread()
        .time
        .borrow_mut()
        .set_itimer(ty, interval, remained);

    if let Some(old_value) = old_value.check_non_null() {
        old_value.write_vm(itimerval {
            it_interval: timeval::from_time_value(old.0),
            it_value: timeval::from_time_value(old.1),
        })?;
    }
    Ok(0)
}
