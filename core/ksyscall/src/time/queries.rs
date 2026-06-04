// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time query syscall adapters.

use kerrno::{KError, KResult};
use khal::time::{TimeValue, monotonic_time, wall_time};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE,
    CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE,
    CLOCK_THREAD_CPUTIME_ID, timespec, timeval,
};
use posix_types::{TimeValueLike, UserPtr};

/// Returns the current time from the specified clock.
pub fn sys_clock_gettime(clock_id: __kernel_clockid_t, ts: UserPtr<timespec>) -> KResult<isize> {
    let now = match clock_id as u32 {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => wall_time(),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            monotonic_time()
        }
        CLOCK_PROCESS_CPUTIME_ID => kthread::current_process_state().process_cpu_time(),
        CLOCK_THREAD_CPUTIME_ID => kthread::current_thread().cpu_time(),
        _ => {
            warn!("Called sys_clock_gettime for unsupported clock {clock_id}");
            return Err(KError::InvalidInput);
        }
    };
    ts.write_vm(timespec::from_time_value(now))?;
    Ok(0)
}

/// Returns the current wall-clock time of day.
pub fn sys_gettimeofday(ts: UserPtr<timeval>) -> KResult<isize> {
    ts.write_vm(timeval::from_time_value(wall_time()))?;
    Ok(0)
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
        res.write_vm(timespec::from_time_value(TimeValue::from_micros(1)))?;
    }
    Ok(0)
}
