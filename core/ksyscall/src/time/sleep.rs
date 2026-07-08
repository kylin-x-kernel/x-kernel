// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sleep-related time syscall adapters.

use kerrno::{KError, KResult};
use khal::time::TimeValue;
use ktask::future::{block_on, interruptible, sleep};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME, TIMER_ABSTIME,
    timespec,
};
use posix_types::{TimeValueLike, UserConstPtr, UserPtr};

fn sleep_impl(clock: impl Fn() -> TimeValue, dur: TimeValue) -> (KResult<()>, TimeValue) {
    debug!("sleep_impl <= {dur:?}");

    let start = clock();
    let result = block_on(interruptible(sleep(dur)));
    let elapsed = clock() - start;
    (result.map_err(Into::into), elapsed)
}

/// Sleeps for the requested relative duration.
pub fn sys_nanosleep(req: UserConstPtr<timespec>, rem: UserPtr<timespec>) -> KResult<isize> {
    let req = req.read_vm()?.try_into_time_value()?;
    debug!("sys_nanosleep <= req: {req:?}");

    match sleep_impl(khal::time::monotonic_time, req) {
        (Ok(()), _) => Ok(0),
        (Err(err), elapsed) => {
            let remaining = req.saturating_sub(elapsed);
            debug!("sys_nanosleep => rem: {remaining:?}");
            if let Some(rem) = rem.check_non_null() {
                rem.write_vm(timespec::from_time_value(remaining))?;
            }
            Err(err)
        }
    }
}

/// Sleeps against a specific clock, optionally using an absolute deadline.
///
/// Absolute deadlines at or before the current clock complete immediately and
/// do not report a remaining relative duration.
pub fn sys_clock_nanosleep(
    clock_id: __kernel_clockid_t,
    flags: u32,
    req: UserConstPtr<timespec>,
    rem: UserPtr<timespec>,
) -> KResult<isize> {
    let clock = match clock_id as u32 {
        CLOCK_REALTIME => khal::time::wall_time,
        CLOCK_MONOTONIC => khal::time::monotonic_time,
        CLOCK_MONOTONIC_RAW => return Err(KError::OperationNotSupported),
        _ => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(KError::InvalidInput);
        }
    };

    let req = req.read_vm()?.try_into_time_value()?;
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, req: {req:?}");

    let absolute = flags & TIMER_ABSTIME != 0;
    let dur = if absolute {
        req.saturating_sub(clock())
    } else {
        req
    };

    match sleep_impl(clock, dur) {
        (Ok(()), _) => Ok(0),
        (Err(err), elapsed) => {
            if !absolute {
                let remaining = dur.saturating_sub(elapsed);
                debug!("sys_clock_nanosleep => rem: {remaining:?}");
                if let Some(rem) = rem.check_non_null() {
                    rem.write_vm(timespec::from_time_value(remaining))?;
                }
            }
            Err(err)
        }
    }
}
