// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec;

use ::core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use unittest::def_test;

use super::*;
use crate::{
    SocketErrorOrigin,
    buf::PacketType,
    ip::{IpAddress, IpEndpoint},
    ipv4::{Icmpv4Error, Ipv4Header},
    udp::{UdpSocketState, register_udp_state_for_test},
};

fn endpoint(addr: Ipv4Addr, port: u16) -> IpEndpoint {
    SocketAddrV4::new(addr, port).into()
}

fn icmp_packet(ty: u8, code: u8, info: u16) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[0] = ty;
    bytes[1] = code;
    bytes[6..8].copy_from_slice(&info.to_be_bytes());
    bytes
}

fn assert_icmpv4_errno(ty: u8, code: u8, info: u16, expected: Option<(LinuxError, u32)>) {
    let bytes = icmp_packet(ty, code, info);
    let icmp = Icmpv4Slice::from_slice(&bytes).unwrap();
    assert_eq!(icmpv4_errno(&icmp), expected);
}

fn queued_error(errno: LinuxError, payload_byte: u8) -> QueuedUdpError {
    QueuedUdpError {
        payload: vec![payload_byte],
        addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234)),
        ancillary: SocketErrorInfo {
            errno,
            origin: SocketErrorOrigin::Icmp,
            error_type: 3,
            error_code: 3,
            info: 0,
            data: 0,
            offender: Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(192, 0, 2, 254),
                0,
            ))),
        },
    }
}

fn clear_registry() {
    udp::clear_udp_registry_for_test();
}

#[def_test]
fn test_icmpv4_errno_maps_error_types() {
    assert_icmpv4_errno(
        ICMPV4_DST_UNREACHABLE,
        ICMPV4_CODE_PORT_UNREACHABLE,
        0,
        Some((LinuxError::ECONNREFUSED, 0)),
    );
    assert_icmpv4_errno(
        ICMPV4_DST_UNREACHABLE,
        ICMPV4_CODE_FRAG_REQUIRED,
        1400,
        Some((LinuxError::EMSGSIZE, 1400)),
    );
    assert_icmpv4_errno(
        ICMPV4_DST_UNREACHABLE,
        ICMPV4_CODE_PROTO_UNREACHABLE,
        0,
        Some((LinuxError::ENOPROTOOPT, 0)),
    );
    assert_icmpv4_errno(
        ICMPV4_DST_UNREACHABLE,
        ICMPV4_CODE_SR_FAILED,
        0,
        Some((LinuxError::EOPNOTSUPP, 0)),
    );
    assert_icmpv4_errno(
        ICMPV4_TIME_EXCEEDED,
        ICMPV4_CODE_TTL_EXPIRED,
        0,
        Some((LinuxError::EHOSTUNREACH, 0)),
    );
    assert_icmpv4_errno(ICMPV4_TIME_EXCEEDED, ICMPV4_CODE_FRAG_EXPIRED, 0, None);
    assert_icmpv4_errno(ICMPV4_ECHO_REQUEST, 0, 0, None);
}

#[def_test(serial)]
fn test_connected_udp_error_state_takes_precedence() {
    clear_registry();

    let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
    let bound = UdpSocketState::new();
    bound.set_recv_err(true);
    bound.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
    let connected = UdpSocketState::new();
    connected.set_recv_err(true);
    connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
    connected.set_peer_endpoint(Some((
        endpoint(*remote.ip(), remote.port()),
        IpAddress::Ipv4((*local.ip()).into()),
    )));

    register_udp_state_for_test(bound.clone());
    register_udp_state_for_test(connected.clone());
    queue_ipv4_error(local, remote, 1, queued_error(LinuxError::ECONNREFUSED, 1));

    assert!(!bound.has_pending_error());
    assert!(connected.has_pending_error());

    clear_registry();
}

#[def_test(serial)]
fn test_connected_udp_error_sets_socket_error_without_recverr() {
    clear_registry();

    let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
    let connected = UdpSocketState::new();
    connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
    connected.set_peer_endpoint(Some((
        endpoint(*remote.ip(), remote.port()),
        IpAddress::Ipv4((*local.ip()).into()),
    )));

    register_udp_state_for_test(connected.clone());
    queue_ipv4_error(local, remote, 1, queued_error(LinuxError::ECONNREFUSED, 1));

    assert_eq!(
        connected.consume_socket_error(),
        LinuxError::ECONNREFUSED.into_raw()
    );
    assert!(!connected.has_pending_error());

    clear_registry();
}

#[def_test(serial)]
fn test_udp_error_respects_bound_device() {
    clear_registry();

    let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
    let connected = UdpSocketState::new();
    connected.set_recv_err(true);
    connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
    connected.set_peer_endpoint(Some((
        endpoint(*remote.ip(), remote.port()),
        IpAddress::Ipv4((*local.ip()).into()),
    )));
    connected.set_bound_dev_if_for_test(2);
    register_udp_state_for_test(connected.clone());

    queue_ipv4_error(local, remote, 0, queued_error(LinuxError::ECONNREFUSED, 0));
    assert!(!connected.has_pending_error());

    queue_ipv4_error(local, remote, 1, queued_error(LinuxError::ECONNREFUSED, 1));
    assert!(!connected.has_pending_error());

    queue_ipv4_error(local, remote, 2, queued_error(LinuxError::ECONNREFUSED, 2));
    assert!(connected.has_pending_error());

    clear_registry();
}

#[def_test(serial)]
fn test_icmpv4_error_accepts_truncated_original_datagram() {
    clear_registry();

    let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
    let connected = UdpSocketState::new();
    connected.set_recv_err(true);
    connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
    connected.set_peer_endpoint(Some((
        endpoint(*remote.ip(), remote.port()),
        IpAddress::Ipv4((*local.ip()).into()),
    )));
    register_udp_state_for_test(connected.clone());

    let mut udp = vec![0u8; 24];
    let udp_len = udp.len() as u16;
    udp[0..2].copy_from_slice(&local.port().to_be_bytes());
    udp[2..4].copy_from_slice(&remote.port().to_be_bytes());
    udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
    let original = ipv4::build_ipv4_packet(
        (*local.ip()).into(),
        (*remote.ip()).into(),
        ipv4::PROTOCOL_UDP,
        64,
        &udp,
    )
    .unwrap();
    let original_header = Ipv4Header::parse_input(&original).unwrap();
    let error = ipv4::build_icmpv4_error_packet(
        Icmpv4Error::PortUnreachable,
        PacketType::Host,
        original_header,
        &original,
    )
    .unwrap();

    inspect_icmpv4_error(&error, 1);

    assert_eq!(
        connected.consume_socket_error(),
        LinuxError::ECONNREFUSED.into_raw()
    );
    let queued = connected.pop_error().unwrap();
    assert!(queued.payload.is_empty());
    assert_eq!(queued.addr, SocketAddr::V4(remote));
    clear_registry();
}

#[def_test]
fn test_udp_error_queue_drops_oldest_entry_when_full() {
    let state = UdpSocketState::new();
    state.set_recv_err(true);

    for byte in 0..33 {
        state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, byte));
    }

    let first = state.pop_error().unwrap();
    assert_eq!(first.payload, vec![1]);
}

#[def_test]
fn test_udp_socket_error_is_read_and_clear() {
    let state = UdpSocketState::new();
    state.set_recv_err(true);
    state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));

    assert_eq!(
        state.consume_socket_error(),
        LinuxError::ECONNREFUSED.into_raw()
    );
    assert_eq!(state.consume_socket_error(), 0);
}
