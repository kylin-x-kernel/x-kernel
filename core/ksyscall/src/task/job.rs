// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-group and session syscalls.

use kerrno::{KError, KResult};
use kprocess::Pid;

/// Returns the session ID of the given process.
pub fn sys_getsid(pid: Pid) -> KResult<isize> {
    Ok(kthread::get_process_state(pid)?
        .proc
        .group()
        .session()
        .sid() as _)
}

/// Creates a new session and makes the caller its leader.
pub fn sys_setsid() -> KResult<isize> {
    let current_thread = kthread::current_thread();
    let proc = &current_thread.process_state().proc;
    if kthread::get_process_group(proc.pid()).is_ok() {
        return Err(KError::OperationNotPermitted);
    }

    if let Some((session, _)) = proc.create_session() {
        Ok(session.sid() as _)
    } else {
        Ok(proc.pid() as _)
    }
}

/// Returns the process group ID of the given process.
pub fn sys_getpgid(pid: Pid) -> KResult<isize> {
    Ok(kthread::get_process_state(pid)?.proc.group().pgid() as _)
}

/// Sets the process group ID of the given process.
pub fn sys_setpgid(pid: Pid, pgid: Pid) -> KResult<isize> {
    let proc = &kthread::get_process_state(pid)?.proc;

    if pgid == 0 {
        proc.create_group();
    } else if !proc.move_to_group(&kthread::get_process_group(pgid)?) {
        return Err(KError::OperationNotPermitted);
    }

    Ok(0)
}
