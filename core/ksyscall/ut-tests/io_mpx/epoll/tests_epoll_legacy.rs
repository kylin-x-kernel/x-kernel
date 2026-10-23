// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use kerrno::KError;
use linux_raw_sys::general::{EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLLIN};
use posix_types::{UserConstPtr, UserPtr, k_epoll_event};
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserBuffer, TestUserValue};

use super::{sys_epoll_create, sys_epoll_ctl, sys_epoll_wait};
use crate::{ipc::sys_eventfd2, vfs::sys_close};

// x86_64 keeps the legacy epoll_create(213)/epoll_wait(232) entries that
// newer architectures dropped; programs may call them directly.

#[def_test(user)]
fn test_epoll_create_rejects_nonpositive_size() {
    assert_eq!(sys_epoll_create(0), Err(KError::InvalidInput));
    assert_eq!(sys_epoll_create(-7), Err(KError::InvalidInput));
}

#[def_test(user)]
fn test_epoll_create_ignores_positive_size() {
    // Linux ignores the size hint; a positive value acts as epoll_create1(0).
    let epfd = sys_epoll_create(16).unwrap() as i32;
    sys_close(epfd).unwrap();
}

#[def_test(user)]
fn test_epoll_wait_reports_ready_and_timeout() {
    let epfd = sys_epoll_create(1).unwrap() as i32;
    let fd = sys_eventfd2(1, 0).unwrap() as i32;
    let interest = TestUserValue::from_value(k_epoll_event::new(EPOLLIN, 7)).unwrap();
    sys_epoll_ctl(epfd, EPOLL_CTL_ADD, fd, UserConstPtr::from(interest.as_user_ptr().addr()))
        .unwrap();

    let buf = TestUserBuffer::new(size_of::<k_epoll_event>()).unwrap();
    let ready = sys_epoll_wait(
        epfd,
        UserPtr::from(buf.as_user_ptr::<k_epoll_event>()),
        1,
        0,
    )
    .unwrap();
    assert_eq!(ready, 1);
    let bytes = buf.read_bytes(size_of::<k_epoll_event>()).unwrap();
    let events = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let data = u64::from_le_bytes(bytes[bytes.len() - size_of::<u64>()..].try_into().unwrap());
    assert_eq!(events & EPOLLIN, EPOLLIN);
    assert_eq!(data, 7);

    // Readiness is level-based: waiting does not consume the eventfd
    // counter. Deregister the source to exercise the no-events path.
    sys_epoll_ctl(epfd, EPOLL_CTL_DEL, fd, UserConstPtr::default()).unwrap();
    let ready = sys_epoll_wait(
        epfd,
        UserPtr::from(buf.as_user_ptr::<k_epoll_event>()),
        1,
        0,
    )
    .unwrap();
    assert_eq!(ready, 0);

    sys_close(epfd).unwrap();
    sys_close(fd).unwrap();
}
