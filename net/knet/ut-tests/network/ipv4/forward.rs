// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use etherparse::{IpFragOffset, Ipv4HeaderSlice};
use unittest::def_test;

use super::*;

fn packet(ttl: u8, df: bool, offset: u16, more: bool) -> PacketBuf {
    let mut bytes = process::build_ipv4_packet(
        Ipv4Address::new(198, 51, 100, 7),
        Ipv4Address::new(203, 0, 113, 9),
        process::PROTOCOL_UDP,
        ttl,
        &[42; 144],
    )
    .unwrap();
    let mut header = Ipv4HeaderSlice::from_slice(&bytes).unwrap().to_header();
    header.identification = 0x1234;
    header.fragment_offset = IpFragOffset::try_new(offset).unwrap();
    header.more_fragments = more;
    header.dont_fragment = df;
    header.header_checksum = header.calc_header_checksum();
    bytes[..20].copy_from_slice(&header.to_bytes());
    PacketBuf::from_ip_packet_vec(7, bytes, PacketOwner::DeviceRx)
}
fn input(mut packet: PacketBuf) -> Ipv4ForwardInput {
    process::validate_input_packet(&mut packet).unwrap();
    Ipv4ForwardInput::from_validated(packet).unwrap()
}

fn prepare(packet: PacketBuf, mtu: u16) -> PreparedIpv4Forward {
    input(packet).prepare(mtu).unwrap()
}

#[def_test]
fn forward_preserves_addresses_id_and_payload_and_decrements_ttl_once() {
    let original = packet(64, false, 0, false);
    let work = prepare(original.clone(), 1500);
    assert_eq!(work.packet().ifindex(), 7);
    let packets = work.into_packets().unwrap();
    assert_eq!(packets.len(), 1);
    let header = Ipv4Header::parse_input(packets[0].network_packet().unwrap()).unwrap();
    assert_eq!(header.bytes()[8], 63);
    assert_eq!(header.src_addr(), Ipv4Address::new(198, 51, 100, 7));
    assert_eq!(header.dst_addr(), Ipv4Address::new(203, 0, 113, 9));
    assert_eq!(header.identification(), 0x1234);
    assert_eq!(packets[0].ifindex(), 7);
    assert_eq!(packets[0].transport_packet().unwrap(), &[42; 144]);
    assert_eq!(original.network_packet().unwrap()[8], 64);
}

#[def_test]
fn forward_fragments_preserve_offsets_id_and_inherited_more_flag() {
    for (offset, more) in [(0, false), (4, true), (4, false)] {
        let packets = prepare(packet(64, false, offset, more), 68)
            .into_packets()
            .unwrap();
        assert_eq!(packets.len(), 3);
        for (index, packet) in packets.iter().enumerate() {
            let header = Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
            assert_eq!(
                header.fragment_offset(),
                usize::from(offset) * 8 + index * 48
            );
            assert_eq!(header.more_fragments(), index < 2 || more);
            assert_eq!(header.identification(), 0x1234);
            assert_eq!(header.bytes()[8], 63);
            assert!(header.total_len() <= 68);
            assert_eq!(packet.transport_packet().unwrap(), &[42; 48]);
        }
    }
}

#[def_test]
fn forward_failures_quote_original_header_with_a_local_reply_source() {
    let local = Ipv4Address::new(192, 0, 2, 1);
    for (ttl, df, kind, code) in [(1, false, 11, 0), (64, true, 3, 4)] {
        let result = input(packet(ttl, df, 0, false)).prepare(68);
        let reply = result.err().unwrap().icmp_reply(local).unwrap();
        let header = Ipv4Header::parse_input(&reply).unwrap();
        assert_eq!(header.src_addr(), local);
        assert_eq!(header.dst_addr(), Ipv4Address::new(198, 51, 100, 7));
        let icmp = &reply[20..];
        assert_eq!((icmp[0], icmp[1]), (kind, code));
        assert_eq!(icmp[8 + 8], ttl);
        if df {
            assert_eq!(&icmp[6..8], &68u16.to_be_bytes());
        }
    }
    let failure = input(packet(64, false, 0, false)).no_route();
    let reply = failure.icmp_reply(local).unwrap();
    assert_eq!(&reply[20..22], &[3, 0]);
}

#[def_test]
fn forward_suppresses_errors_for_later_fragments_and_icmp_errors() {
    let local = Ipv4Address::new(192, 0, 2, 1);
    let failure = input(packet(1, false, 4, true))
        .prepare(1500)
        .err()
        .unwrap();
    assert!(failure.icmp_reply(local).is_none());
    let mut original = packet(1, false, 0, false);
    let bytes = original.network_packet_mut().unwrap();
    bytes[9] = process::PROTOCOL_ICMP;
    bytes[20] = 3;
    process::prepare_output_packet(&mut original).unwrap();
    assert!(
        input(original)
            .prepare(1500)
            .err()
            .unwrap()
            .icmp_reply(local)
            .is_none()
    );
    let mut bad = packet(64, false, 0, false);
    bad.network_packet_mut().unwrap()[10] ^= 1;
    assert!(process::validate_input_packet(&mut bad).is_err());
}

#[def_test]
fn forwarding_rejects_options_and_malformed_fragment_geometry_without_replies() {
    let original = packet(64, false, 0, false);
    let mut options = original.network_packet().unwrap().to_vec();
    drop(options.splice(20..20, [1, 1, 0, 0]));
    options[0] = 0x46;
    let mut options = PacketBuf::from_ip_packet_vec(7, options, PacketOwner::DeviceRx);
    process::prepare_output_packet(&mut options).unwrap();
    assert!(Ipv4ForwardInput::from_validated(options).is_none());
    let mut misaligned = packet(64, false, 0, true);
    misaligned.truncate_network_packet(163).unwrap();
    process::prepare_output_packet(&mut misaligned).unwrap();
    assert!(Ipv4ForwardInput::from_validated(misaligned).is_none());
    let mut overflow = packet(64, false, 8191, false);
    process::validate_input_packet(&mut overflow).unwrap();
    assert!(Ipv4ForwardInput::from_validated(overflow).is_none());
}
