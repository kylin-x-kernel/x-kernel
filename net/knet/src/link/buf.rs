// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Packet buffer owned by the in-kernel network stack.

use alloc::vec::Vec;
use core::ops::Range;

use super::wire::{ETHERNET_HEADER_LEN, EtherType, MacAddress};

/// Link-layer packet type used by packet sockets and link input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PacketType {
    Host,
    Broadcast,
    Multicast,
    OtherHost,
}

/// Packet checksum validation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChecksumState {
    Unverified,
    Verified,
}

/// Current owner of a packet buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PacketOwner {
    DeviceRx,
    DeviceTx,
    Ipv4Stack,
    Loopback,
}

/// Link-layer metadata attached to an Ethernet frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LinkMetadata {
    pub(crate) dst_addr: MacAddress,
    pub(crate) src_addr: MacAddress,
    pub(crate) protocol: EtherType,
}

/// A packet buffer with protocol offsets and ownership metadata.
#[derive(Clone, Debug)]
pub(crate) struct PacketBuf {
    data: Vec<u8>,
    head: usize,
    tail: usize,
    ifindex: i32,
    packet_type: PacketType,
    checksum_state: ChecksumState,
    owner: PacketOwner,
    link_metadata: Option<LinkMetadata>,
    network_offset: Option<usize>,
    transport_offset: Option<usize>,
}

impl PacketBuf {
    pub(crate) fn from_ip_packet_vec(ifindex: i32, data: Vec<u8>, owner: PacketOwner) -> Self {
        let tail = data.len();
        Self {
            data,
            head: 0,
            tail,
            ifindex,
            packet_type: PacketType::Host,
            checksum_state: ChecksumState::Unverified,
            owner,
            link_metadata: None,
            network_offset: Some(0),
            transport_offset: None,
        }
    }

    pub(crate) fn from_ip_packet_vec_with_type(
        ifindex: i32,
        data: Vec<u8>,
        owner: PacketOwner,
        packet_type: PacketType,
    ) -> Self {
        let mut packet = Self::from_ip_packet_vec(ifindex, data, owner);
        packet.packet_type = packet_type;
        packet
    }

    pub(crate) fn from_ethernet_frame(
        ifindex: i32,
        frame: &[u8],
        dst_addr: MacAddress,
        src_addr: MacAddress,
        protocol: EtherType,
        local_addr: MacAddress,
        owner: PacketOwner,
    ) -> Self {
        // TODO: Parse 802.1Q and 802.1ad tags before deriving the network offset.
        let network_offset = match protocol {
            EtherType::Ipv4 | EtherType::Ipv6 => Some(ETHERNET_HEADER_LEN),
            _ => None,
        };

        Self {
            data: frame.to_vec(),
            head: 0,
            tail: frame.len(),
            ifindex,
            packet_type: packet_type(dst_addr, local_addr),
            checksum_state: ChecksumState::Unverified,
            owner,
            link_metadata: Some(LinkMetadata {
                dst_addr,
                src_addr,
                protocol,
            }),
            network_offset,
            transport_offset: None,
        }
    }

    pub(crate) fn ifindex(&self) -> i32 {
        self.ifindex
    }

    pub(crate) fn set_ifindex(&mut self, ifindex: i32) {
        self.ifindex = ifindex;
    }

    pub(crate) fn packet_type(&self) -> PacketType {
        self.packet_type
    }

    #[cfg(unittest)]
    pub(crate) fn checksum_state(&self) -> ChecksumState {
        self.checksum_state
    }

    pub(crate) fn set_checksum_state(&mut self, checksum_state: ChecksumState) {
        self.checksum_state = checksum_state;
    }

    #[cfg(unittest)]
    pub(crate) fn owner(&self) -> PacketOwner {
        self.owner
    }

    pub(crate) fn set_owner(&mut self, owner: PacketOwner) {
        self.owner = owner;
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data[self.data_range()]
    }

    pub(crate) fn link_metadata(&self) -> Option<LinkMetadata> {
        self.link_metadata
    }

    pub(crate) fn network_packet(&self) -> Option<&[u8]> {
        let offset = self.network_offset?;
        self.data.get(offset..self.tail)
    }

    pub(crate) fn network_offset(&self) -> Option<usize> {
        self.network_offset
    }

    pub(crate) fn network_packet_mut(&mut self) -> Option<&mut [u8]> {
        let offset = self.network_offset?;
        self.data.get_mut(offset..self.tail)
    }

    pub(crate) fn truncate_network_packet(&mut self, len: usize) -> Option<()> {
        let offset = self.network_offset?;
        let tail = offset.checked_add(len)?;
        if tail > self.data.len() {
            return None;
        }
        self.tail = tail;
        Some(())
    }

    #[cfg(unittest)]
    pub(crate) fn transport_offset(&self) -> Option<usize> {
        self.transport_offset
    }

    pub(crate) fn set_transport_offset(&mut self, offset: usize) {
        self.transport_offset = Some(offset);
    }

    #[cfg(unittest)]
    pub(crate) fn is_ip_packet(&self) -> bool {
        self.network_packet()
            .and_then(|packet| packet.first().map(|byte| byte >> 4))
            .is_some_and(|version| matches!(version, 4 | 6))
    }

    #[cfg(unittest)]
    pub(crate) fn copy_packet(&self) -> Self {
        self.clone()
    }

    fn data_range(&self) -> Range<usize> {
        self.head..self.tail
    }
}

fn packet_type(dst_addr: MacAddress, local_addr: MacAddress) -> PacketType {
    if dst_addr.is_broadcast() {
        PacketType::Broadcast
    } else if dst_addr.is_multicast() {
        PacketType::Multicast
    } else if dst_addr == local_addr {
        PacketType::Host
    } else {
        PacketType::OtherHost
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::def_test;

    use super::*;
    use crate::link::wire::{EtherType, MacAddress};

    const LOCAL_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 1]);
    const REMOTE_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);

    #[def_test]
    fn test_ip_packet_records_network_offset_and_owner() {
        let packet = PacketBuf::from_ip_packet_vec(
            2,
            vec![
                0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 127, 0, 0, 1, 127, 0, 0, 1,
            ],
            PacketOwner::Loopback,
        );

        assert_eq!(packet.ifindex(), 2);
        assert_eq!(packet.owner(), PacketOwner::Loopback);
        assert_eq!(packet.network_offset(), Some(0));
        assert!(packet.is_ip_packet());
        assert_eq!(packet.network_packet().unwrap(), packet.data());
    }

    #[def_test]
    fn test_ethernet_frame_records_link_metadata_and_packet_type() {
        let frame = [
            LOCAL_MAC.0[0],
            LOCAL_MAC.0[1],
            LOCAL_MAC.0[2],
            LOCAL_MAC.0[3],
            LOCAL_MAC.0[4],
            LOCAL_MAC.0[5],
            REMOTE_MAC.0[0],
            REMOTE_MAC.0[1],
            REMOTE_MAC.0[2],
            REMOTE_MAC.0[3],
            REMOTE_MAC.0[4],
            REMOTE_MAC.0[5],
            0x08,
            0x00,
            0x45,
            0,
        ];
        let packet = PacketBuf::from_ethernet_frame(
            3,
            &frame,
            LOCAL_MAC,
            REMOTE_MAC,
            EtherType::Ipv4,
            LOCAL_MAC,
            PacketOwner::DeviceRx,
        );

        assert_eq!(packet.ifindex(), 3);
        assert_eq!(packet.packet_type(), PacketType::Host);
        assert_eq!(packet.network_offset(), Some(14));
        assert_eq!(packet.network_packet().unwrap(), &[0x45, 0]);
        assert_eq!(packet.link_metadata().unwrap().protocol, EtherType::Ipv4);
    }

    #[def_test]
    fn test_packet_copy_preserves_offsets_and_metadata() {
        let mut packet =
            PacketBuf::from_ip_packet_vec(1, vec![0x60, 0, 0, 0], PacketOwner::DeviceRx);
        packet.set_transport_offset(40);
        packet.set_owner(PacketOwner::DeviceTx);

        let copy = packet.copy_packet();

        assert_eq!(copy.owner(), PacketOwner::DeviceTx);
        assert_eq!(copy.transport_offset(), Some(40));
        assert_eq!(copy.data(), packet.data());
    }

    #[def_test]
    fn test_ip_packet_vec_records_device_tx_owner() {
        let packet = PacketBuf::from_ip_packet_vec(0, vec![0x45, 0, 0, 20], PacketOwner::DeviceTx);

        assert_eq!(packet.owner(), PacketOwner::DeviceTx);
        assert_eq!(packet.network_offset(), Some(0));
        assert_eq!(packet.network_packet().unwrap(), &[0x45, 0, 0, 20]);
    }
}
