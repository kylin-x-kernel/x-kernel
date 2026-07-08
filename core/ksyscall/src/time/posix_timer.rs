// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process timer syscalls.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kprocess::{AsThread, Pid};
use ksignal::Signo;
use ktimer::{PosixTimerCreateNotify, PosixTimerSigValue, TimerSigValue};
use linux_raw_sys::general::{
    SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD_ID, TIMER_ABSTIME, itimerspec,
};
use posix_types::{TimeValueLike, UserConstPtr, UserPtr, k_sigevent};

fn parse_signo(signo: i32) -> KResult<Signo> {
    Signo::from_repr(signo as u8).ok_or(KError::InvalidInput)
}

fn parse_timespec_ns(ts: linux_raw_sys::general::timespec) -> KResult<usize> {
    let tv = ts.try_into_time_value()?;
    usize::try_from(tv.as_nanos()).map_err(|_| KError::InvalidInput)
}

fn build_itimerspec(
    interval: khal::time::TimeValue,
    remaining: khal::time::TimeValue,
) -> itimerspec {
    itimerspec {
        it_interval: linux_raw_sys::general::timespec::from_time_value(interval),
        it_value: linux_raw_sys::general::timespec::from_time_value(remaining),
    }
}

fn parse_sigevent(sevp: UserConstPtr<k_sigevent>) -> KResult<PosixTimerCreateNotify> {
    let Some(sevp) = sevp.check_non_null() else {
        return Ok(PosixTimerCreateNotify::Signal {
            signo: Signo::SIGALRM,
            target_tid: None,
            value: PosixTimerSigValue::TimerId,
        });
    };

    let sevp = sevp.read_vm()?;
    match sevp.sigev_notify as u32 {
        SIGEV_NONE => Ok(PosixTimerCreateNotify::None),
        SIGEV_SIGNAL => Ok(PosixTimerCreateNotify::Signal {
            signo: parse_signo(sevp.sigev_signo)?,
            target_tid: None,
            value: PosixTimerSigValue::Explicit(TimerSigValue::from_raw(sevp.sigev_value)),
        }),
        SIGEV_THREAD_ID => {
            // SAFETY: `_sigev_un._tid` is the ABI-defined field for
            // SIGEV_THREAD_ID. Reading it as i32 is a valid interpretation
            // of the union when sigev_notify == SIGEV_THREAD_ID.
            let tid = unsafe { sevp._sigev_un._tid };
            if tid <= 0 {
                return Err(KError::InvalidInput);
            }
            let tid = tid as Pid;
            let current = kprocess::current_user_process();
            if !kprocess::scheduler::process_owns_tid(current.as_ref(), tid) {
                return Err(KError::InvalidInput);
            }
            let target_task = kprocess::scheduler::task_by_tid(tid)?;
            let target_thread = target_task.try_as_thread().ok_or(KError::InvalidInput)?;
            if !Arc::ptr_eq(target_thread.process(), &current) {
                return Err(KError::InvalidInput);
            }
            Ok(PosixTimerCreateNotify::Signal {
                signo: parse_signo(sevp.sigev_signo)?,
                target_tid: Some(tid),
                value: PosixTimerSigValue::Explicit(TimerSigValue::from_raw(sevp.sigev_value)),
            })
        }
        _ => Err(KError::InvalidInput),
    }
}

/// Creates a POSIX timer object for the current process.
pub fn sys_timer_create(
    clockid: i32,
    sevp: UserConstPtr<k_sigevent>,
    timerid: UserPtr<i32>,
) -> KResult<isize> {
    let notify = parse_sigevent(sevp)?;
    let process = kprocess::current_user_process();
    let timer_id = process
        .timer_manager()?
        .lock()
        .create_posix_timer(clockid, notify)?;
    timerid.write_vm(timer_id)?;
    Ok(0)
}

/// Queries the current setting of a POSIX timer.
pub fn sys_timer_gettime(timerid: i32, curr_value: UserPtr<itimerspec>) -> KResult<isize> {
    let process = kprocess::current_user_process();
    let (process_utime_ns, process_stime_ns) = process.process_cpu_time_ns();
    let spec = process.timer_manager()?.lock().get_posix_timer(
        timerid,
        process_utime_ns,
        process_stime_ns,
    )?;
    curr_value.write_vm(build_itimerspec(spec.0, spec.1))?;
    Ok(0)
}

/// Arms, rearms, or disarms a POSIX timer.
pub fn sys_timer_settime(
    timerid: i32,
    flags: i32,
    new_value: UserConstPtr<itimerspec>,
    old_value: UserPtr<itimerspec>,
) -> KResult<isize> {
    if flags & !(TIMER_ABSTIME as i32) != 0 {
        return Err(KError::InvalidInput);
    }

    let new_value = new_value.read_vm()?;
    let interval_ns = parse_timespec_ns(new_value.it_interval)?;
    let value_ns = parse_timespec_ns(new_value.it_value)?;
    let absolute = flags & TIMER_ABSTIME as i32 != 0;

    let current_thread = kprocess::current_user_thread();
    let process = current_thread.process().clone();
    let (process_utime_ns, process_stime_ns) = process.process_cpu_time_ns();
    let (old, delivery) = process.timer_manager()?.lock().set_posix_timer(
        timerid,
        absolute,
        interval_ns,
        value_ns,
        process_utime_ns,
        process_stime_ns,
    )?;

    if let Some(old_value) = old_value.check_non_null() {
        old_value.write_vm(build_itimerspec(old.0, old.1))?;
    }
    if let Some(delivery) = delivery {
        kprocess::dispatch_timer_delivery(current_thread.pid(), delivery);
    }
    Ok(0)
}

/// Deletes a POSIX timer object.
pub fn sys_timer_delete(timerid: i32) -> KResult<isize> {
    kprocess::current_user_process()
        .timer_manager()?
        .lock()
        .delete_posix_timer(timerid)?;
    Ok(0)
}

/// Returns the overrun count for the last timer notification.
pub fn sys_timer_getoverrun(timerid: i32) -> KResult<isize> {
    let overrun = kprocess::current_user_process()
        .timer_manager()?
        .lock()
        .get_posix_timer_overrun(timerid)?;
    Ok(overrun as isize)
}
