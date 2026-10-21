// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::mem::size_of;

use linux_raw_sys::general::{EPOLL_CTL_ADD, EPOLLIN};
use posix_types::{UserConstPtr, UserPtr, k_epoll_event};
use unittest::def_test;
use unittest_support::{TestUserBuffer, TestUserValue};

use super::{sys_epoll_create1, sys_epoll_ctl, sys_epoll_pwait};
use crate::{ipc::sys_eventfd2, vfs::sys_close};

#[def_test(user)]
fn test_epoll_wait_zeroes_abi_padding() {
    const EVENTS: usize = 4;
    let epfd = sys_epoll_create1(0).unwrap() as i32;
    let interest = TestUserValue::from_value(k_epoll_event::new(EPOLLIN, 7)).unwrap();
    let mut fds = vec![];
    for _ in 0..EVENTS {
        let fd = sys_eventfd2(1, 0).unwrap() as i32;
        sys_epoll_ctl(
            epfd,
            EPOLL_CTL_ADD,
            fd,
            UserConstPtr::from(interest.as_user_ptr().addr()),
        )
        .unwrap();
        fds.push(fd);
    }

    let event_size = size_of::<k_epoll_event>();
    let buf = TestUserBuffer::new(EVENTS * event_size).unwrap();
    buf.write_bytes(&vec![0x5a; EVENTS * event_size]).unwrap();
    let ready = sys_epoll_pwait(
        epfd,
        UserPtr::from(buf.as_user_ptr::<k_epoll_event>()),
        EVENTS as i32,
        0,
        UserConstPtr::default(),
        0,
    )
    .unwrap();

    assert_eq!(ready, EVENTS as isize);
    // The padding sits between the `u32` events and the `u64` data; the
    // range is empty on x86_64, where the ABI packs the structure.
    let pad = size_of::<u32>()..event_size - size_of::<u64>();
    let bytes = buf.read_bytes(EVENTS * event_size).unwrap();
    for event in bytes.chunks_exact(event_size) {
        assert!(event[pad.clone()].iter().all(|&byte| byte == 0));
    }
    for fd in fds.into_iter().chain([epfd]) {
        sys_close(fd).unwrap();
    }
}
