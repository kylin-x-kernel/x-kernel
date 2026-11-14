// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::{buf::PacketOwner, ip::Ipv4Address};

fn test_packet(ifindex: i32) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(ifindex, vec![0; 20], PacketOwner::Loopback)
}

// Serial: fills and discards entries on the shared NetRx queue.
#[def_test(serial)]
fn overflow_returns_the_packet_and_discard_drops_it_after_unlock() {
    ensure_net_rx_queue();
    const IFINDEX: i32 = 1_000_001;
    let overflowed = loop {
        match enqueue(test_packet(IFINDEX), "lo", crate::netfilter::global()) {
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
    enqueue(test_packet(IFINDEX), "lo", crate::netfilter::global()).unwrap();

    assert!(!process_pending(NET_RX_BUDGET));
    let packet = pop_deferred(IFINDEX).expect("non-UDP should remain queued");
    assert_eq!(packet.packet().ifindex(), IFINDEX);
    assert!(packet.packet().udp_metadata().is_none());
    assert!(!has_work_for(IFINDEX));
}

#[def_test(serial)]
fn prepared_udp_fallback_keeps_its_receive_stage_and_fifo_order() {
    ensure_net_rx_queue();
    const IFINDEX: i32 = 1_000_003;
    let mut udp = vec![0; 8];
    udp[..2].copy_from_slice(&12345u16.to_be_bytes());
    udp[2..4].copy_from_slice(&64234u16.to_be_bytes());
    udp[4..6].copy_from_slice(&8u16.to_be_bytes());
    let bytes = ipv4::build_ipv4_packet(
        Ipv4Address::LOOPBACK,
        Ipv4Address::LOOPBACK,
        ipv4::PROTOCOL_UDP,
        64,
        &udp,
    )
    .unwrap();
    let packet = PacketBuf::from_ip_packet_vec(IFINDEX, bytes, PacketOwner::Loopback);
    enqueue(packet, "lo", crate::netfilter::global()).unwrap();
    assert!(pop_deferred(IFINDEX).is_none());
    enqueue(test_packet(IFINDEX), "lo", crate::netfilter::global()).unwrap();
    assert!(!process_pending(NET_RX_BUDGET));
    assert!(matches!(pop_deferred(IFINDEX), Some(RxPacket::Network(_))));
    assert!(matches!(pop_deferred(IFINDEX), Some(RxPacket::LocalUdp(_))));
    assert!(!has_work_for(IFINDEX));
}
