// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use smoltcp::{
    iface::{Config, Interface, PollResult},
    phy::{Loopback, Medium},
    socket::tcp,
    time::Instant,
    wire::{HardwareAddress, IpCidr, Ipv4Address},
};
use unittest::{assert, assert_eq, def_test};

use super::{ORPHAN_FIN_WAIT2_TIMEOUT, orphan_fin_wait2_deadline};

fn poll_until_idle(
    iface: &mut Interface,
    device: &mut Loopback,
    sockets: &mut super::SocketSetState<'_>,
    current: Instant,
) {
    while iface.poll(current, device, sockets) != PollResult::None {}
}

#[def_test]
fn orphan_receiving_data_after_close_resets_peer_and_releases_handle() {
    let current = Instant::from_millis(1);
    let mut device = Loopback::new(Medium::Ip);
    let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut device, current);
    let address = Ipv4Address::new(127, 0, 0, 1);
    iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(address.into(), 8)).unwrap());
    let mut sockets = super::SocketSetState::new();
    let server = sockets.add(crate::tcp::new_tcp_socket());
    sockets
        .get_mut::<tcp::Socket>(server)
        .listen(40000)
        .unwrap();
    let client = sockets.add(crate::tcp::new_tcp_socket());
    sockets
        .get_mut::<tcp::Socket>(client)
        .connect(iface.context(), (address, 40000), (address, 40001))
        .unwrap();
    poll_until_idle(&mut iface, &mut device, &mut sockets, current);
    assert_eq!(
        sockets.get::<tcp::Socket>(server).state(),
        tcp::State::Established
    );
    assert_eq!(
        sockets.get::<tcp::Socket>(client).state(),
        tcp::State::Established
    );

    sockets.get_mut::<tcp::Socket>(server).close();
    sockets.defer_tcp_close(server);
    poll_until_idle(&mut iface, &mut device, &mut sockets, current);
    sockets
        .get_mut::<tcp::Socket>(client)
        .send_slice(b"late data")
        .unwrap();
    poll_until_idle(&mut iface, &mut device, &mut sockets, current);
    assert_eq!(
        sockets.get::<tcp::Socket>(server).recv_queue(),
        b"late data".len()
    );

    sockets.reap_deferred_tcp_closes(current);
    assert_eq!(
        sockets.get::<tcp::Socket>(server).state(),
        tcp::State::Closed
    );
    poll_until_idle(&mut iface, &mut device, &mut sockets, current);
    assert_eq!(
        sockets.get::<tcp::Socket>(client).state(),
        tcp::State::Closed
    );
    sockets.reap_deferred_tcp_closes(current);
    assert!(sockets.iter().all(|(handle, _)| handle != server));
}

#[def_test]
fn only_orphan_fin_wait2_gets_a_cleanup_deadline() {
    let current = Instant::from_millis(10);

    for state in [
        tcp::State::FinWait1,
        tcp::State::Closing,
        tcp::State::LastAck,
        tcp::State::TimeWait,
    ] {
        assert_eq!(orphan_fin_wait2_deadline(state, current, None), None);
    }
    assert_eq!(
        orphan_fin_wait2_deadline(tcp::State::FinWait2, current, None),
        Some(current + ORPHAN_FIN_WAIT2_TIMEOUT)
    );
}

#[def_test]
fn orphan_fin_wait2_keeps_its_original_cleanup_deadline() {
    let current = Instant::from_millis(10);
    let deadline = current + ORPHAN_FIN_WAIT2_TIMEOUT;

    assert_eq!(
        orphan_fin_wait2_deadline(
            tcp::State::FinWait2,
            Instant::from_millis(20),
            Some(deadline)
        ),
        Some(deadline)
    );
}
