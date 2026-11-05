// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `syslog(2)`: per-action privilege policy and the read-side contract.

use posix_types::UserPtr;
use unittest::{assert, assert_eq, def_test};
use unittest_support::TestUserBuffer;

use super::{
    SYSLOG_ACTION_CLEAR, SYSLOG_ACTION_READ_ALL, SYSLOG_ACTION_READ_CLEAR,
    SYSLOG_ACTION_SIZE_BUFFER, SYSLOG_ACTION_SIZE_UNREAD, action_needs_privilege, sys_syslog,
};
use klogger::LOGBUF_SIZE;

/// The typed user pointer the dispatch arm builds for `syslog(2)`.
fn user_buf(buffer: &TestUserBuffer) -> UserPtr<u8> {
    UserPtr::from(buffer.as_user_ptr::<u8>())
}

/// Reading the log is open to every caller, like Linux with `dmesg_restrict`
/// off; mutating it and the unread count stay privileged. Unknown actions are
/// restricted, which is how Linux reaches `EPERM` before its `EINVAL` default.
#[def_test]
fn action_privilege_policy_matches_linux_per_action() {
    assert!(!action_needs_privilege(SYSLOG_ACTION_READ_ALL));
    assert!(!action_needs_privilege(SYSLOG_ACTION_SIZE_BUFFER));

    assert!(action_needs_privilege(SYSLOG_ACTION_READ_CLEAR));
    assert!(action_needs_privilege(SYSLOG_ACTION_CLEAR));
    assert!(action_needs_privilege(SYSLOG_ACTION_SIZE_UNREAD));

    // Unknown actions are restricted as well.
    assert!(action_needs_privilege(0));
    assert!(action_needs_privilege(6));
    assert!(action_needs_privilege(i32::MAX));
}

/// `SIZE_BUFFER` reports the ring capacity and needs no destination buffer.
#[def_test]
fn size_buffer_reports_the_ring_capacity() {
    assert_eq!(
        sys_syslog(SYSLOG_ACTION_SIZE_BUFFER, UserPtr::<u8>::default(), 0),
        Ok(LOGBUF_SIZE as isize)
    );
}

/// `READ_ALL` returns the newest whole records and leaves them buffered, so a
/// second read returns the same window.
///
/// `user`: the destination must be a real user mapping, so the case runs in the
/// user-test context that `TestUserBuffer` needs.
#[def_test(user)]
fn read_all_returns_bytes_without_consuming_them() {
    let buffer = TestUserBuffer::new(4096).unwrap();
    // A fresh marker is the newest record, so a consuming read would remove it.
    klogger::error!("syslog read-all marker");
    let read = sys_syslog(
        SYSLOG_ACTION_READ_ALL,
        user_buf(&buffer),
        buffer.len() as i32,
    )
    .expect("a privileged read succeeds");
    assert!(read >= 0);
    assert!(read as usize <= buffer.len());

    // Reading does not consume: the marker is still readable afterwards, even
    // though other kernel threads keep logging.
    let mut probe = [0u8; 4096];
    let probe_len = klogger::logbuf_read_newest(&mut probe);
    let probe_text = core::str::from_utf8(&probe[..probe_len]).expect("log text is utf-8");
    assert!(
        probe_text.contains("syslog read-all marker"),
        "READ_ALL consumed the ring: {probe_text}"
    );

    let bytes = buffer.read_bytes(read as usize).unwrap();
    if !bytes.is_empty() {
        // The window starts at a record boundary and is character-aligned.
        let text = core::str::from_utf8(&bytes).expect("a whole-record window is valid UTF-8");
        assert!(text.starts_with('<'), "window does not start a record: {text:?}");
    }

    // No length comparison between two calls: records another CPU logs in
    // between shift the newest-record window, so only "the same records are still
    // there" is a property of a non-consuming read (asserted above with the
    // marker probe).
}

/// `READ_CLEAR` consumes the whole unread range, including the records older
/// than the returned window — Linux advances its clear cursor past everything it
/// skipped as well, so `dmesg -c` does clear the log.
///
/// `user` for the same reason as the read case.
#[def_test(user)]
fn read_clear_drops_the_unread_range() {
    klogger::error!("syslog read-clear marker");
    assert!(klogger::logbuf_len() > 0);

    let buffer = TestUserBuffer::new(klogger::LOGBUF_SIZE).unwrap();
    let read = sys_syslog(
        SYSLOG_ACTION_READ_CLEAR,
        user_buf(&buffer),
        buffer.len() as i32,
    )
    .expect("a privileged read succeeds");
    assert!(read > 0);
    let bytes = buffer.read_bytes(read as usize).unwrap();
    assert!(
        core::str::from_utf8(&bytes).is_ok(),
        "the cleared window is not whole records"
    );

    // The cleared range does not come back; other contexts may have logged since.
    // Probe the whole ring, otherwise more than one window of newer records would
    // hide a failed clear behind the window choice alone.
    let mut probe = alloc::vec![0u8; klogger::LOGBUF_SIZE];
    let len = klogger::logbuf_read_newest(&mut probe);
    let text = core::str::from_utf8(&probe[..len]).expect("log text is utf-8");
    assert!(
        !text.contains("syslog read-clear marker"),
        "READ_CLEAR left the record behind: {text}"
    );
}

/// A zero length returns 0 and keeps the ring: nothing was delivered, so nothing
/// may be consumed (Linux returns before touching the buffer for `len == 0`).
///
/// The invariant is "the ring is still non-empty", not "its length is unchanged":
/// other kernel threads keep logging, and a full ring may even shrink briefly
/// while a record is evicted to make room. A fresh record is emitted before each
/// call so a clear would leave the ring empty at the check.
#[def_test(user)]
fn zero_length_keeps_the_ring() {
    let buffer = TestUserBuffer::new(64).unwrap();

    for action in [SYSLOG_ACTION_READ_ALL, SYSLOG_ACTION_READ_CLEAR] {
        klogger::error!("syslog zero-length marker");
        assert!(klogger::logbuf_len() > 0);

        assert_eq!(sys_syslog(action, user_buf(&buffer), 0), Ok(0));
        assert!(
            klogger::logbuf_len() > 0,
            "a zero-length read cleared the ring"
        );
    }
}

/// `len` is Linux's signed `int`: a negative value fails like Linux's `len < 0`
/// instead of being clamped to the ring size and copied past the declared length.
#[def_test(user)]
fn negative_length_is_rejected() {
    let buffer = TestUserBuffer::new(64).unwrap();
    for action in [SYSLOG_ACTION_READ_ALL, SYSLOG_ACTION_READ_CLEAR] {
        assert_eq!(
            sys_syslog(action, user_buf(&buffer), -1),
            Err(kerrno::KError::InvalidInput)
        );
    }
}

/// A null destination is invalid whatever the length, and is rejected before the
/// zero-length fast path.
///
/// `user` because the syscall asks for the current credential first.
#[def_test(user)]
fn null_buffer_is_rejected_even_with_a_zero_length() {
    for action in [SYSLOG_ACTION_READ_ALL, SYSLOG_ACTION_READ_CLEAR] {
        assert_eq!(
            sys_syslog(action, UserPtr::<u8>::default(), 0),
            Err(kerrno::KError::InvalidInput)
        );
    }
}

/// An unknown action from a privileged caller reports `EINVAL`, like Linux's
/// `switch` default.
#[def_test(user)]
fn unknown_action_is_rejected() {
    let buffer = TestUserBuffer::new(64).unwrap();
    assert_eq!(
        sys_syslog(6, user_buf(&buffer), 64),
        Err(kerrno::KError::InvalidInput)
    );
}