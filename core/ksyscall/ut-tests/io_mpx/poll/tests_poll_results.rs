// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
