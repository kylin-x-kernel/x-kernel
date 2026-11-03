// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::task::Wake;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Waker},
};

use kpoll::PollRegistrations;
use smoltcp::{
    iface::{Config, Interface, PollResult},
    phy::{Loopback, Medium},
    time::Instant,
    wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address},
};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn pending_conn() -> PendingConn {
    PendingConn {
        accepted: AcceptedTcp {
            handle: SocketHandle::default(),
            local_endpoint: IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)),
                8080,
            ),
            remote_endpoint: IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2)),
                49152,
            ),
        },
    }
}

#[def_test]
fn pending_handshake_remains_refreshable_after_packet_mark_is_consumed() {
    let mut entry = ListenTableEntry::new(1);
    entry.touched = true;
    assert!(entry.needs_refresh());

    entry.syn_queue.push_back(pending_conn());
    entry.touched = false;
    assert!(entry.needs_refresh());

    entry.syn_queue.clear();
    assert!(!entry.needs_refresh());

    entry.syn_queue.push_back(pending_conn());
    entry.closed = true;
    assert!(!entry.needs_refresh());
}

#[def_test]
fn accept_waiter_registered_with_ready_backlog_observes_later_arrival() {
    struct AcceptWakeProbe(AtomicUsize);
    impl Wake for AcceptWakeProbe {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let current = Instant::from_millis(1);
    let address = Ipv4Address::new(127, 0, 0, 1);
    let mut device = Loopback::new(Medium::Ip);
    let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut device, current);
    iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(address.into(), 8)).unwrap());
    let mut sockets = SocketSet::new(vec![]);
    let [first, second] = [40001, 40002].map(|client_port| {
        let server = sockets.add(crate::tcp::new_tcp_socket());
        sockets.get_mut::<tcp::Socket>(server).listen(40000).unwrap();
        let client = sockets.add(crate::tcp::new_tcp_socket());
        sockets
            .get_mut::<tcp::Socket>(client)
            .connect(iface.context(), (address, 40000), (address, client_port))
            .unwrap();
        while iface.poll(current, &mut device, &mut sockets) != PollResult::None {}
        let socket = sockets.get::<tcp::Socket>(server);
        PendingConn {
            accepted: AcceptedTcp {
                handle: server,
                local_endpoint: socket.local_endpoint().unwrap(),
                remote_endpoint: socket.remote_endpoint().unwrap(),
            },
        }
    });
    for connection in [&first, &second] {
        assert_eq!(
            sockets.get::<tcp::Socket>(connection.accepted.handle).state(),
            State::Established
        );
    }
    let first_handle = first.accepted.handle;
    let endpoint = IpListenEndpoint::from(first.accepted.local_endpoint);
    let table = ListenTable::new();
    table.listen(endpoint, 2).unwrap();
    let entry = table.get_entry(&endpoint).unwrap();
    entry.state.lock().accept_queue.push_back(first);

    let probe = Arc::new(AcceptWakeProbe(AtomicUsize::new(0)));
    let waker = Waker::from(probe.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    table
        .register_accept_waker(endpoint, &mut registrations.context(&context))
        .unwrap();
    assert!(table.can_accept(endpoint, &sockets).unwrap());
    let wakes_before_arrival = probe.0.load(Ordering::Relaxed);
    assert_eq!(table.accept(endpoint, &mut sockets).unwrap().handle, first_handle);

    // Model a second handshake completing after the existing backlog drains.
    entry.state.lock().syn_queue.push_back(second);
    table.refresh_acceptors(&mut sockets);
    assert!(table.can_accept(endpoint, &sockets).unwrap());
    assert_eq!(probe.0.load(Ordering::Relaxed), wakes_before_arrival + 1);
}
