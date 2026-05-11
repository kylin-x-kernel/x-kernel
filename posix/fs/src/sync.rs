// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem sync syscalls.

use kerrno::{KError, KResult};
use kservices::file::{Directory, File};

pub fn sys_sync() -> KResult<isize> {
    let root = kthread::current_process_state()
        .fs_context()
        .lock()
        .root_dir()
        .clone();
    root.filesystem().flush()?;
    Ok(0)
}

pub fn sys_syncfs(fd: i32) -> KResult<isize> {
    let file_like = kthread::current_resources().get_file_like(fd)?;

    if let Some(file) = file_like.downcast_ref::<File>() {
        file.inner().location().filesystem().flush()?;
        return Ok(0);
    }

    if let Some(dir) = file_like.downcast_ref::<Directory>() {
        dir.inner().filesystem().flush()?;
        return Ok(0);
    }

    Err(KError::InvalidInput)
}
