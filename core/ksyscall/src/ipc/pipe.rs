// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pipe syscall adapters.

use core::ffi::c_int;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kfd_objects::pipe::create_pipe_files;
use linux_raw_sys::general::{O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_WRONLY};
use posix_types::UserPtr;

bitflags! {
    /// Flags for the `pipe2` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    struct PipeFlags: u32 {
        /// Create a pipe with close-on-exec flag.
        const CLOEXEC = O_CLOEXEC;
        /// Create a non-blocking pipe.
        const NONBLOCK = O_NONBLOCK;
    }
}

/// Creates a pipe and returns the read/write file descriptors.
pub fn sys_pipe2(fds: UserPtr<[c_int; 2]>, flags: u32) -> KResult<isize> {
    let flags = PipeFlags::from_bits(flags).ok_or(KError::InvalidInput)?;

    let cloexec = flags.contains(PipeFlags::CLOEXEC);
    let status_flags = if flags.contains(PipeFlags::NONBLOCK) {
        O_NONBLOCK
    } else {
        0
    };
    let (read_file, write_file) =
        create_pipe_files(O_RDONLY | status_flags, O_WRONLY | status_flags)?;
    let resources = kprocess::current_resources();
    let read_fd = resources.add_file(read_file, cloexec)?;
    let write_fd = resources.add_file(write_file, cloexec).inspect_err(|_| {
        if let Err(err) = resources.close_file(read_fd) {
            warn!("sys_pipe2 cleanup failed for read fd {read_fd}: {err:?}");
        }
    })?;

    fds.write_vm([read_fd, write_fd])?;

    debug!(
        "sys_pipe2 <= fds: {:?}, flags: {:?}",
        [read_fd, write_fd],
        flags
    );
    Ok(0)
}
