//! Job control syscalls.
//!
//! This module implements job control operations including:
//! - Session management (getsid, setsid, etc.)
//! - Process group operations (getpgid, setpgid, etc.)
//! - Process group queries

use kcore::task::{AsThread, get_process_data, get_process_group};
use kerrno::{KError, KResult};
use kprocess::Pid;
use ktask::current;

/// Get the session ID of the process
pub fn sys_getsid(pid: Pid) -> KResult<isize> {
    Ok(get_process_data(pid)?.proc.group().session().sid() as _)
}

/// Create a new session and become the session leader
pub fn sys_setsid() -> KResult<isize> {
    let curr = current();
    let proc = &curr.as_thread().proc_data.proc;
    if get_process_group(proc.pid()).is_ok() {
        return Err(KError::OperationNotPermitted);
    }

    if let Some((session, _)) = proc.create_session() {
        Ok(session.sid() as _)
    } else {
        Ok(proc.pid() as _)
    }
}

/// Get the process group ID of the process
pub fn sys_getpgid(pid: Pid) -> KResult<isize> {
    Ok(get_process_data(pid)?.proc.group().pgid() as _)
}

/// Set the process group ID of the process
pub fn sys_setpgid(pid: Pid, pgid: Pid) -> KResult<isize> {
    let proc = &get_process_data(pid)?.proc;

    if pgid == 0 {
        proc.create_group();
    } else if !proc.move_to_group(&get_process_group(pgid)?) {
        return Err(KError::OperationNotPermitted);
    }

    Ok(0)
}

// TODO: job control
