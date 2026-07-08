// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process resource-limit syscalls.

use kerrno::KResult;
use kprocess::Pid;
use linux_raw_sys::general::rlimit64;
use posix_types::{UserConstPtr, UserPtr};

/// Gets and/or sets resource limits for a process.
pub fn sys_prlimit64(
    pid: Pid,
    resource: u32,
    new_limit: UserConstPtr<rlimit64>,
    old_limit: UserPtr<rlimit64>,
) -> KResult<isize> {
    let process = kprocess::resource_limits::target_process(pid)?;
    let resources = process.resources()?;
    if let Some(old_limit) = old_limit.check_non_null() {
        let (current, max) = resources.rlimit_values(resource)?;
        old_limit.write_vm(rlimit64 {
            rlim_cur: current,
            rlim_max: max,
        })?;
    }

    if let Some(new_limit) = new_limit.check_non_null() {
        let new_limit = new_limit.read_vm()?;
        resources.set_rlimit_values(resource, new_limit.rlim_cur, new_limit.rlim_max)?;
    }

    Ok(0)
}

/// Returns the current resource limits of the calling process.
pub fn sys_getrlimit(resource: u32, old_limit: UserPtr<rlimit64>) -> KResult<isize> {
    sys_prlimit64(0, resource, UserConstPtr::from(0usize), old_limit)
}

/// Updates the resource limits of the calling process.
pub fn sys_setrlimit(resource: u32, new_limit: UserConstPtr<rlimit64>) -> KResult<isize> {
    sys_prlimit64(0, resource, new_limit, UserPtr::from(0usize))
}
