// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor abstractions and concrete file-like implementations.

pub mod event;
mod fs;
mod net;
mod pidfd;
mod pipe;
pub mod timerfd;

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kfd::FdTable;
pub use kfd::{FileLike, IoDst, IoSrc, Kstat, ReadBuf, WriteBuf};
use kfs::{FsContext, OpenOptions};
use linux_raw_sys::general::{O_RDONLY, O_WRONLY};

pub use self::{
    fs::{Directory, File, ResolveAtResult, metadata_to_kstat, resolve_at, with_fs},
    net::Socket,
    pidfd::PidFd,
    pipe::Pipe,
};

/// Adds stdin/stdout/stderr entries using the provided filesystem view.
pub fn add_stdio(fd_table: &mut FdTable, fs_context: &FsContext) -> KResult<()> {
    assert_eq!(fd_table.count(), 0);
    let open = |options: &mut OpenOptions, flags| {
        KResult::Ok(Arc::new(File::new(
            options.open(fs_context, "/dev/console")?.into_file()?,
            flags,
        )))
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
