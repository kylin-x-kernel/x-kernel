// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process CPU-time accounting syscalls.

use kerrno::KResult;
use khal::time::{monotonic_time_nanos, ns2t};
use posix_types::{Tms, UserPtr};

/// Returns timing information including user and system CPU time.
pub fn sys_times(tms: UserPtr<Tms>) -> KResult<isize> {
    let proc_state = kthread::current_process_state();
    let (utime, stime) = proc_state.process_cpu_times();
    let (child_utime_ns, child_stime_ns) = proc_state.child_time_ns();
    let utime = utime.as_micros() as usize;
    let stime = stime.as_micros() as usize;
    tms.write_vm(Tms {
        tms_utime: utime,
        tms_stime: stime,
        tms_cutime: ns2t(child_utime_ns as u64) as usize,
        tms_cstime: ns2t(child_stime_ns as u64) as usize,
    })?;
    Ok(ns2t(monotonic_time_nanos()) as _)
}
