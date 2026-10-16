// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use ktime_types::{NANOS_PER_SEC, TimeSpan};
use linux_raw_sys::general::timeval;
use unittest::{assert_eq, def_test};

use super::{
    BPF_JA, BPF_JMP, BPF_RET, IFNAMSIZ, SockFilter, conv::SocketTimeout, encode_bound_device_name,
    validate_classic_filter_instructions,
};

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
