// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, vec, vec::Vec};

use etherparse::{IpFragOffset, Ipv4Header as EtherIpv4Header};
use unittest::def_test;

use super::{
    chain::{Chain, ChainId, FilterHook},
    matcher::{
        BaseMatch, ExtensionMatch, IcmpMatch, InterfacePattern, Negated, PortRange,
        TcpFlagsCondition, TcpMatch, UdpMatch,
    },
    packet::{MatchContext, MatchOutcome},
    rule::{Rule, Target, Verdict},
    table::{FilterTable, MAX_CHAINS, MAX_RULE_STEPS},
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    ip::{Ipv4Address, Ipv4Cidr},
    ipv4::{self, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
};

fn packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
    ipv4::build_ipv4_packet(
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        protocol,
        64,
        payload,
    )
    .unwrap()
}

fn validated_packet(bytes: &[u8]) -> PacketBuf {
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes.to_vec(), PacketOwner::Ipv4Stack);
    ipv4::validate_input_packet(&mut packet).unwrap();
    packet
}

fn rule(target: Target) -> Rule {
    Rule::new(BaseMatch::default(), Vec::new(), target).unwrap()
}

fn transport_rule(extension: ExtensionMatch) -> Rule {
    Rule::new(
        BaseMatch {
            protocol: Some(Negated::new(extension.required_protocol(), false)),
            ..BaseMatch::default()
        },
        vec![extension],
        Target::Continue,
    )
    .unwrap()
}

fn chains(input: Vec<Rule>, policy: Verdict) -> Vec<Chain> {
    vec![
        Chain::built_in(FilterHook::LocalIn, policy, input),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
    ]
}

fn run(table: &FilterTable, bytes: &[u8]) -> (Verdict, Vec<(usize, Option<usize>)>) {
    let buffer = validated_packet(bytes);
    let mut events = Vec::new();
    let verdict = table
        .evaluate_with_observer(
            FilterHook::LocalIn,
            &buffer,
            "eth0",
            "eth1",
            |chain, rule| {
                events.push((chain.index(), rule));
            },
        )
        .unwrap();
    (verdict, events)
}

#[def_test]
fn nested_returns_resume_after_jumps_and_record_only_executed_targets() {
    let mut input = chains(
        vec![
            rule(Target::Jump(ChainId::from_index(3))),
            rule(Target::Continue),
            rule(Target::Verdict(Verdict::Accept)),
            rule(Target::Verdict(Verdict::Drop)),
        ],
        Verdict::Drop,
    );
    input.push(
        Chain::user_defined(
            "outer".to_string(),
            vec![
                rule(Target::Jump(ChainId::from_index(4))),
                rule(Target::Return),
                rule(Target::Verdict(Verdict::Drop)),
            ],
        )
        .unwrap(),
    );
    input.push(Chain::user_defined("inner".to_string(), Vec::new()).unwrap());
    let table = FilterTable::new(input).unwrap();
    assert_eq!(
        run(&table, &packet(PROTOCOL_UDP, &[])),
        (
            Verdict::Accept,
            vec![
                (0, Some(0)),
                (3, Some(0)),
                (4, None),
                (3, Some(1)),
                (0, Some(1)),
                (0, Some(2))
            ]
        )
    );
}

#[def_test]
fn builtin_return_and_fallthrough_record_policy_after_matching_rules() {
    for rules in [Vec::new(), vec![rule(Target::Return)]] {
        let explicit_return = !rules.is_empty();
        let table = FilterTable::new(chains(rules, Verdict::Drop)).unwrap();
        let (verdict, events) = run(&table, &packet(PROTOCOL_UDP, &[]));
        assert_eq!(verdict, Verdict::Drop);
        assert_eq!(
            events,
            if explicit_return {
                vec![(0, Some(0)), (0, None)]
            } else {
                vec![(0, None)]
            }
        );
        let bytes = packet(PROTOCOL_UDP, &[]);
        let buffer = validated_packet(&bytes);
        for hook in [FilterHook::Forward, FilterHook::LocalOut] {
            assert_eq!(
                table.evaluate_with_observer(hook, &buffer, "", "", |_, _| {}),
                Ok(Verdict::Accept)
            );
        }
    }
}

#[def_test]
fn base_conditions_apply_independent_inversion_before_extensions() {
    let bytes = packet(PROTOCOL_TCP, &[]);
    let buffer = validated_packet(&bytes);
    let packet = MatchContext::new(&buffer, "eth0", "eth1").unwrap();
    for is_inverted in [false, true] {
        for is_matched in [false, true] {
            let conditions = [
                BaseMatch {
                    source: Some(Negated::new(
                        Ipv4Cidr::new(
                            Ipv4Address::new(192, 0, if is_matched { 2 } else { 3 }, 0),
                            24,
                        )
                        .unwrap(),
                        is_inverted,
                    )),
                    ..BaseMatch::default()
                },
                BaseMatch {
                    destination: Some(Negated::new(
                        Ipv4Cidr::new(
                            Ipv4Address::new(192, 0, 2, if is_matched { 2 } else { 3 }),
                            32,
                        )
                        .unwrap(),
                        is_inverted,
                    )),
                    ..BaseMatch::default()
                },
                BaseMatch {
                    input_interface: Some(Negated::new(
                        InterfacePattern::prefix(if is_matched { "eth" } else { "lo" }.to_string())
                            .unwrap(),
                        is_inverted,
                    )),
                    ..BaseMatch::default()
                },
                BaseMatch {
                    output_interface: Some(Negated::new(
                        InterfacePattern::exact(
                            if is_matched { "eth1" } else { "eth" }.to_string(),
                        )
                        .unwrap(),
                        is_inverted,
                    )),
                    ..BaseMatch::default()
                },
                BaseMatch {
                    protocol: Some(Negated::new(
                        if is_matched {
                            PROTOCOL_TCP
                        } else {
                            PROTOCOL_UDP
                        },
                        is_inverted,
                    )),
                    ..BaseMatch::default()
                },
            ];
            for base in conditions {
                let rule = Rule::new(base, Vec::new(), Target::Continue).unwrap();
                assert_eq!(
                    rule.evaluate_match(&packet),
                    if is_matched ^ is_inverted {
                        MatchOutcome::Matched
                    } else {
                        MatchOutcome::NotMatched
                    }
                );
            }
        }
    }
    let rule = Rule::new(
        BaseMatch {
            source: Some(Negated::new(
                Ipv4Cidr::new(Ipv4Address::new(192, 0, 3, 0), 24).unwrap(),
                false,
            )),
            protocol: Some(Negated::new(PROTOCOL_TCP, false)),
            ..BaseMatch::default()
        },
        vec![ExtensionMatch::Tcp(TcpMatch::new())],
        Target::Continue,
    )
    .unwrap();
    assert_eq!(rule.evaluate_match(&packet), MatchOutcome::NotMatched);
}

#[def_test]
fn tcp_ports_and_flags_honor_inversion_without_validating_options() {
    for inversion in 0..8 {
        let rule = transport_rule(ExtensionMatch::Tcp(
            TcpMatch::new()
                .with_source_port(Negated::new(
                    PortRange::new(1000, 2000).unwrap(),
                    inversion & 1 != 0,
                ))
                .with_destination_port(Negated::new(
                    PortRange::new(80, 80).unwrap(),
                    inversion & 2 != 0,
                ))
                .with_flags(Negated::new(
                    TcpFlagsCondition::new(0x12, 0x02).unwrap(),
                    inversion & 4 != 0,
                )),
        ));
        for (source, destination, flags) in
            [(1000u16, 80u16, 0x02), (2000, 443, 0x12), (999, 81, 0)]
        {
            let mut payload = [0u8; 20];
            payload[..2].copy_from_slice(&source.to_be_bytes());
            payload[2..4].copy_from_slice(&destination.to_be_bytes());
            payload[13] = flags;
            let is_matched = ((1000..=2000).contains(&source) ^ (inversion & 1 != 0))
                && ((destination == 80) ^ (inversion & 2 != 0))
                && ((flags & 0x12 == 0x02) ^ (inversion & 4 != 0));
            for data_offset in [0, 5, 15] {
                payload[12] = data_offset << 4;
                let bytes = packet(PROTOCOL_TCP, &payload);
                let buffer = validated_packet(&bytes);
                let packet = MatchContext::new(&buffer, "", "").unwrap();
                assert_eq!(
                    rule.evaluate_match(&packet),
                    if is_matched {
                        MatchOutcome::Matched
                    } else {
                        MatchOutcome::NotMatched
                    }
                );
            }
        }
    }
}

#[def_test]
fn udp_and_icmp_apply_port_and_type_code_conditions() {
    let udp = transport_rule(ExtensionMatch::Udp(
        UdpMatch::new()
            .with_source_port(Negated::new(PortRange::new(53, 54).unwrap(), true))
            .with_destination_port(Negated::new(PortRange::new(1000, 2000).unwrap(), false)),
    ));
    for (source, destination, expected) in [
        (53u16, 1000u16, false),
        (55, 1000, true),
        (55, 2000, true),
        (55, 2001, false),
    ] {
        let mut payload = [0u8; 8];
        payload[..2].copy_from_slice(&source.to_be_bytes());
        payload[2..4].copy_from_slice(&destination.to_be_bytes());
        let bytes = packet(PROTOCOL_UDP, &payload);
        let buffer = validated_packet(&bytes);
        let packet = MatchContext::new(&buffer, "", "").unwrap();
        assert_eq!(
            udp.evaluate_match(&packet),
            if expected {
                MatchOutcome::Matched
            } else {
                MatchOutcome::NotMatched
            }
        );
    }
    for is_inverted in [false, true] {
        for code in [[0, 255], [255, 255], [9, 1]] {
            let icmp = transport_rule(ExtensionMatch::Icmp(
                IcmpMatch::new(None, code[0], code[1])
                    .unwrap()
                    .with_inversion(is_inverted),
            ));
            let bytes = packet(PROTOCOL_ICMP, &[8, 0, 0, 0, 0, 0, 0, 0]);
            let buffer = validated_packet(&bytes);
            let packet = MatchContext::new(&buffer, "", "").unwrap();
            assert_eq!(
                icmp.evaluate_match(&packet),
                if is_inverted {
                    MatchOutcome::NotMatched
                } else {
                    MatchOutcome::Matched
                }
            );
        }
        let icmp = transport_rule(ExtensionMatch::Icmp(
            IcmpMatch::new(Some(13), 0, 1)
                .unwrap()
                .with_inversion(is_inverted),
        ));
        for (kind, code, expected) in [(13, 0, true), (13, 1, true), (13, 2, false), (3, 0, false)]
        {
            let bytes = packet(PROTOCOL_ICMP, &[kind, code, 0, 0, 0, 0, 0, 0]);
            let buffer = validated_packet(&bytes);
            let packet = MatchContext::new(&buffer, "", "").unwrap();
            assert_eq!(
                icmp.evaluate_match(&packet),
                if expected ^ is_inverted {
                    MatchOutcome::Matched
                } else {
                    MatchOutcome::NotMatched
                }
            );
        }
    }
}

#[def_test]
fn short_transport_headers_hot_drop_without_using_padding_or_counting_rules() {
    for extension in [
        ExtensionMatch::Tcp(TcpMatch::new()),
        ExtensionMatch::Udp(UdpMatch::new()),
        ExtensionMatch::Icmp(IcmpMatch::new(None, 0, 255).unwrap()),
    ] {
        let protocol = extension.required_protocol();
        let bytes = packet(protocol, &[]);
        let rule = transport_rule(extension);
        let table = FilterTable::new(chains(vec![rule], Verdict::Accept)).unwrap();
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 40]);
        let buffer = validated_packet(&padded);
        assert_eq!(buffer.network_packet().unwrap().len(), bytes.len());
        assert_eq!(run(&table, &padded), (Verdict::Drop, Vec::new()));
    }
}

#[def_test]
fn fragments_follow_transport_match_availability() {
    for (extension, offset, expected) in [
        (
            ExtensionMatch::Tcp(TcpMatch::new()),
            1,
            MatchOutcome::HotDrop,
        ),
        (
            ExtensionMatch::Tcp(TcpMatch::new()),
            2,
            MatchOutcome::NotMatched,
        ),
        (
            ExtensionMatch::Udp(UdpMatch::new()),
            1,
            MatchOutcome::NotMatched,
        ),
        (
            ExtensionMatch::Icmp(IcmpMatch::new(None, 0, 255).unwrap()),
            1,
            MatchOutcome::NotMatched,
        ),
    ] {
        let mut bytes = packet(extension.required_protocol(), &[0; 24]);
        let mut header = EtherIpv4Header::from_slice(&bytes).unwrap().0;
        header.fragment_offset = IpFragOffset::try_new(offset).unwrap();
        header.header_checksum = header.calc_header_checksum();
        bytes[..20].copy_from_slice(&header.to_bytes());
        let buffer = validated_packet(&bytes);
        let packet = MatchContext::new(&buffer, "", "").unwrap();
        assert_eq!(transport_rule(extension).evaluate_match(&packet), expected);
        assert_eq!(
            rule(Target::Continue).evaluate_match(&packet),
            MatchOutcome::Matched
        );
    }
}

#[def_test]
fn output_evaluation_requires_preparation_and_accepts_absent_input_interface() {
    let mut bytes = packet(PROTOCOL_UDP, &[0; 8]);
    bytes[2..4].fill(0);
    bytes[10..12].fill(0);
    let packet_len = bytes.len();
    let mut buffer = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx);
    let table = FilterTable::accept_all();
    let mut events = Vec::new();
    assert_eq!(
        table.evaluate_with_observer(FilterHook::LocalOut, &buffer, "", "eth0", |chain, rule| {
            events.push((chain.index(), rule));
        }),
        Err(ipv4::Ipv4Error::Malformed)
    );
    assert!(events.is_empty());
    ipv4::prepare_output_packet(&mut buffer).unwrap();
    assert_eq!(buffer.network_packet().unwrap().len(), packet_len);
    for pattern in [
        InterfacePattern::exact("".to_string()).unwrap(),
        InterfacePattern::prefix("".to_string()).unwrap(),
    ] {
        let rule = Rule::new(
            BaseMatch {
                input_interface: Some(Negated::new(pattern, false)),
                ..BaseMatch::default()
            },
            Vec::new(),
            Target::Verdict(Verdict::Accept),
        )
        .unwrap();
        let table = FilterTable::new(vec![
            Chain::built_in(FilterHook::LocalIn, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::LocalOut, Verdict::Drop, vec![rule]),
        ])
        .unwrap();
        assert_eq!(
            table.evaluate_with_observer(
                FilterHook::LocalOut,
                &buffer,
                "",
                "eth0",
                |chain, rule| {
                    events.push((chain.index(), rule));
                }
            ),
            Ok(Verdict::Accept)
        );
    }
    assert_eq!(events, vec![(2, Some(0)); 2]);
    events.clear();
    buffer.network_packet_mut().unwrap()[8] -= 1;
    assert_eq!(
        table.evaluate_with_observer(FilterHook::LocalOut, &buffer, "", "eth0", |chain, rule| {
            events.push((chain.index(), rule));
        }),
        Err(ipv4::Ipv4Error::Malformed)
    );
    assert!(events.is_empty());
    ipv4::prepare_output_packet(&mut buffer).unwrap();
    assert_eq!(
        table.evaluate_with_observer(FilterHook::LocalOut, &buffer, "", "eth0", |chain, rule| {
            events.push((chain.index(), rule));
        }),
        Ok(Verdict::Accept)
    );
    assert_eq!(events, vec![(2, None)]);
}

#[def_test]
fn validated_maximum_chain_depth_fits_the_return_stack() {
    let mut input = chains(
        vec![rule(Target::Jump(ChainId::from_index(3)))],
        Verdict::Accept,
    );
    for index in 3..MAX_CHAINS {
        let rules = if index + 1 == MAX_CHAINS {
            Vec::new()
        } else {
            vec![rule(Target::Jump(ChainId::from_index(index + 1)))]
        };
        input.push(Chain::user_defined(alloc::format!("chain{index}"), rules).unwrap());
    }
    let table = FilterTable::new(input).unwrap();
    let (verdict, events) = run(&table, &packet(PROTOCOL_UDP, &[]));
    assert_eq!(verdict, Verdict::Accept);
    assert_eq!(events.len(), 2 * (MAX_CHAINS - 3) + 1);
}

#[def_test]
fn repeated_acyclic_jumps_stop_at_the_per_packet_rule_budget() {
    let linear = FilterTable::new(chains(
        (0..MAX_RULE_STEPS)
            .map(|_| rule(Target::Continue))
            .collect(),
        Verdict::Accept,
    ))
    .unwrap();
    let (verdict, events) = run(&linear, &packet(PROTOCOL_UDP, &[]));
    assert_eq!(verdict, Verdict::Accept);
    assert_eq!(events.len(), MAX_RULE_STEPS + 1);
    assert_eq!(events.last(), Some(&(0, None)));
    let mut input = chains(
        vec![rule(Target::Jump(ChainId::from_index(3)))],
        Verdict::Accept,
    );
    for index in 3..16 {
        let rules = if index == 15 {
            vec![rule(Target::Continue)]
        } else {
            (0..2)
                .map(|_| rule(Target::Jump(ChainId::from_index(index + 1))))
                .collect()
        };
        input.push(Chain::user_defined(alloc::format!("chain{index}"), rules).unwrap());
    }
    let table = FilterTable::new(input).unwrap();
    let (verdict, events) = run(&table, &packet(PROTOCOL_UDP, &[]));
    assert_eq!(verdict, Verdict::Drop);
    assert_eq!(
        events.iter().filter(|(_, rule)| rule.is_some()).count(),
        MAX_RULE_STEPS
    );
    assert!(!events.contains(&(0, None)));
}
