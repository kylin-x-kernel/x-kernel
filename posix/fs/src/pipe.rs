// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pipe creation syscalls.
//!
//! This module implements pipe creation operations including:
//! - Pipe creation (pipe, pipe2, etc.)
//! - Pipe flags and configuration (O_CLOEXEC, O_NONBLOCK, etc.)

use alloc::sync::Arc;
use core::ffi::c_int;

use bitflags::bitflags;
use kerrno::KResult;
use kfd::FileLike;
use kservices::file::Pipe;
use linux_raw_sys::general::{O_CLOEXEC, O_NONBLOCK};
use osvm::VirtMutPtr;
use posix_types::UserPtr;

bitflags! {
    /// Flags for the `pipe2` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct PipeFlags: u32 {
        /// Create a pipe with close-on-exec flag.
        const CLOEXEC = O_CLOEXEC;
        /// Create a non-blocking pipe.
        const NONBLOCK = O_NONBLOCK;
    }
}

/// Creates a pipe and returns the read/write file descriptors.
pub fn sys_pipe2(fds: UserPtr<[c_int; 2]>, flags: u32) -> KResult<isize> {
    let flags = {
        let new_flags = PipeFlags::from_bits_truncate(flags);
        if new_flags.bits() != flags {
            warn!("sys_pipe2 <= unrecognized flags: {flags}");
        }
        new_flags
    };

    let cloexec = flags.contains(PipeFlags::CLOEXEC);
    let (read_end, write_end) = Pipe::new();
    if flags.contains(PipeFlags::NONBLOCK) {
        read_end.set_nonblocking(true)?;
        write_end.set_nonblocking(true)?;
    }
    let resources = kthread::current_resources();
    let read_fd = resources.add_file_like(Arc::new(read_end), cloexec)?;
    let write_fd = resources
        .add_file_like(Arc::new(write_end), cloexec)
        .inspect_err(|_| resources.close_file_like(read_fd).unwrap())?;

    fds.write_vm([read_fd, write_fd])?;

    debug!(
        "sys_pipe2 <= fds: {:?}, flags: {:?}",
        [read_fd, write_fd],
        flags
    );
    Ok(0)
}
