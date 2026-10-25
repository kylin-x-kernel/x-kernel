// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `SO_TYPE` reports the type the socket was created with.
//!
//! Every `Socket` variant holds a `Box<T>`, so an unfowarded method in that
//! layer silently answers with the trait default (`SOCK_STREAM`) for sockets of
//! any type. These cases therefore query through the enum, not the concrete
//! types, and cover the datagram/raw types that the stream case cannot detect.

use alloc::boxed::Box;

use linux_raw_sys::net::{SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET, SOCK_STREAM};
use unittest::{assert_eq, def_test};

use crate::{
    Socket, SocketOps,
    netlink::{NETLINK_ROUTE, NetlinkSocket, NetlinkSocketType},
    packet::{PacketSocket, PacketSocketKind},
    raw::{IpProtocol, IpVersion, RawSocket},
    udp::UdpSocket,
    unix::{DgramTransport, StreamTransport, UnixDomainSocket, UnixSocketType},
};

/// A zero protocol leaves the packet socket protocol unset; `SO_TYPE` does not
/// depend on it.
const PACKET_PROTOCOL: u16 = 0;

#[def_test]
fn so_type_reports_the_creation_type_through_the_socket_enum() {
    let cases = [
        (Socket::Tcp(Box::default()), SOCK_STREAM),
        (Socket::Udp(Box::new(UdpSocket::new())), SOCK_DGRAM),
        (
            Socket::Raw(Box::new(RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp))),
            SOCK_RAW,
        ),
        (
            Socket::Packet(Box::new(
                PacketSocket::new(PacketSocketKind::Raw, PACKET_PROTOCOL.to_be()).unwrap(),
            )),
            SOCK_RAW,
        ),
        (
            Socket::Packet(Box::new(
                PacketSocket::new(PacketSocketKind::Datagram, PACKET_PROTOCOL.to_be()).unwrap(),
            )),
            SOCK_DGRAM,
        ),
        (
            Socket::Netlink(Box::new(NetlinkSocket::new(
                NETLINK_ROUTE,
                NetlinkSocketType::Raw,
            ))),
            SOCK_RAW,
        ),
        (
            Socket::Netlink(Box::new(NetlinkSocket::new(
                NETLINK_ROUTE,
                NetlinkSocketType::Datagram,
            ))),
            SOCK_DGRAM,
        ),
        (
            Socket::Unix(Box::new(UnixDomainSocket::new(
                StreamTransport::new(1),
                UnixSocketType::Stream,
            ))),
            SOCK_STREAM,
        ),
        (
            Socket::Unix(Box::new(UnixDomainSocket::new(
                DgramTransport::new(1),
                UnixSocketType::Datagram,
            ))),
            SOCK_DGRAM,
        ),
        (
            Socket::Unix(Box::new(UnixDomainSocket::new(
                DgramTransport::new(1),
                UnixSocketType::Seqpacket,
            ))),
            SOCK_SEQPACKET,
        ),
    ];

    for (socket, expected) in cases {
        assert_eq!(socket.socket_type(), expected as i32);
    }
}