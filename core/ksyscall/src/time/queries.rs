// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time query and setter syscall adapters.

use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use ktime_types::{ClockId, SystemTime, TimeSpan};
use linux_raw_sys::general::{__kernel_clockid_t, timespec, timeval, timezone};
use posix_types::{SystemTimeLike, TimeSpanLike, UserConstPtr, UserPtr};

use super::clock_id::parse_clock_id;

/// Returns the current time from the specified clock.
pub(crate) fn sys_clock_gettime(
    clock_id: __kernel_clockid_t,
    ts: UserPtr<timespec>,
) -> KResult<isize> {
    let clock = parse_clock_id(clock_id).ok_or_else(|| {
        warn!("Called sys_clock_gettime for unsupported clock {clock_id}");
        KError::InvalidInput
    })?;
    let value = match clock {
        ClockId::Realtime | ClockId::RealtimeCoarse => {
            timespec::from_system_time(ktime::realtime())
        }
        ClockId::Boottime => timespec::from_time_span(ktime::boottime().span_since_origin()),
        ClockId::Monotonic | ClockId::MonotonicRaw | ClockId::MonotonicCoarse => {
            timespec::from_time_span(monotonic_time().span_since_origin())
        }
        ClockId::ProcessCpu => {
            timespec::from_time_span(kprocess::current_user_process().process_cpu_time())
        }
        ClockId::ThreadCpu => timespec::from_time_span(kprocess::current_user_thread().cpu_time()),
    };
    ts.write_vm(value)?;
    Ok(0)
}

/// Returns the current wall-clock time of day.
pub(crate) fn sys_gettimeofday(ts: UserPtr<timeval>) -> KResult<isize> {
    if let Some(ts) = ts.check_non_null() {
        ts.write_vm(timeval::from_system_time(ktime::realtime()))?;
    }
    Ok(0)
}

/// Returns the current wall-clock time in seconds since the Unix epoch.
#[cfg(target_arch = "x86_64")]
pub(crate) fn sys_time(tloc: UserPtr<linux_raw_sys::general::__kernel_time_t>) -> KResult<isize> {
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
pub(crate) fn sys_settimeofday(
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

/// Sets `CLOCK_REALTIME` from a `timespec` value.
pub(crate) fn sys_clock_settime(
    clock_id: __kernel_clockid_t,
    ts: UserConstPtr<timespec>,
) -> KResult<isize> {
    // Coarse clocks are query-only on Linux, even though gettime accepts them.
    let clock = parse_clock_id(clock_id).ok_or(KError::InvalidInput)?;
    if clock != ClockId::Realtime {
        return Err(KError::InvalidInput);
    }
    let Some(ts) = ts.check_non_null() else {
        return Err(KError::BadAddress);
    };
    set_wall_time(ts.read_vm()?.try_into_system_time()?)
}

/// Returns the resolution of the specified clock.
pub(crate) fn sys_clock_getres(
    clock_id: __kernel_clockid_t,
    res: UserPtr<timespec>,
) -> KResult<isize> {
    let _clock = parse_clock_id(clock_id).ok_or_else(|| {
        warn!("Called sys_clock_getres for unsupported clock {clock_id}");
        KError::InvalidInput
    })?;
    if let Some(res) = res.check_non_null() {
        res.write_vm(timespec::from_time_span(TimeSpan::from_micros(1)))?;
    }
    Ok(0)
}

#[cfg(all(unittest, target_arch = "x86_64"))]
unittest::path_mod_test!(tests, "../../ut-tests/time/queries/tests.rs");

unittest::path_mod_test!(
    tests_time_query_arguments,
    "../../ut-tests/time/queries/tests_time_query_arguments.rs"
);
