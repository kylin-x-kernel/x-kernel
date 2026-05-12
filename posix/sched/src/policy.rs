// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX scheduler policy syscalls.

use kerrno::KResult;
use linux_raw_sys::general::SCHED_RR;
use posix_types::{UserConstPtr, UserPtr};

/// Returns the current scheduler policy.
pub fn sys_sched_getscheduler(_pid: i32) -> KResult<isize> {
    Ok(SCHED_RR as _)
}

/// Sets the scheduler policy.
pub fn sys_sched_setscheduler(_pid: i32, _policy: i32, _param: UserConstPtr<()>) -> KResult<isize> {
    Ok(0)
}

/// Returns scheduler parameters.
pub fn sys_sched_getparam(_pid: i32, _param: UserPtr<()>) -> KResult<isize> {
    Ok(0)
}
