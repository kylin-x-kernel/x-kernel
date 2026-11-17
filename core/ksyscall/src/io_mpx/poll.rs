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
use kpoll::IoEvents;
use ksignal::SignalSet;
use ktask::future::{self, block_on, poll_io};
use ktime_types::TimeSpan;
use linux_raw_sys::general::{POLLNVAL, RLIMIT_NOFILE, pollfd, timespec};
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr, k_sigset};

use super::FdPollSet;

/// Monitors the current user thread's descriptors with an optional timeout.
///
/// Negative descriptors are ignored and return zero `revents`. Invalid
/// nonnegative descriptors report `POLLNVAL` alongside other ready entries;
/// the result counts every entry with nonzero `revents`. Unrecognized request
/// bits are ignored. Only error and hangup events bypass the requested mask;
/// a peer write-half closure (`POLLRDHUP`) must be explicitly requested.
fn do_poll(
    poll_fds: &mut [pollfd],
    timeout: Option<TimeSpan>,
    sigmask: Option<SignalSet>,
) -> KResult<isize> {
    debug!("do_poll fds={poll_fds:?} timeout={timeout:?}");

    let mut invalid_count = 0usize;
    let mut fds = Vec::with_capacity(poll_fds.len());
    let mut revents = Vec::with_capacity(poll_fds.len());
    for fd in poll_fds.iter_mut() {
        fd.revents = 0;
        if fd.fd < 0 {
            continue;
        }
        match kprocess::current_files().get_file(fd.fd) {
            Ok(f) => {
                fds.push((
                    f,
                    IoEvents::from_bits_truncate(fd.events as _) | IoEvents::ALWAYS_POLL,
                ));
                revents.push(&mut fd.revents);
            }
            Err(_) => {
                fd.revents = POLLNVAL as _;
                invalid_count += 1;
            }
        }
    }
    let fds = FdPollSet(fds);

    kprocess::current_user_thread().with_temp_blocked(sigmask, || {
        match block_on(future::timeout(
            timeout,
            poll_io(&fds, IoEvents::empty(), false, || {
                // Invalid entries make the call ready, but valid entries still
                // need their current events reported in the same result.
                let mut res = invalid_count;
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
                    let always_report = result & (IoEvents::HUP | IoEvents::ERR | IoEvents::NVAL);
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

fn do_sys_poll(
    fds: UserPtr<pollfd>,
    nfds: u32,
    timeout: Option<TimeSpan>,
    sigmask: Option<SignalSet>,
) -> KResult<isize> {
    let limit_fds = kprocess::current_user_process()
        .resources()?
        .rlimit(RLIMIT_NOFILE)?
        .current;
    if u64::from(nfds) > limit_fds {
        return Err(KError::InvalidInput);
    }
    let mut poll_fds = if nfds == 0 {
        Vec::new()
    } else {
        fds.load_vm_vec(nfds as usize)?
    };
    let result = do_poll(&mut poll_fds, timeout, sigmask)?;
    if nfds != 0 {
        fds.write_vm_slice(&poll_fds)?;
    }
    Ok(result)
}

/// Poll file descriptors with millisecond timeout
#[cfg(target_arch = "x86_64")]
pub(crate) fn sys_poll(fds: UserPtr<pollfd>, nfds: u32, timeout_ms: i32) -> KResult<isize> {
    let timeout = if timeout_ms < 0 {
        None
    } else {
        Some(TimeSpan::from_millis(timeout_ms as u64))
    };
    do_sys_poll(fds, nfds, timeout, None)
}

/// Poll file descriptors with high-precision timeout and signal masking
pub(crate) fn sys_ppoll(
    fds: UserPtr<pollfd>,
    nfds: u32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    let timeout = if timeout.is_null() {
        None
    } else {
        Some(timeout.read_vm()?.try_into_time_span()?)
    };
    let sigmask = if sigmask.is_null() {
        None
    } else {
        posix_types::check_sigset_size(sigsetsize)?;
        Some(sigmask.read_vm()?.into())
    };
    do_sys_poll(fds, nfds, timeout, sigmask)
}

unittest::path_mod_test!(
    tests_poll_results,
    "../../ut-tests/io_mpx/poll/tests_poll_results.rs"
);

unittest::path_mod_test!(
    tests_ppoll_sigmask,
    "../../ut-tests/io_mpx/poll/tests_ppoll_sigmask.rs"
);

unittest::path_mod_test!(
    tests_poll_null_array,
    "../../ut-tests/io_mpx/poll/tests_poll_null_array.rs"
);
