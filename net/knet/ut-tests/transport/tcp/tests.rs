// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{task::Wake, vec};
use core::{net::Ipv4Addr, sync::atomic::AtomicUsize, task::Context};

use kpoll::PollRegistrations;
use smoltcp::{
    iface::{Config, Interface, PollResult},
    phy::{Loopback, Medium},
    time::Instant,
    wire::{HardwareAddress, IpCidr, Ipv4Address},
};
use unittest::def_test;

use super::*;

const PORT_START: u16 = 0xc000;
const PORT_END: u16 = 0xffff;

#[def_test]
fn tcp_info_reports_unsupported_instead_of_an_empty_snapshot() {
    let socket = TcpSocket::new();
    assert_eq!(
        socket.get_option(GetSocketOption::TcpInfo(&mut ())),
        Err(KError::OperationNotSupported)
    );
}

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn tcp_waiters_only_wake_for_their_socket() {
    let socket = TcpSocket::new();
    let other = TcpSocket::new();
    socket.with_smol_socket(|socket| socket.listen(49152).unwrap());
    other.with_smol_socket(|socket| socket.listen(49153).unwrap());

    let counters: [_; 3] = core::array::from_fn(|_| Arc::new(WakeCounter::default()));
    let mut registrations = Vec::new();
    for (counter, events) in
        counters
            .iter()
            .zip([IoEvents::IN | IoEvents::RDHUP, IoEvents::OUT, IoEvents::OUT])
    {
        let waker = Waker::from(counter.clone());
        let context = Context::from_waker(&waker);
        let mut owner = PollRegistrations::new();
        socket
            .register(&mut owner.context(&context), events)
            .unwrap();
        registrations.push(owner);
    }
    let mut other_registrations = PollRegistrations::new();
    other
        .register(
            &mut other_registrations.context(&Context::from_waker(Waker::noop())),
            IoEvents::OUT,
        )
        .unwrap();

    other.with_smol_socket(|socket| socket.abort());
    for counter in &counters {
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    }

    // Both send waiters must survive smoltcp's single send-waker slot.
    socket.with_smol_socket(|socket| socket.abort());
    for counter in &counters {
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }
}

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

fn poll_tcp_until_idle(
    iface: &mut Interface,
    device: &mut Loopback,
    sockets: &mut crate::stack::socket_set::SocketSetState<'_>,
    current: Instant,
) {
    while iface.poll(current, device, sockets) != PollResult::None {}
}

#[def_test]
fn reads_publish_window_updates_without_new_ingress_or_timer_expiry() {
    for (capacity_bytes, available_bytes) in [
        (TCP_RX_BUF_LEN, 312),
        (TCP_RX_BUF_LEN, 1296),
        (TCP_RX_BUF_LEN - 1, 0),
        (TCP_RX_BUF_LEN, 1),
    ] {
        let current = Instant::from_millis(1);
        let address = Ipv4Address::new(127, 0, 0, 1);
        let mut device = Loopback::new(Medium::Ip);
        let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut device, current);
        iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(address.into(), 8)).unwrap());
        let mut sockets = crate::stack::socket_set::SocketSetState::new();
        let receiver = sockets.add(smol::Socket::new(
            smol::SocketBuffer::new(vec![0; capacity_bytes]),
            smol::SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
        ));
        sockets
            .get_mut::<smol::Socket>(receiver)
            .listen(40000)
            .unwrap();
        sockets
            .get_mut::<smol::Socket>(receiver)
            .set_ack_delay(None);
        let sender = sockets.add(new_tcp_socket());
        sockets
            .get_mut::<smol::Socket>(sender)
            .connect(iface.context(), (address, 40000), (address, 40001))
            .unwrap();
        poll_tcp_until_idle(&mut iface, &mut device, &mut sockets, current);
        let queued_bytes = capacity_bytes - available_bytes;
        assert_eq!(
            sockets
                .get_mut::<smol::Socket>(sender)
                .send_slice(&vec![0x5a; queued_bytes])
                .unwrap(),
            queued_bytes
        );
        poll_tcp_until_idle(&mut iface, &mut device, &mut sockets, current);
        assert_eq!(
            sockets.get::<smol::Socket>(receiver).recv_queue(),
            queued_bytes
        );

        let mut bytes = [0; 4096];
        let mut should_poll_rx_window = false;
        for peek in [true, false] {
            let mut dst = if peek {
                &mut bytes[..]
            } else {
                &mut bytes[..0]
            };
            let received = TcpSocket::recv_from_socket(
                sockets.get_mut::<smol::Socket>(receiver),
                &mut dst,
                peek,
                &mut should_poll_rx_window,
            )
            .unwrap();
            assert_eq!(received, if peek { bytes.len() } else { 0 });
            assert!(!should_poll_rx_window);
            assert_eq!(
                sockets.get::<smol::Socket>(receiver).recv_queue(),
                queued_bytes
            );
        }
        assert_eq!(
            TcpSocket::recv_from_socket(
                sockets.get_mut::<smol::Socket>(receiver),
                &mut &mut bytes[..],
                false,
                &mut should_poll_rx_window,
            )
            .unwrap(),
            bytes.len()
        );
        assert!(
            should_poll_rx_window,
            "read must publish receive-window work"
        );
        assert_eq!(
            iface.poll_delay(current, &sockets),
            Some(Duration::from_millis(0))
        );
        poll_tcp_until_idle(&mut iface, &mut device, &mut sockets, current);

        sockets
            .get_mut::<smol::Socket>(sender)
            .send_slice(&bytes)
            .unwrap();
        poll_tcp_until_idle(&mut iface, &mut device, &mut sockets, current);
        assert_eq!(
            sockets.get::<smol::Socket>(receiver).recv_queue(),
            queued_bytes
        );
    }
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

    // KEEPIDLE is applied to smoltcp's single keep-alive interval.
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
fn keepalive_rejects_keepidle_change_after_disabling() {
    let socket = TcpSocket::new();
    socket
        .set_option(SetSocketOption::KeepAlive(&true))
        .unwrap();
    socket
        .set_option(SetSocketOption::KeepAlive(&false))
        .unwrap();

    let mut enabled = true;
    socket
        .get_option(GetSocketOption::KeepAlive(&mut enabled))
        .unwrap();
    assert!(!enabled);

    // smoltcp may still hold the deadline armed by the first enable, so a
    // disable/re-enable cycle must not accept a new idle value.
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

#[def_test]
fn keepalive_records_probe_interval_and_count_without_changing_transport() {
    let socket = TcpSocket::new();
    socket
        .set_option(SetSocketOption::KeepInterval(&5))
        .unwrap();
    socket.set_option(SetSocketOption::KeepCount(&2)).unwrap();
    socket.set_option(SetSocketOption::KeepIdle(&30)).unwrap();
    socket
        .set_option(SetSocketOption::KeepAlive(&true))
        .unwrap();

    assert_eq!(
        socket.with_smol_socket(|s| s.keep_alive()),
        Some(Duration::from_secs(30))
    );

    // Live INTVL/CNT updates are recorded, but smoltcp has no per-phase timing
    // or probe-count control, so its KEEPIDLE-derived interval is unchanged.
    socket
        .set_option(SetSocketOption::KeepInterval(&10))
        .unwrap();
    socket.set_option(SetSocketOption::KeepCount(&4)).unwrap();
    assert_eq!(
        socket.with_smol_socket(|s| s.keep_alive()),
        Some(Duration::from_secs(30))
    );

    let mut interval_secs = 0_u32;
    let mut count = 0_u32;
    socket
        .get_option(GetSocketOption::KeepInterval(&mut interval_secs))
        .unwrap();
    socket
        .get_option(GetSocketOption::KeepCount(&mut count))
        .unwrap();
    assert_eq!((interval_secs, count), (10, 4));
}
