// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, vec, vec::Vec};

use kerrno::{KError, LinuxError};
use unittest::def_test;

use super::*;
use crate::{
    ip::{Ipv4Address, Ipv4Cidr},
    netfilter::filter::matcher::{IcmpMatch, TcpFlagsCondition, TcpMatch, UdpMatch},
};

fn standard_entry(verdict: i32) -> Vec<u8> {
    let mut expected = vec![0; 152];
    expected[88..90].copy_from_slice(&112u16.to_ne_bytes());
    expected[90..92].copy_from_slice(&152u16.to_ne_bytes());
    expected[112..114].copy_from_slice(&40u16.to_ne_bytes());
    expected[144..148].copy_from_slice(&verdict.to_ne_bytes());
    expected
}

#[def_test]
fn standard_and_error_entries_match_native_layout() {
    for verdict in [-1, -2, -5, 304] {
        let mut output = vec![0xa5; 152];
        encode_entry(&mut output, &BaseMatch::default(), &[], verdict).unwrap();
        assert_eq!(output, standard_entry(verdict));
    }
    for name in ["WEB", "ERROR"] {
        let mut output = vec![0xa5; 176];
        encode_error_entry(&mut output, name).unwrap();
        let mut expected = vec![0; 176];
        expected[88..90].copy_from_slice(&112u16.to_ne_bytes());
        expected[90..92].copy_from_slice(&176u16.to_ne_bytes());
        expected[112..114].copy_from_slice(&64u16.to_ne_bytes());
        expected[114..119].copy_from_slice(b"ERROR");
        expected[144..144 + name.len()].copy_from_slice(name.as_bytes());
        assert_eq!(output, expected);
    }
}

#[def_test]
fn base_conditions_preserve_networks_interface_masks_and_inversions() {
    let base = BaseMatch {
        source: Some(Negated::new(
            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 129), 24).unwrap(),
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
    let mut output = vec![0xa5; 152];
    encode_entry(&mut output, &base, &[], -1).unwrap();
    let mut expected = standard_entry(-1);
    expected[..4].copy_from_slice(&[192, 0, 2, 0]);
    expected[4..8].copy_from_slice(&[198, 51, 100, 7]);
    expected[8..12].copy_from_slice(&[255, 255, 255, 0]);
    expected[12..16].fill(255);
    expected[16..20].copy_from_slice(b"eth0");
    expected[32..36].copy_from_slice(b"eth+");
    expected[48..53].fill(255);
    expected[64..67].fill(255);
    expected[80..82].copy_from_slice(&17u16.to_ne_bytes());
    expected[83] = 0x49;
    assert_eq!(output, expected);
}

#[def_test]
fn transport_matches_follow_native_extension_layouts() {
    let tcp = TcpMatch::new()
        .with_source_port(Negated::new(PortRange::new(1234, 5678).unwrap(), true))
        .with_destination_port(Negated::new(PortRange::new(80, 443).unwrap(), false))
        .with_flags(Negated::new(
            TcpFlagsCondition::new(0x12, 0x02).unwrap(),
            true,
        ));
    let udp =
        UdpMatch::new().with_destination_port(Negated::new(PortRange::new(53, 53).unwrap(), true));
    let icmp = IcmpMatch::new(Some(3), 1, 4).unwrap().with_inversion(true);
    let mut tcp_data = vec![0; 16];
    for (offset, value) in [(0, 1234u16), (2, 5678), (4, 80), (6, 443)] {
        tcp_data[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
    }
    tcp_data[9..12].copy_from_slice(&[0x12, 0x02, 0x05]);
    let mut udp_data = vec![0; 16];
    for (offset, value) in [(2, u16::MAX), (4, 53), (6, 53)] {
        udp_data[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
    }
    udp_data[8] = 2;
    for (protocol, extension, name, data) in [
        (6, ExtensionMatch::Tcp(tcp), "tcp", tcp_data),
        (17, ExtensionMatch::Udp(udp), "udp", udp_data),
        (
            1,
            ExtensionMatch::Icmp(icmp),
            "icmp",
            vec![3, 1, 4, 1, 0, 0, 0, 0],
        ),
    ] {
        let extensions = [extension];
        let base = BaseMatch {
            protocol: Some(Negated::new(protocol, false)),
            ..Default::default()
        };
        let mut output = vec![0xa5; entry_size(&extensions).unwrap()];
        encode_entry(&mut output, &base, &extensions, -2).unwrap();
        let mut expected = vec![0; 32];
        expected[..2].copy_from_slice(&((32 + data.len()) as u16).to_ne_bytes());
        expected[2..2 + name.len()].copy_from_slice(name.as_bytes());
        expected.extend_from_slice(&data);
        assert_eq!(&output[112..112 + expected.len()], expected);
        assert_eq!(
            u16::from_ne_bytes(output[88..90].try_into().unwrap()) as usize,
            112 + expected.len()
        );
        assert_eq!(
            u16::from_ne_bytes(output[90..92].try_into().unwrap()) as usize,
            output.len()
        );
        assert_eq!(
            &output[output.len() - 8..output.len() - 4],
            &(-2i32).to_ne_bytes()
        );
    }
}

#[def_test]
fn encoding_rejects_conditions_without_equivalent_legacy_semantics() {
    let mut output = vec![0; 192];
    let base = BaseMatch {
        protocol: Some(Negated::new(1, false)),
        ..Default::default()
    };
    for (icmp, accepted) in [
        (IcmpMatch::new(None, 0, 255).unwrap(), true),
        (
            IcmpMatch::new(None, 0, 255).unwrap().with_inversion(true),
            true,
        ),
        (IcmpMatch::new(None, 255, 255).unwrap(), true),
        (IcmpMatch::new(None, 1, 4).unwrap(), true),
        (IcmpMatch::new(Some(255), 0, 255).unwrap(), false),
    ] {
        let result = encode_entry(&mut output, &base, &[ExtensionMatch::Icmp(icmp)], -2);
        if accepted {
            assert_eq!(result, Ok(()));
            assert_eq!(
                &output[144..148],
                &[255, icmp.code_start(), icmp.code_end(), u8::from(icmp.is_inverted())]
            );
        } else {
            assert_eq!(result, Err(EncodeError::UnsupportedMatch));
        }
    }
    for inverted in [false, true] {
        let base = BaseMatch {
            protocol: Some(Negated::new(0, inverted)),
            ..Default::default()
        };
        assert_eq!(
            encode_entry(&mut output[..152], &base, &[], -2),
            Err(EncodeError::UnsupportedMatch)
        );
    }
    let base = BaseMatch {
        input_interface: Some(Negated::new(
            InterfacePattern::exact("eth\0x".to_string()).unwrap(),
            false,
        )),
        ..Default::default()
    };
    assert_eq!(
        encode_entry(&mut output[..152], &base, &[], -2),
        Err(EncodeError::UnsupportedMatch)
    );
    for name in ["", "bad\0name", &"a".repeat(30)] {
        assert_eq!(
            encode_error_entry(&mut output[..176], name),
            Err(EncodeError::InvalidName)
        );
    }
}

#[def_test]
fn entry_sizes_bound_offsets_and_output_writes() {
    let extensions = vec![ExtensionMatch::Tcp(TcpMatch::new()); 16];
    assert_eq!(entry_size(&extensions), Ok(920));
    let mut output = vec![0xa5; 922];
    let base = BaseMatch {
        protocol: Some(Negated::new(6, false)),
        ..Default::default()
    };
    encode_entry(&mut output[1..921], &base, &extensions, -2).unwrap();
    assert_eq!(output[0], 0xa5);
    assert_eq!(output[921], 0xa5);
    let error = encode_entry(&mut output[1..920], &base, &extensions, -2).unwrap_err();
    assert_eq!(error, EncodeError::BufferSize);
    assert_eq!(KError::from(error), LinuxError::EIO.into());
    assert_eq!(
        entry_size(&vec![ExtensionMatch::Tcp(TcpMatch::new()); 1363]),
        Err(EncodeError::EntryTooLarge)
    );
}

#[def_test]
fn fixed_headers_reject_unrepresentable_fields() {
    let max_offset = usize::from(u16::MAX);
    let entry = IptEntry::new(IptIp::default(), max_offset, max_offset).unwrap();
    assert_eq!(entry.target_offset, u16::MAX);
    assert_eq!(entry.next_offset, u16::MAX);
    for (target_offset, next_offset) in [(max_offset + 1, max_offset), (max_offset, max_offset + 1)] {
        assert!(matches!(
            IptEntry::new(IptIp::default(), target_offset, next_offset),
            Err(EncodeError::EntryTooLarge)
        ));
    }
    assert!(matches!(
        XtEntryHeader::new(max_offset + 1, b"tcp"),
        Err(EncodeError::EntryTooLarge)
    ));
    let name_capacity = XtEntryHeader::default().name.len();
    let name = vec![b'x'; name_capacity];
    assert!(matches!(
        XtEntryHeader::new(XT_TCP_MATCH_SIZE, &name),
        Err(EncodeError::InvalidName)
    ));
    let header = XtEntryHeader::new(max_offset, &name[..name_capacity - 1]).unwrap();
    assert_eq!(header.size, u16::MAX);
    assert_eq!(header.name[name_capacity - 1], 0);
}
