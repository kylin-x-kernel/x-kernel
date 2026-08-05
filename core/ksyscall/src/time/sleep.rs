// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sleep-related time syscall adapters.

use kerrno::{KError, KResult};
use ktask::future::{block_on, interruptible, sleep};
use ktime_types::TimeSpan;
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME, TIMER_ABSTIME,
    timespec,
};
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr};

fn sleep_impl(duration: TimeSpan) -> (KResult<()>, TimeSpan) {
    debug!("sleep_impl <= {duration:?}");

    let start = khal::time::monotonic_time();
    let result = block_on(interruptible(sleep(duration)));
    let elapsed = khal::time::monotonic_time().saturating_duration_since(start);
    (result.map_err(Into::into), elapsed)
}

/// Sleeps for the requested relative duration.
pub fn sys_nanosleep(req: UserConstPtr<timespec>, rem: UserPtr<timespec>) -> KResult<isize> {
    let req = req.read_vm()?.try_into_time_span()?;
    debug!("sys_nanosleep <= req: {req:?}");

    match sleep_impl(req) {
        (Ok(()), _) => Ok(0),
        (Err(err), elapsed) => {
            let remaining = req.saturating_sub(elapsed);
            debug!("sys_nanosleep => rem: {remaining:?}");
            if let Some(rem) = rem.check_non_null() {
                rem.write_vm(timespec::from_time_span(remaining))?;
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
    match clock_id as u32 {
        CLOCK_REALTIME | CLOCK_MONOTONIC => {}
        CLOCK_MONOTONIC_RAW => return Err(KError::OperationNotSupported),
        _ => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(KError::InvalidInput);
        }
    };

    let absolute = flags & TIMER_ABSTIME != 0;
    let raw_req = req.read_vm()?;
    let duration = if absolute && clock_id as u32 == CLOCK_REALTIME {
        posix_types::try_into_realtime_deadline(raw_req)?
            .duration_since(ktime::realtime())
            .unwrap_or(TimeSpan::ZERO)
    } else if absolute {
        raw_req
            .try_into_time_span()?
            .saturating_sub(khal::time::monotonic_time().span_since_origin())
    } else {
        raw_req.try_into_time_span()?
    };
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, duration: {duration:?}");

    match sleep_impl(duration) {
        (Ok(()), _) => Ok(0),
        (Err(err), elapsed) => {
            if !absolute {
                let remaining = duration.saturating_sub(elapsed);
                debug!("sys_clock_nanosleep => rem: {remaining:?}");
                if let Some(rem) = rem.check_non_null() {
                    rem.write_vm(timespec::from_time_span(remaining))?;
                }
            }
            Err(err)
        }
    }
}
