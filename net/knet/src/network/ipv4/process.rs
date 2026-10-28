// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 packet validation and ICMPv4 error helpers.

use alloc::{vec, vec::Vec};

use etherparse::{
    Icmpv4Header, Icmpv4Type, IpEcn, IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header,
    Ipv4HeaderSlice,
    icmpv4::{DestUnreachableHeader, TimeExceededCode},
};

use crate::{
    buf::{ChecksumState, PacketBuf, PacketType},
    ip::Ipv4Address,
};

pub(crate) const PROTOCOL_ICMP: u8 = 1;
pub(crate) const PROTOCOL_TCP: u8 = 6;
pub(crate) const PROTOCOL_UDP: u8 = 17;

pub(crate) const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV4_DEFAULT_TTL: u8 = 64;
const ICMPV4_ERROR_QUOTE_BYTES: usize = 8;
// ICMPv4 types through Address Mask Reply have explicit request, reply, or
// error policy below. Higher values are conservatively treated as errors.
const ICMPV4_EXPLICIT_POLICY_MAX_TYPE: u8 = 18;
const IPV4_FRAGMENT_OFFSET_UNIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ipv4HeaderFragment {
    dont_fragment: bool,
    more_fragments: bool,
    offset_units: u16,
}

impl Ipv4HeaderFragment {
    const DONT_FRAGMENT: Self = Self {
        dont_fragment: true,
        more_fragments: false,
        offset_units: 0,
    };
    const NONE: Self = Self {
        dont_fragment: false,
        more_fragments: false,
        offset_units: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv4Error {
    Malformed,
    BadChecksum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ipv4FragmentError {
    Malformed,
    DontFragment,
    MtuTooSmall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Icmpv4Error {
    ProtocolUnreachable,
    PortUnreachable,
    FragmentReassemblyTimeout,
}

/// Borrowed IPv4 header bytes, including options.
///
/// Structural parsing and complete-input validation are separate entry points.
/// Fields are read from the packet; buffer normalization and protocol policy
/// remain with their processing stages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4Header<'a> {
    slice: Ipv4HeaderSlice<'a>,
}

impl<'a> Ipv4Header<'a> {
    /// Parses a complete datagram and validates its length and header checksum.
    /// Trailing link padding is allowed.
    pub(crate) fn parse_input(packet: &'a [u8]) -> Result<Self, Ipv4Error> {
        let header = Self::parse_header_only(packet)?;
        if header.total_len() < header.header_len() || header.total_len() > packet.len() {
            return Err(Ipv4Error::Malformed);
        }
        header.check_checksum()?;
        Ok(header)
    }

    /// Parses a complete header without validating datagram bounds or checksum.
    pub(crate) fn parse_header_only(packet: &'a [u8]) -> Result<Self, Ipv4Error> {
        let slice = Ipv4HeaderSlice::from_slice(packet).map_err(|_| Ipv4Error::Malformed)?;
        Ok(Self { slice })
    }

    /// Validates the checksum over the original header, including options.
    pub(crate) fn check_checksum(self) -> Result<(), Ipv4Error> {
        if self.slice.header_checksum() != self.slice.to_header().calc_header_checksum() {
            return Err(Ipv4Error::BadChecksum);
        }
        Ok(())
    }

    /// Returns the complete header bytes, including options.
    pub(crate) fn bytes(self) -> &'a [u8] {
        self.slice.slice()
    }

    pub(crate) fn src_addr(self) -> Ipv4Address {
        Ipv4Address::from_octets(self.slice.source())
    }

    pub(crate) fn dst_addr(self) -> Ipv4Address {
        Ipv4Address::from_octets(self.slice.destination())
    }

    pub(crate) fn protocol(self) -> u8 {
        self.slice.protocol().0
    }

    pub(crate) fn header_len(self) -> usize {
        self.bytes().len()
    }

    /// Returns the datagram length in bytes declared by the header.
    pub(crate) fn total_len(self) -> usize {
        usize::from(self.slice.total_len())
    }

    pub(crate) fn identification(self) -> u16 {
        self.slice.identification()
    }

    /// Returns the fragment payload offset in bytes.
    pub(crate) fn fragment_offset(self) -> usize {
        usize::from(self.slice.fragments_offset().value()) * IPV4_FRAGMENT_OFFSET_UNIT
    }

    pub(crate) fn more_fragments(self) -> bool {
        self.slice.more_fragments()
    }

    pub(crate) fn dont_fragment(self) -> bool {
        self.slice.dont_fragment()
    }

    pub(crate) fn ecn(self) -> u8 {
        self.slice.ecn().value()
    }

    pub(crate) fn is_fragmented(self) -> bool {
        self.more_fragments() || self.fragment_offset() != 0
    }

    pub(crate) fn is_broadcast_or_multicast(self) -> bool {
        self.dst_addr().is_broadcast() || self.dst_addr().is_multicast()
    }
}

/// Validates input and normalizes the packet's length and transport offset.
pub(crate) fn validate_input_packet(packet: &mut PacketBuf) -> Result<(), Ipv4Error> {
    let header = Ipv4Header::parse_input(packet.network_packet().ok_or(Ipv4Error::Malformed)?)?;
    let (total_len, header_len) = (header.total_len(), header.header_len());
    apply_to_packet(packet, total_len, header_len)
}

/// Finalizes output length and checksum before publishing packet offsets.
pub(crate) fn prepare_output_packet(packet: &mut PacketBuf) -> Result<(), Ipv4Error> {
    let data = packet.network_packet_mut().ok_or(Ipv4Error::Malformed)?;
    let total_len = u16::try_from(data.len()).map_err(|_| Ipv4Error::Malformed)?;
    let mut header = Ipv4HeaderSlice::from_slice(data)
        .map_err(|_| Ipv4Error::Malformed)?
        .to_header();
    header.total_len = total_len;
    header.header_checksum = header.calc_header_checksum();
    let header_len = header.header_len();
    data[..header_len].copy_from_slice(&header.to_bytes());
    apply_to_packet(packet, usize::from(total_len), header_len)
}

fn apply_to_packet(
    packet: &mut PacketBuf,
    total_len: usize,
    header_len: usize,
) -> Result<(), Ipv4Error> {
    packet
        .truncate_network_packet(total_len)
        .ok_or(Ipv4Error::Malformed)?;
    let offset = packet
        .network_offset()
        .and_then(|offset| offset.checked_add(header_len))
        .ok_or(Ipv4Error::Malformed)?;
    packet.set_transport_offset(offset);
    packet.set_checksum_state(ChecksumState::Verified);
    Ok(())
}

pub(crate) fn ip_version(packet: &[u8]) -> Option<u8> {
    packet.first().map(|byte| byte >> 4)
}

pub(crate) fn build_icmpv4_error_packet(
    error: Icmpv4Error,
    packet_type: PacketType,
    offending_packet: &[u8],
) -> Option<Vec<u8>> {
    let offending_header = Ipv4Header::parse_header_only(offending_packet).ok()?;
    if !can_send_icmpv4_error(packet_type, offending_header, offending_packet) {
        return None;
    }

    let quote_len = offending_header
        .total_len()
        .min(offending_header.header_len() + ICMPV4_ERROR_QUOTE_BYTES);
    let icmp_len = Icmpv4Header::MIN_LEN + quote_len;
    let total_len = IPV4_MIN_HEADER_LEN + icmp_len;
    let mut packet = vec![0u8; total_len];

    write_ipv4_header(
        &mut packet[..IPV4_MIN_HEADER_LEN],
        offending_header.dst_addr(),
        offending_header.src_addr(),
        PROTOCOL_ICMP,
        IPV4_DEFAULT_TTL,
        icmp_len,
        Ipv4HeaderFragment::NONE,
    )?;

    let quoted_packet = offending_packet.get(..quote_len)?;
    let icmp_type = match error {
        Icmpv4Error::ProtocolUnreachable => {
            Icmpv4Type::DestinationUnreachable(DestUnreachableHeader::Protocol)
        }
        Icmpv4Error::PortUnreachable => {
            Icmpv4Type::DestinationUnreachable(DestUnreachableHeader::Port)
        }
        Icmpv4Error::FragmentReassemblyTimeout => {
            Icmpv4Type::TimeExceeded(TimeExceededCode::FragmentReassemblyTimeExceeded)
        }
    };
    let icmp_header = Icmpv4Header::with_checksum(icmp_type, quoted_packet);
    let icmp = &mut packet[IPV4_MIN_HEADER_LEN..];
    icmp[..Icmpv4Header::MIN_LEN].copy_from_slice(&icmp_header.to_bytes());
    icmp[Icmpv4Header::MIN_LEN..].copy_from_slice(quoted_packet);

    Some(packet)
}

#[cfg(unittest)]
pub(crate) fn build_ipv4_packet(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV4_MIN_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv4_packet_header(&mut packet, src_addr, dst_addr, protocol, ttl, false)?;
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

#[cfg(unittest)]
pub(crate) fn build_ipv4_packet_dont_fragment(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV4_MIN_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv4_packet_header(&mut packet, src_addr, dst_addr, protocol, ttl, true)?;
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

pub(crate) fn fragment_output_packet(
    packet: &[u8],
    path_mtu: usize,
    identification: u16,
) -> Result<Vec<Vec<u8>>, Ipv4FragmentError> {
    if packet.len() > usize::from(u16::MAX) {
        return Err(Ipv4FragmentError::Malformed);
    }
    let header = Ipv4Header::parse_header_only(packet).map_err(|_| Ipv4FragmentError::Malformed)?;
    if packet.len() <= path_mtu {
        return Ok(vec![packet.to_vec()]);
    }
    if header.dont_fragment() {
        return Err(Ipv4FragmentError::DontFragment);
    }
    if header.is_fragmented() {
        return Err(Ipv4FragmentError::Malformed);
    }

    // The caller supplies the route-selected path MTU and the IPv4
    // identification value. RFC 791 stores fragment offsets in 8-byte units,
    // so every non-final fragment must carry a payload length aligned to that
    // unit. Stack-generated IPv4 output packets currently use a 20-byte header;
    // callers that introduce IPv4 options must add option-copy handling here.
    // Output length comes from storage until the header is finalized.
    let payload = &packet[header.header_len()..];
    let max_payload_len = path_mtu
        .checked_sub(header.header_len())
        .ok_or(Ipv4FragmentError::MtuTooSmall)?;
    let aligned_payload_len =
        max_payload_len / IPV4_FRAGMENT_OFFSET_UNIT * IPV4_FRAGMENT_OFFSET_UNIT;
    if aligned_payload_len == 0 {
        return Err(Ipv4FragmentError::MtuTooSmall);
    }

    let template = header.slice.to_header();
    let mut fragments = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let remaining = payload.len() - offset;
        let fragment_payload_len = if remaining <= max_payload_len {
            remaining
        } else {
            aligned_payload_len
        };
        let more_fragments = offset + fragment_payload_len < payload.len();
        fragments.push(build_ipv4_fragment(
            &template,
            payload,
            offset,
            fragment_payload_len,
            more_fragments,
            identification,
        )?);
        offset += fragment_payload_len;
    }

    Ok(fragments)
}

fn can_send_icmpv4_error(
    packet_type: PacketType,
    offending_header: Ipv4Header<'_>,
    offending_packet: &[u8],
) -> bool {
    if packet_type != PacketType::Host {
        return false;
    }

    if offending_header.src_addr().is_unspecified() || offending_header.is_broadcast_or_multicast()
    {
        return false;
    }

    if offending_header.is_fragmented() && offending_header.fragment_offset() != 0 {
        return false;
    }

    if offending_header.protocol() != PROTOCOL_ICMP {
        return true;
    }

    let Some(icmp_type) = offending_packet
        .get(offending_header.header_len()..offending_header.total_len())
        .and_then(|payload| payload.first().copied())
    else {
        return false;
    };

    !is_icmpv4_error_type(icmp_type)
}

fn is_icmpv4_error_type(icmp_type: u8) -> bool {
    // Unknown or discarded ICMP control types are treated as errors so that
    // error generation cannot recurse on packets the stack will not handle.
    matches!(icmp_type, 1..=7 | 9..=12) || icmp_type > ICMPV4_EXPLICIT_POLICY_MAX_TYPE
}

/// Writes an IPv4 header into a complete preallocated packet.
pub(crate) fn write_ipv4_packet_header(
    packet: &mut [u8],
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    dont_fragment: bool,
) -> Option<()> {
    let payload_len = packet.len().checked_sub(IPV4_MIN_HEADER_LEN)?;
    let fragment = if dont_fragment {
        Ipv4HeaderFragment::DONT_FRAGMENT
    } else {
        Ipv4HeaderFragment::NONE
    };
    write_ipv4_header(
        packet.get_mut(..IPV4_MIN_HEADER_LEN)?,
        src_addr,
        dst_addr,
        protocol,
        ttl,
        payload_len,
        fragment,
    )
}

fn write_ipv4_header(
    header: &mut [u8],
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload_len: usize,
    fragment: Ipv4HeaderFragment,
) -> Option<()> {
    if header.len() < IPV4_MIN_HEADER_LEN {
        return None;
    }

    let payload_len = u16::try_from(payload_len).ok()?;
    let fragment_offset = IpFragOffset::try_new(fragment.offset_units).ok()?;
    let mut ether_header = EtherIpv4Header::new(
        payload_len,
        ttl,
        IpNumber(protocol),
        src_addr.octets(),
        dst_addr.octets(),
    )
    .ok()?;
    ether_header.dont_fragment = fragment.dont_fragment;
    ether_header.more_fragments = fragment.more_fragments;
    ether_header.fragment_offset = fragment_offset;
    ether_header.header_checksum = ether_header.calc_header_checksum();

    header[..IPV4_MIN_HEADER_LEN].copy_from_slice(&ether_header.to_bytes());
    Some(())
}

fn build_ipv4_fragment(
    header: &EtherIpv4Header,
    payload: &[u8],
    offset: usize,
    fragment_payload_len: usize,
    more_fragments: bool,
    identification: u16,
) -> Result<Vec<u8>, Ipv4FragmentError> {
    let mut ether_header = header.clone();
    let total_len = header
        .header_len()
        .checked_add(fragment_payload_len)
        .and_then(|len| u16::try_from(len).ok())
        .ok_or(Ipv4FragmentError::Malformed)?;
    let offset_units = offset / IPV4_FRAGMENT_OFFSET_UNIT;
    let fragment_offset =
        IpFragOffset::try_new(offset_units as u16).map_err(|_| Ipv4FragmentError::Malformed)?;

    ether_header.total_len = total_len;
    ether_header.identification = identification;
    ether_header.dont_fragment = false;
    ether_header.more_fragments = more_fragments;
    ether_header.fragment_offset = fragment_offset;
    ether_header.header_checksum = ether_header.calc_header_checksum();

    let mut fragment = vec![0u8; header.header_len() + fragment_payload_len];
    fragment[..header.header_len()].copy_from_slice(&ether_header.to_bytes());
    fragment[header.header_len()..].copy_from_slice(
        payload
            .get(offset..offset + fragment_payload_len)
            .ok_or(Ipv4FragmentError::Malformed)?,
    );
    Ok(fragment)
}

pub(crate) fn rewrite_reassembled_header(
    packet: &mut [u8],
    payload_len: usize,
    ecn: u8,
) -> Result<(), Ipv4Error> {
    let header_slice = Ipv4HeaderSlice::from_slice(packet).map_err(|_| Ipv4Error::Malformed)?;
    let mut header = header_slice.to_header();
    let total_len = header
        .header_len()
        .checked_add(payload_len)
        .and_then(|len| u16::try_from(len).ok())
        .ok_or(Ipv4Error::Malformed)?;
    header.total_len = total_len;
    header.ecn = IpEcn::try_new(ecn).map_err(|_| Ipv4Error::Malformed)?;
    header.more_fragments = false;
    header.fragment_offset = IpFragOffset::ZERO;
    header.header_checksum = header.calc_header_checksum();
    let header_len = header.header_len();
    packet
        .get_mut(..header_len)
        .ok_or(Ipv4Error::Malformed)?
        .copy_from_slice(&header.to_bytes());
    Ok(())
}

unittest::path_mod_test!(tests, "../../../ut-tests/network/ipv4/process/tests.rs");
