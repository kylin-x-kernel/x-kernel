// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validated IPv4 filter rules and their actions.

use alloc::vec::Vec;

use super::{
    chain::ChainId,
    error::RuleError,
    matcher::{BaseMatch, ExtensionMatch},
};

// X-Kernel implementation resource limit.
pub(crate) const MAX_EXTENSION_MATCHES_PER_RULE: usize = 16;

/// A terminal packet-filtering decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Accept,
    Drop,
}

/// Action executed after every match in a rule succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Target {
    Verdict(Verdict),
    Continue,
    Return,
    /// Calls a user chain by its position in this rule's final table.
    Jump(ChainId),
}

/// Fixed IPv4 conditions, ordered extension matches, and their selected action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Rule {
    base: BaseMatch,
    extensions: Vec<ExtensionMatch>,
    target: Target,
}

impl Rule {
    /// Builds a rule whose extensions match its non-inverted base protocol.
    ///
    /// # Errors
    ///
    /// Returns `TooManyExtensionMatches` above the per-rule extension limit,
    /// or a protocol error for a missing, inverted, or incompatible protocol.
    pub(crate) fn new(
        base: BaseMatch,
        extensions: Vec<ExtensionMatch>,
        target: Target,
    ) -> Result<Self, RuleError> {
        if extensions.len() > MAX_EXTENSION_MATCHES_PER_RULE {
            return Err(RuleError::TooManyExtensionMatches);
        }
        validate_extensions(&base, &extensions)?;

        Ok(Self {
            base,
            extensions,
            target,
        })
    }

    pub(crate) const fn base(&self) -> &BaseMatch {
        &self.base
    }

    pub(crate) fn extensions(&self) -> &[ExtensionMatch] {
        &self.extensions
    }

    pub(crate) const fn target(&self) -> Target {
        self.target
    }
}

fn validate_extensions(base: &BaseMatch, extensions: &[ExtensionMatch]) -> Result<(), RuleError> {
    for extension in extensions {
        let required_protocol = extension.required_protocol();
        let Some(protocol) = base.protocol.as_ref() else {
            return Err(RuleError::MissingProtocol { required_protocol });
        };
        let actual_protocol = *protocol.value();
        if protocol.is_inverted() {
            return Err(RuleError::InvertedProtocol {
                required_protocol,
                actual_protocol,
            });
        }
        if actual_protocol != required_protocol {
            return Err(RuleError::ProtocolMismatch {
                required_protocol,
                actual_protocol,
            });
        }
    }

    Ok(())
}

#[unittest::mod_test]
mod tests {
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
}
