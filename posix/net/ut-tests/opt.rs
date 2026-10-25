// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use kerrno::KError;
use ktime_types::{NANOS_PER_SEC, TimeSpan};
use linux_raw_sys::general::timeval;
use linux_raw_sys::net::{AF_UNIX, SO_TYPE, SOCK_DGRAM, SOL_SOCKET, socklen_t};
use posix_types::UserPtr;
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserBuffer, TestUserValue};

use super::{
    BPF_JA, BPF_JMP, BPF_RET, IFNAMSIZ, SockFilter, conv::SocketTimeout, encode_bound_device_name,
    sys_getsockopt, validate_classic_filter_instructions,
};
use crate::socket::sys_socket;

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
}
