// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX resource-limit syscalls.

use kerrno::{KError, KResult};
use kprocess::Pid;
use kthread::get_process_state;
use linux_raw_sys::general::{RLIM_NLIMITS, rlimit64};
use posix_types::{UserConstPtr, UserPtr};

/// Gets and/or sets resource limits for a process.
pub fn sys_prlimit64(
    pid: Pid,
    resource: u32,
    new_limit: UserConstPtr<rlimit64>,
    old_limit: UserPtr<rlimit64>,
) -> KResult<isize> {
    if resource >= RLIM_NLIMITS {
        return Err(KError::InvalidInput);
    }

    let proc_state = get_process_state(pid)?;
    if let Some(old_limit) = old_limit.check_non_null() {
        let limit = &proc_state.resources.rlimits.read()[resource];
        old_limit.write_vm(rlimit64 {
            rlim_cur: limit.current,
            rlim_max: limit.max,
        })?;
    }

    if let Some(new_limit) = new_limit.check_non_null() {
        let new_limit = new_limit.read_vm()?;
        if new_limit.rlim_cur > new_limit.rlim_max {
            return Err(KError::InvalidInput);
        }

        let limit = &mut proc_state.resources.rlimits.write()[resource];
        if new_limit.rlim_max > limit.max {
            // Raising the hard limit requires CAP_SYS_RESOURCE.
            // Return EPERM until proper credential checks are in place.
            return Err(KError::OperationNotPermitted);
        }

        limit.max = new_limit.rlim_max;
        limit.current = new_limit.rlim_cur;
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
