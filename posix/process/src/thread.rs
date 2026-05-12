// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX thread identity and thread-local state syscalls.

use kerrno::KResult;
use ktask::current;

/// Returns the thread ID of the current thread.
pub fn sys_gettid() -> KResult<isize> {
    Ok(current().id().as_u64() as _)
}

/// Sets the `clear_child_tid` pointer for the current thread.
pub fn sys_set_tid_address(clear_child_tid: usize) -> KResult<isize> {
    let current = current();
    kthread::current_thread().set_clear_child_tid(clear_child_tid);
    Ok(current.id().as_u64() as isize)
}
