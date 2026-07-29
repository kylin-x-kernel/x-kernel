// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::{SocketAddr, SocketAddrV4};
use etherparse::UdpHeaderSlice;

use super::{
    UDP_HEADER_LEN,
    output::{has_valid_udp_checksum, ipv4_to_core},
    pcb::{UdpDatagram, UdpPayload},
    registry,
};
use crate::{buf::PacketBuf, ipv4};

/// Result of UDP input delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputDisposition {
    Accepted,
    NoSocket,
    Malformed,
}

/// Deliver an IPv4 UDP packet while preserving the original packet storage.
pub(crate) fn handle_ipv4_packet(
    header: ipv4::Ipv4Header,
    packet: PacketBuf,
) -> (InputDisposition, Option<PacketBuf>) {
    let Some((local, remote, payload_offset, payload_len)) = packet
        .network_packet()
        .and_then(|ip_packet| parse_udp_datagram(header, ip_packet))
    else {
        return (InputDisposition::Malformed, Some(packet));
    };

    let Some(pcb) = registry::lookup_udp_pcb(local, remote) else {
        return (InputDisposition::NoSocket, Some(packet));
    };

    let payload = match UdpPayload::new(packet, payload_offset, payload_len) {
        Ok(payload) => payload,
        Err(packet) => return (InputDisposition::Malformed, Some(packet)),
    };
    let datagram = UdpDatagram {
        payload,
        remote_addr: remote,
    };
    pcb.enqueue(datagram);
    (InputDisposition::Accepted, None)
}

fn parse_udp_datagram(
    header: ipv4::Ipv4Header,
    ip_packet: &[u8],
) -> Option<(SocketAddr, SocketAddr, usize, usize)> {
    let udp_packet = ipv4::payload(ip_packet, &header)?;
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
