// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::net::Ipv4Addr;

use smoltcp::{time::Instant, wire::Ipv4Address};
use unittest::def_test;

use super::*;

const PORT_START: u16 = 0xc000;
const PORT_END: u16 = 0xffff;

struct BoundRegistryReset;

impl BoundRegistryReset {
    fn new() -> Self {
        clear_bound_registry();
        Self
    }
}

impl Drop for BoundRegistryReset {
    fn drop(&mut self) {
        clear_bound_registry();
    }
}

fn clear_bound_registry() {
    TCP_BOUND_ENDPOINTS.lock().clear();
}

fn next_ephemeral_port(port: u16) -> u16 {
    if port == PORT_END {
        PORT_START
    } else {
        port + 1
    }
}

fn listen_endpoint(addr: Option<Ipv4Addr>, port: u16) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: addr.map(IpAddress::Ipv4),
        port,
    }
}

#[def_test]
fn recv_window_poll_requires_consumed_data() {
    assert!(!should_poll_receive_window(64 * 1024, 0, 0));
    assert!(!should_poll_receive_window(64 * 1024, 1, 0));
}

#[def_test]
fn recv_window_poll_covers_the_scaled_zero_window_range() {
    assert!(should_poll_receive_window(64 * 1024, 0, 1));
    assert!(should_poll_receive_window(64 * 1024, 1, 1));
    assert!(should_poll_receive_window(64 * 1024, 1, 4096));
}

#[def_test]
fn recv_window_poll_stops_after_the_scale_quantum() {
    assert!(!should_poll_receive_window(64 * 1024, 2, 4096));
    assert!(!should_poll_receive_window(64 * 1024, 4096, 4096));
}

#[def_test]
fn recv_window_poll_covers_unscaled_receive_buffers() {
    assert!(should_poll_receive_window(64 * 1024 - 1, 0, 1));
    assert!(!should_poll_receive_window(64 * 1024 - 1, 1, 1));
}

#[def_test]
fn test_send_state_error_maps_connection_and_shutdown_states() {
    assert_eq!(
        TcpSocket::send_state_error(smol::State::Listen),
        KError::NotConnected
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::SynSent),
        KError::NotConnected
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::SynReceived),
        KError::NotConnected
    );

    assert_eq!(
        TcpSocket::send_state_error(smol::State::Closed),
        KError::BrokenPipe
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::FinWait1),
        KError::BrokenPipe
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::FinWait2),
        KError::BrokenPipe
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::Closing),
        KError::BrokenPipe
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::LastAck),
        KError::BrokenPipe
    );
    assert_eq!(
        TcpSocket::send_state_error(smol::State::TimeWait),
        KError::BrokenPipe
    );
}

#[def_test]
fn deferred_close_reaps_protocol_closed_socket() {
    let current = Instant::from_millis(10);
    let mut sockets = crate::socket_set::SocketSetState::new();
    let handle = sockets.add(new_tcp_socket());
    sockets.defer_tcp_close(handle);

    let next_deadline = sockets.reap_deferred_tcp_closes(current);

    assert_eq!(next_deadline, None);
    assert_eq!(sockets.iter().count(), 0);
}

#[def_test]
fn deferred_close_does_not_impose_deadline_on_live_socket() {
    let current = Instant::from_millis(10);
    let mut socket = new_tcp_socket();
    socket.listen(49152).unwrap();
    let mut sockets = crate::socket_set::SocketSetState::new();
    let handle = sockets.add(socket);
    sockets.defer_tcp_close(handle);

    assert_eq!(sockets.reap_deferred_tcp_closes(current), None);
    assert_eq!(sockets.iter().count(), 1);
}

#[def_test]
fn test_listen_addrs_conflict_handles_wildcards_and_exact_matches() {
    let addr_a = Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1)));
    let addr_b = Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2)));

    assert!(listen_addrs_conflict(None, addr_a));
    assert!(listen_addrs_conflict(addr_a, None));
    assert!(listen_addrs_conflict(addr_a, addr_a));
    assert!(!listen_addrs_conflict(addr_a, addr_b));
}

#[def_test(serial)]
fn test_register_tcp_bound_rejects_conflicts_but_keeps_distinct_specific_addrs() {
    let _reset = BoundRegistryReset::new();
    let addr_a = listen_endpoint(Some(Ipv4Addr::new(192, 0, 2, 1)), 40000);
    let addr_b = listen_endpoint(Some(Ipv4Addr::new(192, 0, 2, 2)), 40000);
    let wildcard = listen_endpoint(None, 40000);

    assert!(register_tcp_bound(addr_a).is_ok());
    assert!(register_tcp_bound(addr_b).is_ok());
    assert_eq!(register_tcp_bound(addr_a), Err(KError::AddrInUse));
    assert_eq!(register_tcp_bound(wildcard), Err(KError::AddrInUse));

    let bound_addrs = TCP_BOUND_ENDPOINTS.lock().get(&40000).cloned().unwrap();
    assert_eq!(bound_addrs.len(), 2);
    assert!(bound_addrs.contains(&addr_a.addr));
    assert!(bound_addrs.contains(&addr_b.addr));
}

#[def_test(serial)]
fn test_unregister_tcp_bound_removes_only_matching_address_and_cleans_empty_port() {
    let _reset = BoundRegistryReset::new();
    let addr_a = listen_endpoint(Some(Ipv4Addr::new(192, 0, 2, 10)), 40001);
    let addr_b = listen_endpoint(Some(Ipv4Addr::new(192, 0, 2, 11)), 40001);

    register_tcp_bound(addr_a).unwrap();
    register_tcp_bound(addr_b).unwrap();

    unregister_tcp_bound(addr_a);
    {
        let bound_addrs = TCP_BOUND_ENDPOINTS.lock().get(&40001).cloned().unwrap();
        assert_eq!(bound_addrs, vec![addr_b.addr]);
    }

    unregister_tcp_bound(addr_a);
    {
        let bound_addrs = TCP_BOUND_ENDPOINTS.lock().get(&40001).cloned().unwrap();
        assert_eq!(bound_addrs, vec![addr_b.addr]);
    }

    unregister_tcp_bound(addr_b);
    assert!(TCP_BOUND_ENDPOINTS.lock().get(&40001).is_none());
}

#[def_test(serial)]
fn test_tcp_port_available_respects_specific_and_wildcard_conflicts() {
    let _reset = BoundRegistryReset::new();
    let addr_a = listen_endpoint(Some(Ipv4Addr::new(198, 51, 100, 1)), 40002);
    let addr_b = listen_endpoint(Some(Ipv4Addr::new(198, 51, 100, 2)), 40002);
    let wildcard = listen_endpoint(None, 40002);

    assert!(tcp_port_available(addr_a));

    register_tcp_bound(addr_a).unwrap();
    assert!(!tcp_port_available(addr_a));
    assert!(tcp_port_available(addr_b));
    assert!(!tcp_port_available(wildcard));
}

#[def_test(serial)]
fn test_get_ephemeral_port_skips_conflicting_candidate_and_stays_in_range() {
    let _reset = BoundRegistryReset::new();
    let local_addr = IpAddress::Ipv4(Ipv4Address::new(203, 0, 113, 10));

    let first = get_ephemeral_port(local_addr).unwrap();
    assert!((PORT_START..=PORT_END).contains(&first));

    let blocked = listen_endpoint(
        Some(Ipv4Addr::new(203, 0, 113, 10)),
        next_ephemeral_port(first),
    );
    register_tcp_bound(blocked).unwrap();

    let second = get_ephemeral_port(local_addr).unwrap();
    assert!((PORT_START..=PORT_END).contains(&second));
    assert_ne!(second, blocked.port);
}

#[def_test]
fn keepalive_uses_keepidle_as_transport_interval() {
    let socket = TcpSocket::new();
    socket.set_option(SetSocketOption::KeepIdle(&30)).unwrap();
    socket
        .set_option(SetSocketOption::KeepAlive(&true))
        .unwrap();

    // KEEPIDLE is enforced: enabling keep-alive installs the
    // configured idle seconds as the transport's interval.
    assert_eq!(
        socket.with_smol_socket(|s| s.keep_alive()),
        Some(Duration::from_secs(30))
    );

    let mut idle = 0_u32;
    socket
        .get_option(GetSocketOption::KeepIdle(&mut idle))
        .unwrap();
    assert_eq!(idle, 30);
}

#[def_test]
fn keepalive_rejects_keepidle_change_after_enabling() {
    let socket = TcpSocket::new();
    socket
        .set_option(SetSocketOption::KeepAlive(&true))
        .unwrap();
    assert_eq!(
        socket.set_option(SetSocketOption::KeepIdle(&45)),
        Err(KError::OperationNotSupported)
    );

    let mut idle = 0_u32;
    socket
        .get_option(GetSocketOption::KeepIdle(&mut idle))
        .unwrap();
    assert_eq!(idle, TCP_KEEPIDLE_DEFAULT_SECS);
}

#[def_test]
fn keepalive_options_reject_invalid_values() {
    let socket = TcpSocket::new();
    // Linux range check on the enforced knob: 1..=TCP_KEEPIDLE_MAX_SECS,
    // else EINVAL.
    assert!(socket.set_option(SetSocketOption::KeepIdle(&0)).is_err());
    assert!(
        socket
            .set_option(SetSocketOption::KeepIdle(&(TCP_KEEPIDLE_MAX_SECS + 1)))
            .is_err()
    );
    assert!(
        socket
            .set_option(SetSocketOption::KeepIdle(&TCP_KEEPIDLE_MAX_SECS))
            .is_ok()
    );
    // the boundary value itself is accepted and readable back
    socket
        .set_option(SetSocketOption::KeepIdle(&TCP_KEEPIDLE_MAX_SECS))
        .unwrap();
    let mut readback = 0_u32;
    socket
        .get_option(GetSocketOption::KeepIdle(&mut readback))
        .unwrap();
    assert_eq!(readback, TCP_KEEPIDLE_MAX_SECS);
    // an out-of-range write does not clobber the last valid value
    assert!(socket.set_option(SetSocketOption::KeepIdle(&0)).is_err());
    socket
        .get_option(GetSocketOption::KeepIdle(&mut readback))
        .unwrap();
    assert_eq!(readback, TCP_KEEPIDLE_MAX_SECS);
}
