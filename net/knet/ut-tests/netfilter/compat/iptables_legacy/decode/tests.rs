// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, vec};

use unittest::def_test;

use super::*;
use crate::netfilter::{
    compat::iptables_legacy::encode,
    filter::rule::{Rule, Target},
};

fn standard(verdict: i32) -> Vec<u8> {
    let mut bytes = vec![0; 152];
    bytes[88..90].copy_from_slice(&112u16.to_ne_bytes());
    bytes[90..92].copy_from_slice(&152u16.to_ne_bytes());
    bytes[112..114].copy_from_slice(&40u16.to_ne_bytes());
    bytes[144..148].copy_from_slice(&verdict.to_ne_bytes());
    bytes
}

fn transport(protocol: u16, name: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; 200];
    bytes[..112].copy_from_slice(&standard(-2)[..112]);
    bytes[80..82].copy_from_slice(&protocol.to_ne_bytes());
    bytes[88..90].copy_from_slice(&160u16.to_ne_bytes());
    bytes[90..92].copy_from_slice(&200u16.to_ne_bytes());
    bytes[112..114].copy_from_slice(&48u16.to_ne_bytes());
    bytes[114..114 + name.len()].copy_from_slice(name);
    bytes[144..144 + payload.len()].copy_from_slice(payload);
    bytes[160..].copy_from_slice(&standard(-2)[112..]);
    bytes
}

fn tcp_entry() -> Vec<u8> {
    transport(6, b"tcp", &[0, 0, 255, 255, 80, 0, 187, 1, 0, 0x12, 2, 4])
}

#[def_test]
fn native_targets_decode_without_resolving_the_jump_graph() {
    for (code, expected) in [
        (-1, DecodedTarget::Verdict(Verdict::Drop)),
        (-2, DecodedTarget::Verdict(Verdict::Accept)),
        (-5, DecodedTarget::Return),
        (0, DecodedTarget::Offset(TableOffset(0))),
        (304, DecodedTarget::Offset(TableOffset(304))),
        (65536, DecodedTarget::Offset(TableOffset(65536))),
    ] {
        let mut input = vec![0xa5];
        input.extend(standard(code));
        input.extend(standard(-2));
        let entry = DecodedEntry::decode(&input[1..]).unwrap();
        assert_eq!(entry.next_offset, 152);
        assert_eq!(entry.target, expected);
        if let DecodedTarget::Offset(offset) = entry.target {
            assert_eq!(offset.bytes(), code as u32);
        }
        assert_eq!(entry.base, BaseMatch::default());
        assert!(entry.extensions.is_empty());
    }
    for code in [-3, -4, i32::MIN, 1, i32::MAX] {
        assert!(DecodedEntry::decode(&standard(code)).is_err());
    }
    let mut marker = vec![0; 176];
    marker[..112].copy_from_slice(&standard(-2)[..112]);
    marker[90..92].copy_from_slice(&176u16.to_ne_bytes());
    marker[112..114].copy_from_slice(&64u16.to_ne_bytes());
    marker[114..119].copy_from_slice(b"ERROR");
    marker[144..148].copy_from_slice(b"WEB\0");
    marker[149..174].fill(0xff);
    assert_eq!(
        DecodedEntry::decode(&marker).unwrap().target,
        DecodedTarget::ErrorTarget("WEB".to_string())
    );
    marker[80..82].copy_from_slice(&6u16.to_ne_bytes());
    assert!(DecodedEntry::decode(&marker).is_err());
}

#[def_test]
fn base_conditions_preserve_networks_masks_and_inversions() {
    let base = BaseMatch {
        source: Some(Negated::new(
            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
            true,
        )),
        destination: Some(Negated::new(
            Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 32).unwrap(),
            false,
        )),
        input_interface: Some(Negated::new(
            InterfacePattern::exact("eth0".to_string()).unwrap(),
            true,
        )),
        output_interface: Some(Negated::new(
            InterfacePattern::prefix("eth".to_string()).unwrap(),
            false,
        )),
        protocol: Some(Negated::new(17, true)),
    };
    let mut bytes = standard(-1);
    encode::encode_entry(&mut bytes, &base, &[], -1).unwrap();
    bytes[84..88].fill(0xff);
    bytes[92..112].fill(0xff);
    bytes[148..152].fill(0xff);
    assert_eq!(DecodedEntry::decode(&bytes).unwrap().base, base);

    bytes[80..82].fill(0);
    assert_eq!(DecodedEntry::decode(&bytes).unwrap().base.protocol, None);
    let mut bytes = standard(-2);
    bytes[83] = IPT_INV_SRCIP;
    assert!(
        DecodedEntry::decode(&bytes)
            .unwrap()
            .base
            .source
            .unwrap()
            .is_inverted()
    );
    for (offset, value) in [(8, 127), (9, 1), (0, 1), (82, 1), (83, 0x80)] {
        let mut bytes = standard(-2);
        bytes[offset] = value;
        assert!(DecodedEntry::decode(&bytes).is_err());
    }
    let mut bytes = standard(-2);
    bytes[80..82].copy_from_slice(&256u16.to_ne_bytes());
    assert!(DecodedEntry::decode(&bytes).is_err());
}

#[def_test]
fn interface_decoding_uses_only_masked_bytes() {
    for (wire, mask_len, expected) in [
        (
            &b"eth+\0"[..],
            3,
            InterfacePattern::prefix("eth".to_string()).unwrap(),
        ),
        (
            &b"eth\0"[..],
            3,
            InterfacePattern::prefix("eth".to_string()).unwrap(),
        ),
        (
            &b"eth0\0"[..],
            5,
            InterfacePattern::exact("eth0".to_string()).unwrap(),
        ),
        (
            &b"\0"[..],
            1,
            InterfacePattern::exact(String::new()).unwrap(),
        ),
    ] {
        let mut bytes = standard(-2);
        bytes[16..32].fill(0xff);
        bytes[16..16 + wire.len()].copy_from_slice(wire);
        bytes[48..48 + mask_len].fill(0xff);
        assert_eq!(
            DecodedEntry::decode(&bytes)
                .unwrap()
                .base
                .input_interface
                .unwrap(),
            Negated::new(expected, false)
        );
    }
    let mut bytes = standard(-2);
    bytes[83] = IPT_INV_VIA_IN;
    let condition = DecodedEntry::decode(&bytes)
        .unwrap()
        .base
        .input_interface
        .unwrap();
    assert!(condition.is_inverted());
    assert_eq!(
        condition.value(),
        &InterfacePattern::prefix(String::new()).unwrap()
    );
    for (offset, value) in [(48, 1), (49, 255)] {
        let mut bytes = standard(-2);
        bytes[offset] = value;
        assert!(DecodedEntry::decode(&bytes).is_err());
    }
}

#[def_test]
fn transport_extensions_decode_native_values_and_keep_their_order() {
    let tcp = DecodedEntry::decode(&tcp_entry()).unwrap();
    let ExtensionMatch::Tcp(condition) = tcp.extensions[0] else {
        panic!("expected TCP")
    };
    assert!(condition.flags_condition().unwrap().is_inverted());
    assert_eq!(condition.flags_condition().unwrap().value().mask(), 0x12);
    assert_eq!(
        condition.destination_port_range().unwrap().value().start(),
        80
    );
    assert_eq!(
        condition.destination_port_range().unwrap().value().end(),
        443
    );
    assert_eq!(
        condition.source_port_range().unwrap().value().end(),
        u16::MAX
    );

    let udp = transport(17, b"udp", &[0, 0, 255, 255, 53, 0, 53, 0, 2]);
    let parsed = DecodedEntry::decode(&udp).unwrap();
    let ExtensionMatch::Udp(condition) = parsed.extensions[0] else {
        panic!("expected UDP")
    };
    assert!(condition.destination_port_range().unwrap().is_inverted());
    assert_eq!(
        condition.destination_port_range().unwrap().value().start(),
        53
    );

    let mut icmp = transport(1, b"icmp", &[3, 1, 4, 1]);
    icmp.drain(152..160);
    icmp[88..90].copy_from_slice(&152u16.to_ne_bytes());
    icmp[90..92].copy_from_slice(&192u16.to_ne_bytes());
    icmp[112..114].copy_from_slice(&40u16.to_ne_bytes());
    let parsed = DecodedEntry::decode(&icmp).unwrap();
    let ExtensionMatch::Icmp(condition) = parsed.extensions[0] else {
        panic!("expected ICMP")
    };
    assert_eq!(
        condition,
        IcmpMatch::new(Some(3), 1, 4).unwrap().with_inversion(true)
    );
    for code in [[0, 255], [255, 255], [9, 1]] {
        for is_inverted in [false, true] {
            icmp[144..148].copy_from_slice(&[255, code[0], code[1], u8::from(is_inverted)]);
            let parsed = DecodedEntry::decode(&icmp).unwrap();
            assert_eq!(
                parsed.extensions,
                vec![ExtensionMatch::Icmp(
                    IcmpMatch::new(None, code[0], code[1])
                        .unwrap()
                        .with_inversion(is_inverted)
                )]
            );
            let mut encoded = vec![0; icmp.len()];
            encode::encode_entry(&mut encoded, &parsed.base, &parsed.extensions, -2).unwrap();
            assert_eq!(encoded, icmp);
        }
    }

    let extensions = [
        ExtensionMatch::Tcp(
            TcpMatch::new()
                .with_destination_port(Negated::new(PortRange::new(80, 80).unwrap(), false)),
        ),
        ExtensionMatch::Tcp(
            TcpMatch::new()
                .with_destination_port(Negated::new(PortRange::new(443, 443).unwrap(), true)),
        ),
    ];
    let base = BaseMatch {
        protocol: Some(Negated::new(6, false)),
        ..Default::default()
    };
    let mut bytes = vec![0; encode::entry_size(&extensions).unwrap()];
    encode::encode_entry(&mut bytes, &base, &extensions, -5).unwrap();
    let parsed = DecodedEntry::decode(&bytes).unwrap();
    let mut encoded = vec![0; bytes.len()];
    encode::encode_entry(&mut encoded, &parsed.base, &parsed.extensions, -5).unwrap();
    assert_eq!(encoded, bytes);
    assert_eq!(parsed.extensions.len(), 2);
}

#[def_test]
fn malformed_record_and_extension_bounds_are_rejected() {
    for bytes in [standard(-2), tcp_entry()] {
        for len in [0, 83, 111, 112, 143, bytes.len() - 1] {
            assert!(DecodedEntry::decode(&bytes[..len]).is_err());
        }
        for (offset, value) in [
            (88, 0u16),
            (88, 113),
            (88, 65528),
            (90, 0),
            (90, 151),
            (90, 65528),
        ] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
            assert!(DecodedEntry::decode(&invalid).is_err());
        }
    }
    for value in [0u16, 24, 47, 56, 65528] {
        let mut bytes = tcp_entry();
        bytes[112..114].copy_from_slice(&value.to_ne_bytes());
        assert!(DecodedEntry::decode(&bytes).is_err());
    }
    let mut bytes = standard(-2);
    bytes[112..114].copy_from_slice(&32u16.to_ne_bytes());
    assert!(DecodedEntry::decode(&bytes).is_err());
}

#[def_test]
fn unsupported_extensions_revisions_and_flags_are_rejected() {
    for (offset, value) in [
        (143, 1),
        (191, 1),
        (152, 1),
        (155, 0x80),
        (154, 0x80),
        (149, 255),
    ] {
        let mut bytes = tcp_entry();
        bytes[offset] = value;
        assert!(DecodedEntry::decode(&bytes).is_err());
    }
    for extension in [b"unknown".as_slice(), b"TCP", b"ERROR", b""] {
        assert!(DecodedEntry::decode(&transport(6, extension, &[0; 12])).is_err());
    }
    let mut target = standard(-2);
    target[114..118].copy_from_slice(b"icmp");
    assert!(DecodedEntry::decode(&target).is_err());
    let mut bytes = tcp_entry();
    bytes[114..143].fill(b'x');
    assert!(DecodedEntry::decode(&bytes).is_err());
    let mut bytes = standard(-2);
    bytes[114..143].fill(b'x');
    assert!(DecodedEntry::decode(&bytes).is_err());
    let mut udp = transport(17, b"udp", &[0; 10]);
    udp[152] = 4;
    assert!(DecodedEntry::decode(&udp).is_err());
}

#[def_test]
fn extension_limit_and_rule_protocol_validation_stay_separate() {
    let base = BaseMatch {
        protocol: Some(Negated::new(6, false)),
        ..Default::default()
    };
    for count in [16, 17] {
        let extensions = vec![ExtensionMatch::Tcp(TcpMatch::new()); count];
        let mut bytes = vec![0; encode::entry_size(&extensions).unwrap()];
        encode::encode_entry(&mut bytes, &base, &extensions, -2).unwrap();
        assert_eq!(DecodedEntry::decode(&bytes).is_ok(), count == 16);
    }
    let mut bytes = tcp_entry();
    bytes[80..82].copy_from_slice(&17u16.to_ne_bytes());
    let decoded = DecodedEntry::decode(&bytes).unwrap();
    assert!(Rule::new(decoded.base, decoded.extensions, Target::Return).is_err());
}
