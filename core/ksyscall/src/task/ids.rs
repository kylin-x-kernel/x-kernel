// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process identity syscalls.

use kerrno::{KError, KResult};

/// Returns the process ID of the current process.
pub fn sys_getpid() -> KResult<isize> {
    Ok(kprocess::current_user_thread().pid() as _)
}

/// Returns the parent process ID of the current process.
pub fn sys_getppid() -> KResult<isize> {
    let current_thread = kprocess::current_user_thread();
    current_thread
        .process()
        .parent()
        .ok_or(KError::NoSuchProcess)
        .map(|parent| parent.pid() as _)
}
