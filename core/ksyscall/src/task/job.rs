// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-group and session syscalls.

use kerrno::{KError, KResult};
use kprocess::Pid;

fn validate_job_pid(pid: i32) -> KResult<()> {
    if pid < 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

/// Returns the session ID of the given process.
pub(crate) fn sys_getsid(pid: i32) -> KResult<isize> {
    validate_job_pid(pid)?;
    let proc = if pid == 0 {
        kprocess::current_user_process()
    } else {
        kprocess::job_control::query_process(pid as Pid)?
    };
    Ok(proc.group().session().sid() as _)
}

/// Creates a new session and makes the caller its leader.
pub(crate) fn sys_setsid() -> KResult<isize> {
    let current_thread = kprocess::current_user_thread();
    let proc = current_thread.process();
    if kprocess::job_control::target_group(proc.pid()).is_ok() {
        return Err(KError::OperationNotPermitted);
    }

    if let Some((session, _)) = proc.create_session() {
        Ok(session.sid() as _)
    } else {
        Ok(proc.pid() as _)
    }
}

/// Returns the process group ID of the given process.
pub(crate) fn sys_getpgid(pid: i32) -> KResult<isize> {
    validate_job_pid(pid)?;
    let proc = if pid == 0 {
        kprocess::current_user_process()
    } else {
        kprocess::job_control::query_process(pid as Pid)?
    };
    Ok(proc.group().pgid() as _)
}

/// Returns the process group ID of the calling process.
#[cfg(target_arch = "x86_64")]
pub(crate) fn sys_getpgrp() -> KResult<isize> {
    Ok(kprocess::current_user_process().group().pgid() as _)
}

/// Sets the process group ID of the given process.
pub(crate) fn sys_setpgid(pid: i32, pgid: i32) -> KResult<isize> {
    if pgid < 0 {
        return Err(KError::InvalidInput);
    }

    validate_job_pid(pid)?;
    let proc = if pid == 0 {
        kprocess::current_user_process()
    } else {
        kprocess::job_control::target_process(pid as Pid)?
    };
    let target_pid = proc.pid();
    let pgid = if pgid == 0 { target_pid } else { pgid as Pid };

    if pgid == target_pid {
        proc.create_group();
    } else if !proc.move_to_group(&kprocess::job_control::target_group(pgid)?) {
        return Err(KError::OperationNotPermitted);
    }

    Ok(0)
}

unittest::path_mod_test!(tests, "../../ut-tests/task/job/tests.rs");
