//! Event file descriptor syscalls.
//!
//! This module implements event notification operations including:
//! - Event file creation (eventfd, eventfd2, etc.)
//! - Event flags and modes (semaphore, non-blocking, etc.)

use bitflags::bitflags;
use kerrno::{KError, KResult};
use linux_raw_sys::general::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE};

use crate::file::{FileLike, add_file_like, event::EventFd};

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

    let event_fd = EventFd::new(initval as _, flags.contains(EventFdFlags::SEMAPHORE));
    event_fd.set_nonblocking(flags.contains(EventFdFlags::NONBLOCK))?;
    add_file_like(event_fd as _, flags.contains(EventFdFlags::CLOEXEC)).map(|fd| fd as _)
}
