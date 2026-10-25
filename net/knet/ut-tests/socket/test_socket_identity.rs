// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `SO_TYPE`, `SO_DOMAIN` and `SO_PROTOCOL` report the socket itself.
//!
//! Every `Socket` variant holds a `Box<T>`, so a method that layer fails to
//! forward silently answers with a trait default for sockets of any kind.
//! These cases therefore query through the enum, not the concrete types, and
//! cover the datagram/raw families that a stream socket cannot detect.
//!
//! Expected values are the ones Linux reports from `sk_getsockopt`
//! (`net/core/sock.c`): `sk_type`, `sk_family` and `sk_protocol`. Families that
//! never record a protocol report 0, which is what `AF_PACKET` and `AF_UNIX`
//! leave in `sk_protocol`.

use alloc::boxed::Box;

use linux_raw_sys::net::{
    AF_INET, AF_INET6, AF_NETLINK, AF_PACKET, AF_UNIX, IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_TCP,
    IPPROTO_UDP, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET, SOCK_STREAM,
};
use unittest::{assert_eq, def_test};

use crate::{
    Socket, SocketOps,
    netlink::{NETLINK_KOBJECT_UEVENT, NETLINK_ROUTE, NetlinkSocket, NetlinkSocketType},
    packet::{PacketSocket, PacketSocketKind},
    raw::{IpProtocol, IpVersion, RawSocket},
    udp::UdpSocket,
    unix::{DgramTransport, StreamTransport, UnixDomainSocket, UnixSocketType},
};

/// A zero protocol leaves the packet socket protocol unset; none of the three
/// reported values depends on it.
const PACKET_PROTOCOL: u16 = 0;

#[def_test]
fn socket_identity_reports_creation_family_type_and_protocol() {
    let cases = [
        (Socket::Tcp(Box::default()), SOCK_STREAM as i32, AF_INET as i32, IPPROTO_TCP as i32),
        (
            Socket::Udp(Box::new(UdpSocket::new())),
            SOCK_DGRAM as i32,
            AF_INET as i32,
            IPPROTO_UDP as i32,
        ),
        (
            Socket::Raw(Box::new(RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp))),
            SOCK_RAW as i32,
            AF_INET as i32,
            IPPROTO_ICMP as i32,
        ),
        (
            Socket::Raw(Box::new(RawSocket::new(IpVersion::Ipv6, IpProtocol::Icmpv6))),
            SOCK_RAW as i32,
            AF_INET6 as i32,
            IPPROTO_ICMPV6 as i32,
        ),
        (
            Socket::Packet(Box::new(
                PacketSocket::new(PacketSocketKind::Raw, PACKET_PROTOCOL.to_be()).unwrap(),
            )),
            SOCK_RAW as i32,
            AF_PACKET as i32,
            0,
        ),
        (
            Socket::Packet(Box::new(
                PacketSocket::new(PacketSocketKind::Datagram, PACKET_PROTOCOL.to_be()).unwrap(),
            )),
            SOCK_DGRAM as i32,
            AF_PACKET as i32,
            0,
        ),
        (
            Socket::Netlink(Box::new(NetlinkSocket::new(
                NETLINK_ROUTE,
                NetlinkSocketType::Raw,
            ))),
            SOCK_RAW as i32,
            AF_NETLINK as i32,
            NETLINK_ROUTE,
        ),
        (
            Socket::Netlink(Box::new(NetlinkSocket::new(
                NETLINK_ROUTE,
                NetlinkSocketType::Datagram,
            ))),
            SOCK_DGRAM as i32,
            AF_NETLINK as i32,
            NETLINK_ROUTE,
        ),
        // A non-zero protocol, so this case cannot also be satisfied by an
        // implementation that answers a constant zero: `NETLINK_ROUTE` is 0.
        (
            Socket::Netlink(Box::new(NetlinkSocket::new(
                NETLINK_KOBJECT_UEVENT,
                NetlinkSocketType::Raw,
            ))),
            SOCK_RAW as i32,
            AF_NETLINK as i32,
            NETLINK_KOBJECT_UEVENT,
        ),
        (
            Socket::Unix(Box::new(UnixDomainSocket::new(
                StreamTransport::new(1),
                UnixSocketType::Stream,
            ))),
            SOCK_STREAM as i32,
            AF_UNIX as i32,
            0,
        ),
        (
            Socket::Unix(Box::new(UnixDomainSocket::new(
                DgramTransport::new(1),
                UnixSocketType::Datagram,
            ))),
            SOCK_DGRAM as i32,
            AF_UNIX as i32,
            0,
        ),
        (
            Socket::Unix(Box::new(UnixDomainSocket::new(
                DgramTransport::new(1),
                UnixSocketType::Seqpacket,
            ))),
            SOCK_SEQPACKET as i32,
            AF_UNIX as i32,
            0,
        ),
    ];

    for (socket, socket_type, family, protocol) in cases {
        assert_eq!(socket.socket_type(), socket_type);
        assert_eq!(socket.socket_family(), family);
        assert_eq!(socket.socket_protocol(), protocol);
    }
}