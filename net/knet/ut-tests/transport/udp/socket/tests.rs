// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::task::Wake;

use ::core::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Waker},
};
use kpoll::PollRegistrations;
use unittest::def_test;

use super::{super::output::has_valid_udp_checksum, *};
use crate::{
    buf::{PacketBuf, PacketOwner},
    udp::{InputDisposition, PreparedUdpPacket, deliver_ipv4_packet},
};

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn received_packet(ifindex: i32, local: IpEndpoint) -> PreparedUdpPacket {
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 4321).into();
    let packet = PacketBuf::from_ip_packet_vec(ifindex, vec![42], PacketOwner::Ipv4Stack);
    PreparedUdpPacket::new(packet, 0, 1, local.into(), remote).unwrap()
}

// Serial: exercises the shared UDP registry; socket Drop unregisters both PCBs.
#[def_test(serial)]
fn receive_waiter_isolated_from_other_sockets_and_device_binding_changes() {
    let socket = UdpSocket::new();
    let other = UdpSocket::new();
    let local_addr = Ipv4Address::new(192, 0, 2, 1).into();
    let local =
        registry::bind_udp_auto_ephemeral_pcb(socket.pcb.clone(), local_addr, false).unwrap();
    let other_local =
        registry::bind_udp_auto_ephemeral_pcb(other.pcb.clone(), local_addr, false).unwrap();
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();

    assert_eq!(
        deliver_ipv4_packet(received_packet(2, other_local)).0,
        InputDisposition::Accepted
    );
    assert!(other.pcb.has_recv_data());
    assert!(!socket.pcb.has_recv_data());
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);

    socket.state().set_bound_dev_if_for_test(1);
    let (disposition, rejected) = deliver_ipv4_packet(received_packet(2, local));
    assert_eq!(disposition, InputDisposition::NoSocket);
    drop(rejected);
    socket.state().set_bound_dev_if_for_test(0);
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);

    // The original read subscription remains valid after unbinding, even
    // when a matching datagram arrives through a different interface.
    assert_eq!(
        deliver_ipv4_packet(received_packet(2, local)).0,
        InputDisposition::Accepted
    );
    assert!(socket.pcb.has_recv_data());
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);

    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    drop(registrations);
    assert_eq!(
        deliver_ipv4_packet(received_packet(1, local)).0,
        InputDisposition::Accepted
    );
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
}

#[def_test]
fn receive_waiter_observes_errors_and_shutdown() {
    let socket = UdpSocket::new();
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    for events in [IoEvents::IN, IoEvents::RDNORM, IoEvents::RDBAND] {
        socket
            .register(&mut registrations.context(&context), events)
            .unwrap();
        socket.state().record_socket_error(LinuxError::ECONNREFUSED);
        assert_eq!(counter.0.swap(0, Ordering::Relaxed), 1);
        assert!(socket.state().take_socket_error().is_some());
    }

    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    socket.state().shutdown(Shutdown::Read);
    assert!(socket.state().is_read_shutdown());
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
}

#[def_test]
fn error_msg_trunc_returns_original_payload_len() {
    let mut data = [0u8; 2];
    let mut flags = RecvFlags::empty();
    let mut msg = RecvErrorMsg {
        data: &mut data,
        from: None,
        flags: RecvFlags::TRUNCATE,
        ancillary: None,
        out_flags: Some(&mut flags),
    };

    let copied = copy_udp_error_payload(&[1, 2, 3, 4], &mut msg);

    assert_eq!(copied, 4);
    assert_eq!(msg.data, &[1, 2]);
    assert!(flags.contains(RecvFlags::TRUNCATE));
}

#[def_test]
fn udp_datagram_is_written_into_preallocated_ipv4_packet() {
    let socket = UdpSocket::new();
    let src_addr = Ipv4Address::new(192, 0, 2, 1);
    let dst_addr = Ipv4Address::new(192, 0, 2, 2);
    let local_endpoint: IpEndpoint =
        SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234).into();
    let remote_endpoint: IpEndpoint =
        SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 4321).into();
    socket.state().set_local_endpoint(Some(local_endpoint));
    let mut packet = vec![0u8; IPV4_HEADER_LEN + UDP_HEADER_LEN + 4];
    packet[IPV4_HEADER_LEN + UDP_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);

    socket
        .write_datagram(
            &mut packet,
            remote_endpoint,
            IpAddress::Ipv4(src_addr),
            false,
        )
        .unwrap();

    let header = ipv4::Ipv4Header::parse_input(&packet).unwrap();
    let udp_packet = &packet[header.header_len()..];
    assert_eq!(header.total_len(), packet.len());
    assert!(has_valid_udp_checksum(src_addr, dst_addr, udp_packet));
    assert_eq!(&udp_packet[UDP_HEADER_LEN..], &[1, 2, 3, 4]);
}

#[def_test]
fn mtu_discovery_selects_df_like_linux_ipv4_output() {
    assert!(!should_set_dont_fragment(
        IP_PMTUDISC_DONT,
        1500,
        Some(1500)
    ));
    assert!(should_set_dont_fragment(IP_PMTUDISC_WANT, 1500, Some(1500)));
    assert!(!should_set_dont_fragment(
        IP_PMTUDISC_WANT,
        1501,
        Some(1500)
    ));
    assert!(should_set_dont_fragment(IP_PMTUDISC_DO, 1501, Some(1500)));
    assert!(should_set_dont_fragment(
        IP_PMTUDISC_PROBE,
        1501,
        Some(1500)
    ));
    assert!(!should_set_dont_fragment(
        IP_PMTUDISC_INTERFACE,
        1500,
        Some(1500)
    ));
    assert!(!should_set_dont_fragment(
        IP_PMTUDISC_OMIT,
        1500,
        Some(1500)
    ));
}
