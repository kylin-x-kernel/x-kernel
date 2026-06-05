// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Poll syscalls.
//!
//! This module implements poll I/O multiplexing including:
//! - Poll operations (poll, ppoll, etc.)
//! - File descriptor monitoring
//! - Event notification and handling

use alloc::vec::Vec;

use kerrno::{KError, KResult};
use khal::time::TimeValue;
use kpoll::IoEvents;
use ksignal::SignalSet;
use ktask::future::{self, block_on, poll_io};
use linux_raw_sys::general::{POLLNVAL, pollfd, timespec};
use posix_types::{TimeValueLike, UserConstPtr, UserPtr, k_sigset};

use super::FdPollSet;

/// Monitor multiple file descriptors for I/O events with optional timeout
fn do_poll(
    poll_fds: &mut [pollfd],
    timeout: Option<TimeValue>,
    sigmask: Option<SignalSet>,
) -> KResult<isize> {
    debug!("do_poll fds={poll_fds:?} timeout={timeout:?}");

    let mut res = 0isize;
    let mut fds = Vec::with_capacity(poll_fds.len());
    let mut revents = Vec::with_capacity(poll_fds.len());
    for fd in poll_fds.iter_mut() {
        if fd.fd == -1 {
            // Skip -1
            continue;
        }
        match kthread::current_resources().get_file_like(fd.fd) {
            Ok(f) => {
                fds.push((
                    f,
                    IoEvents::from_bits(fd.events as _).ok_or(KError::InvalidInput)?
                        | IoEvents::ALWAYS_POLL,
                ));
                revents.push(&mut fd.revents);
            }
            Err(_) => {
                // If the fd is invalid, set revents to POLLNVAL
                fd.revents = POLLNVAL as _;
                res += 1;
            }
        }
    }
    if res > 0 {
        return Ok(res);
    }
    let fds = FdPollSet(fds);

    kthread::current_thread().with_temp_blocked(sigmask, || {
        match block_on(future::timeout(
            timeout,
            poll_io(&fds, IoEvents::empty(), false, || {
                let mut res = 0usize;
                for ((fd, events), revents) in fds.0.iter().zip(revents.iter_mut()) {
                    let mut result = fd.poll();
                    if result.contains(IoEvents::IN) {
                        result |= IoEvents::RDNORM;
                    }
                    if result.contains(IoEvents::OUT) {
                        result |= IoEvents::WRNORM;
                    }
                    // POSIX: POLLHUP and POLLERR are always reported in revents,
                    // even if not requested in events. They must NOT be masked out.
                    let always_report =
                        result & (IoEvents::HUP | IoEvents::ERR | IoEvents::RDHUP | IoEvents::NVAL);
                    result &= *events;
                    result |= always_report;

                    **revents = result.bits() as _;
                    if **revents != 0 {
                        res += 1;
                    }
                }
                if res > 0 {
                    Ok(res as _)
                } else {
                    Err(KError::WouldBlock)
                }
            }),
        )) {
            Ok(r) => r,
            Err(_) => Ok(0),
        }
    })
}

/// Poll file descriptors with millisecond timeout
#[cfg(target_arch = "x86_64")]
pub fn sys_poll(fds: UserPtr<pollfd>, nfds: u32, timeout: i32) -> KResult<isize> {
    let mut poll_fds = fds.load_vm_vec(nfds as usize)?;
    let timeout = if timeout < 0 {
        None
    } else {
        Some(TimeValue::from_millis(timeout as u64))
    };
    let result = do_poll(&mut poll_fds, timeout, None);
    if result.is_ok() {
        fds.write_vm_slice(&poll_fds)?;
    }
    result
}

/// Poll file descriptors with high-precision timeout and signal masking
pub fn sys_ppoll(
    fds: UserPtr<pollfd>,
    nfds: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    posix_types::check_sigset_size(sigsetsize)?;
    let nfds = nfds.try_into().map_err(|_| KError::InvalidInput)?;
    let mut poll_fds = fds.load_vm_vec(nfds)?;
    let timeout = if timeout.is_null() {
        None
    } else {
        Some(timeout.read_vm()?.try_into_time_value()?)
    };
    // TODO: dispatch_irq signal
    let sigmask = if sigmask.is_null() {
        None
    } else {
        Some(sigmask.read_vm()?.into())
    };
    let result = do_poll(&mut poll_fds, timeout, sigmask);
    if result.is_ok() {
        fds.write_vm_slice(&poll_fds)?;
    }
    result
}
