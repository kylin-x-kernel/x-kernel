// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Link-layer wire format helpers.

use core::fmt;

use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, byteorder::network_endian::U16,
};

use crate::ip::Ipv4Address;

/// Ethernet header length in bytes.
pub(crate) const ETHERNET_HEADER_LEN: usize = 14;

const ARP_ETHERNET_IPV4_LEN: usize = 28;
const ARP_HARDWARE_ETHERNET: u16 = 1;
const ARP_PROTOCOL_IPV4: u16 = 0x0800;
const ARP_HARDWARE_LEN_ETHERNET: u8 = 6;
const ARP_PROTOCOL_LEN_IPV4: u8 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned)]
struct EthernetHeader {
    dst_addr: [u8; 6],
    src_addr: [u8; 6],
    ethertype: U16,
}

/// A six-octet Ethernet address.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MacAddress(pub(crate) [u8; 6]);

impl MacAddress {
    pub(crate) const BROADCAST: Self = Self([0xff; 6]);
    pub(crate) const ZERO: Self = Self([0; 6]);

    pub(crate) const fn bytes(self) -> [u8; 6] {
        self.0
    }

    pub(crate) fn is_broadcast(self) -> bool {
        self == Self::BROADCAST
    }

    pub(crate) fn is_multicast(self) -> bool {
        self.0[0] & 1 != 0
    }

    pub(crate) fn is_unicast(self) -> bool {
        !self.is_broadcast() && !self.is_multicast()
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        write!(
            f,
            "{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
        )
    }
}

/// Ethernet DIX EtherType.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum EtherType {
    Ipv4,
    Arp,
    Ipv6,
    Other(u16),
}

impl EtherType {
    pub(crate) fn from_u16(value: u16) -> Self {
        match value {
            0x0800 => Self::Ipv4,
            0x0806 => Self::Arp,
            0x86dd => Self::Ipv6,
            other => Self::Other(other),
        }
    }

    pub(crate) fn as_u16(self) -> u16 {
        match self {
            Self::Ipv4 => 0x0800,
            Self::Arp => 0x0806,
            Self::Ipv6 => 0x86dd,
            Self::Other(value) => value,
        }
    }
}

impl From<EtherType> for u16 {
    fn from(value: EtherType) -> Self {
        value.as_u16()
    }
}

/// A checked reference to an Ethernet frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EthernetFrameRef<'a> {
    header: &'a EthernetHeader,
    payload: &'a [u8],
}

impl<'a> EthernetFrameRef<'a> {
    pub(crate) fn new_checked(data: &'a [u8]) -> Option<Self> {
        let (header, payload) = EthernetHeader::ref_from_prefix(data).ok()?;
        Some(Self { header, payload })
    }

    pub(crate) fn dst_addr(self) -> MacAddress {
        MacAddress(self.header.dst_addr)
    }

    pub(crate) fn src_addr(self) -> MacAddress {
        MacAddress(self.header.src_addr)
    }

    pub(crate) fn ethertype(self) -> EtherType {
        EtherType::from_u16(self.header.ethertype.get())
    }

    pub(crate) fn payload(self) -> &'a [u8] {
        self.payload
    }
}

pub(crate) fn emit_ethernet_header(
    frame: &mut [u8],
    dst_addr: MacAddress,
    src_addr: MacAddress,
    ethertype: EtherType,
) -> Option<&mut [u8]> {
    if frame.len() < ETHERNET_HEADER_LEN {
        return None;
    }

    let header = EthernetHeader {
        dst_addr: dst_addr.0,
        src_addr: src_addr.0,
        ethertype: U16::new(ethertype.as_u16()),
    };
    frame[..ETHERNET_HEADER_LEN].copy_from_slice(header.as_bytes());
    Some(&mut frame[ETHERNET_HEADER_LEN..])
}

/// Ethernet/IPv4 ARP operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArpOperation {
    Request,
    Reply,
    Unknown(u16),
}

impl ArpOperation {
    fn from_u16(value: u16) -> Self {
        match value {
            1 => Self::Request,
            2 => Self::Reply,
            other => Self::Unknown(other),
        }
    }

    fn as_u16(self) -> u16 {
        match self {
            Self::Request => 1,
            Self::Reply => 2,
            Self::Unknown(value) => value,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned)]
struct ArpIpv4Header {
    hardware_type: U16,
    protocol_type: U16,
    hardware_len: u8,
    protocol_len: u8,
    operation: U16,
    source_hardware_addr: [u8; 6],
    source_protocol_addr: [u8; 4],
    target_hardware_addr: [u8; 6],
    target_protocol_addr: [u8; 4],
}

/// Parsed Ethernet/IPv4 ARP packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArpIpv4Packet {
    pub(crate) operation: ArpOperation,
    pub(crate) source_hardware_addr: MacAddress,
    pub(crate) source_protocol_addr: Ipv4Address,
    pub(crate) target_hardware_addr: MacAddress,
    pub(crate) target_protocol_addr: Ipv4Address,
}

impl ArpIpv4Packet {
    pub(crate) const LEN: usize = ARP_ETHERNET_IPV4_LEN;

    pub(crate) fn parse(data: &[u8]) -> Option<Self> {
        let (header, _) = ArpIpv4Header::ref_from_prefix(data).ok()?;
        if header.hardware_type.get() != ARP_HARDWARE_ETHERNET
            || header.protocol_type.get() != ARP_PROTOCOL_IPV4
            || header.hardware_len != ARP_HARDWARE_LEN_ETHERNET
            || header.protocol_len != ARP_PROTOCOL_LEN_IPV4
        {
            return None;
        }

        Some(Self {
            operation: ArpOperation::from_u16(header.operation.get()),
            source_hardware_addr: MacAddress(header.source_hardware_addr),
            source_protocol_addr: Ipv4Address::from_octets(header.source_protocol_addr),
            target_hardware_addr: MacAddress(header.target_hardware_addr),
            target_protocol_addr: Ipv4Address::from_octets(header.target_protocol_addr),
        })
    }

    pub(crate) fn emit(self, data: &mut [u8]) -> Option<()> {
        if data.len() < ARP_ETHERNET_IPV4_LEN {
            return None;
        }

        let header = ArpIpv4Header {
            hardware_type: U16::new(ARP_HARDWARE_ETHERNET),
            protocol_type: U16::new(ARP_PROTOCOL_IPV4),
            hardware_len: ARP_HARDWARE_LEN_ETHERNET,
            protocol_len: ARP_PROTOCOL_LEN_IPV4,
            operation: U16::new(self.operation.as_u16()),
            source_hardware_addr: self.source_hardware_addr.0,
            source_protocol_addr: self.source_protocol_addr.octets(),
            target_hardware_addr: self.target_hardware_addr.0,
            target_protocol_addr: self.target_protocol_addr.octets(),
        };
        data[..ARP_ETHERNET_IPV4_LEN].copy_from_slice(header.as_bytes());
        Some(())
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/link/wire/tests.rs");
