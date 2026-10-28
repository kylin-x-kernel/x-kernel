// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use etherparse::{IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header};
use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::*;
use crate::{buf::PacketOwner, ipv4};

fn fragment(offset: usize, more_fragments: bool, payload: &[u8]) -> PacketBuf {
    fragment_with_id(0x1234, offset, more_fragments, payload)
}

fn fragment_with_id(
    identification: u16,
    offset: usize,
    more_fragments: bool,
    payload: &[u8],
) -> PacketBuf {
    let offset_units = u16::try_from(offset / 8).unwrap();
    let mut header = EtherIpv4Header::new(
        payload.len() as u16,
        64,
        IpNumber(ipv4::PROTOCOL_UDP),
        [192, 0, 2, 1],
        [192, 0, 2, 2],
    )
    .unwrap();
    header.identification = identification;
    header.dont_fragment = false;
    header.more_fragments = more_fragments;
    header.fragment_offset = IpFragOffset::try_new(offset_units).unwrap();
    header.header_checksum = header.calc_header_checksum();

    let mut bytes: Vec<u8> = header.to_bytes().into_iter().collect();
    bytes.extend_from_slice(payload);
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
    crate::ipv4::validate_input_packet(&mut packet).unwrap();
    packet
}

fn reassemble(parts: &[(usize, bool, &[u8])]) -> Ipv4ReassemblyResult {
    let mut reassembler = Ipv4Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let mut result = Ipv4ReassemblyResult::Pending;
    for (offset, more, payload) in parts {
        let packet = fragment(*offset, *more, payload);
        result = reassembler.reassemble(packet, now);
    }
    result
}

#[def_test]
fn test_ipv4_reassembly_accepts_in_order_fragments() {
    let result = reassemble(&[(0, true, b"abcdefgh"), (8, false, b"ijkl")]);

    let Ipv4ReassemblyResult::Complete(mut packet) = result else {
        panic!("expected complete packet");
    };
    crate::ipv4::validate_input_packet(&mut packet).unwrap();
    let ip_packet = packet.network_packet().unwrap();
    let header = ipv4::Ipv4Header::parse_input(ip_packet).unwrap();
    assert!(!header.is_fragmented());
    assert_eq!(packet.transport_packet().unwrap(), b"abcdefghijkl");
}

#[def_test]
fn test_ipv4_reassembly_accepts_reverse_order_fragments() {
    let result = reassemble(&[(8, false, b"ijkl"), (0, true, b"abcdefgh")]);

    let Ipv4ReassemblyResult::Complete(mut packet) = result else {
        panic!("expected complete packet");
    };
    crate::ipv4::validate_input_packet(&mut packet).unwrap();
    assert_eq!(packet.transport_packet().unwrap(), b"abcdefghijkl");
}

#[def_test]
fn test_ipv4_reassembly_drops_overlapping_fragments() {
    let result = reassemble(&[(0, true, b"abcdefgh"), (0, false, b"abcdefghi")]);

    assert!(matches!(result, Ipv4ReassemblyResult::Dropped));
}

#[def_test]
fn test_ipv4_reassembly_keeps_queue_on_duplicate_fragment() {
    let mut reassembler = Ipv4Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let packet = fragment(0, true, b"abcdefgh");
    let retained_bytes = packet.network_packet().unwrap().len();
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));
    assert_eq!(reassembler.memory_bytes, retained_bytes);
    let packet = fragment(0, true, b"abcdefgh");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));
    assert_eq!(reassembler.memory_bytes, retained_bytes);
    let packet = fragment(8, false, b"ijkl");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Complete(_)
    ));
    assert_eq!(reassembler.expiration_deadline(), None);
    assert_eq!(reassembler.memory_bytes, 0);
}

#[def_test]
fn test_ipv4_reassembly_deadline_is_fixed_at_queue_creation() {
    let mut reassembler = Ipv4Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let packet = fragment(0, true, b"abcdefgh");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));
    let expires_at = now + IPV4_REASSEMBLY_TIMEOUT;
    assert_eq!(reassembler.expiration_deadline(), Some(expires_at));

    let packet = fragment(0, true, b"abcdefgh");
    assert!(matches!(
        reassembler.reassemble(packet, now + TimeSpan::from_secs(1)),
        Ipv4ReassemblyResult::Pending
    ));
    assert_eq!(reassembler.expiration_deadline(), Some(expires_at));
}

#[def_test]
fn test_ipv4_reassembly_expires_first_fragment() {
    let mut reassembler = Ipv4Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let packet = fragment(0, true, b"abcdefgh");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));

    let expired = reassembler.remove_expired(now + IPV4_REASSEMBLY_TIMEOUT, usize::MAX);

    assert_eq!(expired.timer_events, 1);
    assert_eq!(expired.fragments.len(), 1);
    assert_eq!(reassembler.expiration_deadline(), None);
    let fragment = &expired.fragments[0];
    let header = ipv4::Ipv4Header::parse_input(&fragment.packet).unwrap();
    assert_eq!(header.fragment_offset(), 0);
    assert_eq!(&fragment.packet[header.header_len()..header.total_len()], b"abcdefgh");
}

#[def_test]
fn test_ipv4_reassembly_expire_advances_to_the_next_queue_deadline() {
    let mut reassembler = Ipv4Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let packet = fragment_with_id(0x1234, 0, true, b"abcdefgh");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));
    let later = now + TimeSpan::from_secs(5);
    let packet = fragment_with_id(0x1235, 0, true, b"ijklmnop");
    assert!(matches!(
        reassembler.reassemble(packet, later),
        Ipv4ReassemblyResult::Pending
    ));

    let expired = reassembler.remove_expired(now + IPV4_REASSEMBLY_TIMEOUT, usize::MAX);

    assert_eq!(expired.timer_events, 1);
    assert_eq!(expired.fragments.len(), 1);
    let header = ipv4::Ipv4Header::parse_input(&expired.fragments[0].packet).unwrap();
    assert_eq!(header.identification(), 0x1234);
    assert_eq!(
        reassembler.expiration_deadline(),
        Some(later + IPV4_REASSEMBLY_TIMEOUT)
    );
}

#[def_test]
fn test_ipv4_reassembly_expiration_respects_timer_budget() {
    let mut reassembler = Ipv4Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let packet = fragment_with_id(0x1234, 0, true, b"abcdefgh");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));
    let packet = fragment_with_id(0x1235, 0, true, b"ijklmnop");
    assert!(matches!(
        reassembler.reassemble(packet, now),
        Ipv4ReassemblyResult::Pending
    ));

    let first = reassembler.remove_expired(now + IPV4_REASSEMBLY_TIMEOUT, 1);

    assert_eq!(first.timer_events, 1);
    assert_eq!(first.fragments.len(), 1);
    assert_eq!(
        reassembler.expiration_deadline(),
        Some(now + IPV4_REASSEMBLY_TIMEOUT)
    );

    let second = reassembler.remove_expired(now + IPV4_REASSEMBLY_TIMEOUT, 1);

    assert_eq!(second.timer_events, 1);
    assert_eq!(second.fragments.len(), 1);
    assert_eq!(reassembler.expiration_deadline(), None);
}
