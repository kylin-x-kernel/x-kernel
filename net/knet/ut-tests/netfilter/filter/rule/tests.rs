// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::{
    ipv4::{PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
    netfilter::filter::matcher::{IcmpMatch, Negated, TcpMatch, UdpMatch},
};

#[def_test]
fn rule_validates_extensions() {
    let tcp = ExtensionMatch::Tcp(TcpMatch::new());
    for (protocol, error) in [
        (
            None,
            RuleError::MissingProtocol {
                required_protocol: PROTOCOL_TCP,
            },
        ),
        (
            Some(Negated::new(PROTOCOL_TCP, true)),
            RuleError::InvertedProtocol {
                required_protocol: PROTOCOL_TCP,
                actual_protocol: PROTOCOL_TCP,
            },
        ),
        (
            Some(Negated::new(PROTOCOL_UDP, false)),
            RuleError::ProtocolMismatch {
                required_protocol: PROTOCOL_TCP,
                actual_protocol: PROTOCOL_UDP,
            },
        ),
    ] {
        assert_eq!(
            Rule::new(
                BaseMatch {
                    protocol,
                    ..BaseMatch::default()
                },
                vec![tcp.clone()],
                Target::Continue
            ),
            Err(error)
        );
    }
    for (protocol, extension) in [
        (PROTOCOL_TCP, tcp.clone()),
        (PROTOCOL_UDP, ExtensionMatch::Udp(UdpMatch::new())),
        (
            PROTOCOL_ICMP,
            ExtensionMatch::Icmp(IcmpMatch::new(None, 0, u8::MAX).unwrap()),
        ),
    ] {
        assert!(
            Rule::new(
                BaseMatch {
                    protocol: Some(Negated::new(protocol, false)),
                    ..BaseMatch::default()
                },
                vec![extension],
                Target::Continue,
            )
            .is_ok()
        );
    }
    let base = BaseMatch {
        protocol: Some(Negated::new(PROTOCOL_TCP, false)),
        ..BaseMatch::default()
    };
    let mut extensions = vec![tcp.clone(); MAX_EXTENSION_MATCHES_PER_RULE];
    assert!(Rule::new(base.clone(), extensions.clone(), Target::Continue).is_ok());
    extensions.push(tcp);
    assert_eq!(
        Rule::new(base, extensions, Target::Continue),
        Err(RuleError::TooManyExtensionMatches)
    );
}
