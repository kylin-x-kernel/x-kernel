// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `epoll_ctl` requires a poll handler on the target, mirroring Linux
//! `file_can_poll()`: a memory-backed regular file has none, so `EPOLL_CTL_ADD`
//! and `EPOLL_CTL_DEL` must answer `EPERM`.
//!
//! User-space runtimes rely on that rejection: the Go runtime keeps such a
//! descriptor in blocking mode instead of registering it with its netpoller
//! (`os.File.SetDeadline` reports `ErrNoDeadline` exactly because the poller
//! refused the file).

use kerrno::KError;
use linux_raw_sys::general::{EPOLLIN, EPOLL_CTL_ADD, EPOLL_CTL_DEL};
use posix_types::{UserConstPtr, k_epoll_event};
use unittest::{assert_eq, def_test};
use unittest_support::TestUserValue;

use super::{sys_epoll_create1, sys_epoll_ctl};
use crate::{ipc::sys_eventfd2, vfs::sys_close};

/// A regular file: `memfd_create` yields a memory-backed file with no poll handler.
fn memfd() -> i32 {
    let name = TestUserValue::from_value(*b"epoll-pollable-test\0").unwrap();
    posix_mm::sys_memfd_create(UserConstPtr::from(name.as_user_ptr() as usize), 0).unwrap() as i32
}

fn interest() -> TestUserValue<k_epoll_event> {
    TestUserValue::from_value(k_epoll_event::new(EPOLLIN, 7)).unwrap()
}

#[def_test(user)]
fn test_epoll_ctl_rejects_regular_file_target() {
    let epfd = sys_epoll_create1(0).unwrap() as i32;
    let target = memfd();
    let event = interest();

    // The capability gate precedes the operation switch, so DEL is covered too.
    for op in [EPOLL_CTL_ADD, EPOLL_CTL_DEL] {
        assert_eq!(
            sys_epoll_ctl(
                epfd,
                op,
                target,
                UserConstPtr::from(event.as_user_ptr().addr()),
            ),
            Err(KError::OperationNotPermitted)
        );
    }

    sys_close(target).unwrap();
    sys_close(epfd).unwrap();
}

#[def_test(user)]
fn test_epoll_ctl_accepts_pollable_target() {
    let epfd = sys_epoll_create1(0).unwrap() as i32;
    let target = sys_eventfd2(0, 0).unwrap() as i32;
    let event = interest();

    assert_eq!(
        sys_epoll_ctl(
            epfd,
            EPOLL_CTL_ADD,
            target,
            UserConstPtr::from(event.as_user_ptr().addr()),
        ),
        Ok(0)
    );

    sys_close(target).unwrap();
    sys_close(epfd).unwrap();
}
/// The event copy, the capability gate and the operation dispatch happen in
/// that order, as in Linux: `__do_sys_epoll_ctl` copies the user event for
/// every operation except `DEL`, `do_epoll_ctl` then tests `file_can_poll`
/// before `switch (op)`.
#[def_test(user)]
fn test_epoll_ctl_orders_copy_gate_and_dispatch() {
    let epfd = sys_epoll_create1(0).unwrap() as i32;
    let unpollable = memfd();
    let pollable = sys_eventfd2(0, 0).unwrap() as i32;
    let event = interest();
    let valid = || UserConstPtr::from(event.as_user_ptr().addr());
    // An unknown operation still carries an event in Linux (`ep_op_has_event`
    // only excludes DEL), so the copy runs before anything else.
    const UNKNOWN_OP: u32 = 0xdead_beef;

    // A bad event pointer answers EFAULT even though the target is unpollable.
    assert_eq!(
        sys_epoll_ctl(epfd, EPOLL_CTL_ADD, unpollable, UserConstPtr::from(1)),
        Err(KError::BadAddress)
    );
    // Unpollable target: the gate answers before the dispatch sees the bad op.
    assert_eq!(
        sys_epoll_ctl(epfd, UNKNOWN_OP, unpollable, valid()),
        Err(KError::OperationNotPermitted)
    );
    // Pollable target: the dispatch is what rejects the unknown operation.
    assert_eq!(
        sys_epoll_ctl(epfd, UNKNOWN_OP, pollable, valid()),
        Err(KError::InvalidInput)
    );

    sys_close(pollable).unwrap();
    sys_close(unpollable).unwrap();
    sys_close(epfd).unwrap();
}
