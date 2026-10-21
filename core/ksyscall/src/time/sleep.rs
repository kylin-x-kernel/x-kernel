// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sleep-related time syscall adapters.

use kerrno::{KError, KResult};
use ktask::future::{block_on, interruptible, sleep};
use ktime_types::{ClockId, TimeSpan};
use linux_raw_sys::general::{__kernel_clockid_t, TIMER_ABSTIME, timespec};
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr};

use super::clock_id::parse_clock_id;

fn sleep_impl(duration: TimeSpan) -> (KResult<()>, TimeSpan) {
    debug!("sleep_impl <= {duration:?}");

    let start = khal::time::monotonic_time();
    let result = block_on(interruptible(sleep(duration)));
    let elapsed = khal::time::monotonic_time().saturating_duration_since(start);
    (result.map_err(Into::into), elapsed)
}

/// Sleeps for the requested relative duration.
pub(crate) fn sys_nanosleep(req: UserConstPtr<timespec>, rem: UserPtr<timespec>) -> KResult<isize> {
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
pub(crate) fn sys_clock_nanosleep(
    clock_id: __kernel_clockid_t,
    flags: u32,
    req: UserConstPtr<timespec>,
    rem: UserPtr<timespec>,
) -> KResult<isize> {
    let clock = parse_clock_id(clock_id).ok_or_else(|| {
        warn!("Unsupported clock_id: {clock_id}");
        KError::InvalidInput
    })?;
    match clock {
        ClockId::Realtime | ClockId::Monotonic | ClockId::Boottime => {}
        ClockId::MonotonicRaw => return Err(KError::OperationNotSupported),
        ClockId::ProcessCpu
        | ClockId::ThreadCpu
        | ClockId::RealtimeCoarse
        | ClockId::MonotonicCoarse => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(KError::InvalidInput);
        }
    }

    let absolute = flags & TIMER_ABSTIME != 0;
    let raw_req = req.read_vm()?;
    if absolute && clock == ClockId::Realtime {
        let deadline = posix_types::try_into_realtime_deadline(raw_req)?;
        block_on(interruptible(ktask::future::sleep_until_realtime(deadline)))
            .map_err(KError::from)??;
        return Ok(0);
    }
    if clock == ClockId::Boottime {
        let value = raw_req.try_into_time_span()?;
        let start = ktime::boottime();
        let deadline = if absolute {
            ktime_types::BoottimeInstant::from_span_since_origin(value)
        } else {
            start.checked_add(value).ok_or(KError::InvalidInput)?
        };
        let result = block_on(interruptible(ktask::future::sleep_until_boottime(deadline)));
        match result {
            Ok(result) => {
                result?;
                return Ok(0);
            }
            Err(error) => {
                if !absolute && let Some(rem) = rem.check_non_null() {
                    let remaining = deadline.saturating_duration_since(ktime::boottime());
                    rem.write_vm(timespec::from_time_span(remaining))?;
                }
                return Err(error.into());
            }
        }
    }
    let duration = if absolute {
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
