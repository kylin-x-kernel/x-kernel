// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `sigsetsize` is only meaningful next to a real sigmask pointer.
//!
//! Linux validates the size inside `set_user_sigmask()`, which returns early
//! for a NULL mask (fs/eventpoll.c `do_epoll_pwait`). Go's runtime issues
//! `SYS_EPOLL_PWAIT` with a NULL mask and never initializes the size register,
//! so an unconditional check crashed every Go netpoll wait with EINVAL.

use core::mem::size_of;

use kerrno::KError;
use posix_types::{UserConstPtr, UserPtr, k_epoll_event, k_sigset};
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserBuffer, TestUserValue};

use super::{sys_epoll_create1, sys_epoll_pwait};
use crate::vfs::sys_close;

#[def_test(user)]
fn test_sigmask_less_epoll_pwait_accepts_any_sigsetsize() {
    let epfd = sys_epoll_create1(0).unwrap() as i32;
    let events = TestUserBuffer::new(size_of::<k_epoll_event>()).unwrap();

    for sigsetsize in [0, size_of::<k_sigset>(), 64, 128, 0xdead_beef] {
        assert_eq!(
            sys_epoll_pwait(
                epfd,
                UserPtr::from(events.as_user_ptr::<k_epoll_event>()),
                1,
                0,
                UserConstPtr::default(),
                sigsetsize,
            ),
            Ok(0)
        );
    }

    sys_close(epfd).unwrap();
}

#[def_test(user)]
fn test_epoll_pwait_with_sigmask_rejects_foreign_sigsetsize() {
    let epfd = sys_epoll_create1(0).unwrap() as i32;
    let events = TestUserBuffer::new(size_of::<k_epoll_event>()).unwrap();
    let sigmask = TestUserValue::from_value(k_sigset(0)).unwrap();

    assert_eq!(
        sys_epoll_pwait(
            epfd,
            UserPtr::from(events.as_user_ptr::<k_epoll_event>()),
            1,
            0,
            UserConstPtr::from(sigmask.as_user_ptr().addr()),
            64,
        ),
        Err(KError::InvalidInput)
    );

    sys_close(epfd).unwrap();
}