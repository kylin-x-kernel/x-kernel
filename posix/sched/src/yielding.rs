// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX yield syscalls.

use kerrno::KResult;

/// Yields the processor voluntarily.
pub fn sys_sched_yield() -> KResult<isize> {
    ktask::yield_now();
    Ok(0)
}
