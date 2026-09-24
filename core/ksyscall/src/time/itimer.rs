// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process interval timer syscalls.

use kerrno::{KError, KResult};
use linux_raw_sys::general::{itimerval, timeval};
use posix_types::{ITimerType, TimeSpanLike, UserConstPtr, UserPtr};

/// Returns the current value of an interval timer.
pub fn sys_getitimer(which: i32, value: UserPtr<itimerval>) -> KResult<isize> {
    let timer_type = ITimerType::from_repr(which).ok_or(KError::InvalidInput)?;
    let process = kprocess::current_user_process();
    let (it_interval, it_value) = process.get_itimer(timer_type)?;

    value.write_vm(itimerval {
        it_interval: timeval::from_time_span(it_interval),
        it_value: timeval::from_time_span(it_value),
    })?;
    Ok(0)
}

/// Sets an interval timer and optionally returns the previous value.
pub fn sys_setitimer(
    which: i32,
    new_value: UserConstPtr<itimerval>,
    old_value: UserPtr<itimerval>,
) -> KResult<isize> {
    let timer_type = ITimerType::from_repr(which).ok_or(KError::InvalidInput)?;

    let (interval, remaining) = match new_value.check_non_null() {
        Some(new_value) => {
            let new_value = new_value.read_vm()?;
            (
                new_value.it_interval.try_into_time_span()?,
                new_value.it_value.try_into_time_span()?,
            )
        }
        None => (ktime_types::TimeSpan::ZERO, ktime_types::TimeSpan::ZERO),
    };

    debug!(
        "sys_setitimer <= type: {timer_type:?}, interval: {interval:?}, remaining: {remaining:?}"
    );

    let process = kprocess::current_user_process();
    let old = process.set_itimer(timer_type, interval, remaining)?;

    if let Some(old_value) = old_value.check_non_null() {
        old_value.write_vm(itimerval {
            it_interval: timeval::from_time_span(old.0),
            it_value: timeval::from_time_span(old.1),
        })?;
    }
    Ok(0)
}

/// Schedules a `SIGALRM` delivery after `seconds` seconds.
///
/// Equivalent to `setitimer(ITIMER_REAL, {it_value: seconds, it_interval: 0})`
/// with the one-shot interval timer owned by the process. Returns the number of
/// seconds remaining on any previous alarm, rounding a partial second up, or
/// `0` when no alarm was pending. `seconds == 0` cancels a pending alarm.
///
/// See <https://man7.org/linux/man-pages/man2/alarm.2.html>.
#[cfg(target_arch = "x86_64")]
pub fn sys_alarm(seconds: u32) -> KResult<isize> {
    let process = kprocess::current_user_process();
    let (_, old_remaining) = process.set_itimer(
        ITimerType::Real,
        ktime_types::TimeSpan::ZERO,
        ktime_types::TimeSpan::from_secs(seconds.into()),
    )?;

    let old_secs = old_remaining.as_secs() + u64::from(old_remaining.subsec_nanos() != 0);
    Ok(old_secs as isize)
}

#[cfg(all(unittest, target_arch = "x86_64"))]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test(user, serial)]
    fn test_sys_alarm_reports_previous_remaining_seconds() {
        // No alarm pending: both the cancel and the first arm report 0.
        assert_eq!(sys_alarm(0).unwrap(), 0);
        assert_eq!(sys_alarm(3).unwrap(), 0);

        // Replacing the pending ~3s alarm returns the whole seconds left,
        // rounded up (Linux `alarm(2)` semantics).
        let prev = sys_alarm(0).unwrap();
        assert!(
            (2..=3).contains(&prev),
            "previous remaining seconds should round to 2 or 3, got {prev}"
        );
    }
}
