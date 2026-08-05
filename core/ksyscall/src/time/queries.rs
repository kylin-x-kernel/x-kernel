// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time query and setter syscall adapters.

use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use ktime_types::{SystemTime, TimeSpan};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE,
    CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE,
    CLOCK_THREAD_CPUTIME_ID, timespec, timeval, timezone,
};
use posix_types::{SystemTimeLike, TimeSpanLike, UserConstPtr, UserPtr};

/// Returns the current time from the specified clock.
pub fn sys_clock_gettime(clock_id: __kernel_clockid_t, ts: UserPtr<timespec>) -> KResult<isize> {
    let value = match clock_id as u32 {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => timespec::from_system_time(ktime::realtime()),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            timespec::from_time_span(monotonic_time().span_since_origin())
        }
        CLOCK_PROCESS_CPUTIME_ID => {
            timespec::from_time_span(kprocess::current_user_process().process_cpu_time())
        }
        CLOCK_THREAD_CPUTIME_ID => {
            timespec::from_time_span(kprocess::current_user_thread().cpu_time())
        }
        _ => {
            warn!("Called sys_clock_gettime for unsupported clock {clock_id}");
            return Err(KError::InvalidInput);
        }
    };
    ts.write_vm(value)?;
    Ok(0)
}

/// Returns the current wall-clock time of day.
pub fn sys_gettimeofday(ts: UserPtr<timeval>) -> KResult<isize> {
    ts.write_vm(timeval::from_system_time(ktime::realtime()))?;
    Ok(0)
}

/// Returns the current wall-clock time in seconds since the Unix epoch.
#[cfg(target_arch = "x86_64")]
pub fn sys_time(tloc: UserPtr<linux_raw_sys::general::__kernel_time_t>) -> KResult<isize> {
    let seconds = ktime::realtime().unix_seconds() as linux_raw_sys::general::__kernel_time_t;
    if let Some(tloc) = tloc.check_non_null() {
        tloc.write_vm(seconds)?;
    }
    Ok(seconds as isize)
}

fn set_wall_time(value: SystemTime) -> KResult<isize> {
    if !kprocess::current_cred().is_privileged() {
        return Err(KError::OperationNotPermitted);
    }
    // Range validation (the Linux rule that the wall clock may not move before
    // CLOCK_MONOTONIC, plus an upper bound keeping the clock able to advance)
    // is enforced atomically inside the timekeeper under a single write lock.
    ktime::set_realtime_checked(value).map_err(|_| KError::InvalidInput)?;
    Ok(0)
}

/// Sets the wall clock from a `timeval` value.
pub fn sys_settimeofday(
    ts: UserConstPtr<timeval>,
    timezone: UserConstPtr<timezone>,
) -> KResult<isize> {
    // Linux no longer allows setting the timezone; a non-NULL one is rejected.
    if timezone.check_non_null().is_some() {
        return Err(KError::InvalidInput);
    }
    // A NULL `tv` leaves the clock unchanged and needs no privilege check,
    // matching Linux `do_sys_settimeofday64()`.
    let Some(ts) = ts.check_non_null() else {
        return Ok(0);
    };
    set_wall_time(ts.read_vm()?.try_into_system_time()?)
}

/// Sets `CLOCK_REALTIME` (or its coarse variant) from a `timespec` value.
pub fn sys_clock_settime(
    clock_id: __kernel_clockid_t,
    ts: UserConstPtr<timespec>,
) -> KResult<isize> {
    // Keep symmetry with `sys_clock_gettime`: both realtime clocks are settable.
    if !matches!(clock_id as u32, CLOCK_REALTIME | CLOCK_REALTIME_COARSE) {
        return Err(KError::InvalidInput);
    }
    let Some(ts) = ts.check_non_null() else {
        return Err(KError::BadAddress);
    };
    set_wall_time(ts.read_vm()?.try_into_system_time()?)
}

/// Returns the resolution of the specified clock.
pub fn sys_clock_getres(clock_id: __kernel_clockid_t, res: UserPtr<timespec>) -> KResult<isize> {
    match clock_id as u32 {
        CLOCK_REALTIME
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID => {}
        _ => {
            warn!("Called sys_clock_getres for unsupported clock {clock_id}");
            return Err(KError::InvalidInput);
        }
    }
    if let Some(res) = res.check_non_null() {
        res.write_vm(timespec::from_time_span(TimeSpan::from_micros(1)))?;
    }
    Ok(0)
}

#[cfg(all(unittest, target_arch = "x86_64"))]
mod tests {
    use posix_types::UserPtr;
    use unittest::def_test;

    #[def_test]
    fn time_with_null_tloc_returns_current_epoch_seconds() {
        let before = ktime::realtime().unix_seconds() as isize;
        let seconds = super::sys_time(UserPtr::default()).expect("time(NULL) must succeed");
        let after = ktime::realtime().unix_seconds() as isize;

        assert!((before..=after).contains(&seconds));
    }
}
