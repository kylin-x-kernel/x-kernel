// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem sync syscalls.

use kerrno::{KError, KResult};
use kfs::{File, sync_filesystems};

pub fn sys_sync() -> KResult<isize> {
    sync_filesystems()?;
    Ok(0)
}

pub fn sys_syncfs(fd: i32) -> KResult<isize> {
    let file_like = kprocess::current_resources().get_file_like(fd)?;

    if let Some(file) = file_like.downcast_ref::<File>() {
        file.location().super_block().sync_fs()?;
        return Ok(0);
    }

    Err(KError::InvalidInput)
}
