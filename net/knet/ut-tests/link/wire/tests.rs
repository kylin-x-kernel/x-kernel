// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

const LOCAL_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 1]);
const REMOTE_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);

#[def_test]
fn test_ethernet_frame_parse_and_emit() {
    let mut frame = [0u8; ETHERNET_HEADER_LEN + 2];
    let payload = emit_ethernet_header(&mut frame, LOCAL_MAC, REMOTE_MAC, EtherType::Ipv4)
        .expect("Ethernet header buffer is large enough");
    payload.copy_from_slice(&[1, 2]);

    let frame_ref =
        EthernetFrameRef::new_checked(&frame).expect("Ethernet frame length is valid");
    assert_eq!(frame_ref.dst_addr(), LOCAL_MAC);
    assert_eq!(frame_ref.src_addr(), REMOTE_MAC);
    assert_eq!(frame_ref.ethertype(), EtherType::Ipv4);
    assert_eq!(frame_ref.payload(), &[1, 2]);
}

#[def_test]
fn test_arp_ipv4_parse_and_emit() {
    let packet = ArpIpv4Packet {
        operation: ArpOperation::Reply,
        source_hardware_addr: REMOTE_MAC,
        source_protocol_addr: Ipv4Address::new(10, 0, 2, 2),
        target_hardware_addr: LOCAL_MAC,
        target_protocol_addr: Ipv4Address::new(10, 0, 2, 15),
    };
    let mut data = [0u8; ArpIpv4Packet::LEN];
    packet.emit(&mut data).expect("ARP buffer is large enough");

    assert_eq!(ArpIpv4Packet::parse(&data), Some(packet));
}
