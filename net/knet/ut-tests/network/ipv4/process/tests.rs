// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::buf::PacketOwner;

fn ipv4_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0u8; IPV4_MIN_HEADER_LEN + payload.len()];
    write_ipv4_header(
        &mut packet[..IPV4_MIN_HEADER_LEN],
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        protocol,
        64,
        payload.len(),
        Ipv4HeaderFragment::NONE,
    )
    .unwrap();
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    packet
}

fn dont_fragment_ipv4_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
    build_ipv4_packet_dont_fragment(
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        protocol,
        64,
        payload,
    )
    .unwrap()
}

#[def_test]
fn test_validate_input_packet_accepts_valid_checksum() {
    let mut packet =
        PacketBuf::from_ip_packet_vec(1, ipv4_packet(PROTOCOL_UDP, &[1, 2]), PacketOwner::DeviceRx);

    let original = packet.clone();
    crate::ipv4::validate_input_packet(&mut packet).unwrap();
    let header = Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
    assert_eq!(original.checksum_state(), ChecksumState::Unverified);

    assert_eq!(header.protocol(), PROTOCOL_UDP);
    assert_eq!(header.header_len(), IPV4_MIN_HEADER_LEN);
    assert_eq!(header.total_len(), IPV4_MIN_HEADER_LEN + 2);
    assert_eq!(packet.transport_offset(), Some(IPV4_MIN_HEADER_LEN));
    assert_eq!(packet.transport_packet(), Some([1, 2].as_slice()));
    assert_eq!(packet.checksum_state(), ChecksumState::Verified);
}

#[def_test]
fn test_validate_input_packet_marks_fragment() {
    let mut bytes = ipv4_packet(PROTOCOL_UDP, &[1, 2]);
    let mut header = Ipv4HeaderSlice::from_slice(&bytes).unwrap().to_header();
    header.more_fragments = true;
    header.header_checksum = header.calc_header_checksum();
    bytes[..IPV4_MIN_HEADER_LEN].copy_from_slice(&header.to_bytes());
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    crate::ipv4::validate_input_packet(&mut packet).unwrap();
    let header = Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();

    assert!(header.is_fragmented());
}

#[def_test]
fn test_validate_input_packet_rejects_bad_checksum() {
    let mut bytes = ipv4_packet(PROTOCOL_TCP, &[]);
    bytes[10] ^= 0xff;
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    assert_eq!(
        crate::ipv4::validate_input_packet(&mut packet),
        Err(Ipv4Error::BadChecksum)
    );
}

#[def_test]
fn test_prepare_output_packet_repairs_checksum() {
    let mut bytes = ipv4_packet(PROTOCOL_ICMP, &[]);
    bytes[2..4].fill(0);
    bytes[10..12].fill(0);
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx);

    crate::ipv4::prepare_output_packet(&mut packet).unwrap();

    let header = Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
    assert_eq!(header.protocol(), PROTOCOL_ICMP);
    assert_eq!(header.total_len(), packet.network_packet().unwrap().len());
}

#[def_test]
fn test_fragment_output_packet_splits_on_aligned_payload() {
    let original_payload: Vec<u8> = (0..40).collect();
    let mut packet = ipv4_packet(PROTOCOL_UDP, &original_payload);
    // Fragmentation uses the stored payload even before output is finalized.
    packet[2..4].fill(0);
    packet[10..12].fill(0);

    let fragments = fragment_output_packet(&packet, IPV4_MIN_HEADER_LEN + 18, 0x1234).unwrap();

    assert_eq!(fragments.len(), 3);
    let mut reassembled = Vec::new();
    for (idx, fragment) in fragments.iter().enumerate() {
        let mut packet = PacketBuf::from_ip_packet_vec(1, fragment.clone(), PacketOwner::DeviceRx);
        crate::ipv4::validate_input_packet(&mut packet).unwrap();
        let header = Ipv4Header::parse_header_only(packet.network_packet().unwrap()).unwrap();
        assert!(header.is_fragmented());
        assert_eq!(header.identification(), 0x1234);
        assert_eq!(header.fragment_offset(), idx * 16);
        assert_eq!(header.more_fragments(), idx != 2);
        assert!(!header.dont_fragment());
        reassembled.extend_from_slice(packet.transport_packet().unwrap());
    }
    assert_eq!(reassembled, original_payload);
}

#[def_test]
fn test_fragment_output_packet_rejects_dont_fragment() {
    let packet = dont_fragment_ipv4_packet(PROTOCOL_UDP, &[0u8; 32]);

    assert_eq!(
        fragment_output_packet(&packet, IPV4_MIN_HEADER_LEN + 16, 0x1234),
        Err(Ipv4FragmentError::DontFragment)
    );
}

#[def_test]
fn test_build_icmpv4_protocol_unreachable_packet() {
    let offending = ipv4_packet(99, &[1, 2, 3, 4]);
    let header = Ipv4Header::parse_input(&offending).unwrap();

    let error = build_icmpv4_error_packet(
        Icmpv4Error::ProtocolUnreachable,
        PacketType::Host,
        &offending,
    )
    .expect("ICMP error should be emitted");

    let error_header = Ipv4Header::parse_input(&error).unwrap();
    assert_eq!(error_header.protocol(), PROTOCOL_ICMP);
    assert_eq!(error_header.src_addr(), header.dst_addr());
    assert_eq!(error_header.dst_addr(), header.src_addr());
    assert_eq!(
        error[IPV4_MIN_HEADER_LEN],
        etherparse::icmpv4::TYPE_DEST_UNREACH
    );
    assert_eq!(
        error[IPV4_MIN_HEADER_LEN + 1],
        etherparse::icmpv4::CODE_DST_UNREACH_PROTOCOL
    );
}

#[def_test]
fn header_view_borrows_options_and_separates_structural_validation() {
    let mut wire =
        EtherIpv4Header::new(16, 37, IpNumber::UDP, [192, 0, 2, 1], [192, 0, 2, 2]).unwrap();
    wire.options = [1, 1, 0, 0].into();
    wire.set_payload_len(16).unwrap();
    wire.header_checksum = wire.calc_header_checksum();
    let mut bytes = wire.to_bytes().to_vec();
    bytes.extend_from_slice(&[0xaa; 16]);
    bytes.extend_from_slice(&[0xbb; 12]);
    let header = Ipv4Header::parse_input(&bytes).unwrap();
    assert_eq!(header.bytes().as_ptr(), bytes.as_ptr());
    assert_eq!(&header.bytes()[20..], &[1, 1, 0, 0]);
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes.clone(), PacketOwner::DeviceRx);
    assert!(packet.transport_packet().is_none());
    crate::ipv4::validate_input_packet(&mut packet).unwrap();
    assert_eq!(packet.network_packet().unwrap().len(), wire.total_len as usize);
    assert_eq!(packet.transport_packet().unwrap(), &[0xaa; 16]);
    for len in [0, 19, 20, 23] {
        assert!(Ipv4Header::parse_header_only(&bytes[..len]).is_err());
    }
    let quote = &bytes[..32];
    assert_eq!(Ipv4Header::parse_input(quote), Err(Ipv4Error::Malformed));
    bytes[22] ^= 1;
    assert_eq!(Ipv4Header::parse_input(&bytes), Err(Ipv4Error::BadChecksum));
    wire.total_len = (wire.header_len() - 1) as u16;
    wire.header_checksum = wire.calc_header_checksum();
    bytes[..wire.header_len()].copy_from_slice(&wire.to_bytes());
    assert_eq!(Ipv4Header::parse_input(&bytes), Err(Ipv4Error::Malformed));
}
