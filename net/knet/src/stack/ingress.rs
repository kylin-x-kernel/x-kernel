// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network-layer ingress processing outside the router lock.

use alloc::vec::Vec;

use ktime_types::MonotonicInstant;
use smoltcp::{
    iface::SocketSet,
    wire::{IpAddress as SmoltcpIpAddress, IpProtocol, Ipv6Packet, TcpPacket},
};

use super::{
    fragment::{Ipv4Reassembler, Ipv4ReassemblyResult},
    ipv4::{self, Icmpv4Error, Ipv4Error, Ipv4Header},
};
use crate::{
    LISTEN_TABLE,
    buf::PacketBuf,
    ip::{Ipv4Address, Ipv4Cidr},
    udp, udp_err,
};

pub(crate) struct IngressProcessor {
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    ipv4_reassembler: Ipv4Reassembler,
}

impl IngressProcessor {
    pub fn new() -> Self {
        Self {
            local_ipv4_addrs: Vec::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
        }
    }

    pub fn update_local_ipv4_addrs(&mut self, addrs: &[Ipv4Cidr]) {
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(addrs);
    }

    pub fn handle_rx_packets(
        &mut self,
        timestamp: MonotonicInstant,
        packets: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        sockets: &mut SocketSet<'_>,
    ) {
        accepted_packets.clear();
        for expired in self.ipv4_reassembler.remove_expired(timestamp) {
            queue_icmpv4_error(
                control_packets,
                Icmpv4Error::FragmentReassemblyTimeout,
                expired.packet_type,
                expired.header,
                &expired.packet,
            );
        }
        for packet in packets.drain(..) {
            self.handle_rx_packet(
                timestamp,
                packet,
                accepted_packets,
                control_packets,
                sockets,
            );
        }
    }

    fn handle_rx_packet(
        &mut self,
        timestamp: MonotonicInstant,
        packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        sockets: &mut SocketSet<'_>,
    ) {
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        match ipv4::ip_version(ip_packet) {
            Some(4) => self.handle_ipv4_input(
                timestamp,
                packet,
                accepted_packets,
                control_packets,
                sockets,
            ),
            Some(6) => {
                // UDP error delivery currently supports only ICMPv4.
                snoop_ipv6_tcp_packet(ip_packet, sockets);
                accepted_packets.push(packet);
            }
            _ => debug!("Dropping packet with invalid IP version"),
        }
    }

    fn handle_ipv4_input(
        &mut self,
        timestamp: MonotonicInstant,
        mut packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        sockets: &mut SocketSet<'_>,
    ) {
        let header = match Ipv4Header::validate_input_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return,
        };

        if !self.is_local_ipv4_destination(header.dst_addr()) {
            return;
        }

        if header.is_fragmented() {
            match self.ipv4_reassembler.reassemble(packet, header, timestamp) {
                Ipv4ReassemblyResult::Complete(packet) => self.handle_ipv4_input(
                    timestamp,
                    packet,
                    accepted_packets,
                    control_packets,
                    sockets,
                ),
                Ipv4ReassemblyResult::Pending | Ipv4ReassemblyResult::Dropped => {}
            }
            return;
        }

        if header.protocol() == ipv4::PROTOCOL_UDP {
            let packet_type = packet.packet_type();
            let (disposition, returned_packet) = udp::handle_ipv4_packet(header, packet);
            match disposition {
                udp::InputDisposition::Accepted | udp::InputDisposition::Malformed => return,
                udp::InputDisposition::NoSocket if !header.is_broadcast_or_multicast() => {
                    let Some(packet) = returned_packet else {
                        return;
                    };
                    let Some(ip_packet) = packet.network_packet() else {
                        return;
                    };
                    queue_icmpv4_error(
                        control_packets,
                        Icmpv4Error::PortUnreachable,
                        packet_type,
                        header,
                        ip_packet,
                    );
                    return;
                }
                udp::InputDisposition::NoSocket => return,
            }
        }

        if should_emit_protocol_unreachable(header) {
            if let Some(ip_packet) = packet.network_packet() {
                queue_icmpv4_error(
                    control_packets,
                    Icmpv4Error::ProtocolUnreachable,
                    packet.packet_type(),
                    header,
                    ip_packet,
                );
            }
            return;
        }

        if let Some(ip_packet) = packet.network_packet() {
            udp_err::inspect_icmpv4_error(ip_packet);
            if let Some(payload) = ipv4::payload(ip_packet, &header) {
                snoop_tcp_segment(
                    header.protocol(),
                    SmoltcpIpAddress::Ipv4(header.src_addr().into()),
                    SmoltcpIpAddress::Ipv4(header.dst_addr().into()),
                    payload,
                    sockets,
                );
            }
        }
        accepted_packets.push(packet);
    }

    fn is_local_ipv4_destination(&self, dst_addr: Ipv4Address) -> bool {
        dst_addr.is_broadcast()
            || dst_addr.is_multicast()
            || self
                .local_ipv4_addrs
                .iter()
                .any(|addr| addr.address() == dst_addr || addr.broadcast() == Some(dst_addr))
    }
}

fn queue_icmpv4_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: Icmpv4Error,
    packet_type: crate::buf::PacketType,
    offending_header: Ipv4Header,
    offending_packet: &[u8],
) {
    let Some(packet) =
        ipv4::build_icmpv4_error_packet(error, packet_type, offending_header, offending_packet)
    else {
        return;
    };
    control_packets.push(packet);
}

fn should_emit_protocol_unreachable(header: Ipv4Header) -> bool {
    !header.is_fragmented()
        && !header.is_broadcast_or_multicast()
        && !matches!(
            header.protocol(),
            ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP | ipv4::PROTOCOL_ICMP
        )
}

fn snoop_ipv6_tcp_packet(buf: &[u8], sockets: &mut SocketSet<'_>) {
    let Ok(ip_packet) = Ipv6Packet::new_checked(buf) else {
        return;
    };
    // TODO: Traverse IPv6 extension headers before TCP snooping.
    // `next_header` and `payload` only refer to the base IPv6 header.
    let protocol = match ip_packet.next_header() {
        IpProtocol::Tcp => ipv4::PROTOCOL_TCP,
        _ => return,
    };
    snoop_tcp_segment(
        protocol,
        SmoltcpIpAddress::Ipv6(ip_packet.src_addr()),
        SmoltcpIpAddress::Ipv6(ip_packet.dst_addr()),
        ip_packet.payload(),
        sockets,
    );
}

fn snoop_tcp_segment(
    protocol: u8,
    src_addr: SmoltcpIpAddress,
    dst_addr: SmoltcpIpAddress,
    payload: &[u8],
    sockets: &mut SocketSet<'_>,
) {
    if protocol != ipv4::PROTOCOL_TCP {
        return;
    }

    let Ok(tcp_packet) = TcpPacket::new_checked(payload) else {
        return;
    };
    let src_addr = (src_addr, tcp_packet.src_port()).into();
    let dst_addr = (dst_addr, tcp_packet.dst_port()).into();
    LISTEN_TABLE.note_tcp_packet(dst_addr);
    if tcp_packet.syn() && !tcp_packet.ack() {
        LISTEN_TABLE.incoming_tcp_packet(src_addr, dst_addr, sockets);
    }
}
