// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Event file descriptor syscalls.
//!
//! This module implements event notification operations including:
//! - Event file creation (`eventfd2`)
//! - Event flags and modes (semaphore, non-blocking, etc.)

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kfd_objects::eventfd::EventFd;
use linux_raw_sys::general::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, O_RDWR};

bitflags! {
    /// Flags for the `eventfd2` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EventFdFlags: u32 {
        /// Create a file descriptor that is closed on `exec`.
        const CLOEXEC = EFD_CLOEXEC;
        /// Create a non-blocking eventfd.
        const NONBLOCK = EFD_NONBLOCK;
        /// Create a semaphore eventfd.
        const SEMAPHORE = EFD_SEMAPHORE;
    }
}

/// Creates an eventfd object and returns a new file descriptor.
pub fn sys_eventfd2(initval: u32, flags: u32) -> KResult<isize> {
    debug!("sys_eventfd2 <= initval: {initval}, flags: {flags}");

    let flags = EventFdFlags::from_bits(flags).ok_or(KError::InvalidInput)?;

    let event_fd = EventFd::new_file(
        initval as _,
        flags.contains(EventFdFlags::SEMAPHORE),
        O_RDWR | (flags.bits() & EFD_NONBLOCK),
        kprocess::current_cred(),
    )?;
    kprocess::current_resources()
        .add_file(event_fd, flags.contains(EventFdFlags::CLOEXEC))
        .map(|fd| fd as _)
}
