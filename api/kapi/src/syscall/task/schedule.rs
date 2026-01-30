//! Scheduling and timer syscalls.
//!
//! This module implements scheduling and timer operations including:
//! - Yield control (sched_yield, etc.)
//! - Sleep operations (sleep, nanosleep, etc.)
//! - Scheduling priority (getpriority, setpriority, nice, etc.)
//! - CPU affinity (sched_setaffinity, sched_getaffinity, etc.)

use kcore::task::{get_process_data, get_process_group};
use kerrno::{KError, KResult};
use khal::time::TimeValue;
use ktask::{
    KCpuMask, current,
    future::{block_on, interruptible, sleep},
};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_MONOTONIC, CLOCK_REALTIME, PRIO_PGRP, PRIO_PROCESS, PRIO_USER,
    SCHED_RR, TIMER_ABSTIME, timespec,
};
use osvm::{VirtMutPtr, VirtPtr, load_vec, write_vm_mem};

use crate::time::TimeValueLike;

pub fn sys_sched_yield() -> KResult<isize> {
    ktask::yield_now();
    Ok(0)
}

fn sleep_impl(clock: impl Fn() -> TimeValue, dur: TimeValue) -> TimeValue {
    debug!("sleep_impl <= {dur:?}");

    // Record the time before sleeping
    let start = clock();

    // TODO: currently ignoring concrete clock type
    // We detect EINTR manually if the slept time is not enough.
    // Block on interruptible sleep - returns early if interrupted by signals
    let _ = block_on(interruptible(sleep(dur)));

    // Return the actual time elapsed
    clock() - start
}

/// Sleep some nanoseconds
pub fn sys_nanosleep(req: *const timespec, rem: *mut timespec) -> KResult<isize> {
    // FIXME: AnyBitPattern
    let req = unsafe { req.read_uninit()?.assume_init() }.try_into_time_value()?;
    debug!("sys_nanosleep <= req: {req:?}");

    let actual = sleep_impl(khal::time::monotonic_time, req);

    if let Some(diff) = req.checked_sub(actual) {
        debug!("sys_nanosleep => rem: {diff:?}");
        if let Some(rem) = rem.check_non_null() {
            rem.write_vm(timespec::from_time_value(diff))?;
        }
        Err(KError::Interrupted)
    } else {
        Ok(0)
    }
}

pub fn sys_clock_nanosleep(
    clock_id: __kernel_clockid_t,
    flags: u32,
    req: *const timespec,
    rem: *mut timespec,
) -> KResult<isize> {
    // Select appropriate clock function based on clock_id
    let clock = match clock_id as u32 {
        CLOCK_REALTIME => khal::time::wall_time,
        CLOCK_MONOTONIC => khal::time::monotonic_time,
        _ => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(KError::InvalidInput);
        }
    };

    let req = unsafe { req.read_uninit()?.assume_init() }.try_into_time_value()?;
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, req: {req:?}");

    // If TIMER_ABSTIME flag is set, request is absolute time; otherwise it's relative
    let dur = if flags & TIMER_ABSTIME != 0 {
        req.saturating_sub(clock())
    } else {
        req
    };

    let actual = sleep_impl(clock, dur);

    if let Some(diff) = dur.checked_sub(actual) {
        debug!("sys_clock_nanosleep => rem: {diff:?}");
        if let Some(rem) = rem.check_non_null() {
            rem.write_vm(timespec::from_time_value(diff))?;
        }
        Err(KError::Interrupted)
    } else {
        Ok(0)
    }
}

pub fn sys_sched_getaffinity(pid: i32, cpusetsize: usize, user_mask: *mut u8) -> KResult<isize> {
    // Check if the buffer is large enough
    if cpusetsize * 8 < platconfig::plat::CPU_NUM {
        return Err(KError::InvalidInput);
    }

    // TODO: support other threads
    // Currently only supports getting the calling thread's affinity (pid=0)
    if pid != 0 {
        return Err(KError::OperationNotPermitted);
    }

    // Get current thread's CPU affinity mask
    let mask = current().cpumask();
    let mask_bytes = mask.as_bytes();

    // Write the mask to user space
    write_vm_mem(user_mask, mask_bytes)?;

    Ok(mask_bytes.len() as _)
}

pub fn sys_sched_setaffinity(_pid: i32, cpusetsize: usize, user_mask: *const u8) -> KResult<isize> {
    // Load the CPU mask from user space (limit to actual CPU count)
    let size = cpusetsize.min(platconfig::plat::CPU_NUM.div_ceil(8));
    let user_mask = load_vec(user_mask, size)?;
    let mut cpu_mask = KCpuMask::new();

    // Convert byte array to CPU mask bitset
    for i in 0..(size * 8).min(platconfig::plat::CPU_NUM) {
        if user_mask[i / 8] & (1 << (i % 8)) != 0 {
            cpu_mask.set(i, true);
        }
    }

    // TODO: support other threads
    // Apply the CPU affinity to the current thread
    ktask::set_current_affinity(cpu_mask);

    Ok(0)
}

pub fn sys_sched_getscheduler(_pid: i32) -> KResult<isize> {
    Ok(SCHED_RR as _)
}

pub fn sys_sched_setscheduler(_pid: i32, _policy: i32, _param: *const ()) -> KResult<isize> {
    Ok(0)
}

pub fn sys_sched_getparam(_pid: i32, _param: *mut ()) -> KResult<isize> {
    Ok(0)
}

pub fn sys_getpriority(which: u32, who: u32) -> KResult<isize> {
    debug!("sys_getpriority <= which: {which}, who: {who}");

    match which {
        PRIO_PROCESS => {
            if who != 0 {
                let _proc = get_process_data(who)?;
            }
            Ok(20)
        }
        PRIO_PGRP => {
            if who != 0 {
                let _pg = get_process_group(who)?;
            }
            Ok(20)
        }
        PRIO_USER => {
            if who == 0 {
                Ok(20)
            } else {
                Err(KError::NoSuchProcess)
            }
        }
        _ => Err(KError::InvalidInput),
    }
}
