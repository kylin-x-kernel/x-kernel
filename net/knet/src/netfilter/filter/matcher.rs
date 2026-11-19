// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validated match conditions and their IPv4 packet evaluation.

use alloc::string::String;

use etherparse::{Icmpv4Header, TcpHeader, UdpHeaderSlice};

use super::packet::{MatchContext, MatchOutcome};
use crate::{
    ip::{IpAddress, Ipv4Cidr},
    ipv4::{PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
};

/// A condition value with an independently controlled logical inversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Negated<T> {
    value: T,
    is_inverted: bool,
}

impl<T> Negated<T> {
    pub(crate) const fn new(value: T, is_inverted: bool) -> Self {
        Self { value, is_inverted }
    }

    pub(crate) const fn value(&self) -> &T {
        &self.value
    }

    pub(crate) const fn is_inverted(&self) -> bool {
        self.is_inverted
    }

    fn matches(&self, match_fn: impl FnOnce(&T) -> bool) -> bool {
        match_fn(&self.value) ^ self.is_inverted
    }
}

/// IPv4 conditions shared by all transport protocols.
///
/// Absent fields impose no condition; populated fields are ANDed together.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BaseMatch {
    pub(crate) source: Option<Negated<Ipv4Cidr>>,
    pub(crate) destination: Option<Negated<Ipv4Cidr>>,
    pub(crate) input_interface: Option<Negated<InterfacePattern>>,
    pub(crate) output_interface: Option<Negated<InterfacePattern>>,
    pub(crate) protocol: Option<Negated<u8>>,
}

impl BaseMatch {
    /// Checks all populated conditions with their independent inversions.
    ///
    /// Returns `Matched` or `NotMatched`; base conditions cannot request a hot-drop.
    pub(super) fn evaluate(&self, packet: &MatchContext<'_>) -> MatchOutcome {
        MatchOutcome::from(
            self.source.as_ref().is_none_or(|condition| {
                condition.matches(|cidr| {
                    cidr.contains_addr(&IpAddress::Ipv4(packet.header().src_addr()))
                })
            }) && self.destination.as_ref().is_none_or(|condition| {
                condition.matches(|cidr| {
                    cidr.contains_addr(&IpAddress::Ipv4(packet.header().dst_addr()))
                })
            }) && self.input_interface.as_ref().is_none_or(|condition| {
                condition.matches(|pattern| pattern.matches(packet.input_interface()))
            }) && self.output_interface.as_ref().is_none_or(|condition| {
                condition.matches(|pattern| pattern.matches(packet.output_interface()))
            }) && self.protocol.as_ref().is_none_or(|condition| {
                condition.matches(|protocol| packet.header().protocol() == *protocol)
            }),
        )
    }
}

/// A closed inclusive transport-port range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    pub(crate) const fn new(start: u16, end: u16) -> Option<Self> {
        if start <= end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub(crate) const fn contains(self, port: u16) -> bool {
        self.start <= port && port <= self.end
    }

    pub(in crate::netfilter) const fn start(self) -> u16 {
        self.start
    }

    pub(in crate::netfilter) const fn end(self) -> u16 {
        self.end
    }
}

/// Maximum interface-name length in bytes, excluding the trailing NUL.
pub(crate) const MAX_INTERFACE_NAME_LEN: usize = 15;

/// Exact or prefix interface-name condition.
///
/// Name lengths follow legacy iptables command-line interface syntax.
/// An empty exact name matches only an empty interface name;
/// an empty prefix matches any interface name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InterfacePattern {
    name: String,
    is_prefix: bool,
}

impl InterfacePattern {
    /// Creates an exact condition, rejecting names longer than 15 bytes.
    ///
    /// Callers must bound name allocation before transferring ownership here.
    pub(crate) fn exact(name: String) -> Option<Self> {
        Self::new(name, false)
    }

    /// Creates a prefix condition from a name without the trailing `+`.
    ///
    /// The prefix is limited to 14 bytes: iptables' `xtables_parse_interface`
    /// in `libxtables/xtables.c` fits the name, `+`, and NUL into `IFNAMSIZ`.
    /// Callers must bound name allocation before transferring ownership here.
    pub(crate) fn prefix(name: String) -> Option<Self> {
        Self::new(name, true)
    }

    fn new(name: String, is_prefix: bool) -> Option<Self> {
        let max_len = MAX_INTERFACE_NAME_LEN - usize::from(is_prefix);
        (name.len() <= max_len).then_some(Self { name, is_prefix })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn is_prefix(&self) -> bool {
        self.is_prefix
    }

    fn matches(&self, device_name: &str) -> bool {
        if self.is_prefix() {
            device_name.starts_with(self.name())
        } else {
            device_name == self.name()
        }
    }
}

/// TCP flag mask and expected value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TcpFlagsCondition {
    mask: u8,
    expected: u8,
}

impl TcpFlagsCondition {
    pub(crate) const fn new(mask: u8, expected: u8) -> Option<Self> {
        if expected & !mask == 0 {
            Some(Self { mask, expected })
        } else {
            None
        }
    }

    pub(crate) const fn mask(self) -> u8 {
        self.mask
    }

    pub(crate) const fn expected(self) -> u8 {
        self.expected
    }
}

/// TCP conditions, ANDed after applying each populated field's inversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TcpMatch {
    source_port: Option<Negated<PortRange>>,
    destination_port: Option<Negated<PortRange>>,
    flags: Option<Negated<TcpFlagsCondition>>,
}

impl TcpMatch {
    pub(crate) const fn new() -> Self {
        Self {
            source_port: None,
            destination_port: None,
            flags: None,
        }
    }

    pub(crate) const fn with_source_port(mut self, range: Negated<PortRange>) -> Self {
        self.source_port = Some(range);
        self
    }

    pub(crate) const fn with_destination_port(mut self, range: Negated<PortRange>) -> Self {
        self.destination_port = Some(range);
        self
    }

    pub(crate) const fn with_flags(mut self, flags: Negated<TcpFlagsCondition>) -> Self {
        self.flags = Some(flags);
        self
    }

    pub(crate) const fn source_port_range(self) -> Option<Negated<PortRange>> {
        self.source_port
    }

    pub(crate) const fn destination_port_range(self) -> Option<Negated<PortRange>> {
        self.destination_port
    }

    pub(crate) const fn flags_condition(self) -> Option<Negated<TcpFlagsCondition>> {
        self.flags
    }

    /// Checks the fixed TCP header, hot-dropping short headers and offset-eight fragments.
    fn evaluate(&self, packet: &MatchContext<'_>) -> MatchOutcome {
        if packet.header().fragment_offset() != 0 {
            // Linux 6.8 xt_tcpudp.c::tcp_mt hot-drops TCP fragments at offset 1.
            return if packet.header().fragment_offset() == 8 {
                MatchOutcome::HotDrop
            } else {
                MatchOutcome::NotMatched
            };
        }
        // Port and flag matching needs only the fixed header, not TCP options.
        let Some(header) = packet.payload().get(..TcpHeader::MIN_LEN) else {
            return MatchOutcome::HotDrop;
        };
        MatchOutcome::from(
            self.source_port_range().is_none_or(|range| {
                range.matches(|range| range.contains(u16::from_be_bytes([header[0], header[1]])))
            }) && self.destination_port_range().is_none_or(|range| {
                range.matches(|range| range.contains(u16::from_be_bytes([header[2], header[3]])))
            }) && self.flags_condition().is_none_or(|flags| {
                flags.matches(|flags| header[13] & flags.mask() == flags.expected())
            }),
        )
    }
}

/// UDP conditions, ANDed after applying each populated field's inversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct UdpMatch {
    source_port: Option<Negated<PortRange>>,
    destination_port: Option<Negated<PortRange>>,
}

impl UdpMatch {
    pub(crate) const fn new() -> Self {
        Self {
            source_port: None,
            destination_port: None,
        }
    }

    pub(crate) const fn with_source_port(mut self, range: Negated<PortRange>) -> Self {
        self.source_port = Some(range);
        self
    }

    pub(crate) const fn with_destination_port(mut self, range: Negated<PortRange>) -> Self {
        self.destination_port = Some(range);
        self
    }

    pub(crate) const fn source_port_range(self) -> Option<Negated<PortRange>> {
        self.source_port
    }

    pub(crate) const fn destination_port_range(self) -> Option<Negated<PortRange>> {
        self.destination_port
    }

    /// Checks UDP ports, rejecting later fragments and hot-dropping short headers.
    fn evaluate(&self, packet: &MatchContext<'_>) -> MatchOutcome {
        if packet.header().fragment_offset() != 0 {
            return MatchOutcome::NotMatched;
        }
        let Ok(header) = UdpHeaderSlice::from_slice(packet.payload()) else {
            return MatchOutcome::HotDrop;
        };
        MatchOutcome::from(
            self.source_port_range()
                .is_none_or(|range| range.matches(|range| range.contains(header.source_port())))
                && self.destination_port_range().is_none_or(|range| {
                    range.matches(|range| range.contains(header.destination_port()))
                }),
        )
    }
}

/// ICMP type and inclusive code range with joint inversion.
///
/// An absent type matches any type and code. The ignored code range is retained
/// so rules can be exported with their original match data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IcmpMatch {
    icmp_type: Option<u8>,
    code_start: u8,
    code_end: u8,
    is_inverted: bool,
}

impl IcmpMatch {
    pub(crate) const fn new(icmp_type: Option<u8>, code_start: u8, code_end: u8) -> Option<Self> {
        if icmp_type.is_none() || code_start <= code_end {
            Some(Self {
                icmp_type,
                code_start,
                code_end,
                is_inverted: false,
            })
        } else {
            None
        }
    }

    pub(crate) const fn icmp_type(self) -> Option<u8> {
        self.icmp_type
    }

    /// Inverts the combined type and code condition.
    pub(crate) const fn with_inversion(mut self, is_inverted: bool) -> Self {
        self.is_inverted = is_inverted;
        self
    }

    pub(crate) const fn code_start(self) -> u8 {
        self.code_start
    }

    pub(crate) const fn code_end(self) -> u8 {
        self.code_end
    }

    pub(in crate::netfilter) const fn is_inverted(self) -> bool {
        self.is_inverted
    }

    /// Checks ICMP type and code with joint inversion, hot-dropping short headers.
    fn evaluate(&self, packet: &MatchContext<'_>) -> MatchOutcome {
        if packet.header().fragment_offset() != 0 {
            return MatchOutcome::NotMatched;
        }
        // Linux's ICMP match reads the base header even for timestamp messages.
        let Some(header) = packet.payload().get(..Icmpv4Header::MIN_LEN) else {
            return MatchOutcome::HotDrop;
        };
        MatchOutcome::from(
            self.icmp_type().is_none_or(|expected| {
                expected == header[0]
                    && header[1] >= self.code_start()
                    && header[1] <= self.code_end()
            }) ^ self.is_inverted(),
        )
    }
}

/// Transport-specific rule conditions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionMatch {
    Tcp(TcpMatch),
    Udp(UdpMatch),
    Icmp(IcmpMatch),
}

impl ExtensionMatch {
    /// Evaluates the selected transport-specific condition.
    pub(super) fn evaluate(&self, packet: &MatchContext<'_>) -> MatchOutcome {
        match self {
            Self::Tcp(spec) => spec.evaluate(packet),
            Self::Udp(spec) => spec.evaluate(packet),
            Self::Icmp(spec) => spec.evaluate(packet),
        }
    }

    pub(super) const fn required_protocol(&self) -> u8 {
        match self {
            Self::Tcp(_) => PROTOCOL_TCP,
            Self::Udp(_) => PROTOCOL_UDP,
            Self::Icmp(_) => PROTOCOL_ICMP,
        }
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/netfilter/filter/matcher/tests.rs");
