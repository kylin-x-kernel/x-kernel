// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::{
    general::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLRDHUP, pollfd},
    net::{AF_UNIX, SHUT_WR, SOCK_STREAM},
};
use posix_types::UserPtr;
use unittest::{assert_eq, def_test};
use unittest_support::TestUserValue;

use super::do_poll;
use crate::{
    ipc::{sys_eventfd2, sys_pipe2},
    vfs::sys_close,
};

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

#[def_test(user)]
fn test_unknown_event_bits_are_ignored_without_suppressing_requested_events() {
    let ready_fd = sys_eventfd2(1, 0).unwrap() as i32;
    // Neither bit is a userspace poll event; the sign bit also exercises the
    // signed-short ABI conversion without changing the requested known bits.
    let requested = [
        0x4000,
        0x4000 | POLLIN as i16,
        i16::MIN,
        i16::MIN | POLLIN as i16,
    ];
    let mut fds = requested.map(|events| pollfd {
        fd: ready_fd,
        events,
        revents: 0x5a5a,
    });
    let result = do_poll(&mut fds, Some(ktime_types::TimeSpan::from_millis(0)), None);
    sys_close(ready_fd).unwrap();

    assert_eq!(result, Ok(2));
    for (index, fd) in fds.iter().enumerate() {
        assert_eq!(fd.events, requested[index]);
        assert_eq!(fd.revents, if index % 2 == 0 { 0 } else { POLLIN as i16 });
    }
}

fn stream_socketpair() -> kerrno::KResult<[i32; 2]> {
    let fds = TestUserValue::from_value([-1i32; 2])?;
    posix_net::sys_socketpair(AF_UNIX, SOCK_STREAM, 0, UserPtr::from(fds.as_user_ptr()))?;
    Ok(fds.read())
}

#[def_test(user)]
fn test_half_close_reports_rdhup_only_when_requested() {
    let [receiver, peer] = stream_socketpair().unwrap();
    posix_net::sys_shutdown(peer, SHUT_WR).unwrap();
    let requested = [0, POLLIN, POLLRDHUP, POLLIN | POLLRDHUP];
    let mut fds = requested.map(|events| pollfd {
        fd: receiver,
        events: events as _,
        revents: 0x5a5a,
    });
    let result = do_poll(&mut fds, Some(ktime_types::TimeSpan::from_millis(0)), None);
    sys_close(peer).unwrap();
    sys_close(receiver).unwrap();

    assert_eq!(result, Ok(3));
    for (fd, events) in fds.iter().zip(requested) {
        assert_eq!(fd.revents, events as i16);
    }
}

#[def_test(user)]
fn test_full_close_reports_hup_without_requesting_it() {
    let [receiver, peer] = stream_socketpair().unwrap();
    sys_close(peer).unwrap();
    let requested = [0, POLLIN, POLLRDHUP];
    let mut fds = requested.map(|events| pollfd {
        fd: receiver,
        events: events as _,
        revents: 0x5a5a,
    });
    let result = do_poll(&mut fds, Some(ktime_types::TimeSpan::from_millis(0)), None);
    sys_close(receiver).unwrap();

    assert_eq!(result, Ok(3));
    for (fd, events) in fds.iter().zip(requested) {
        assert_eq!(fd.revents, (events | POLLHUP) as i16);
    }
}

#[def_test(user)]
fn test_closed_pipe_reader_reports_err_without_requesting_it() {
    let fds = TestUserValue::from_value([-1i32; 2]).unwrap();
    assert_eq!(sys_pipe2(UserPtr::from(fds.as_user_ptr()), 0), Ok(0));
    let [reader, writer] = fds.read();
    sys_close(reader).unwrap();
    let mut fds = [pollfd {
        fd: writer,
        events: 0,
        revents: 0x5a5a,
    }];
    let result = do_poll(&mut fds, Some(ktime_types::TimeSpan::from_millis(0)), None);
    sys_close(writer).unwrap();

    assert_eq!(result, Ok(1));
    assert_eq!(fds[0].revents, POLLERR as i16);
}
