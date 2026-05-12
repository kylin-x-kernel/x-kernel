// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX CPU time syscalls and helpers.

use kerrno::KResult;
use khal::time::{TimeValue, monotonic_time_nanos, ns2t};
use posix_types::{Tms, UserPtr};

/// Returns the current thread's accumulated user and system CPU time.
pub fn current_thread_cpu_times() -> (TimeValue, TimeValue) {
    kthread::current_thread().time.lock().output()
}

/// Returns the current thread's accumulated CPU time.
pub fn current_thread_cpu_time() -> TimeValue {
    let (utime, stime) = current_thread_cpu_times();
    utime + stime
}

/// Returns timing information including user and system CPU time.
pub fn sys_times(tms: UserPtr<Tms>) -> KResult<isize> {
    let (utime, stime) = current_thread_cpu_times();
    let utime = utime.as_micros() as usize;
    let stime = stime.as_micros() as usize;
    tms.write_vm(Tms {
        tms_utime: utime,
        tms_stime: stime,
        tms_cutime: utime,
        tms_cstime: stime,
    })?;
    Ok(ns2t(monotonic_time_nanos()) as _)
}
