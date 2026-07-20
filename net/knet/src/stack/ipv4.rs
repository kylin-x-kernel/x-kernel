// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 packet validation and ICMPv4 error helpers.

use alloc::{vec, vec::Vec};

use etherparse::{
    Icmpv4Header, Icmpv4Type, IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header,
    Ipv4HeaderSlice, icmpv4::DestUnreachableHeader,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4Header {
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    header_len: usize,
    total_len: usize,
    fragment_offset: usize,
    more_fragments: bool,
    dont_fragment: bool,
    is_fragmented: bool,
}

impl Ipv4Header {
    pub(crate) fn parse_input(packet: &[u8]) -> Result<Self, Ipv4Error> {
        Self::parse(packet, true)
    }

    pub(crate) fn validate_input_packet(packet: &mut PacketBuf) -> Result<Self, Ipv4Error> {
        let header = Self::parse_input(packet.network_packet().ok_or(Ipv4Error::Malformed)?)?;
        header.apply_to_packet(packet)
    }

    pub(crate) fn prepare_output_packet(packet: &mut PacketBuf) -> Result<Self, Ipv4Error> {
        let header = Self::parse_output(packet.network_packet().ok_or(Ipv4Error::Malformed)?)?;
        {
            let data = packet.network_packet_mut().ok_or(Ipv4Error::Malformed)?;
            let total_len = u16::try_from(data.len()).map_err(|_| Ipv4Error::Malformed)?;
            data[2..4].copy_from_slice(&total_len.to_be_bytes());
            let header_data = data
                .get_mut(..header.header_len)
                .ok_or(Ipv4Error::Malformed)?;
            let mut ether_header = Ipv4HeaderSlice::from_slice(header_data)
                .map_err(|_| Ipv4Error::Malformed)?
                .to_header();
            ether_header.header_checksum = ether_header.calc_header_checksum();
            header_data.copy_from_slice(&ether_header.to_bytes());
        }
        header.apply_to_packet(packet)
    }

    pub(crate) fn parse_output(packet: &[u8]) -> Result<Self, Ipv4Error> {
        if packet.len() > u16::MAX as usize {
            return Err(Ipv4Error::Malformed);
        }

        let header_slice = Ipv4HeaderSlice::from_slice(packet).map_err(|_| Ipv4Error::Malformed)?;

        Ok(Self::from_slice(&header_slice, packet.len()))
    }

    pub(crate) fn src_addr(self) -> Ipv4Address {
        self.src_addr
    }

    pub(crate) fn dst_addr(self) -> Ipv4Address {
        self.dst_addr
    }

    pub(crate) fn protocol(self) -> u8 {
        self.protocol
    }

    pub(crate) fn header_len(self) -> usize {
        self.header_len
    }

    pub(crate) fn total_len(self) -> usize {
        self.total_len
    }

    pub(crate) fn fragment_offset(self) -> usize {
        self.fragment_offset
    }

    pub(crate) fn more_fragments(self) -> bool {
        self.more_fragments
    }

    pub(crate) fn dont_fragment(self) -> bool {
        self.dont_fragment
    }

    pub(crate) fn is_fragmented(self) -> bool {
        self.is_fragmented
    }

    pub(crate) fn is_broadcast_or_multicast(self) -> bool {
        self.dst_addr.is_broadcast() || self.dst_addr.is_multicast()
    }

    fn parse(packet: &[u8], verify_checksum: bool) -> Result<Self, Ipv4Error> {
        let header_slice = Ipv4HeaderSlice::from_slice(packet).map_err(|_| Ipv4Error::Malformed)?;
        let header_len = header_slice.slice().len();
        let total_len = header_slice.total_len() as usize;
        if total_len < header_len || total_len > packet.len() {
            return Err(Ipv4Error::Malformed);
        }

        if verify_checksum
            && header_slice.header_checksum() != header_slice.to_header().calc_header_checksum()
        {
            return Err(Ipv4Error::BadChecksum);
        }

        Ok(Self::from_slice(&header_slice, total_len))
    }

    fn from_slice(header_slice: &Ipv4HeaderSlice<'_>, total_len: usize) -> Self {
        Self {
            src_addr: Ipv4Address::from_octets(header_slice.source()),
            dst_addr: Ipv4Address::from_octets(header_slice.destination()),
            protocol: header_slice.protocol().0,
            ttl: header_slice.ttl(),
            header_len: header_slice.slice().len(),
            total_len,
            fragment_offset: usize::from(header_slice.fragments_offset().value())
                * IPV4_FRAGMENT_OFFSET_UNIT,
            more_fragments: header_slice.more_fragments(),
            dont_fragment: header_slice.dont_fragment(),
            is_fragmented: header_slice.is_fragmenting_payload(),
        }
    }

    fn apply_to_packet(self, packet: &mut PacketBuf) -> Result<Self, Ipv4Error> {
        packet
            .truncate_network_packet(self.total_len)
            .ok_or(Ipv4Error::Malformed)?;
        let transport_offset = packet
            .network_offset()
            .and_then(|offset| offset.checked_add(self.header_len))
            .ok_or(Ipv4Error::Malformed)?;
        packet.set_transport_offset(transport_offset);
        packet.set_checksum_state(ChecksumState::Verified);
        Ok(self)
    }
}

pub(crate) fn ip_version(packet: &[u8]) -> Option<u8> {
    packet.first().map(|byte| byte >> 4)
}

pub(crate) fn payload<'a>(packet: &'a [u8], header: &Ipv4Header) -> Option<&'a [u8]> {
    packet.get(header.header_len..header.total_len)
}

pub(crate) fn build_icmpv4_error_packet(
    error: Icmpv4Error,
    packet_type: PacketType,
    offending_header: Ipv4Header,
    offending_packet: &[u8],
) -> Option<Vec<u8>> {
    if !can_send_icmpv4_error(packet_type, offending_header, offending_packet) {
        return None;
    }

    let quote_len = offending_header
        .total_len
        .min(offending_header.header_len + ICMPV4_ERROR_QUOTE_BYTES);
    let icmp_len = Icmpv4Header::MIN_LEN + quote_len;
    let total_len = IPV4_MIN_HEADER_LEN + icmp_len;
    let mut packet = vec![0u8; total_len];

    write_ipv4_header(
        &mut packet[..IPV4_MIN_HEADER_LEN],
        offending_header.dst_addr,
        offending_header.src_addr,
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
    };
    let icmp_header = Icmpv4Header::with_checksum(icmp_type, quoted_packet);
    let icmp = &mut packet[IPV4_MIN_HEADER_LEN..];
    icmp[..Icmpv4Header::MIN_LEN].copy_from_slice(&icmp_header.to_bytes());
    icmp[Icmpv4Header::MIN_LEN..].copy_from_slice(quoted_packet);

    Some(packet)
}

pub(crate) fn build_ipv4_packet(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV4_MIN_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv4_header(
        &mut packet[..IPV4_MIN_HEADER_LEN],
        src_addr,
        dst_addr,
        protocol,
        ttl,
        payload.len(),
        Ipv4HeaderFragment::NONE,
    )?;
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

pub(crate) fn build_ipv4_packet_dont_fragment(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV4_MIN_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv4_header(
        &mut packet[..IPV4_MIN_HEADER_LEN],
        src_addr,
        dst_addr,
        protocol,
        ttl,
        payload.len(),
        Ipv4HeaderFragment::DONT_FRAGMENT,
    )?;
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

pub(crate) fn fragment_output_packet(
    packet: &[u8],
    path_mtu: usize,
    identification: u16,
) -> Result<Vec<Vec<u8>>, Ipv4FragmentError> {
    let header = Ipv4Header::parse_output(packet).map_err(|_| Ipv4FragmentError::Malformed)?;
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
    let payload = payload(packet, &header).ok_or(Ipv4FragmentError::Malformed)?;
    let max_payload_len = path_mtu
        .checked_sub(header.header_len())
        .ok_or(Ipv4FragmentError::MtuTooSmall)?;
    let aligned_payload_len =
        max_payload_len / IPV4_FRAGMENT_OFFSET_UNIT * IPV4_FRAGMENT_OFFSET_UNIT;
    if aligned_payload_len == 0 {
        return Err(Ipv4FragmentError::MtuTooSmall);
    }

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
            packet,
            &header,
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
    offending_header: Ipv4Header,
    offending_packet: &[u8],
) -> bool {
    if packet_type != PacketType::Host {
        return false;
    }

    if offending_header.src_addr.is_unspecified() || offending_header.is_broadcast_or_multicast() {
        return false;
    }

    if offending_header.is_fragmented() && offending_header.fragment_offset() != 0 {
        return false;
    }

    if offending_header.protocol != PROTOCOL_ICMP {
        return true;
    }

    let Some(icmp_type) =
        payload(offending_packet, &offending_header).and_then(|payload| payload.first().copied())
    else {
        return false;
    };

    !suppress_icmpv4_error_response(icmp_type)
}

fn suppress_icmpv4_error_response(icmp_type: u8) -> bool {
    // Suppress responses to ICMP error and discarded control types to prevent
    // error generation from recurring on packets the stack does not handle.
    matches!(icmp_type, 1..=7 | 9..=12) || icmp_type > ICMPV4_EXPLICIT_POLICY_MAX_TYPE
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
    packet: &[u8],
    header: &Ipv4Header,
    payload: &[u8],
    offset: usize,
    fragment_payload_len: usize,
    more_fragments: bool,
    identification: u16,
) -> Result<Vec<u8>, Ipv4FragmentError> {
    let header_slice =
        Ipv4HeaderSlice::from_slice(packet).map_err(|_| Ipv4FragmentError::Malformed)?;
    let mut ether_header = header_slice.to_header();
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

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    use crate::buf::PacketOwner;

    fn ipv4_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
        build_ipv4_packet(
            Ipv4Address::new(192, 0, 2, 1),
            Ipv4Address::new(192, 0, 2, 2),
            protocol,
            64,
            payload,
        )
        .unwrap()
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
        let mut packet = PacketBuf::from_ip_packet_vec(
            1,
            ipv4_packet(PROTOCOL_UDP, &[1, 2]),
            PacketOwner::DeviceRx,
        );

        let header = Ipv4Header::validate_input_packet(&mut packet).unwrap();

        assert_eq!(header.protocol(), PROTOCOL_UDP);
        assert_eq!(header.header_len(), IPV4_MIN_HEADER_LEN);
        assert_eq!(header.total_len(), IPV4_MIN_HEADER_LEN + 2);
        assert_eq!(packet.transport_offset(), Some(IPV4_MIN_HEADER_LEN));
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

        let header = Ipv4Header::validate_input_packet(&mut packet).unwrap();

        assert!(header.is_fragmented());
    }

    #[def_test]
    fn test_validate_input_packet_rejects_bad_checksum() {
        let mut bytes = ipv4_packet(PROTOCOL_TCP, &[]);
        bytes[10] ^= 0xff;
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

        assert_eq!(
            Ipv4Header::validate_input_packet(&mut packet),
            Err(Ipv4Error::BadChecksum)
        );
    }

    #[def_test]
    fn test_prepare_output_packet_repairs_checksum() {
        let mut bytes = ipv4_packet(PROTOCOL_ICMP, &[]);
        bytes[10] = 0;
        bytes[11] = 0;
        let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx);

        Ipv4Header::prepare_output_packet(&mut packet).unwrap();

        let header = Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
        assert_eq!(header.protocol(), PROTOCOL_ICMP);
    }

    #[def_test]
    fn test_fragment_output_packet_splits_on_aligned_payload() {
        let original_payload: Vec<u8> = (0..40).collect();
        let packet = ipv4_packet(PROTOCOL_UDP, &original_payload);

        let fragments = fragment_output_packet(&packet, IPV4_MIN_HEADER_LEN + 18, 0x1234).unwrap();

        assert_eq!(fragments.len(), 3);
        let mut fragmented_payload = Vec::new();
        for (idx, fragment) in fragments.iter().enumerate() {
            let header = Ipv4Header::parse_input(fragment).unwrap();
            assert_eq!(
                Ipv4HeaderSlice::from_slice(fragment)
                    .unwrap()
                    .identification(),
                0x1234
            );
            assert_eq!(header.fragment_offset(), idx * 16);
            assert_eq!(header.more_fragments(), idx != 2);
            assert!(!header.dont_fragment());
            fragmented_payload.extend_from_slice(payload(fragment, &header).unwrap());
        }
        assert_eq!(fragmented_payload, original_payload);
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
            header,
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
    fn test_build_icmpv4_port_unreachable() {
        let offending = ipv4_packet(PROTOCOL_UDP, &[1, 2, 3, 4]);
        let header = Ipv4Header::parse_input(&offending).unwrap();

        assert!(
            build_icmpv4_error_packet(
                Icmpv4Error::PortUnreachable,
                PacketType::Host,
                header,
                &offending,
            )
            .is_some()
        );
    }
}
