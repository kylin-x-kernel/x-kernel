// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use unittest::def_test;

use super::*;
use crate::buf::{PacketOwner, UdpPacketMetadata};

fn test_packet(ifindex: i32) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(ifindex, vec![0; 20], PacketOwner::Loopback)
}

// Serial: fills and discards entries on the shared NetRx queue.
#[def_test(serial)]
fn overflow_returns_the_packet_and_discard_drops_it_after_unlock() {
    ensure_net_rx_queue();
    const IFINDEX: i32 = 1_000_001;
    let overflowed = loop {
        match enqueue(prepare_for_enqueue(test_packet(IFINDEX)).unwrap()) {
            Ok(()) => {}
            Err(packet) => break packet,
        }
    };
    assert_eq!(overflowed.ifindex(), IFINDEX);
    drop(overflowed);
    discard_ifindex(IFINDEX);
    assert!(!has_work_for(IFINDEX));
}

#[def_test(serial)]
fn unstamped_packets_are_visible_to_the_task_poller() {
    ensure_net_rx_queue();
    const IFINDEX: i32 = 1_000_002;
    enqueue(prepare_for_enqueue(test_packet(IFINDEX)).unwrap()).unwrap();

    assert!(!process_pending(NET_RX_BUDGET));
    let packet = pop_deferred(IFINDEX).expect("non-UDP should remain queued");
    assert_eq!(packet.ifindex(), IFINDEX);
    assert!(packet.udp_metadata().is_none());
    assert!(!has_work_for(IFINDEX));
}

#[def_test(serial)]
fn stamped_udp_is_not_visible_to_the_task_poller() {
    ensure_net_rx_queue();
    const IFINDEX: i32 = 1_000_003;
    let mut packet = test_packet(IFINDEX);
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12_345));
    packet.set_udp_metadata(UdpPacketMetadata::new(addr, addr, 0, 0));
    enqueue(packet).unwrap();

    assert!(pop_deferred(IFINDEX).is_none());
    discard_ifindex(IFINDEX);
    assert!(!has_work_for(IFINDEX));
}
