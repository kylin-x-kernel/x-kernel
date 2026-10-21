// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use ::core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use kerrno::LinuxError;
use kpoll::IoEvents;
use unittest::def_test;

use super::*;
use crate::{SocketErrorOrigin, ip::IpEndpoint};

fn endpoint(addr: Ipv4Addr, port: u16) -> IpEndpoint {
    SocketAddrV4::new(addr, port).into()
}

fn queued_error(errno: LinuxError, payload_byte: u8) -> UdpSocketQueuedError {
    UdpSocketQueuedError {
        payload: vec![payload_byte],
        addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234)),
        ancillary: SocketErrorInfo {
            errno,
            origin: SocketErrorOrigin::Icmp,
            error_type: 3,
            error_code: 3,
            info: 0,
            data: 0,
            offender: None,
        },
    }
}

#[def_test]
fn test_state_endpoint_state_transitions() {
    let state = UdpSocketState::new();
    let local = endpoint(Ipv4Addr::new(10, 0, 0, 2), 8080);
    let peer = endpoint(Ipv4Addr::new(192, 0, 2, 1), 5353);

    assert_eq!(state.lifecycle(), UdpSocketLifecycle::Init);
    state.set_local_endpoint(Some(local));
    assert_eq!(state.local_endpoint(), Some(local));
    assert_eq!(state.lifecycle(), UdpSocketLifecycle::Bound);

    state.set_peer_endpoint(Some((
        peer,
        IpAddress::Ipv4(Ipv4Addr::new(10, 0, 0, 2).into()),
    )));
    assert_eq!(state.peer_endpoint().map(|it| it.0), Some(peer));
    assert_eq!(state.lifecycle(), UdpSocketLifecycle::Connected);

    state.set_peer_endpoint(None);
    assert_eq!(state.peer_endpoint(), None);
    assert_eq!(state.lifecycle(), UdpSocketLifecycle::Bound);
}

#[def_test]
fn test_state_error_queue_respects_recv_err_option() {
    let state = UdpSocketState::new();

    state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));
    assert!(!state.has_pending_error());

    state.set_recv_err(true);
    state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 2));
    assert!(state.has_pending_error());
    assert_eq!(
        state.consume_socket_error(),
        LinuxError::ECONNREFUSED.into_raw()
    );
}

#[def_test]
fn test_state_recorded_error_is_read_once() {
    let state = UdpSocketState::new();

    state.record_socket_error(LinuxError::ECONNREFUSED);

    assert_eq!(
        state.take_socket_error(),
        Some(KError::from(LinuxError::ECONNREFUSED))
    );
    assert_eq!(state.take_socket_error(), None);
}

#[def_test]
fn test_state_readiness_merges_shutdown_and_error_state() {
    let state = UdpSocketState::new();

    state.set_recv_err(true);
    state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));
    let events = state.readiness(IoEvents::IN | IoEvents::OUT);
    assert!(events.contains(IoEvents::IN));
    assert!(events.contains(IoEvents::OUT));
    assert!(events.contains(IoEvents::ERR));

    state.shutdown(Shutdown::Both);
    let events = state.readiness(IoEvents::IN | IoEvents::OUT);
    assert!(events.contains(IoEvents::HUP));
    assert!(events.contains(IoEvents::RDHUP));
    assert!(!events.contains(IoEvents::OUT));
}
