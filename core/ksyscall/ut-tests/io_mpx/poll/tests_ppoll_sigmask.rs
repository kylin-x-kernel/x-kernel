// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Same rule as the epoll wait: `sigsetsize` is validated only next to a real
//! sigmask pointer, mirroring `set_user_sigmask()` for `ppoll` (fs/select.c).

use core::mem::size_of;

use kerrno::KError;
use linux_raw_sys::general::{pollfd, timespec};
use posix_types::{UserConstPtr, UserPtr, k_sigset};
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserBuffer, TestUserValue};

use super::sys_ppoll;

/// A zeroed `timespec` keeps the wait non-blocking.
fn zero_timeout() -> TestUserBuffer {
    let timeout = TestUserBuffer::new(size_of::<timespec>()).unwrap();
    timeout.write_bytes(&[0; size_of::<timespec>()]).unwrap();
    timeout
}

/// A negative descriptor is ignored by `poll`, so the call reports no events.
fn ignored_fd() -> TestUserValue<pollfd> {
    TestUserValue::from_value(pollfd {
        fd: -1,
        events: 0,
        revents: 0,
    })
    .unwrap()
}

#[def_test(user)]
fn test_sigmask_less_ppoll_accepts_any_sigsetsize() {
    let timeout = zero_timeout();
    let fds = ignored_fd();

    for sigsetsize in [0, size_of::<k_sigset>(), 64, 128, 0xdead_beef] {
        assert_eq!(
            sys_ppoll(
                UserPtr::from(fds.as_user_ptr()),
                1,
                UserConstPtr::from(UserPtr::from(timeout.as_user_ptr::<timespec>())),
                UserConstPtr::default(),
                sigsetsize,
            ),
            Ok(0)
        );
    }
}

#[def_test(user)]
fn test_ppoll_with_sigmask_rejects_foreign_sigsetsize() {
    let timeout = zero_timeout();
    let fds = ignored_fd();
    let sigmask = TestUserValue::from_value(k_sigset(0)).unwrap();

    assert_eq!(
        sys_ppoll(
            UserPtr::from(fds.as_user_ptr()),
            1,
            UserConstPtr::from(UserPtr::from(timeout.as_user_ptr::<timespec>())),
            UserConstPtr::from(sigmask.as_user_ptr().addr()),
            64,
        ),
        Err(KError::InvalidInput)
    );
}