// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process identity syscalls.

use kerrno::KResult;

/// Returns the process ID of the current process: the number in the PID
/// namespace the process was created in, so a `CLONE_NEWPID` child reads 1.
pub(crate) fn sys_getpid() -> KResult<isize> {
    let current_thread = kprocess::current_user_thread();
    Ok(current_thread.process().visible_pid() as _)
}

/// Returns the parent process ID of the current process, projected into the
/// process's active PID namespace (Linux `task_ppid_nr_ns`). Both a reaped
/// parent and a parent living outside the namespace read 0, as
/// `pid_namespaces(7)` documents for `getppid(2)`.
pub(crate) fn sys_getppid() -> KResult<isize> {
    let current_thread = kprocess::current_user_thread();
    Ok(current_thread.process().visible_parent_pid().unwrap_or(0) as _)
}
