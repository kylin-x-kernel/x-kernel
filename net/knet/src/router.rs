// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing table and route selection.
use alloc::{boxed::Box, vec, vec::Vec};

use smoltcp::{
    iface::SocketSet,
    phy::{DeviceCapabilities, Medium},
    storage::PacketMetadata,
    time::Instant,
    wire::{
        IpAddress, IpCidr, IpProtocol, IpVersion, Ipv4Address, Ipv4Cidr, Ipv4Packet, Ipv6Packet,
        TcpPacket,
    },
};

use crate::{
    LISTEN_TABLE,
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::NetDevice,
    netlink::{RT_TABLE_MAIN, RTN_UNICAST, RtnetlinkState},
    udp_err,
};

#[derive(Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
}

impl Rule {
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: usize, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
        }
    }
}

type PacketBuffer = smoltcp::storage::PacketBuffer<'static, ()>;

// TODO(mivik): optimize
pub struct RouteTable {
    rules: Vec<Rule>,
}
impl RouteTable {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, rule: Rule) {
        let idx = self
            .rules
            .binary_search_by(|it| rule.filter.prefix_len().cmp(&it.filter.prefix_len()))
            .unwrap_or_else(|idx| idx);
        self.rules.insert(idx, rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }

    pub fn clear(&mut self) {
        self.rules.clear();
    }
}

pub struct Router {
    rx_buffer: PacketBuffer,
    tx_buffer: PacketBuffer,
    pub(crate) devices: Vec<Box<dyn NetDevice>>,
    pub(crate) table: RouteTable,
}
impl Router {
    pub fn new() -> Self {
        let rx_buffer = PacketBuffer::new(
            vec![PacketMetadata::EMPTY; SOCKET_BUFFER_SIZE],
            vec![0u8; STANDARD_MTU * SOCKET_BUFFER_SIZE],
        );
        let tx_buffer = PacketBuffer::new(
            vec![PacketMetadata::EMPTY; SOCKET_BUFFER_SIZE],
            vec![0u8; STANDARD_MTU * SOCKET_BUFFER_SIZE],
        );
        Self {
            rx_buffer,
            tx_buffer,
            devices: Vec::new(),
            table: RouteTable::new(),
        }
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(device);
        self.devices.len() - 1
    }

    pub fn sync_netlink(&mut self, state: &RtnetlinkState) {
        for (dev_index, device) in self.devices.iter_mut().enumerate() {
            let ifindex = dev_index as i32 + 1;
            let link = state.links.iter().find(|link| link.index == ifindex);
            let addrs: Vec<_> = state
                .addrs
                .iter()
                .copied()
                .filter(|addr| addr.index == ifindex as u32)
                .collect();
            let neighs: Vec<_> = state
                .neighs
                .iter()
                .copied()
                .filter(|neigh| neigh.ifindex == ifindex as u32)
                .collect();
            device.sync_netlink(link, &addrs, &neighs);
        }

        self.table.clear();
        for route in state.routes.iter().filter(|route| {
            route.family == 2 && route.table == RT_TABLE_MAIN && route.route_type == RTN_UNICAST
        }) {
            let dev = route.oif.saturating_sub(1) as usize;
            if dev >= self.devices.len() {
                continue;
            }

            let filter = match route
                .dst
                .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED))
            {
                IpAddress::Ipv4(addr) => IpCidr::Ipv4(Ipv4Cidr::new(addr, route.dst_len)),
                IpAddress::Ipv6(_) => continue,
            };
            let src = route
                .prefsrc
                .or_else(|| {
                    state.addrs.iter().find_map(|addr| {
                        (addr.index == route.oif && addr.family == 2).then_some(addr.address)
                    })
                })
                .unwrap_or(filter.address());
            self.table
                .add_rule(Rule::new(filter, route.gateway, dev, src));
        }
    }

    pub fn poll(&mut self, timestamp: Instant, sockets: &mut SocketSet<'_>) {
        for dev in &mut self.devices {
            let mut packet_snoop = |packet: &[u8]| {
                snoop_udp_error_packet(packet);
                snoop_tcp_packet(packet, sockets);
            };
            while !self.rx_buffer.is_full()
                && dev.poll_rx(&mut self.rx_buffer, timestamp, &mut packet_snoop)
            {}
        }
    }

    pub fn dispatch(&mut self, timestamp: Instant) -> bool {
        let mut poll_next = false;
        while let Ok(((), ip_packet)) = self.tx_buffer.dequeue() {
            match IpVersion::of_packet(ip_packet).expect("got invalid IP packet") {
                IpVersion::Ipv4 => {
                    let ip_packet = smoltcp::wire::Ipv4Packet::new_checked(ip_packet)
                        .expect("got invalid IPv4 packet");
                    let dst_addr = IpAddress::Ipv4(ip_packet.dst_addr());
                    if ip_packet.dst_addr().is_broadcast() {
                        let buf = ip_packet.into_inner();
                        for dev in &mut self.devices {
                            poll_next |= dev.send_ip_packet(dst_addr, buf, timestamp);
                        }
                    } else {
                        let Some(rule) = self.table.lookup(&dst_addr) else {
                            warn!("No route found for destination: {}", dst_addr);
                            continue;
                        };
                        assert_eq!(rule.src, IpAddress::Ipv4(ip_packet.src_addr()));

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let dev = &mut self.devices[rule.dev];
                        poll_next |=
                            dev.send_ip_packet(next_hop, ip_packet.into_inner(), timestamp);
                    }
                }
                IpVersion::Ipv6 => {
                    let ip_packet = smoltcp::wire::Ipv6Packet::new_checked(ip_packet)
                        .expect("got invalid IPv6 packet");
                    let dst_addr = IpAddress::Ipv6(ip_packet.dst_addr());
                    if ip_packet.dst_addr().is_multicast() {
                        let buf = ip_packet.into_inner();
                        for dev in &mut self.devices {
                            poll_next |= dev.send_ip_packet(dst_addr, buf, timestamp);
                        }
                    } else {
                        let Some(rule) = self.table.lookup(&dst_addr) else {
                            warn!("No route found for destination: {}", dst_addr);
                            continue;
                        };
                        assert_eq!(rule.src, IpAddress::Ipv6(ip_packet.src_addr()));

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let dev = &mut self.devices[rule.dev];
                        poll_next |=
                            dev.send_ip_packet(next_hop, ip_packet.into_inner(), timestamp);
                    }
                }
            }
        }
        poll_next
    }
}

pub struct TxToken<'a>(&'a mut PacketBuffer);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self
            .0
            .enqueue(len, ())
            .expect("This was checked before creating the TxToken"))
    }
}

fn parse_ip_packet(buf: &[u8]) -> Option<(IpProtocol, IpAddress, IpAddress, &[u8])> {
    let version = IpVersion::of_packet(buf).unwrap_or(IpVersion::Ipv4);
    match version {
        IpVersion::Ipv4 => {
            let ip_packet = Ipv4Packet::new_checked(buf).ok()?;
            Some((
                ip_packet.next_header(),
                IpAddress::Ipv4(ip_packet.src_addr()),
                IpAddress::Ipv4(ip_packet.dst_addr()),
                ip_packet.payload(),
            ))
        }
        IpVersion::Ipv6 => {
            let ip_packet = Ipv6Packet::new_checked(buf).ok()?;
            Some((
                ip_packet.next_header(),
                IpAddress::Ipv6(ip_packet.src_addr()),
                IpAddress::Ipv6(ip_packet.dst_addr()),
                ip_packet.payload(),
            ))
        }
    }
}

fn snoop_tcp_packet(buf: &[u8], sockets: &mut SocketSet<'_>) {
    let Some((protocol, src_addr, dst_addr, payload)) = parse_ip_packet(buf) else {
        return;
    };
    if protocol == IpProtocol::Tcp {
        let Ok(tcp_packet) = TcpPacket::new_checked(payload) else {
            return;
        };
        let src_addr = (src_addr, tcp_packet.src_port()).into();
        let dst_addr = (dst_addr, tcp_packet.dst_port()).into();
        let is_first = tcp_packet.syn() && !tcp_packet.ack();
        LISTEN_TABLE.note_tcp_packet(dst_addr);
        if is_first {
            LISTEN_TABLE.incoming_tcp_packet(src_addr, dst_addr, sockets);
        }
    }
}

fn snoop_udp_error_packet(buf: &[u8]) {
    match IpVersion::of_packet(buf).unwrap_or(IpVersion::Ipv4) {
        IpVersion::Ipv4 => udp_err::inspect_icmpv4_error(buf),
        IpVersion::Ipv6 => {
            // UDP error queue currently supports only ICMPv4 error delivery.
        }
    }
}

pub struct RxToken<'a>(&'a [u8]);

impl<'a> smoltcp::phy::RxToken for RxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.0)
    }
}

impl smoltcp::phy::Device for Router {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_buffer.is_empty() || self.tx_buffer.is_full() {
            None
        } else {
            Some((
                RxToken(self.rx_buffer.dequeue().unwrap().1),
                TxToken(&mut self.tx_buffer),
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx_buffer.is_full() {
            None
        } else {
            Some(TxToken(&mut self.tx_buffer))
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = STANDARD_MTU;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}
