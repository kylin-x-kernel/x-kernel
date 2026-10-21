// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use unittest::def_test;

use super::*;
use crate::{
    buf::PacketOwner,
    ip::Ipv4Address,
    ipv4,
    udp::{UDP_HEADER_LEN, UdpSocketState, output::write_udp_header},
};

fn queued_packet(remote_addr: SocketAddrV4, payload: u8) -> PreparedUdpPacket {
    let source_addr = Ipv4Address::from_octets(remote_addr.ip().octets());
    let destination_addr = Ipv4Address::new(192, 0, 2, 100);
    let mut udp_packet = vec![0; UDP_HEADER_LEN + 1];
    udp_packet[UDP_HEADER_LEN] = payload;
    write_udp_header(
        &mut udp_packet,
        source_addr,
        destination_addr,
        remote_addr.port(),
        3000,
    )
    .unwrap();
    let packet = ipv4::build_ipv4_packet(
        source_addr,
        destination_addr,
        ipv4::PROTOCOL_UDP,
        64,
        &udp_packet,
    )
    .unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, packet, PacketOwner::DeviceRx);
    let header = ipv4::Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
    crate::udp::prepare_ipv4_packet(header, packet).unwrap()
}

#[def_test]
fn receive_preserves_fifo_order() {
    let pcb = UdpPcb::new(UdpSocketState::new());
    let old_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
    let connected_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 2000);
    assert!(pcb.enqueue(queued_packet(old_remote, 1)));
    assert!(pcb.enqueue(queued_packet(connected_remote, 2)));

    let first = pcb.recv_datagram(RecvMode::Consume).unwrap();
    assert_eq!(first.remote_addr, SocketAddr::V4(old_remote));
    assert_eq!(first.payload.as_slice(), &[1]);
    let second = pcb.recv_datagram(RecvMode::Consume).unwrap();
    assert_eq!(second.remote_addr, SocketAddr::V4(connected_remote));
    assert_eq!(second.payload.as_slice(), &[2]);
}

#[def_test]
fn peek_returns_a_copy_without_consuming() {
    let pcb = UdpPcb::new(UdpSocketState::new());
    let first_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
    let second_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 2000);
    assert!(pcb.enqueue(queued_packet(first_remote, 1)));
    assert!(pcb.enqueue(queued_packet(second_remote, 2)));

    let peeked = pcb.recv_datagram(RecvMode::Peek).unwrap();
    assert_eq!(peeked.remote_addr, SocketAddr::V4(first_remote));
    assert_eq!(peeked.payload.as_slice(), &[1]);

    let first = pcb.recv_datagram(RecvMode::Consume).unwrap();
    assert_eq!(first.remote_addr, SocketAddr::V4(first_remote));
    assert_eq!(first.payload.as_slice(), &[1]);
    let second = pcb.recv_datagram(RecvMode::Consume).unwrap();
    assert_eq!(second.remote_addr, SocketAddr::V4(second_remote));
    assert_eq!(second.payload.as_slice(), &[2]);
}

#[def_test]
fn enqueue_and_peek_reuse_the_prepared_packet_allocation() {
    let pcb = UdpPcb::new(UdpSocketState::new());
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
    let packet = queued_packet(remote, 1);
    let packet_observer = packet.clone();

    assert!(pcb.enqueue(packet));
    let peeked = pcb.recv_datagram(RecvMode::Peek).unwrap();

    assert!(
        peeked
            .payload
            .packet
            .packet()
            .shares_storage_with(packet_observer.packet())
    );
    assert_eq!(pcb.rx_queue.lock().len(), 1);
}

#[def_test]
fn prepared_packet_returns_the_same_shared_packet_handle() {
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
    let prepared = queued_packet(remote, 1);
    let observer = prepared.clone();

    let packet = prepared.into_packet();

    assert!(packet.shares_storage_with(observer.packet()));
}

#[def_test]
fn receive_queue_is_reserved_at_pcb_creation() {
    let pcb = UdpPcb::new(UdpSocketState::new());
    let capacity_at_create = pcb.rx_queue.lock().capacity();
    assert_eq!(
        core::mem::size_of::<PreparedUdpPacket>(),
        core::mem::size_of::<usize>()
    );
    assert!(capacity_at_create >= UDP_RX_QUEUE_CAPACITY);

    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
    assert!(pcb.enqueue(queued_packet(remote, 1)));
    assert_eq!(pcb.rx_queue.lock().capacity(), capacity_at_create);
}

#[def_test]
fn full_receive_queue_drops_without_growing() {
    let pcb = UdpPcb::new(UdpSocketState::new());
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
    let capacity_at_create = pcb.rx_queue.lock().capacity();

    for payload in 0..UDP_RX_QUEUE_CAPACITY {
        assert!(pcb.enqueue(queued_packet(remote, payload as u8)));
    }
    assert!(!pcb.enqueue(queued_packet(remote, 0xff)));
    assert_eq!(pcb.rx_queue.lock().len(), UDP_RX_QUEUE_CAPACITY);
    assert_eq!(pcb.rx_queue.lock().capacity(), capacity_at_create);
}
