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
use linux_raw_sys::general::{POLLNVAL, pollfd, timespec};
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr, k_sigset};

use super::FdPollSet;

/// Monitors the current user thread's descriptors with an optional timeout.
///
/// Negative descriptors are ignored and return zero `revents`. Invalid
/// nonnegative descriptors report `POLLNVAL` alongside other ready entries;
/// the result counts every entry with nonzero `revents`.
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
        match kprocess::current_resources().get_file(fd.fd) {
            Ok(f) => {
                fds.push((
                    f,
                    IoEvents::from_bits(fd.events as _).ok_or(KError::InvalidInput)?
                        | IoEvents::ALWAYS_POLL,
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
pub(crate) fn sys_poll(fds: UserPtr<pollfd>, nfds: u32, timeout: i32) -> KResult<isize> {
    let mut poll_fds = fds.load_vm_vec(nfds as usize)?;
    let timeout = if timeout < 0 {
        None
    } else {
        Some(TimeSpan::from_millis(timeout as u64))
    };
    let result = do_poll(&mut poll_fds, timeout, None);
    if result.is_ok() {
        fds.write_vm_slice(&poll_fds)?;
    }
    result
}

/// Poll file descriptors with high-precision timeout and signal masking
pub(crate) fn sys_ppoll(
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
        Some(timeout.read_vm()?.try_into_time_span()?)
    };
    // TODO: handle signal
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

#[unittest::mod_test]
mod tests_poll_results {
    use linux_raw_sys::general::{POLLIN, POLLNVAL, pollfd};
    use unittest::{assert_eq, def_test};

    use super::do_poll;
    use crate::{ipc::sys_eventfd2, vfs::sys_close};

    #[def_test(user)]
    fn test_negative_descriptors_clear_revents() {
        let mut fds = [-1, -2, i32::MIN].map(|fd| pollfd {
            fd,
            events: POLLIN as _,
            revents: 0x5a5a,
        });
        assert_eq!(
            do_poll(&mut fds, Some(ktime_types::TimeSpan::from_millis(0)), None),
            Ok(0)
        );
        for fd in fds {
            assert_eq!(fd.revents, 0);
        }
    }

    #[def_test(user)]
    fn test_invalid_descriptor_does_not_suppress_ready_entry() {
        let ready_fd = sys_eventfd2(1, 0).unwrap() as i32;
        let mut fds = [
            pollfd {
                fd: i32::MAX,
                events: 0,
                revents: 0x5a5a,
            },
            pollfd {
                fd: ready_fd,
                events: POLLIN as _,
                revents: 0x5a5a,
            },
            pollfd {
                fd: -1,
                events: POLLIN as _,
                revents: 0x5a5a,
            },
        ];
        let result = do_poll(&mut fds, None, None);
        sys_close(ready_fd).unwrap();
        assert_eq!(result, Ok(2));
        assert_eq!(fds[0].revents, POLLNVAL as i16);
        assert_eq!(fds[1].revents, POLLIN as i16);
        assert_eq!(fds[2].revents, 0);
    }

    #[def_test(user)]
    fn test_invalid_descriptor_returns_without_waiting_for_unready_entry() {
        let unready_fd = sys_eventfd2(0, 0).unwrap() as i32;
        let mut fds = [
            pollfd {
                fd: unready_fd,
                events: POLLIN as _,
                revents: 0x5a5a,
            },
            pollfd {
                fd: i32::MAX,
                events: 0,
                revents: 0x5a5a,
            },
        ];
        let result = do_poll(&mut fds, None, None);
        sys_close(unready_fd).unwrap();
        assert_eq!(result, Ok(1));
        assert_eq!(fds[0].revents, 0);
        assert_eq!(fds[1].revents, POLLNVAL as i16);
    }

    #[def_test(user)]
    fn test_duplicate_ready_entries_are_counted_with_invalid_entry() {
        let ready_fd = sys_eventfd2(1, 0).unwrap() as i32;
        let mut fds = [
            pollfd {
                fd: ready_fd,
                events: POLLIN as _,
                revents: 0,
            },
            pollfd {
                fd: i32::MAX,
                events: 0,
                revents: 0,
            },
            pollfd {
                fd: ready_fd,
                events: POLLIN as _,
                revents: 0,
            },
        ];
        let result = do_poll(&mut fds, None, None);
        sys_close(ready_fd).unwrap();
        assert_eq!(result, Ok(3));
        assert_eq!(fds[0].revents, POLLIN as i16);
        assert_eq!(fds[1].revents, POLLNVAL as i16);
        assert_eq!(fds[2].revents, POLLIN as i16);
    }

    #[def_test(user)]
    fn test_zero_timeout_clears_unready_entry() {
        let unready_fd = sys_eventfd2(0, 0).unwrap() as i32;
        let mut fds = [pollfd {
            fd: unready_fd,
            events: POLLIN as _,
            revents: 0x5a5a,
        }];
        let result = do_poll(&mut fds, Some(ktime_types::TimeSpan::from_millis(0)), None);
        sys_close(unready_fd).unwrap();
        assert_eq!(result, Ok(0));
        assert_eq!(fds[0].revents, 0);
    }
}
