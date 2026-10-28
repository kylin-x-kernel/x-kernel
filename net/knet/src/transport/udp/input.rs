// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::{SocketAddr, SocketAddrV4};
use etherparse::UdpHeaderSlice;

use super::{
    UDP_HEADER_LEN,
    output::{has_valid_udp_checksum, ipv4_to_core},
    pcb::PreparedUdpPacket,
    registry,
};
use crate::{buf::PacketBuf, ipv4};

/// Result of UDP input delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputDisposition {
    Accepted,
    NoSocket,
}

/// Validates an IPv4 UDP packet and records its receive metadata.
///
/// The caller must validate and normalize IPv4 input before transferring ownership.
/// This function must run in task context. Loopback stamps packets before
/// disabling BH; ordinary ingress is already processed by a task. The existing
/// reference-counted `PacketBuf` is retained without another allocation.
pub(crate) fn prepare_ipv4_packet(packet: PacketBuf) -> Result<PreparedUdpPacket, PacketBuf> {
    debug_assert!(
        !kirq::context::is_in_interrupt_context(),
        "prepare_ipv4_packet requires task context"
    );
    let Some((local_addr, remote_addr, payload_offset, payload_len)) = parse_udp_datagram(&packet)
    else {
        return Err(packet);
    };
    let packet =
        PreparedUdpPacket::new(packet, payload_offset, payload_len, local_addr, remote_addr)?;
    Ok(packet)
}

/// Delivers a prepared packet through the non-sleeping PCB lookup path.
///
/// `PacketBuf` owns the shared packet allocation before `NetRx` starts, and the
/// PCB receive queue is reserved to its occupancy bound, so this function is
/// allocation-free.
pub(crate) fn deliver_ipv4_packet(
    packet: PreparedUdpPacket,
) -> (InputDisposition, Option<PreparedUdpPacket>) {
    let Some(pcb) = registry::lookup_udp_pcb_on_device(
        packet.local_addr(),
        packet.remote_addr(),
        packet.packet().ifindex(),
    ) else {
        return (InputDisposition::NoSocket, Some(packet));
    };

    pcb.enqueue(packet);
    (InputDisposition::Accepted, None)
}

fn parse_udp_datagram(packet: &PacketBuf) -> Option<(SocketAddr, SocketAddr, usize, usize)> {
    let header = ipv4::Ipv4Header::parse_header_only(packet.network_packet()?).ok()?;
    let udp_packet = packet.transport_packet()?;
    let udp_header = UdpHeaderSlice::from_slice(udp_packet).ok()?;
    let udp_len = udp_header.length() as usize;
    if udp_len < UDP_HEADER_LEN || udp_len > udp_packet.len() {
        return None;
    }

    let udp_packet = &udp_packet[..udp_len];
    if !has_valid_udp_checksum(header.src_addr(), header.dst_addr(), udp_packet) {
        return None;
    }

    let remote = SocketAddr::V4(SocketAddrV4::new(
        ipv4_to_core(header.src_addr()),
        udp_header.source_port(),
    ));
    let local = SocketAddr::V4(SocketAddrV4::new(
        ipv4_to_core(header.dst_addr()),
        udp_header.destination_port(),
    ));

    Some((
        local,
        remote,
        header.header_len() + UDP_HEADER_LEN,
        udp_len - UDP_HEADER_LEN,
    ))
}
