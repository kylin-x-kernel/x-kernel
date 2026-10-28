// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
    let mut packet = PacketBuf::from_ethernet_frame(
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
    assert!(packet.transport_packet().is_none());
    for offset in [13, frame.len() + 1] {
        packet.set_transport_offset(offset);
        assert!(packet.transport_packet().is_none());
    }
    packet.set_transport_offset(15);
    assert_eq!(packet.transport_packet(), Some([0].as_slice()));
    packet.set_checksum_state(ChecksumState::Verified);
    packet.truncate_network_packet(1).unwrap();
    assert!(packet.transport_packet().is_none());
    assert_eq!(packet.checksum_state(), ChecksumState::Unverified);
    packet.set_transport_offset(15);
    assert_eq!(packet.transport_packet(), Some([].as_slice()));
    assert!(packet.truncate_network_packet(2).is_none());
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
fn test_packet_handle_is_pointer_sized_and_uses_copy_on_write() {
    let mut packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx);
    packet.set_transport_offset(2);
    packet.set_checksum_state(ChecksumState::Verified);
    let mut copy = packet.clone();

    assert_eq!(
        core::mem::size_of::<PacketBuf>(),
        core::mem::size_of::<usize>()
    );
    assert!(packet.shares_storage_with(&copy));

    copy.network_packet_mut().unwrap()[0] = 0x60;

    assert!(!packet.shares_storage_with(&copy));
    assert_eq!(packet.network_packet().unwrap()[0], 0x45);
    assert_eq!(copy.network_packet().unwrap()[0], 0x60);
    assert_eq!(packet.transport_packet(), Some([0, 20].as_slice()));
    assert_eq!(packet.checksum_state(), ChecksumState::Verified);
    assert!(copy.transport_packet().is_none());
    assert_eq!(copy.checksum_state(), ChecksumState::Unverified);
}

#[def_test]
fn test_ip_packet_vec_records_device_tx_owner() {
    let packet = PacketBuf::from_ip_packet_vec(0, vec![0x45, 0, 0, 20], PacketOwner::DeviceTx);

    assert_eq!(packet.owner(), PacketOwner::DeviceTx);
    assert_eq!(packet.network_offset(), Some(0));
    assert_eq!(packet.network_packet().unwrap(), &[0x45, 0, 0, 20]);
}
