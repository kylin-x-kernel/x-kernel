// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use kio::Cursor;
use smoltcp::wire::{Ipv4Packet, Ipv4Repr, Ipv6Packet, Ipv6Repr};
use unittest::def_test;

use super::*;
use crate::options::{Configurable, GetSocketOption, SetSocketOption};

fn ipv4_packet_bytes(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let repr = Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Icmp,
        payload_len: payload.len(),
        hop_limit: 32,
    };
    let mut bytes = vec![0; repr.buffer_len() + payload.len()];
    repr.emit(
        &mut Ipv4Packet::new_unchecked(&mut bytes),
        &smoltcp::phy::ChecksumCapabilities::ignored(),
    );
    bytes[repr.buffer_len()..].copy_from_slice(payload);
    bytes
}

fn ipv6_packet_bytes(src: Ipv6Addr, dst: Ipv6Addr, payload: &[u8]) -> Vec<u8> {
    let repr = Ipv6Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Icmpv6,
        payload_len: payload.len(),
        hop_limit: 48,
    };
    let mut bytes = vec![0; repr.buffer_len() + payload.len()];
    repr.emit(&mut Ipv6Packet::new_unchecked(&mut bytes));
    bytes[repr.buffer_len()..].copy_from_slice(payload);
    bytes
}

#[def_test(serial)]
fn raw_socket_ip_version_checks_and_defaults() {
    let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);

    assert_eq!(
        socket
            .check_ip_version(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 10)))
            .unwrap(),
        IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 10))
    );
    assert_eq!(
        socket.check_ip_version(IpAddress::Ipv6(Ipv6Addr::LOCALHOST)),
        Err(KError::from(LinuxError::EAFNOSUPPORT))
    );
    assert_eq!(
        match socket.local_addr().unwrap() {
            SocketAddrEx::Ip(addr) => addr,
            other => panic!("expected IP socket addr, got {other:?}"),
        },
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    );
    assert!(matches!(socket.peer_addr(), Err(KError::NotConnected)));
}

#[def_test(serial)]
fn raw_socket_parses_ipv4_and_ipv6_packets() {
    let ipv4 = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);
    let ipv4_packet = ipv4_packet_bytes(
        Ipv4Addr::new(10, 0, 0, 1),
        Ipv4Addr::new(10, 0, 0, 2),
        &[1, 2, 3, 4],
    );
    let (source, packet) = ipv4.parse_ip_packet(&ipv4_packet).unwrap();
    assert_eq!(source, IpAddress::Ipv4(Ipv4Addr::new(10, 0, 0, 1)));
    assert_eq!(packet, ipv4_packet.as_slice());

    let ipv6 = RawSocket::new(IpVersion::Ipv6, IpProtocol::Icmpv6);
    let ipv6_packet = ipv6_packet_bytes(
        Ipv6Addr::LOCALHOST,
        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2),
        &[9, 8, 7],
    );
    let (source, packet) = ipv6.parse_ip_packet(&ipv6_packet).unwrap();
    assert_eq!(source, IpAddress::Ipv6(Ipv6Addr::LOCALHOST));
    assert_eq!(packet, ipv6_packet.as_slice());
    assert_eq!(
        ipv4.parse_ip_packet(&[1, 2, 3]),
        Err(KError::from(LinuxError::EINVAL))
    );
}

#[def_test(serial)]
fn raw_socket_ttl_remote_and_local_shortcuts() {
    let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);
    let mut ttl = 0u8;

    socket
        .get_option_inner(&mut GetSocketOption::Ttl(&mut ttl))
        .unwrap();
    assert_eq!(ttl, 64);

    let ttl_value = 7u8;
    socket
        .set_option_inner(SetSocketOption::Ttl(&ttl_value))
        .unwrap();
    socket
        .get_option_inner(&mut GetSocketOption::Ttl(&mut ttl))
        .unwrap();
    assert_eq!(ttl, 7);

    assert_eq!(
        socket
            .set_option_inner(SetSocketOption::Ttl(&0))
            .unwrap_err(),
        KError::InvalidInput
    );

    *socket.peer_addr.write() = Some(IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9)));
    assert_eq!(
        socket.remote_address(&SendOptions::default()).unwrap(),
        IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9))
    );

    *socket.local_addr.write() = Some(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 44)));
    assert_eq!(
        socket
            .local_address_for(IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9)))
            .unwrap(),
        IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 44))
    );
}

#[def_test(serial)]
fn raw_socket_bind_and_shutdown_gate_fast_paths() {
    let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);

    socket
        .bind(SocketAddrEx::Ip(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            0,
        )))
        .unwrap();
    assert_eq!(
        *socket.local_addr.read(),
        Some(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED))
    );

    assert_eq!(
        socket.bind(SocketAddrEx::Ip(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            0
        ))),
        Err(KError::from(LinuxError::EAFNOSUPPORT))
    );

    socket.shutdown(Shutdown::Both).unwrap();

    let mut recv_buf = [0u8; 8];
    assert_eq!(
        socket.recv(Cursor::new(recv_buf.as_mut_slice()), RecvOptions::default()),
        Err(KError::NotConnected)
    );
    assert_eq!(
        socket.send(Cursor::new(&[1u8, 2, 3][..]), SendOptions::default()),
        Err(KError::BrokenPipe)
    );
}
