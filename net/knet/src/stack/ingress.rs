// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network-layer ingress processing outside the router lock.

use alloc::{string::String, vec::Vec};

use hashbrown::HashMap;
use ktime_types::MonotonicInstant;
use smoltcp::wire::{IpAddress as SmoltcpIpAddress, IpProtocol, Ipv6Packet, TcpPacket};

use super::socket_set::SocketSetState;
use crate::{
    LISTEN_TABLE,
    buf::PacketBuf,
    device::net_rx::RxPacket,
    ip::{Ipv4Address, Ipv4Cidr},
    netfilter::FilterConfig,
    network::{
        fragment::{Ipv4Reassembler, Ipv4ReassemblyResult},
        ipv4::{self, Icmpv4Error, Ipv4Header},
    },
    udp, udp_err,
};

pub(crate) struct IngressProcessor {
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    device_names: HashMap<i32, String>,
    ipv4_reassembler: Ipv4Reassembler,
}

impl IngressProcessor {
    pub(crate) fn new() -> Self {
        Self {
            local_ipv4_addrs: Vec::new(),
            device_names: HashMap::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
        }
    }

    pub(crate) fn update_device_names<'a>(&mut self, names: impl Iterator<Item = (i32, &'a str)>) {
        self.device_names = names.map(|(id, name)| (id, String::from(name))).collect();
    }

    pub(crate) fn update_local_ipv4_addrs(&mut self, addrs: &[Ipv4Cidr]) {
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(addrs);
    }

    pub(crate) fn ipv4_reassembly_deadline(&self) -> Option<MonotonicInstant> {
        self.ipv4_reassembler.expiration_deadline()
    }

    pub(crate) fn expire_ipv4_fragments(
        &mut self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> usize {
        let expired = self
            .ipv4_reassembler
            .remove_expired(timestamp, timer_budget);
        for fragment in expired.fragments {
            queue_icmpv4_error(
                control_packets,
                Icmpv4Error::FragmentReassemblyTimeout,
                fragment.packet_type,
                &fragment.packet,
            );
        }
        expired.timer_events
    }

    /// Consumes `packets` and appends work for the shared stack and control TX.
    ///
    /// The output batches may contain work retained from an earlier poll round
    /// whose shared destination lock was contended.
    pub(crate) fn handle_rx_packets(
        &mut self,
        timestamp: MonotonicInstant,
        filter: &FilterConfig,
        packets: &mut Vec<RxPacket>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) {
        for packet in packets.drain(..) {
            match packet {
                RxPacket::Network(packet) => self.handle_rx_packet(
                    timestamp,
                    filter,
                    packet,
                    accepted_packets,
                    control_packets,
                ),
                RxPacket::LocalUdp(packet) => self.resume_local_udp(packet, control_packets),
            }
        }
    }

    fn handle_rx_packet(
        &mut self,
        timestamp: MonotonicInstant,
        filter: &FilterConfig,
        packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) {
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        match ipv4::ip_version(ip_packet) {
            Some(4) => {
                self.handle_ipv4_input(timestamp, filter, packet, accepted_packets, control_packets)
            }
            Some(6) => {
                accepted_packets.push(packet);
            }
            _ => debug!("Dropping packet with invalid IP version"),
        }
    }

    fn handle_ipv4_input(
        &mut self,
        timestamp: MonotonicInstant,
        filter: &FilterConfig,
        mut packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) {
        if crate::ipv4::validate_input_packet(&mut packet).is_err() {
            return;
        }
        let header =
            Ipv4Header::parse_header_only(packet.network_packet().expect("validated packet"))
                .expect("validated IPv4 header");

        if !self.is_local_ipv4_destination(header.dst_addr()) {
            return;
        }

        if header.is_fragmented() {
            match self.ipv4_reassembler.reassemble(packet, timestamp) {
                Ipv4ReassemblyResult::Complete(packet) => self.handle_ipv4_input(
                    timestamp,
                    filter,
                    packet,
                    accepted_packets,
                    control_packets,
                ),
                Ipv4ReassemblyResult::Pending | Ipv4ReassemblyResult::Dropped => {}
            }
            return;
        }

        let protocol = header.protocol();
        let is_broadcast_or_multicast = header.is_broadcast_or_multicast();
        let emit_protocol_unreachable = should_emit_protocol_unreachable(header);
        let indev = self.device_names.get(&packet.ifindex());
        if !indev.is_some_and(|name| filter.accept_input(&packet, name)) {
            return;
        }

        if protocol == ipv4::PROTOCOL_UDP {
            if let Ok(packet) = udp::prepare_ipv4_packet(packet) {
                deliver_local_udp(packet, is_broadcast_or_multicast, control_packets);
            }
            return;
        }

        if emit_protocol_unreachable {
            if let Some(ip_packet) = packet.network_packet() {
                queue_icmpv4_error(
                    control_packets,
                    Icmpv4Error::ProtocolUnreachable,
                    packet.packet_type(),
                    ip_packet,
                );
            }
            return;
        }

        udp_err::inspect_icmpv4_error(&packet);
        accepted_packets.push(packet);
    }

    fn resume_local_udp(&self, packet: udp::PreparedUdpPacket, control_packets: &mut Vec<Vec<u8>>) {
        let Some(header) = packet
            .packet()
            .network_packet()
            .and_then(|bytes| Ipv4Header::parse_header_only(bytes).ok())
        else {
            return;
        };
        if self.is_local_ipv4_destination(header.dst_addr()) {
            let is_broadcast_or_multicast = header.is_broadcast_or_multicast();
            deliver_local_udp(packet, is_broadcast_or_multicast, control_packets);
        }
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

fn deliver_local_udp(
    packet: udp::PreparedUdpPacket,
    is_broadcast_or_multicast: bool,
    control_packets: &mut Vec<Vec<u8>>,
) {
    let (disposition, returned) = udp::deliver_ipv4_packet(packet);
    if disposition == udp::InputDisposition::NoSocket && !is_broadcast_or_multicast {
        let Some(packet) = returned else {
            return;
        };
        if let Some(bytes) = packet.packet().network_packet() {
            queue_icmpv4_error(
                control_packets,
                Icmpv4Error::PortUnreachable,
                packet.packet().packet_type(),
                bytes,
            );
        }
    }
}

/// Updates TCP listener state before accepted packets enter smoltcp.
///
/// UDP delivery has already completed before this function takes the shared
/// smoltcp socket-set lock. This keeps custom UDP ingress independent from
/// unrelated TCP socket operations while preserving listener setup before a
/// TCP SYN reaches smoltcp.
pub(crate) fn prepare_smoltcp_ingress(packets: &[PacketBuf], sockets: &mut SocketSetState<'_>) {
    for packet in packets {
        let Some(ip_packet) = packet.network_packet() else {
            continue;
        };
        match ipv4::ip_version(ip_packet) {
            Some(4) => {
                let Ok(header) = ipv4::Ipv4Header::parse_input(ip_packet) else {
                    continue;
                };
                let Some(payload) = packet.transport_packet() else {
                    continue;
                };
                snoop_tcp_segment(
                    header.protocol(),
                    SmoltcpIpAddress::Ipv4(header.src_addr().into()),
                    SmoltcpIpAddress::Ipv4(header.dst_addr().into()),
                    payload,
                    sockets,
                );
            }
            Some(6) => snoop_ipv6_tcp_packet(ip_packet, sockets),
            _ => {}
        }
    }
}

fn queue_icmpv4_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: Icmpv4Error,
    packet_type: crate::buf::PacketType,
    offending_packet: &[u8],
) {
    let Some(packet) = ipv4::build_icmpv4_error_packet(error, packet_type, offending_packet) else {
        return;
    };
    control_packets.push(packet);
}

fn should_emit_protocol_unreachable(header: Ipv4Header<'_>) -> bool {
    !header.is_fragmented()
        && !header.is_broadcast_or_multicast()
        && !matches!(
            header.protocol(),
            ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP | ipv4::PROTOCOL_ICMP
        )
}

fn snoop_ipv6_tcp_packet(buf: &[u8], sockets: &mut SocketSetState<'_>) {
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
    sockets: &mut SocketSetState<'_>,
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
