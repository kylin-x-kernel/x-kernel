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
pub fn sys_getsid(pid: i32) -> KResult<isize> {
    validate_job_pid(pid)?;
    let proc = if pid == 0 {
        kprocess::current_user_process()
    } else {
        kprocess::job_control::query_process(pid as Pid)?
    };
    Ok(proc.group().session().sid() as _)
}

/// Creates a new session and makes the caller its leader.
pub fn sys_setsid() -> KResult<isize> {
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
pub fn sys_getpgid(pid: i32) -> KResult<isize> {
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
pub fn sys_getpgrp() -> KResult<isize> {
    Ok(kprocess::current_user_process().group().pgid() as _)
}

/// Sets the process group ID of the given process.
pub fn sys_setpgid(pid: i32, pgid: i32) -> KResult<isize> {
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

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{sys_getpgid, sys_getsid, sys_setpgid};

    #[def_test(user, serial)]
    fn test_getsid_zero_targets_current_process() {
        let proc = kprocess::current_user_process();
        assert_eq!(
            sys_getsid(0).unwrap(),
            proc.group().session().sid() as isize
        );
    }

    #[def_test(user, serial)]
    fn test_getpgid_zero_targets_current_process() {
        let proc = kprocess::current_user_process();
        assert_eq!(sys_getpgid(0).unwrap(), proc.group().pgid() as isize);
    }

    #[cfg(target_arch = "x86_64")]
    #[def_test(user, serial)]
    fn test_getpgrp_returns_current_process_group() {
        assert_eq!(super::sys_getpgrp().unwrap(), sys_getpgid(0).unwrap());
    }

    #[def_test(user, serial)]
    fn test_setpgid_zero_zero_targets_current_process() {
        let proc = kprocess::current_user_process();
        let original_pgid = proc.group().pgid();

        assert_eq!(sys_setpgid(0, 0).unwrap(), 0);
        assert_eq!(proc.group().pgid(), original_pgid);
    }
}
