// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process interval timer syscalls.

use kerrno::{KError, KResult};
use linux_raw_sys::general::{itimerval, timeval};
use posix_types::{ITimerType, TimeValueLike, UserConstPtr, UserPtr};

/// Returns the current value of an interval timer.
pub fn sys_getitimer(which: i32, value: UserPtr<itimerval>) -> KResult<isize> {
    let timer_type = ITimerType::from_repr(which).ok_or(KError::InvalidInput)?;
    let proc_state = kthread::current_thread().process_state().clone();
    let (process_utime_ns, process_stime_ns) = proc_state.process_cpu_time_ns();
    let (it_interval, it_value) = proc_state.timer_manager().lock().get_itimer(
        timer_type,
        process_utime_ns,
        process_stime_ns,
    );

    value.write_vm(itimerval {
        it_interval: timeval::from_time_value(it_interval),
        it_value: timeval::from_time_value(it_value),
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

    let (interval_ns, remained_ns) = match new_value.check_non_null() {
        Some(new_value) => {
            let new_value = new_value.read_vm()?;
            (
                new_value.it_interval.try_into_time_value()?.as_nanos() as usize,
                new_value.it_value.try_into_time_value()?.as_nanos() as usize,
            )
        }
        None => (0, 0),
    };

    debug!(
        "sys_setitimer <= type: {timer_type:?}, interval: {interval_ns:?}, remained: \
         {remained_ns:?}"
    );

    let proc_state = kthread::current_thread().process_state().clone();
    let (process_utime_ns, process_stime_ns) = proc_state.process_cpu_time_ns();
    let old = proc_state.timer_manager().lock().set_itimer(
        timer_type,
        interval_ns,
        remained_ns,
        process_utime_ns,
        process_stime_ns,
    );

    if let Some(old_value) = old_value.check_non_null() {
        old_value.write_vm(itimerval {
            it_interval: timeval::from_time_value(old.0),
            it_value: timeval::from_time_value(old.1),
        })?;
    }
    Ok(0)
}
