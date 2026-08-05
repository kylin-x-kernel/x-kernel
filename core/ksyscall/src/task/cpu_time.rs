// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process CPU-time accounting syscalls.

use kerrno::KResult;
use khal::time::monotonic_time;
use posix_types::{PosixClockTicks, Tms, UserPtr};

/// Returns timing information including user and system CPU time.
pub fn sys_times(tms: UserPtr<Tms>) -> KResult<isize> {
    let process = kprocess::current_user_process();
    let (utime, stime) = process.process_cpu_times();
    let (child_utime, child_stime) = process.child_time();
    tms.write_vm(Tms {
        tms_utime: PosixClockTicks::from_time_span(utime).as_raw() as usize,
        tms_stime: PosixClockTicks::from_time_span(stime).as_raw() as usize,
        tms_cutime: PosixClockTicks::from_time_span(child_utime).as_raw() as usize,
        tms_cstime: PosixClockTicks::from_time_span(child_stime).as_raw() as usize,
    })?;
    Ok(PosixClockTicks::from_time_span(monotonic_time().span_since_origin()).as_raw() as _)
}
