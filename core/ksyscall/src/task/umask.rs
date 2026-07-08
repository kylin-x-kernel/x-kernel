// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process file-mode creation mask syscalls.

use kerrno::KResult;

/// Sets the process umask and returns the previous value.
pub fn sys_umask(mask: u32) -> KResult<isize> {
    let old = kprocess::current_user_process().replace_umask(mask)?;
    Ok(old as isize)
}
