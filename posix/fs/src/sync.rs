// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem sync syscalls.

use kerrno::KResult;

pub fn sys_sync() -> KResult<isize> {
    kvfs::sync_filesystems()?;
    Ok(0)
}

pub fn sys_syncfs(fd: i32) -> KResult<isize> {
    let file = kprocess::current_resources().get_file(fd)?;
    file.sync_filesystem()?;
    Ok(0)
}
