// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 unicast forwarding validation, TTL handling, and fragmentation.

use alloc::{vec, vec::Vec};

use super::process::{self, Icmpv4Error, Ipv4FragmentError, Ipv4Header};
use crate::{
    buf::{ChecksumState, PacketBuf, PacketOwner, PacketType},
    ip::Ipv4Address,
};

/// A validated, forwardable datagram or fragment awaiting route selection.
#[derive(Debug)]
pub(crate) struct Ipv4ForwardInput(PacketBuf);

/// A rejected packet retained for quoting before any TTL modification.
#[derive(Debug)]
pub(crate) struct Ipv4ForwardFailure {
    packet: PacketBuf,
    icmp_error: Option<Icmpv4Error>,
}

impl Ipv4ForwardFailure {
    /// Returns the reply destination only when the failure permits an ICMP error.
    pub(crate) fn reply_destination(&self) -> Option<Ipv4Address> {
        self.icmp_error?;
        Some(
            Ipv4Header::parse_header_only(
                self.packet.network_packet().expect("validated IPv4 packet"),
            )
            .expect("validated IPv4 header")
            .src_addr(),
        )
    }

    /// Builds a permitted ICMP error with a route-selected local source address.
    pub(crate) fn icmp_reply(&self, source: Ipv4Address) -> Option<Vec<u8>> {
        process::build_icmpv4_error_packet_from(
            self.icmp_error?,
            self.packet.packet_type(),
            self.packet.network_packet().expect("validated IPv4 packet"),
            source,
        )
    }
}

/// A datagram ready for output fragmentation; TTL has been reduced exactly once.
#[derive(Debug)]
pub(crate) struct PreparedIpv4Forward {
    packet: PacketBuf,
    mtu: u16,
}

impl Ipv4ForwardInput {
    /// Admits forwarding after ingress has normalized and validated IPv4 input.
    /// Invalid endpoints, unsupported options, and malformed fragments are dropped.
    pub(crate) fn from_validated(packet: PacketBuf) -> Option<Self> {
        let bytes = packet.network_packet()?;
        let header = Ipv4Header::parse_header_only(bytes).ok()?;
        let source = header.src_addr();
        let destination = header.dst_addr();
        if packet.packet_type() != PacketType::Host
            || header.header_len() != process::IPV4_MIN_HEADER_LEN
            || source.octets()[0] == 0
            || source.is_loopback()
            || source.is_multicast()
            || source.is_broadcast()
            || destination.octets()[0] == 0
            || destination.is_loopback()
            || header.is_broadcast_or_multicast()
            || header.fragment_offset() + header.total_len() > usize::from(u16::MAX)
            || (header.more_fragments()
                && !(header.total_len() - header.header_len()).is_multiple_of(8))
        {
            return None;
        }
        Some(Self(packet))
    }

    pub(crate) fn destination(&self) -> Ipv4Address {
        Ipv4Header::parse_header_only(self.0.network_packet().expect("validated IPv4 packet"))
            .expect("validated IPv4 header")
            .dst_addr()
    }

    pub(crate) fn discard(self) -> Ipv4ForwardFailure {
        Ipv4ForwardFailure {
            packet: self.0,
            icmp_error: None,
        }
    }

    pub(crate) fn no_route(self) -> Ipv4ForwardFailure {
        Ipv4ForwardFailure {
            packet: self.0,
            icmp_error: Some(Icmpv4Error::NetworkUnreachable),
        }
    }

    pub(crate) fn prepare(self, mtu: u16) -> Result<PreparedIpv4Forward, Ipv4ForwardFailure> {
        let mut packet = self.0;
        let bytes = packet.network_packet().expect("validated IPv4 packet");
        let header = Ipv4Header::parse_header_only(bytes).expect("validated IPv4 header");
        if header.bytes()[8] <= 1 {
            return Err(Ipv4ForwardFailure {
                packet,
                icmp_error: Some(Icmpv4Error::TimeExceeded),
            });
        }
        if bytes.len() > usize::from(mtu) && header.dont_fragment() {
            return Err(Ipv4ForwardFailure {
                packet,
                icmp_error: Some(Icmpv4Error::FragmentationNeeded { mtu }),
            });
        }
        let bytes = packet.network_packet_mut().expect("validated IPv4 packet");
        bytes[8] -= 1;
        // Only TTL changes; preserve the original addresses, ID, and fragment fields.
        process::prepare_output_packet(&mut packet).expect("validated IPv4 packet");
        Ok(PreparedIpv4Forward { packet, mtu })
    }
}

impl PreparedIpv4Forward {
    pub(crate) fn packet(&self) -> &PacketBuf {
        &self.packet
    }

    /// Fragments for the selected MTU while preserving inherited fragment offsets.
    pub(crate) fn into_packets(self) -> Result<Vec<PacketBuf>, Ipv4FragmentError> {
        let mut packet = self.packet;
        packet.set_owner(PacketOwner::DeviceTx);
        if packet
            .network_packet()
            .expect("validated IPv4 packet")
            .len()
            <= usize::from(self.mtu)
        {
            return Ok(vec![packet]);
        }
        let fragments = process::fragment_forward_packet(
            packet.network_packet().expect("validated IPv4 packet"),
            usize::from(self.mtu),
        )?;
        Ok(fragments
            .into_iter()
            .map(|bytes| {
                let mut packet =
                    PacketBuf::from_ip_packet_vec(packet.ifindex(), bytes, PacketOwner::DeviceTx);
                packet.set_transport_offset(process::IPV4_MIN_HEADER_LEN);
                packet.set_checksum_state(ChecksumState::Verified);
                packet
            })
            .collect())
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/network/ipv4/forward.rs");
