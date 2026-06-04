// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX file-like kernel objects owned by `posix-fs`.
//!
//! This module keeps filesystem-adjacent objects such as pipes.
//! Fd-backed kernel objects whose primary state is not filesystem
//! state, such as `timerfd` and `eventfd`, live in their own owner crate.

mod pipe_impl;

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kfd::FdTable;
use kfs::{FsContext, OpenOptions};
use linux_raw_sys::general::{O_RDONLY, O_WRONLY};

pub use self::pipe_impl::{
    PipeEndpoint, PipeObject, PipeReadEnd, PipeWriteEnd, current_pipe_endpoint,
};

/// Add stdin, stdout, and stderr backed by `/dev/console`.
pub fn add_stdio(fd_table: &mut FdTable, fs_context: &FsContext) -> KResult<()> {
    assert_eq!(fd_table.count(), 0);

    let open = |options: &mut OpenOptions, flags| {
        KResult::Ok(Arc::new(
            options
                .open_flags(flags)
                .open(fs_context, "/dev/console")?
                .into_file()?,
        ))
    };

    let tty_in = open(OpenOptions::new().read(true).write(false), O_RDONLY as _)?;
    let tty_out = open(OpenOptions::new().read(false).write(true), O_WRONLY as _)?;
    fd_table
        .insert_file_like(tty_in, false)
        .map_err(|_| KError::TooManyOpenFiles)?;
    fd_table
        .insert_file_like(tty_out.clone(), false)
        .map_err(|_| KError::TooManyOpenFiles)?;
    fd_table
        .insert_file_like(tty_out, false)
        .map_err(|_| KError::TooManyOpenFiles)?;

    Ok(())
}
