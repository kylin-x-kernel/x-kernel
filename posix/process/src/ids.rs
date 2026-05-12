// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process ID syscalls.

use kerrno::{KError, KResult};

/// Returns the process ID of the current process.
pub fn sys_getpid() -> KResult<isize> {
    Ok(kthread::current_thread().pid() as _)
}

/// Returns the parent process ID of the current process.
pub fn sys_getppid() -> KResult<isize> {
    let current_thread = kthread::current_thread();
    current_thread
        .process_state()
        .proc
        .parent()
        .ok_or(KError::NoSuchProcess)
        .map(|parent| parent.pid() as _)
}
