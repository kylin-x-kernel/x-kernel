// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};
use core::mem::size_of;

use kerrno::{KError, LinuxError};
use ktime_types::{NANOS_PER_SEC, TimeSpan};
use linux_raw_sys::{
    general::timeval,
    net::{
        AF_INET, AF_UNIX, IPPROTO_TCP, SO_DOMAIN, SO_PEERCRED, SO_PROTOCOL, SO_TYPE, SOCK_DGRAM,
        SOCK_STREAM, SOL_SOCKET, TCP_KEEPCNT, TCP_KEEPIDLE, TCP_KEEPINTVL, socklen_t,
    },
};
use posix_types::UserPtr;
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserArray, TestUserBuffer, TestUserValue};

use super::{
    BPF_JA, BPF_JMP, BPF_RET, IFNAMSIZ, SockFilter, conv::SocketTimeout, encode_bound_device_name,
    sys_getsockopt, sys_setsockopt, validate_classic_filter_instructions,
};
use crate::socket::{sys_socket, sys_socketpair};

const NANOS_PER_TICK: u64 = NANOS_PER_SEC / kbuild_config::TICKS_PER_SECOND as u64;

#[def_test]
fn unbound_device_get_accepts_zero_length() {
    let encoded = encode_bound_device_name(None, 0).unwrap();

    assert_eq!(encoded.len(), 0);
    assert_eq!(encode_bound_device_name(None, IFNAMSIZ).unwrap().len(), 0);
}

#[def_test]
fn bound_device_get_requires_ifnamsiz_and_returns_actual_length() {
    assert_eq!(
        encode_bound_device_name(Some("eth0"), IFNAMSIZ - 1).unwrap_err(),
        KError::InvalidInput
    );

    let encoded = encode_bound_device_name(Some("eth0"), IFNAMSIZ).unwrap();
    assert_eq!(encoded.as_slice(), b"eth0\0");
}

#[def_test]
fn zero_socket_timeout_remains_infinite() {
    let timeout = SocketTimeout::sys_to_rust(timeval {
        tv_sec: 0,
        tv_usec: 0,
    })
    .unwrap();

    assert_eq!(timeout, TimeSpan::ZERO);
}

/// Regression test for <https://gitee.com/openkylin/x-kernel/issues/IK97G7>.
#[def_test]
fn one_microsecond_socket_timeout_rounds_up_to_one_tick() {
    let timeout = SocketTimeout::sys_to_rust(timeval {
        tv_sec: 0,
        tv_usec: 1,
    })
    .unwrap();

    assert_eq!(timeout, TimeSpan::from_nanos(NANOS_PER_TICK));
}

#[def_test]
fn exact_tick_socket_timeout_is_unchanged() {
    let timeout = SocketTimeout::sys_to_rust(timeval {
        tv_sec: 0,
        tv_usec: (NANOS_PER_TICK / 1_000) as _,
    })
    .unwrap();

    assert_eq!(timeout, TimeSpan::from_nanos(NANOS_PER_TICK));
}

#[def_test]
fn classic_filter_requires_a_return_terminator() {
    let unterminated = [SockFilter {
        code: 0,
        jt: 0,
        jf: 0,
        k: 0,
    }];
    assert_eq!(
        validate_classic_filter_instructions(&unterminated),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn classic_filter_rejects_out_of_range_jumps() {
    let program = [
        SockFilter {
            code: BPF_JMP | BPF_JA,
            jt: 0,
            jf: 0,
            k: 1,
        },
        SockFilter {
            code: BPF_RET,
            jt: 0,
            jf: 0,
            k: u32::MAX,
        },
    ];
    assert_eq!(
        validate_classic_filter_instructions(&program),
        Err(KError::InvalidInput)
    );
}

/// Releases a descriptor a test created, so the case cleans up after itself
/// instead of relying on the task being torn down.
fn close_socket(fd: i32) {
    kprocess::current_files().close_file(fd).unwrap();
}

/// Linux `sk_getsockopt` copies `min(optlen, sizeof(int))` bytes and reports the
/// length it wrote, so a buffer shorter than the value truncates instead of
/// failing and a zero length succeeds without touching the buffer.
#[def_test(user)]
fn so_type_reports_the_socket_type_and_truncates_short_buffers() {
    let fd = sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap() as i32;
    let value = TestUserBuffer::new(size_of::<i32>()).unwrap();
    let mut optlen = TestUserValue::from_value(size_of::<i32>() as socklen_t).unwrap();
    let value_ptr = UserPtr::from(value.as_user_ptr::<u8>());
    let optlen_ptr = UserPtr::from(optlen.as_user_ptr());

    // The full width reports the type the descriptor was created with.
    assert_eq!(
        sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, value_ptr, optlen_ptr),
        Ok(0)
    );
    assert_eq!(optlen.read(), size_of::<i32>() as socklen_t);
    assert_eq!(
        value.read_bytes(size_of::<i32>()).unwrap(),
        (SOCK_DGRAM as i32).to_ne_bytes().to_vec()
    );

    // Shorter buffers truncate and report the bytes actually written.
    for len in 1..size_of::<i32>() {
        value.write_bytes(&[0xff; size_of::<i32>()]).unwrap();
        optlen.write(len as socklen_t);
        assert_eq!(
            sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, value_ptr, optlen_ptr),
            Ok(0)
        );
        assert_eq!(optlen.read(), len as socklen_t);
        assert_eq!(
            value.read_bytes(len).unwrap(),
            (SOCK_DGRAM as i32).to_ne_bytes()[..len].to_vec()
        );
    }

    // A zero length is accepted without touching the buffer, so null is fine.
    optlen.write(0);
    assert_eq!(
        sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, UserPtr::default(), optlen_ptr),
        Ok(0)
    );
    assert_eq!(optlen.read(), 0);

    // A negative length is rejected before the buffer is touched, like Linux
    // (`sk_getsockopt` reads a signed `int` and returns `EINVAL` below zero).
    optlen.write(u32::MAX);
    assert_eq!(
        sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, UserPtr::default(), optlen_ptr),
        Err(KError::InvalidInput)
    );
    assert_eq!(optlen.read(), u32::MAX);

    // The descriptor is resolved first, so an invalid one reports `EBADF` even
    // when the length is invalid too, matching `__sys_getsockopt`.
    assert_eq!(
        sys_getsockopt(-1, SOL_SOCKET, SO_TYPE, UserPtr::default(), optlen_ptr),
        Err(KError::BadFileDescriptor)
    );

    // The same order holds for the length pointer itself: a bad descriptor
    // wins over an unreachable length, which Linux only reads inside
    // `sk_getsockopt`.
    assert_eq!(
        sys_getsockopt(
            -1,
            SOL_SOCKET,
            SO_TYPE,
            UserPtr::default(),
            UserPtr::default()
        ),
        Err(KError::BadFileDescriptor)
    );

    close_socket(fd);
}

/// `SO_DOMAIN` and `SO_PROTOCOL` describe the socket the way `SO_TYPE` does, so
/// they report what the descriptor was created with and share its short-buffer
/// handling.
#[def_test(user)]
fn so_domain_and_protocol_report_the_creation_family_and_protocol() {
    let value = TestUserValue::<i32>::new().unwrap();
    let mut optlen = TestUserValue::from_value(size_of::<i32>() as socklen_t).unwrap();
    let optlen_ptr = UserPtr::from(optlen.as_user_ptr());

    let stream = sys_socket(AF_INET, SOCK_STREAM, 0).unwrap() as i32;
    assert_eq!(
        sys_getsockopt(
            stream,
            SOL_SOCKET,
            SO_DOMAIN,
            UserPtr::from(value.as_user_ptr().cast::<u8>()),
            optlen_ptr
        ),
        Ok(0)
    );
    assert_eq!(value.read(), AF_INET as i32);

    optlen.write(size_of::<i32>() as socklen_t);
    assert_eq!(
        sys_getsockopt(
            stream,
            SOL_SOCKET,
            SO_PROTOCOL,
            UserPtr::from(value.as_user_ptr().cast::<u8>()),
            optlen_ptr
        ),
        Ok(0)
    );
    assert_eq!(value.read(), IPPROTO_TCP as i32);

    // AF_UNIX records no protocol, and `unix_create` leaves `sk_protocol` 0.
    let dgram = sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap() as i32;
    optlen.write(size_of::<i32>() as socklen_t);
    assert_eq!(
        sys_getsockopt(
            dgram,
            SOL_SOCKET,
            SO_DOMAIN,
            UserPtr::from(value.as_user_ptr().cast::<u8>()),
            optlen_ptr
        ),
        Ok(0)
    );
    assert_eq!(value.read(), AF_UNIX as i32);

    optlen.write(size_of::<i32>() as socklen_t);
    assert_eq!(
        sys_getsockopt(
            dgram,
            SOL_SOCKET,
            SO_PROTOCOL,
            UserPtr::from(value.as_user_ptr().cast::<u8>()),
            optlen_ptr
        ),
        Ok(0)
    );
    assert_eq!(value.read(), 0);

    // A short buffer truncates and reports the length it wrote, like `SO_TYPE`.
    let short = TestUserBuffer::new(size_of::<i32>()).unwrap();
    optlen.write(1);
    assert_eq!(
        sys_getsockopt(
            stream,
            SOL_SOCKET,
            SO_DOMAIN,
            UserPtr::from(short.as_user_ptr::<u8>()),
            optlen_ptr
        ),
        Ok(0)
    );
    assert_eq!(optlen.read(), 1);
    assert_eq!(
        short.read_bytes(1).unwrap(),
        (AF_INET as i32).to_ne_bytes()[..1].to_vec()
    );

    close_socket(stream);
    close_socket(dgram);
}

/// The three Linux TCP keep-alive knobs are wired through the syscall
/// dispatch; their defaults and range checks are covered by the socket-level
/// tests.
#[def_test(user)]
fn tcp_keepalive_options_round_trip_through_syscalls() {
    const VALUE_LEN: socklen_t = size_of::<u32>() as socklen_t;

    let fd = sys_socket(AF_INET, SOCK_STREAM, 0).unwrap() as i32;
    let value = TestUserValue::<u32>::new().unwrap();
    let mut optlen = TestUserValue::from_value(VALUE_LEN).unwrap();
    let optlen_ptr = UserPtr::from(optlen.as_user_ptr());
    let optval = UserPtr::from(value.as_user_ptr().cast::<u8>());

    let mut get = |optname: u32| {
        optlen.write(VALUE_LEN);
        sys_getsockopt(fd, IPPROTO_TCP as u32, optname, optval, optlen_ptr).unwrap();
        value.read()
    };
    let set = |optname: u32, new_value: u32| {
        let input = TestUserValue::<u32>::from_value(new_value).unwrap();
        sys_setsockopt(
            fd,
            IPPROTO_TCP as u32,
            optname,
            UserPtr::from(input.as_user_ptr().cast::<u8>()).as_const(),
            VALUE_LEN,
        )
    };

    assert_eq!(get(TCP_KEEPIDLE), 7200);
    assert_eq!(get(TCP_KEEPINTVL), 75);
    assert_eq!(get(TCP_KEEPCNT), 9);

    assert_eq!(set(TCP_KEEPIDLE, 30), Ok(0));
    assert_eq!(set(TCP_KEEPINTVL, 5), Ok(0));
    assert_eq!(set(TCP_KEEPCNT, 2), Ok(0));
    assert_eq!(get(TCP_KEEPIDLE), 30);
    assert_eq!(get(TCP_KEEPINTVL), 5);
    assert_eq!(get(TCP_KEEPCNT), 2);

    // An invalid value fails with EINVAL and does not clobber the old one.
    assert_eq!(set(TCP_KEEPINTVL, 0), Err(KError::InvalidInput));
    assert_eq!(get(TCP_KEEPINTVL), 5);

    close_socket(fd);
}

/// `SO_PEERCRED` returns the Linux `struct ucred` image of the peer captured
/// when the pair was created, with the same short-buffer rule as other
/// `getsockopt` results. A negative length is invalid even with a writable
/// buffer, and the option is read-only.
#[def_test(user)]
fn peer_credentials_report_the_socketpair_creator_and_truncate_short_buffers() {
    const UCRED_LEN: usize = 3 * size_of::<u32>();

    let fds = TestUserArray::<i32, 2>::new().unwrap();
    assert_eq!(
        sys_socketpair(
            AF_UNIX,
            SOCK_STREAM,
            0,
            UserPtr::from(fds.as_user_ptr().cast::<[i32; 2]>())
        ),
        Ok(0)
    );
    let [first, second] = fds.read();
    let thread = kprocess::current_user_thread();
    let cred = thread.cred();
    let expected: Vec<u8> = [
        thread.process().pid_identity().root_nr(),
        cred.euid(),
        cred.egid(),
    ]
    .into_iter()
    .flat_map(u32::to_ne_bytes)
    .collect();

    let value = TestUserBuffer::new(UCRED_LEN).unwrap();
    let value_ptr = UserPtr::from(value.as_user_ptr::<u8>());
    let mut optlen = TestUserValue::from_value(UCRED_LEN as socklen_t).unwrap();
    let optlen_ptr = UserPtr::from(optlen.as_user_ptr());
    for fd in [first, second] {
        optlen.write(UCRED_LEN as socklen_t);
        assert_eq!(
            sys_getsockopt(fd, SOL_SOCKET, SO_PEERCRED, value_ptr, optlen_ptr),
            Ok(0)
        );
        assert_eq!(optlen.read(), UCRED_LEN as socklen_t);
        assert_eq!(value.read_bytes(UCRED_LEN).unwrap(), expected);
    }

    // Every prefix length copies that many bytes and reports the count. A
    // zero length succeeds without touching the buffer and accepts null.
    for len in 0..UCRED_LEN {
        value.write_bytes(&[0xaa; UCRED_LEN]).unwrap();
        optlen.write(len as socklen_t);
        let dst = if len == 0 {
            UserPtr::default()
        } else {
            value_ptr
        };
        assert_eq!(
            sys_getsockopt(first, SOL_SOCKET, SO_PEERCRED, dst, optlen_ptr),
            Ok(0)
        );
        assert_eq!(optlen.read(), len as socklen_t);
        if len > 0 {
            assert_eq!(value.read_bytes(len).unwrap(), expected[..len].to_vec());
        }
    }

    // A negative signed length is rejected before the copy even when the
    // buffer is writable.
    value.write_bytes(&[0xaa; UCRED_LEN]).unwrap();
    optlen.write(u32::MAX);
    assert_eq!(
        sys_getsockopt(first, SOL_SOCKET, SO_PEERCRED, value_ptr, optlen_ptr),
        Err(KError::InvalidInput)
    );
    assert_eq!(optlen.read(), u32::MAX);
    assert_eq!(value.read_bytes(UCRED_LEN).unwrap(), vec![0xaa; UCRED_LEN]);

    let input = TestUserValue::<[u8; UCRED_LEN]>::new().unwrap();
    assert_eq!(
        sys_setsockopt(
            first,
            SOL_SOCKET,
            SO_PEERCRED,
            UserPtr::from(input.as_user_ptr().cast::<u8>()).as_const(),
            UCRED_LEN as socklen_t
        ),
        Err(KError::from(LinuxError::ENOPROTOOPT))
    );

    close_socket(first);
    close_socket(second);
}
