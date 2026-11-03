// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use knet::netlink::NETLINK_ROUTE;
use linux_raw_sys::net::{
    __kernel_sa_family_t, AF_UNSPEC, MSG_DONTWAIT, MSG_OOB, mmsghdr, msghdr, sockaddr_un,
};
use posix_types::{IoVec, UserConstPtr, net::socket_addr::sockaddr_nl};
use unittest::{assert_eq, def_test};
use unittest_support::TestUserArray;

use super::*;
use crate::{sys_getpeername, sys_getsockname, sys_sendmmsg, sys_sendmsg, sys_sendto};

#[def_test]
fn inet_raw_protocol_rejects_values_above_u8() {
    assert_eq!(inet_raw_protocol(0).unwrap(), IpProtocol::from(0));
    assert_eq!(inet_raw_protocol(255).unwrap(), IpProtocol::from(255));
    assert_eq!(
        inet_raw_protocol(256).unwrap_err(),
        KError::from(LinuxError::EINVAL)
    );
}

#[def_test(user)]
fn netlink_bind_checks_length_and_family_and_accepts_oversized_sockaddr() {
    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let address = sockaddr_nl {
        nl_family: AF_NETLINK as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    };
    let mut buffer = TestUserArray::from_array([address, address]).unwrap();
    let address_len = size_of::<sockaddr_nl>() as u32;

    // Linux requires at least sizeof(sockaddr_nl); the user pointer is not
    // dereferenced for a short length.
    assert_eq!(
        sys_bind(fd, UserConstPtr::<sockaddr>::default(), address_len - 1,),
        Err(KError::InvalidInput)
    );

    // Family validation belongs to netlink_bind rather than the generic socket
    // address parser, and rejects a non-AF_NETLINK address with EINVAL.
    buffer.write([
        sockaddr_nl {
            nl_family: AF_INET as u16,
            ..address
        },
        address,
    ]);
    assert_eq!(
        sys_bind(
            fd,
            UserConstPtr::from(buffer.as_user_ptr().cast::<sockaddr>().cast_const()),
            address_len,
        ),
        Err(LinuxError::EINVAL.into())
    );

    // A larger address length is accepted; netlink only reads the fixed-size
    // prefix and ignores the trailing bytes.
    buffer.write([address, address]);
    assert_eq!(
        sys_bind(
            fd,
            UserConstPtr::from(buffer.as_user_ptr().cast::<sockaddr>().cast_const()),
            address_len * 2,
        ),
        Ok(0)
    );

    // A length of exactly `sizeof(struct sockaddr_storage)` is accepted; the
    // trailing bytes beyond the netlink address are ignored.
    let mut max_len = TestUserArray::from_array([0u8; crate::addr::MAX_SOCKADDR_LEN]).unwrap();
    let mut netlink_prefix = [0u8; crate::addr::MAX_SOCKADDR_LEN];
    netlink_prefix[..size_of::<u16>()].copy_from_slice(&(AF_NETLINK as u16).to_ne_bytes());
    max_len.write(netlink_prefix);
    let max_fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    assert_eq!(
        sys_bind(
            max_fd,
            UserConstPtr::from(max_len.as_user_ptr().cast::<sockaddr>().cast_const()),
            crate::addr::MAX_SOCKADDR_LEN as u32,
        ),
        Ok(0)
    );
    kprocess::current_files().close_file(max_fd).unwrap();

    // `move_addr_to_kernel()` rejects a length larger than
    // `sizeof(struct sockaddr_storage)` before `netlink_bind` sees it.
    let too_long = TestUserArray::from_array([0u8; crate::addr::MAX_SOCKADDR_LEN + 1]).unwrap();
    assert_eq!(
        sys_bind(
            fd,
            UserConstPtr::from(too_long.as_user_ptr().cast::<sockaddr>().cast_const()),
            (crate::addr::MAX_SOCKADDR_LEN + 1) as u32,
        ),
        Err(KError::InvalidInput)
    );

    kprocess::current_files().close_file(fd).unwrap();
}
#[def_test(user)]
fn netlink_connect_accepts_af_unspec_and_reports_unconnected_peer() {
    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let address = sockaddr_nl {
        nl_family: AF_UNSPEC as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    };
    let buffer = TestUserArray::from_array([address, address]).unwrap();
    let connect_addr = UserConstPtr::from(buffer.as_user_ptr().cast::<sockaddr>().cast_const());

    // `AF_UNSPEC` with only the family present is a disconnect request.  Linux
    // validates only `sa_family_t` before dispatching on the family.
    assert_eq!(sys_connect(fd, connect_addr, 1), Err(KError::InvalidInput));
    assert_eq!(
        sys_connect(fd, connect_addr, size_of::<u16>() as u32),
        Ok(0)
    );

    // The maximum accepted length also works for the short disconnect form.
    let max_len = TestUserArray::from_array([0u8; crate::addr::MAX_SOCKADDR_LEN]).unwrap();
    assert_eq!(
        sys_connect(
            fd,
            UserConstPtr::from(max_len.as_user_ptr().cast::<sockaddr>().cast_const()),
            crate::addr::MAX_SOCKADDR_LEN as u32,
        ),
        Ok(0)
    );

    // The generic `move_addr_to_kernel()` cap applies even to the short
    // `AF_UNSPEC` disconnect form.
    let too_long = TestUserArray::from_array([0u8; crate::addr::MAX_SOCKADDR_LEN + 1]).unwrap();
    assert_eq!(
        sys_connect(
            fd,
            UserConstPtr::from(too_long.as_user_ptr().cast::<sockaddr>().cast_const()),
            (crate::addr::MAX_SOCKADDR_LEN + 1) as u32,
        ),
        Err(KError::InvalidInput)
    );

    // `netlink_connect` owns family validation: every family other than
    // `AF_UNSPEC` or `AF_NETLINK` is `EINVAL`, including families the generic
    // socket-address dispatcher does not know.
    for family in [AF_INET as u16, 0x1234] {
        let invalid = sockaddr_nl {
            nl_family: family,
            nl_pad: 0,
            nl_pid: 0,
            nl_groups: 0,
        };
        let invalid_buffer = TestUserArray::from_array([invalid, invalid]).unwrap();
        assert_eq!(
            sys_connect(
                fd,
                UserConstPtr::from(invalid_buffer.as_user_ptr().cast::<sockaddr>().cast_const()),
                size_of::<sockaddr_nl>() as u32,
            ),
            Err(LinuxError::EINVAL.into())
        );
    }

    // An unconnected netlink peer is reported as port 0/groups 0, not as an
    // error, and the returned address is a full `sockaddr_nl`.
    let name = TestUserArray::from_array([sockaddr_nl {
        nl_family: 0,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    }])
    .unwrap();
    let name_len = TestUserArray::from_array([(size_of::<sockaddr_nl>() * 2) as u32]).unwrap();
    assert_eq!(
        sys_getpeername(
            fd,
            UserPtr::from(name.as_user_ptr().cast::<sockaddr>()),
            UserPtr::from(name_len.as_user_ptr()),
        ),
        Ok(0)
    );
    let returned = name.read()[0];
    assert_eq!(returned.nl_family, AF_NETLINK as u16);
    assert_eq!(returned.nl_pid, 0);
    assert_eq!(returned.nl_groups, 0);
    assert_eq!(name_len.read()[0], size_of::<sockaddr_nl>() as u32);

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn sendmsg_rejects_invalid_unix_destination_lengths() {
    let fd = sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap() as i32;
    let destination = sockaddr_un {
        sun_family: AF_UNIX as u16,
        sun_path: [0; 108],
    };
    let destination_buffer = TestUserArray::from_array([destination; 2]).unwrap();
    let payload = TestUserArray::from_array([0x5a_u8; 4]).unwrap();
    let iovec = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: payload.len() as isize,
    }])
    .unwrap();
    let message = |namelen: i32| msghdr {
        msg_name: destination_buffer.as_user_ptr().cast(),
        msg_namelen: namelen,
        msg_iov: iovec.as_user_ptr().cast(),
        msg_iovlen: 1,
        msg_control: core::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };

    // Linux `unix_validate_addr()` rejects both a length that does not cover
    // `sun_path` and one above `sizeof(struct sockaddr_un)`.  The oversized
    // case also goes through the `sendmsg()` clamp to `sockaddr_storage`; the
    // family-specific bound must still reject it afterwards.
    for namelen in [
        -1,
        i32::MIN,
        size_of::<__kernel_sa_family_t>() as i32,
        size_of::<sockaddr_un>() as i32 + 1,
        (crate::addr::MAX_SOCKADDR_LEN + 1) as i32,
    ] {
        let message_buffer = TestUserArray::from_array([message(namelen)]).unwrap();
        assert_eq!(
            sys_sendmsg(
                fd,
                UserConstPtr::from(message_buffer.as_user_ptr().cast_const()),
                0,
            ),
            Err(KError::from(LinuxError::EINVAL))
        );
    }

    let messages = TestUserArray::from_array([mmsghdr {
        msg_hdr: message((crate::addr::MAX_SOCKADDR_LEN + 1) as i32),
        msg_len: 0,
    }])
    .unwrap();
    assert_eq!(
        sys_sendmmsg(fd, UserPtr::from(messages.as_user_ptr()), 1, 0),
        Err(KError::from(LinuxError::EINVAL))
    );

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn sendmsg_netlink_rejects_explicit_unspecified_destination() {
    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let destination = sockaddr_nl {
        nl_family: AF_UNSPEC as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    };
    let destination_buffer = TestUserArray::from_array([destination, destination]).unwrap();
    let payload = TestUserArray::from_array([0x5a_u8; 4]).unwrap();
    let iovec = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: payload.len() as isize,
    }])
    .unwrap();
    let message = TestUserArray::from_array([msghdr {
        msg_name: destination_buffer.as_user_ptr().cast(),
        msg_namelen: size_of::<sockaddr_nl>() as i32,
        msg_iov: iovec.as_user_ptr().cast(),
        msg_iovlen: 1,
        msg_control: core::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    }])
    .unwrap();

    // An explicit `sockaddr_nl` must carry `AF_NETLINK`; `AF_UNSPEC` is not a
    // synonym for an addressless send.  Linux `netlink_sendmsg` rejects the
    // family with EINVAL before it considers the stored peer or kernel port 0.
    assert_eq!(
        sys_sendmsg(
            fd,
            UserConstPtr::from(message.as_user_ptr().cast_const()),
            0,
        ),
        Err(KError::InvalidInput)
    );

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn sendmsg_and_sendmmsg_clamp_oversized_destination_length() {
    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let address = sockaddr_nl {
        nl_family: AF_NETLINK as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    };
    let bind_buffer = TestUserArray::from_array([address, address]).unwrap();
    assert_eq!(
        sys_bind(
            fd,
            UserConstPtr::from(bind_buffer.as_user_ptr().cast::<sockaddr>().cast_const()),
            size_of::<sockaddr_nl>() as u32,
        ),
        Ok(0)
    );

    let local = TestUserArray::from_array([address, address]).unwrap();
    let local_len = TestUserArray::from_array([size_of::<sockaddr_nl>() as u32]).unwrap();
    assert_eq!(
        sys_getsockname(
            fd,
            UserPtr::from(local.as_user_ptr().cast::<sockaddr>()),
            UserPtr::from(local_len.as_user_ptr()),
        ),
        Ok(0)
    );
    let destination = sockaddr_nl {
        nl_pid: local.read()[0].nl_pid,
        ..address
    };

    // Linux's `__copy_msghdr()` clamps an oversized `msg_namelen` to
    // `sizeof(struct sockaddr_storage)` before the address parser runs.  The
    // destination buffer is deliberately larger than that so a clamp still
    // has a complete `sockaddr_nl` prefix to read.
    let destination_buffer = TestUserArray::from_array([destination; 11]).unwrap();
    let payload = TestUserArray::from_array([0x5a_u8; 4]).unwrap();
    let iovec = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: payload.len() as isize,
    }])
    .unwrap();
    let message = msghdr {
        msg_name: destination_buffer.as_user_ptr().cast(),
        msg_namelen: (crate::addr::MAX_SOCKADDR_LEN + 1) as i32,
        msg_iov: iovec.as_user_ptr().cast(),
        msg_iovlen: 1,
        msg_control: core::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let message_buffer = TestUserArray::from_array([message]).unwrap();
    assert_eq!(
        sys_sendmsg(
            fd,
            UserConstPtr::from(message_buffer.as_user_ptr().cast_const()),
            0,
        ),
        Ok(4)
    );

    let messages = TestUserArray::from_array([mmsghdr {
        msg_hdr: message,
        msg_len: 0,
    }])
    .unwrap();
    assert_eq!(
        sys_sendmmsg(fd, UserPtr::from(messages.as_user_ptr()), 1, 0),
        Ok(1)
    );
    assert_eq!(messages.read()[0].msg_len, 4);

    // `sendto()` does not go through `__copy_msghdr()` and must continue to
    // reject the same length.
    assert_eq!(
        sys_sendto(
            fd,
            payload.as_user_ptr(),
            payload.len(),
            0,
            UserConstPtr::from(
                destination_buffer
                    .as_user_ptr()
                    .cast::<sockaddr>()
                    .cast_const()
            ),
            (crate::addr::MAX_SOCKADDR_LEN + 1) as u32,
        ),
        Err(KError::InvalidInput)
    );

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn send_syscalls_resolve_the_descriptor_before_header_validation() {
    // Linux `__sys_sendmsg()` calls `sockfd_lookup_light()` before
    // `sendmsg_copy_msghdr()`, so a bad descriptor wins over a negative
    // `msg_namelen`.
    let invalid_header = msghdr {
        msg_name: core::ptr::dangling_mut(),
        msg_namelen: -1,
        msg_iov: core::ptr::null_mut(),
        msg_iovlen: 0,
        msg_control: core::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let invalid_header = TestUserArray::from_array([invalid_header]).unwrap();
    assert_eq!(
        LinuxError::from(
            sys_sendmsg(
                -1,
                UserConstPtr::from(invalid_header.as_user_ptr().cast_const()),
                0,
            )
            .unwrap_err()
        ),
        LinuxError::EBADF
    );
    // `__sys_sendmmsg()` also resolves the descriptor before checking vlen,
    // so `sendmmsg(-1, ..., 0)` is EBADF rather than a successful no-op.
    assert_eq!(
        LinuxError::from(sys_sendmmsg(-1, UserPtr::from(core::ptr::null_mut()), 0, 0).unwrap_err()),
        LinuxError::EBADF
    );

    // A valid descriptor still makes `vlen == 0` a successful no-op; the
    // message-vector pointer is never dereferenced.
    let fd = sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap() as i32;
    assert_eq!(
        sys_sendmmsg(fd, UserPtr::from(core::ptr::null_mut()), 0, 0),
        Ok(0)
    );
    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn sendmsg_treats_null_name_as_absent_with_nonzero_length() {
    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let payload = TestUserArray::from_array([0x5a_u8]).unwrap();
    let iovec = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: 1,
    }])
    .unwrap();
    let message = msghdr {
        msg_name: core::ptr::null_mut(),
        msg_namelen: size_of::<sockaddr_nl>() as i32,
        msg_iov: iovec.as_user_ptr().cast(),
        msg_iovlen: 1,
        msg_control: core::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let message = TestUserArray::from_array([message]).unwrap();

    // `__copy_msghdr()` clears both `msg_name` and `msg_namelen` when the
    // name pointer is NULL, so the nonzero length does not describe an
    // address.  With a nonempty payload, treating the tuple as an address
    // would fault while copying from NULL; treating it as absent sends to
    // the registered rtnetlink kernel port instead.
    assert_eq!(
        sys_sendmsg(
            fd,
            UserConstPtr::from(message.as_user_ptr().cast_const()),
            0,
        ),
        Ok(1)
    );

    // A NULL name suppresses length validation too: Linux clears the length
    // before checking it, so a negative value is not EINVAL for this header.
    let mut negative = message.read()[0];
    negative.msg_namelen = -1;
    let negative_buffer = TestUserArray::from_array([negative]).unwrap();
    assert_eq!(
        sys_sendmsg(
            fd,
            UserConstPtr::from(negative_buffer.as_user_ptr().cast_const()),
            0,
        ),
        Ok(1)
    );

    // `sendmmsg()` processes headers one at a time.  A validation failure in
    // a later header ends the call and reports the successful prefix rather
    // than discarding the datagrams that were already sent.
    let destination = TestUserArray::from_array([sockaddr_nl {
        nl_family: AF_NETLINK as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    }])
    .unwrap();
    let mut invalid_negative = negative;
    invalid_negative.msg_name = destination.as_user_ptr().cast();
    let messages = TestUserArray::from_array([
        mmsghdr {
            msg_hdr: negative,
            msg_len: 0,
        },
        mmsghdr {
            msg_hdr: invalid_negative,
            msg_len: 0,
        },
    ])
    .unwrap();
    assert_eq!(
        sys_sendmmsg(fd, UserPtr::from(messages.as_user_ptr()), 2, 0),
        Ok(1)
    );
    let sent_messages = messages.read();
    assert_eq!(sent_messages[0].msg_len, 1);
    assert_eq!(sent_messages[1].msg_len, 0);

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn sendmmsg_stops_after_a_short_stream_send() {
    let fds = TestUserArray::<i32, 2>::new().unwrap();
    assert_eq!(
        crate::sys_socketpair(
            AF_UNIX,
            SOCK_STREAM,
            0,
            UserPtr::from(fds.as_user_ptr().cast::<[i32; 2]>())
        ),
        Ok(0)
    );
    let [sender, receiver] = fds.read();

    // A Unix stream send into an empty 64 KiB ring accepts a short prefix of
    // an oversized write when nonblocking.  Linux counts that as one datagram
    // and stops the `sendmmsg` loop because `msg_data_left()` is still true.
    let payload = TestUserArray::<u8, 131_072>::new().unwrap();
    let first_iov = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: payload.len() as isize,
    }])
    .unwrap();
    let second_iov = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: 0,
    }])
    .unwrap();
    let messages = TestUserArray::from_array([
        mmsghdr {
            msg_hdr: msghdr {
                msg_name: core::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: first_iov.as_user_ptr().cast(),
                msg_iovlen: 1,
                msg_control: core::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            },
            msg_len: 0,
        },
        mmsghdr {
            msg_hdr: msghdr {
                msg_name: core::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: second_iov.as_user_ptr().cast(),
                msg_iovlen: 1,
                msg_control: core::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            },
            msg_len: u32::MAX,
        },
    ])
    .unwrap();

    assert_eq!(
        sys_sendmmsg(
            sender,
            UserPtr::from(messages.as_user_ptr()),
            2,
            MSG_DONTWAIT,
        ),
        Ok(1)
    );
    let sent = messages.read();
    assert!(sent[0].msg_len > 0);
    assert!((sent[0].msg_len as usize) < payload.len());
    // The second header was never examined: a datagram count change without a
    // stop would have stored zero in its `msg_len`.
    assert_eq!(sent[1].msg_len, u32::MAX);

    kprocess::current_files().close_file(sender).unwrap();
    kprocess::current_files().close_file(receiver).unwrap();
}

#[def_test(user)]
fn netlink_sendmsg_checks_oob_and_empty_payload_before_control_data() {
    let fd = sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    let payload = TestUserArray::from_array([0x5a_u8]).unwrap();
    let mut iovec = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: 1,
    }])
    .unwrap();
    // One byte cannot hold even a complete `cmsghdr`, so a send that reaches
    // control parsing would fail with EINVAL.
    let malformed_control = TestUserArray::from_array([0u8; 1]).unwrap();
    let message = TestUserArray::from_array([msghdr {
        msg_name: core::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: iovec.as_user_ptr().cast(),
        msg_iovlen: 1,
        msg_control: malformed_control.as_user_ptr().cast(),
        msg_controllen: malformed_control.len(),
        msg_flags: 0,
    }])
    .unwrap();
    let message = UserConstPtr::from(message.as_user_ptr().cast_const());

    // Linux's netlink_sendmsg rejects MSG_OOB and zero-length sends before
    // `scm_send`, so malformed control data is never examined.
    assert_eq!(
        sys_sendmsg(fd, message, MSG_OOB),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    iovec.write([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: 0,
    }]);
    assert_eq!(sys_sendmsg(fd, message, 0), Err(LinuxError::ENODATA.into()));

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test(user)]
fn netlink_socket_creation_rejects_out_of_range_and_unregistered_protocols() {
    assert_eq!(
        sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_MAX_LINKS as u32),
        Err(LinuxError::EPROTONOSUPPORT.into())
    );
    assert_eq!(
        sys_socket(AF_NETLINK, SOCK_RAW, 1),
        Err(LinuxError::EPROTONOSUPPORT.into())
    );
    assert_eq!(
        sys_socket(AF_NETLINK, SOCK_STREAM, NETLINK_ROUTE as u32),
        Err(LinuxError::ESOCKTNOSUPPORT.into())
    );
}
