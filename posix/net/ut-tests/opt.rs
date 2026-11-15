// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};
use core::mem::size_of;

use kerrno::{KError, LinuxError};
use knet::netlink::NETLINK_ROUTE;
use ktime_types::{NANOS_PER_SEC, TimeSpan};
use linux_raw_sys::{
    general::timeval,
    net::{
        AF_INET, AF_NETLINK, AF_UNIX, IPPROTO_IP, IPPROTO_TCP, IP_PKTINFO, IP_RECVFRAGSIZE,
        SO_BINDTOIFINDEX, SO_DOMAIN, SO_SNDBUF, SO_PEERCRED, SO_PROTOCOL, SO_TYPE, SOCK_DGRAM, SOCK_RAW,
        SOCK_STREAM, SOL_SOCKET, TCP_KEEPCNT, TCP_KEEPIDLE, TCP_KEEPINTVL, socklen_t,
    },
};
use posix_types::UserPtr;
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserArray, TestUserBuffer, TestUserValue};
use zerocopy::{FromBytes, IntoBytes};

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
#[def_test(user)]
fn netlink_cap_ack_option_round_trips_through_the_syscall_layer() {
    const SOL_NETLINK: u32 = 270;
    const NETLINK_CAP_ACK: u32 = 10;

    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let value = TestUserValue::<i32>::new().unwrap();
    let mut optlen = TestUserValue::from_value(size_of::<i32>() as socklen_t).unwrap();
    let optlen_ptr = UserPtr::from(optlen.as_user_ptr());
    let value_ptr = UserPtr::from(value.as_user_ptr().cast::<u8>());

    assert_eq!(
        sys_getsockopt(fd, SOL_NETLINK, NETLINK_CAP_ACK, value_ptr, optlen_ptr),
        Ok(0)
    );
    assert_eq!(value.read(), 0);
    assert_eq!(optlen.read(), size_of::<i32>() as socklen_t);

    let enabled = TestUserValue::from_value(2).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_CAP_ACK,
            UserPtr::from(enabled.as_user_ptr().cast::<u8>()).as_const(),
            size_of::<i32>() as socklen_t,
        ),
        Ok(0)
    );
    optlen.write(size_of::<i32>() as socklen_t);
    assert_eq!(
        sys_getsockopt(fd, SOL_NETLINK, NETLINK_CAP_ACK, value_ptr, optlen_ptr),
        Ok(0)
    );
    assert_eq!(value.read(), 1);

    // Linux reads the integer only when `optlen >= sizeof(int)`; short
    // lengths select the zero value and clear the flag instead of returning
    // EINVAL.  A negative length is still rejected.
    let empty = TestUserValue::<u8>::new().unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_CAP_ACK,
            UserPtr::from(empty.as_user_ptr()).as_const(),
            0,
        ),
        Ok(0)
    );
    optlen.write(size_of::<i32>() as socklen_t);
    assert_eq!(
        sys_getsockopt(fd, SOL_NETLINK, NETLINK_CAP_ACK, value_ptr, optlen_ptr),
        Ok(0)
    );
    assert_eq!(value.read(), 0);

    let one = TestUserValue::from_value(1_u8).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_CAP_ACK,
            UserPtr::from(one.as_user_ptr()).as_const(),
            1,
        ),
        Ok(0)
    );
    optlen.write(size_of::<i32>() as socklen_t);
    assert_eq!(
        sys_getsockopt(fd, SOL_NETLINK, NETLINK_CAP_ACK, value_ptr, optlen_ptr),
        Ok(0)
    );
    assert_eq!(value.read(), 0);
    // The negative-length check is shared with every other option and reports
    // the canonical `KError::InvalidInput`; both carriers encode as `-EINVAL`
    // at the syscall boundary.
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_CAP_ACK,
            UserPtr::from(empty.as_user_ptr()).as_const(),
            u32::MAX,
        ),
        Err(KError::InvalidInput)
    );

    close_socket(fd);
}

#[def_test(user)]
fn legacy_filter_gets_report_metadata_entries_and_revision_zero() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let buffer = TestUserBuffer::new(84).unwrap();
    let mut name = [0; 32];
    name[..6].copy_from_slice(b"filter");
    buffer.write_bytes(&name).unwrap();
    let mut len = TestUserValue::from_value(84u32).unwrap();
    let len_ptr = UserPtr::from(len.as_user_ptr());
    assert_eq!(
        sys_getsockopt(fd, 0, 64, UserPtr::from(buffer.as_user_ptr()), len_ptr),
        Ok(0)
    );
    assert_eq!(len.read(), 84);
    let info = knet::IptGetInfo::read_from_bytes(&buffer.read_bytes(84).unwrap()).unwrap();
    assert_eq!(&info.name[..7], b"filter\0");
    assert_eq!(
        (info.valid_hooks, info.num_entries, info.size),
        (0x0e, 4, 632)
    );
    assert_eq!(info.hook_entry, [0, 0, 152, 304, 0]);
    assert_eq!(info.underflow, info.hook_entry);

    let header = knet::IptGetEntries {
        name,
        size: info.size,
        padding: [0x5a; 4],
    };
    let total = 40 + info.size as usize;
    let entries = TestUserBuffer::new(total + 8).unwrap();
    entries.write_bytes(&vec![0xa5; total + 8]).unwrap();
    entries.write_bytes(header.as_bytes()).unwrap();
    len.write(total as u32);
    assert_eq!(
        sys_getsockopt(fd, 0, 65, UserPtr::from(entries.as_user_ptr()), len_ptr),
        Ok(0)
    );
    assert_eq!(len.read(), total as u32);
    let output = entries.read_bytes(total + 8).unwrap();
    assert_eq!(&output[..40], header.as_bytes());
    let expected = knet::LegacyTableHandle::lookup(&name)
        .unwrap()
        .export_entries(info.size)
        .unwrap();
    assert_eq!(&output[40..total], expected.entries());
    assert_eq!(&output[total..], &[0xa5; 8]);

    for (option, extension) in [
        (66, "tcp"),
        (66, "udp"),
        (66, "icmp"),
        (67, ""),
        (67, "ERROR"),
    ] {
        let mut query = knet::XtGetRevision::default();
        query.name[..extension.len()].copy_from_slice(extension.as_bytes());
        buffer.write_bytes(query.as_bytes()).unwrap();
        len.write(30);
        assert_eq!(
            sys_getsockopt(fd, 0, option, UserPtr::from(buffer.as_user_ptr()), len_ptr),
            Ok(0)
        );
        assert_eq!(buffer.read_bytes(30).unwrap(), query.as_bytes());
        query.revision = 1;
        buffer.write_bytes(query.as_bytes()).unwrap();
        assert_eq!(
            sys_getsockopt(fd, 0, option, UserPtr::from(buffer.as_user_ptr()), len_ptr),
            Err(LinuxError::EPROTONOSUPPORT.into())
        );
        assert_eq!(len.read(), 30);
    }
    close_socket(fd);
}

#[def_test(user)]
fn legacy_filter_gets_validate_lengths_pointers_and_socket_family() {
    let fd = sys_socket(AF_INET, SOCK_STREAM, 0).unwrap() as i32;
    let unix = sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap() as i32;
    let mut len = TestUserValue::from_value(84u32).unwrap();
    let len_ptr = UserPtr::from(len.as_user_ptr());
    assert_eq!(
        sys_getsockopt(unix, 0, 64, UserPtr::default(), len_ptr),
        Err(LinuxError::ENOPROTOOPT.into())
    );
    assert_eq!(
        sys_getsockopt(fd, 0, 64, UserPtr::default(), len_ptr),
        Err(KError::BadAddress)
    );
    for (option, invalid_len) in [(64, 83), (64, 85), (65, 39), (66, 29), (67, 31)] {
        len.write(invalid_len);
        assert_eq!(
            sys_getsockopt(fd, 0, option, UserPtr::default(), len_ptr),
            Err(KError::InvalidInput)
        );
        assert_eq!(len.read(), invalid_len);
    }
    len.write(u32::MAX);
    assert_eq!(
        sys_getsockopt(fd, 0, 64, UserPtr::default(), len_ptr),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_getsockopt(-1, 0, 64, UserPtr::default(), UserPtr::default()),
        Err(KError::BadFileDescriptor)
    );
    assert_eq!(
        sys_getsockopt(fd, 0, 64, UserPtr::default(), UserPtr::default()),
        Err(KError::BadAddress)
    );

    let header = knet::IptGetEntries {
        size: 1,
        ..Default::default()
    };
    let buffer = TestUserBuffer::new(40).unwrap();
    buffer.write_bytes(header.as_bytes()).unwrap();
    let ptr = UserPtr::from(buffer.as_user_ptr());
    len.write(40);
    assert_eq!(
        sys_getsockopt(fd, 0, 65, ptr, len_ptr),
        Err(KError::InvalidInput)
    );
    let mut header = header;
    header.name[..6].copy_from_slice(b"filter");
    buffer.write_bytes(header.as_bytes()).unwrap();
    len.write(41);
    assert_eq!(
        sys_getsockopt(fd, 0, 65, ptr, len_ptr),
        Err(LinuxError::EAGAIN.into())
    );
    assert_eq!(buffer.read_bytes(40).unwrap(), header.as_bytes());
    close_socket(unix);
    close_socket(fd);
}

#[def_test(user)]
fn legacy_filter_get_requires_net_admin_even_for_root_uid() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let caller = kprocess::current_user_thread();
    let saved = caller.cred();
    let mut cred = caller.prepare_creds();
    cred.set_capabilities(kcred::CapSets::new(
        kcred::CapSet::EMPTY,
        kcred::CapSet::EMPTY,
        kcred::CapSet::EMPTY,
    ))
    .unwrap();
    let len = TestUserValue::from_value(84u32).unwrap();
    caller.commit_creds(cred);
    let result = sys_getsockopt(
        fd,
        0,
        64,
        UserPtr::default(),
        UserPtr::from(len.as_user_ptr()),
    );
    caller.commit_creds((*saved).clone());
    assert_eq!(result, Err(LinuxError::EPERM.into()));
    close_socket(fd);
}

#[def_test(user, serial)]
fn legacy_filter_writes_round_trip_counters_and_commit_before_copyout() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let mut name = [0; 32];
    name[..6].copy_from_slice(b"filter");
    let reader = knet::LegacyTableHandle::lookup(&name).unwrap();
    let info = reader.info().unwrap();
    let wire = reader.export_entries(info.size).unwrap();
    let counters = TestUserBuffer::new(info.num_entries as usize * 16).unwrap();
    let mut header = knet::IptReplace {
        name,
        valid_hooks: info.valid_hooks,
        num_entries: info.num_entries,
        size: info.size,
        hook_entry: info.hook_entry,
        underflow: info.underflow,
        num_counters: info.num_entries,
        counters: counters.as_user_ptr::<u8>() as u64,
    };
    // Earlier network tests may have counted packets in the global table.
    let _ = reader.replace(header.replacement(wire.entries())).unwrap();
    let wire = reader.export_entries(info.size).unwrap();
    let len = 96 + wire.entries().len();
    let request = TestUserBuffer::new(len + 8).unwrap();
    let mut bytes = header.as_bytes().to_vec();
    bytes.extend_from_slice(wire.entries());
    bytes.extend_from_slice(&[0x5a; 8]);
    request.write_bytes(&bytes).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            0,
            64,
            request.as_user_ptr::<u8>().cast_const().into(),
            (len + 8) as u32
        ),
        Ok(0)
    );
    assert_eq!(
        counters.read_bytes(info.num_entries as usize * 16).unwrap(),
        vec![0; info.num_entries as usize * 16]
    );
    assert_eq!(request.read_bytes(len + 8).unwrap(), bytes);

    let update = knet::XtCountersInfo {
        name,
        num_counters: info.num_entries,
        padding: [0x5a; 4],
    };
    let mut updates = update.as_bytes().to_vec();
    for index in 0..info.num_entries {
        updates.extend_from_slice(
            knet::XtCounters {
                packets: u64::from(index) + 1,
                bytes: u64::from(index) + 100,
            }
            .as_bytes(),
        );
    }
    let add = TestUserBuffer::new(updates.len()).unwrap();
    add.write_bytes(&updates).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            0,
            65,
            add.as_user_ptr::<u8>().cast_const().into(),
            updates.len() as u32
        ),
        Ok(0)
    );
    assert_eq!(add.read_bytes(updates.len()).unwrap(), updates);
    let counted = reader.export_entries(info.size).unwrap();
    let get_header = knet::IptGetEntries {
        name,
        size: info.size,
        padding: [0; 4],
    };
    let get = TestUserBuffer::new(40 + info.size as usize).unwrap();
    get.write_bytes(get_header.as_bytes()).unwrap();
    let get_len = TestUserValue::from_value(40 + info.size).unwrap();
    assert_eq!(
        sys_getsockopt(
            fd,
            0,
            65,
            get.as_user_ptr::<u8>().into(),
            get_len.as_user_ptr().into()
        ),
        Ok(0)
    );
    let output = get.read_bytes(40 + info.size as usize).unwrap();
    assert_eq!(&output[40..], counted.entries());
    header.name = [b'x'; 32];
    request.write_bytes(header.as_bytes()).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            0,
            64,
            request.as_user_ptr::<u8>().cast_const().into(),
            len as u32
        ),
        Err(LinuxError::ENOENT.into())
    );
    header.name = name;
    assert_ne!(counted.entries(), wire.entries());
    // A wrong expected entry count must leave both rule and ERROR entry counters unchanged.
    header.num_counters -= 1;
    request.write_bytes(header.as_bytes()).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            0,
            64,
            request.as_user_ptr::<u8>().cast_const().into(),
            len as u32
        ),
        Err(LinuxError::EAGAIN.into())
    );
    assert_eq!(reader.export_entries(info.size).unwrap(), counted);
    header.num_counters += 1;
    request.write_bytes(header.as_bytes()).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            0,
            64,
            request.as_user_ptr::<u8>().cast_const().into(),
            len as u32
        ),
        Ok(0)
    );
    assert_eq!(
        counters.read_bytes(info.num_entries as usize * 16).unwrap(),
        updates[40..]
    );
    assert_eq!(reader.export_entries(info.size).unwrap(), wire);
    header.counters = 0;
    request.write_bytes(header.as_bytes()).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            0,
            64,
            request.as_user_ptr::<u8>().cast_const().into(),
            len as u32
        ),
        Ok(0)
    );
    assert_eq!(reader.export_entries(info.size).unwrap(), wire);
    close_socket(fd);
}

#[def_test(user)]
fn legacy_filter_writes_check_access_lengths_and_allocation_bounds() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let unix = sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap() as i32;
    for option in [64, 65] {
        assert_eq!(
            sys_setsockopt(-1, 0, option, Default::default(), 96),
            Err(KError::BadFileDescriptor)
        );
        assert_eq!(
            sys_setsockopt(unix, 0, option, Default::default(), 96),
            Err(LinuxError::ENOPROTOOPT.into())
        );
        assert_eq!(
            sys_setsockopt(fd, 0, option, Default::default(), u32::MAX),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_setsockopt(fd, 0, option, Default::default(), 128),
            Err(KError::BadAddress)
        );
    }
    for (option, len) in [(64, 95), (65, 40)] {
        assert_eq!(
            sys_setsockopt(fd, 0, option, Default::default(), len),
            Err(KError::InvalidInput)
        );
    }
    let buffer = TestUserBuffer::new(96).unwrap();
    for (header, len, expected) in [
        (
            knet::IptReplace {
                size: u32::MAX,
                ..Default::default()
            },
            u32::MAX / 2,
            LinuxError::E2BIG.into(),
        ),
        (
            knet::IptReplace {
                size: 632,
                num_counters: 4,
                ..Default::default()
            },
            96,
            KError::InvalidInput,
        ),
        (knet::IptReplace::default(), 96, KError::InvalidInput),
    ] {
        buffer.write_bytes(header.as_bytes()).unwrap();
        assert_eq!(
            sys_setsockopt(
                fd,
                0,
                64,
                buffer.as_user_ptr::<u8>().cast_const().into(),
                len
            ),
            Err(expected)
        );
    }
    for count in [0, 1, u32::MAX] {
        let header = knet::XtCountersInfo {
            num_counters: count,
            ..Default::default()
        };
        buffer.write_bytes(header.as_bytes()).unwrap();
        assert_eq!(
            sys_setsockopt(
                fd,
                0,
                65,
                buffer.as_user_ptr::<u8>().cast_const().into(),
                72
            ),
            Err(KError::InvalidInput)
        );
    }
    let page = TestUserBuffer::new(4096).unwrap();
    let replace_ptr = page.as_user_ptr::<u8>().wrapping_add(4096 - 96);
    let header = knet::IptReplace {
        size: 152,
        num_entries: 1,
        num_counters: 4,
        ..Default::default()
    };
    osvm::write_vm_bytes(replace_ptr, header.as_bytes()).unwrap();
    assert_eq!(
        sys_setsockopt(fd, 0, 64, replace_ptr.cast_const().into(), 96 + 152),
        Err(KError::BadAddress)
    );
    let add_ptr = page.as_user_ptr::<u8>().wrapping_add(4096 - 40);
    let header = knet::XtCountersInfo {
        num_counters: 4,
        ..Default::default()
    };
    osvm::write_vm_bytes(add_ptr, header.as_bytes()).unwrap();
    assert_eq!(
        sys_setsockopt(fd, 0, 65, add_ptr.cast_const().into(), 40 + 64),
        Err(KError::BadAddress)
    );
    let caller = kprocess::current_user_thread();
    let saved = caller.cred();
    let mut cred = caller.prepare_creds();
    cred.set_capabilities(kcred::CapSets::new(
        kcred::CapSet::EMPTY,
        kcred::CapSet::EMPTY,
        kcred::CapSet::EMPTY,
    ))
    .unwrap();
    caller.commit_creds(cred);
    let results = [64, 65].map(|option| sys_setsockopt(fd, 0, option, Default::default(), 96));
    caller.commit_creds((*saved).clone());
    assert_eq!(results, [Err(LinuxError::EPERM.into()); 2]);
    close_socket(unix);
    close_socket(fd);
}

/// `do_sock_setsockopt` rejects a negative length before any option handler
/// runs.
///
/// A `socklen_t` of `0xFFFF_FFFF` is the signed `-1` Linux refuses; treating it
/// as an enormous length would instead select the "at least a byte" branches
/// and enable an option on a request Linux rejects.
#[def_test(user)]
fn negative_set_option_length_is_rejected_before_the_handler() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let value = TestUserValue::from_value(1_i32).unwrap();
    let value_ptr = UserPtr::from(value.as_user_ptr().cast::<u8>()).as_const();

    assert_eq!(
        sys_setsockopt(fd, IPPROTO_IP as u32, IP_PKTINFO, value_ptr, u32::MAX),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_setsockopt(fd, SOL_SOCKET, SO_SNDBUF, value_ptr, u32::MAX),
        Err(KError::InvalidInput)
    );

    let enabled = TestUserValue::<i32>::new().unwrap();
    let optlen = TestUserValue::from_value(size_of::<i32>() as socklen_t).unwrap();
    assert_eq!(
        sys_getsockopt(
            fd,
            IPPROTO_IP as u32,
            IP_PKTINFO,
            UserPtr::from(enabled.as_user_ptr().cast::<u8>()),
            UserPtr::from(optlen.as_user_ptr()),
        ),
        Ok(0)
    );
    assert_eq!(enabled.read(), 0, "the option was not enabled");

    close_socket(fd);
}

/// `NETLINK_ADD_MEMBERSHIP` reads four bytes whenever the length reaches an
/// `int`, and a socket of another family answers `ENOPROTOOPT`.
#[def_test(user)]
fn group_membership_accepts_a_long_option_and_reports_the_family_mismatch() {
    const SOL_NETLINK: u32 = 270;
    const NETLINK_ADD_MEMBERSHIP: u32 = 1;
    const NETLINK_LIST_MEMBERSHIPS: u32 = 9;

    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;

    // An `int` inside a larger object still joins the group: Linux copies the
    // first four bytes and ignores the rest.
    let mut wide = [0u8; size_of::<u64>()];
    wide[..size_of::<u32>()].copy_from_slice(&1u32.to_ne_bytes());
    let wide_buf = TestUserBuffer::new(wide.len()).unwrap();
    wide_buf.write_bytes(&wide).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            UserPtr::from(wide_buf.as_user_ptr::<u8>()).as_const(),
            wide.len() as socklen_t,
        ),
        Ok(0)
    );

    let memberships = TestUserValue::<u32>::new().unwrap();
    let optlen = TestUserValue::from_value(size_of::<u32>() as socklen_t).unwrap();
    assert_eq!(
        sys_getsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_LIST_MEMBERSHIPS,
            UserPtr::from(memberships.as_user_ptr().cast::<u8>()),
            UserPtr::from(optlen.as_user_ptr()),
        ),
        Ok(0)
    );
    assert_eq!(memberships.read(), 1, "group 1 is subscribed");

    // A length below an `int` selects the zero group and fails the group
    // check without reading the (here absent) value buffer.
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            UserPtr::default().as_const(),
            3,
        ),
        Err(KError::InvalidInput)
    );

    // Linux dispatches SOL_NETLINK per family, so a UDP socket answers
    // `inet_setsockopt`'s default: ENOPROTOOPT, before any memory access.
    let udp = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    assert_eq!(
        sys_setsockopt(
            udp,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            UserPtr::default().as_const(),
            size_of::<u32>() as socklen_t,
        ),
        Err(KError::from(LinuxError::ENOPROTOOPT))
    );

    close_socket(udp);
    close_socket(fd);
}

/// `IP_RECVFRAGSIZE` belongs to the `int`-or-byte group and only raw and
/// datagram sockets can take it.
#[def_test(user)]
fn ip_recvfragsize_takes_a_byte_and_refuses_stream_sockets() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let one = TestUserValue::from_value(1_u8).unwrap();
    let one_ptr = UserPtr::from(one.as_user_ptr()).as_const();
    assert_eq!(
        sys_setsockopt(fd, IPPROTO_IP as u32, IP_RECVFRAGSIZE, one_ptr, 1),
        Ok(0)
    );

    let enabled = TestUserValue::<i32>::new().unwrap();
    let optlen = TestUserValue::from_value(size_of::<i32>() as socklen_t).unwrap();
    assert_eq!(
        sys_getsockopt(
            fd,
            IPPROTO_IP as u32,
            IP_RECVFRAGSIZE,
            UserPtr::from(enabled.as_user_ptr().cast::<u8>()),
            UserPtr::from(optlen.as_user_ptr()),
        ),
        Ok(0)
    );
    assert_eq!(enabled.read(), 1);
    close_socket(fd);

    // `do_ip_setsockopt` refuses the option on a stream socket: only raw and
    // datagram sockets receive fragments.
    let stream = sys_socket(AF_INET, SOCK_STREAM, 0).unwrap() as i32;
    assert_eq!(
        sys_setsockopt(stream, IPPROTO_IP as u32, IP_RECVFRAGSIZE, one_ptr, 1),
        Err(KError::InvalidInput)
    );
    close_socket(stream);
}

/// `sock_bindtoindex_locked` answers EINVAL for a negative ifindex instead of
/// storing it.
#[def_test(user)]
fn bind_to_ifindex_rejects_a_negative_index() {
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    let negative = TestUserValue::from_value(-1_i32).unwrap();
    assert_eq!(
        sys_setsockopt(
            fd,
            SOL_SOCKET,
            SO_BINDTOIFINDEX,
            UserPtr::from(negative.as_user_ptr().cast::<u8>()).as_const(),
            size_of::<i32>() as socklen_t,
        ),
        Err(KError::InvalidInput)
    );

    // The invalid request left the socket unbound, and the negative index did
    // not reach the field a later connect would resolve.
    let ifindex = TestUserValue::<i32>::new().unwrap();
    let optlen = TestUserValue::from_value(size_of::<i32>() as socklen_t).unwrap();
    assert_eq!(
        sys_getsockopt(
            fd,
            SOL_SOCKET,
            SO_BINDTOIFINDEX,
            UserPtr::from(ifindex.as_user_ptr().cast::<u8>()),
            UserPtr::from(optlen.as_user_ptr()),
        ),
        Ok(0)
    );
    assert_eq!(ifindex.read(), 0);

    close_socket(fd);
}
