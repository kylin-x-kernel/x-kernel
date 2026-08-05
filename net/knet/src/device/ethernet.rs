// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ethernet device adapter.
use alloc::{collections::VecDeque, string::String, vec::Vec};

use device_res::IrqEventSource;
use hashbrown::HashMap;
use kclass::{ClassDevice, prelude::*};
use kerrno::{KError, KResult, LinuxError};
use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::ETHERNET_MAX_PENDING_PACKETS,
    device::NetDevice as NetDeviceOps,
    ip::{IpAddress, Ipv4Cidr},
    packet,
    wire::{
        ArpIpv4Packet, ArpOperation, ETHERNET_HEADER_LEN, EtherType, EthernetFrameRef, MacAddress,
        emit_ethernet_header,
    },
};

const NET_RX_IRQ_SOURCE: IrqEventSource = 0;

/// ARP table entry mapping an IP address to a MAC address.
struct ArpNeighbor {
    hardware_address: MacAddress,
    /// When this entry expires (TTL = 300s).
    expires_at: MonotonicInstant,
}

/// Queued IP packet awaiting ARP resolution.
struct PendingTxPacket {
    next_hop: IpAddress,
    packet: PacketBuf,
}

/// Ethernet device backed by a driver-provided NIC.
pub struct EthernetDevice {
    #[allow(dead_code)]
    name: String,
    inner: ClassDevice<NetDeviceImpl>,
    neighbors: HashMap<IpAddress, Option<ArpNeighbor>>,
    ip: Ipv4Cidr,

    pending_tx: VecDeque<PendingTxPacket>,
}

impl EthernetDevice {
    const NEIGHBOR_TTL: TimeSpan = TimeSpan::from_secs(300);

    /// Create a new Ethernet device wrapper.
    pub fn new(name: String, inner: ClassDevice<NetDeviceImpl>, ip: Ipv4Cidr) -> Self {
        Self {
            name,
            inner,
            neighbors: HashMap::new(),
            ip,
            pending_tx: VecDeque::with_capacity(ETHERNET_MAX_PENDING_PACKETS),
        }
    }

    /// Spawn the RX poll task for an initialized network stack.
    pub(crate) fn spawn_rx_task(irq: usize) {
        let _ = ktask::spawn_with_name(
            move || {
                use core::{future::poll_fn, task::Poll};

                use kpoll::PollRegistrations;

                let mut registrations = PollRegistrations::new();
                ktask::future::block_on(poll_fn(move |cx| {
                    loop {
                        crate::poll_interfaces();
                        let mut context = registrations.context(cx);
                        if irq_notify::register_source_waker(irq, NET_RX_IRQ_SOURCE, &mut context)
                            .is_err()
                        {
                            drop(context);
                            // Sleeping without an IRQ registration would stall
                            // RX forever; yield and retry under memory pressure.
                            ktask::yield_now();
                            continue;
                        }
                        drop(context);
                        // Recheck after register to close the IRQ/register race.
                        crate::poll_interfaces();
                        return Poll::<()>::Pending;
                    }
                }));
            },
            "knet-rx".into(),
        );
    }

    #[inline]
    fn mac_addr(&self) -> MacAddress {
        MacAddress(self.inner.mac().0)
    }

    fn send_frame<F>(
        inner: &dyn NetDevice,
        dst: MacAddress,
        size: usize,
        f: F,
        proto: EtherType,
    ) -> Option<([u8; 6], Vec<u8>)>
    where
        F: FnOnce(&mut [u8]),
    {
        if let Err(err) = inner.recycle_tx() {
            warn!("recycle_tx failed: {:?}", err);
            return None;
        }

        let src_addr = MacAddress(inner.mac().0);
        let mut tx_buf: NetBufHandle = match inner.alloc_tx_buf(ETHERNET_HEADER_LEN + size) {
            Ok(buf) => buf,
            Err(err) => {
                warn!("alloc_tx_buf failed: {:?}", err);
                return None;
            }
        };
        let Some(payload) = emit_ethernet_header(tx_buf.data_mut(), dst, src_addr, proto) else {
            warn!("alloc_tx_buf returned a short Ethernet buffer");
            return None;
        };
        f(payload);
        let frame_data = packet::has_packet_handlers().then(|| tx_buf.data().to_vec());
        trace!("SEND {} bytes: {:02X?}", tx_buf.len(), tx_buf.data());
        if let Err(err) = inner.send(tx_buf) {
            warn!("send failed: {:?}", err);
            None
        } else {
            frame_data.map(|frame_data| (src_addr.bytes(), frame_data))
        }
    }

    fn send_to<F>(&mut self, ifindex: i32, dst: MacAddress, size: usize, f: F, proto: EtherType)
    where
        F: FnOnce(&mut [u8]),
    {
        let sent = Self::send_frame(&self.inner, dst, size, f, proto);
        if let Some((src_addr, frame_data)) = sent {
            packet::publish_outgoing_frame(ifindex, &frame_data, src_addr);
        }
    }

    fn send_ipv4_packet_to(&mut self, ifindex: i32, dst: MacAddress, packet: &PacketBuf) {
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        self.send_to(
            ifindex,
            dst,
            ip_packet.len(),
            |buf| buf.copy_from_slice(ip_packet),
            EtherType::Ipv4,
        );
    }

    fn handle_rx_frame(
        &mut self,
        ifindex: i32,
        frame_data: &[u8],
        timestamp: MonotonicInstant,
    ) -> Option<PacketBuf> {
        let Some(frame) = EthernetFrameRef::new_checked(frame_data) else {
            warn!("Dropping malformed Ethernet frame");
            return None;
        };
        let dst_addr = frame.dst_addr();
        let src_addr = frame.src_addr();
        let ethertype = frame.ethertype();

        let packet_buf = PacketBuf::from_ethernet_frame(
            ifindex,
            frame_data,
            dst_addr,
            src_addr,
            ethertype,
            self.mac_addr(),
            PacketOwner::DeviceRx,
        );
        packet::publish_link_packet(&packet_buf);

        if !dst_addr.is_broadcast() && dst_addr != MacAddress::ZERO && dst_addr != self.mac_addr() {
            return None;
        }

        match ethertype {
            EtherType::Ipv4 => {
                return Some(packet_buf);
            }
            EtherType::Arp => self.handle_arp_packet(ifindex, frame.payload(), timestamp),
            _ => {}
        }

        None
    }

    fn send_arp_request(&mut self, ifindex: i32, target_ip: IpAddress) {
        let IpAddress::Ipv4(target_ipv4) = target_ip else {
            warn!("IPv6 address ARP is not supported: {}", target_ip);
            return;
        };
        debug!("Requesting ARP for {}", target_ipv4);

        let arp_packet = ArpIpv4Packet {
            operation: ArpOperation::Request,
            source_hardware_addr: self.mac_addr(),
            source_protocol_addr: self.ip.address(),
            target_hardware_addr: MacAddress::BROADCAST,
            target_protocol_addr: target_ipv4,
        };

        self.send_to(
            ifindex,
            MacAddress::BROADCAST,
            ArpIpv4Packet::LEN,
            |buf| {
                let emit_result = arp_packet.emit(buf);
                debug_assert!(emit_result.is_some());
            },
            EtherType::Arp,
        );

        self.neighbors.insert(target_ip, None);
    }

    fn handle_arp_packet(&mut self, ifindex: i32, payload: &[u8], now: MonotonicInstant) {
        let Some(packet) = ArpIpv4Packet::parse(payload) else {
            debug!("Dropping malformed ARP packet");
            return;
        };

        let ArpIpv4Packet {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        } = packet;
        let is_unicast_mac =
            target_hardware_addr != MacAddress::ZERO && !target_hardware_addr.is_broadcast();
        if is_unicast_mac && self.mac_addr() != target_hardware_addr {
            return;
        }

        if let ArpOperation::Unknown(_) = operation {
            return;
        }

        if !source_hardware_addr.is_unicast()
            || source_protocol_addr.is_broadcast()
            || source_protocol_addr.is_multicast()
            || source_protocol_addr.is_unspecified()
        {
            return;
        }
        if self.ip.address() != target_protocol_addr {
            return;
        }

        debug!("ARP: {} -> {}", source_protocol_addr, source_hardware_addr);
        self.neighbors.insert(
            IpAddress::Ipv4(source_protocol_addr),
            Some(ArpNeighbor {
                hardware_address: source_hardware_addr,
                expires_at: now + Self::NEIGHBOR_TTL,
            }),
        );

        if let ArpOperation::Request = operation {
            let response = ArpIpv4Packet {
                operation: ArpOperation::Reply,
                source_hardware_addr: self.mac_addr(),
                source_protocol_addr: self.ip.address(),
                target_hardware_addr: source_hardware_addr,
                target_protocol_addr: source_protocol_addr,
            };

            self.send_to(
                ifindex,
                source_hardware_addr,
                ArpIpv4Packet::LEN,
                |buf| {
                    let emit_result = response.emit(buf);
                    debug_assert!(emit_result.is_some());
                },
                EtherType::Arp,
            );
        }

        enum PendingAction {
            Send(MacAddress),
            Keep,
            Refresh,
        }

        let mut kept_packets = Vec::with_capacity(ETHERNET_MAX_PENDING_PACKETS);
        for _ in 0..ETHERNET_MAX_PENDING_PACKETS {
            let Some(pending) = self.pending_tx.pop_front() else {
                break;
            };

            let action = match self.neighbors.get(&pending.next_hop) {
                Some(Some(neighbor)) if neighbor.expires_at > now => {
                    PendingAction::Send(neighbor.hardware_address)
                }
                Some(Some(_)) => PendingAction::Refresh,
                _ => PendingAction::Keep,
            };

            match action {
                PendingAction::Send(hardware_address) => {
                    self.send_ipv4_packet_to(ifindex, hardware_address, &pending.packet)
                }
                PendingAction::Keep => kept_packets.push(pending),
                PendingAction::Refresh => {
                    self.neighbors.remove(&pending.next_hop);
                    self.send_arp_request(ifindex, pending.next_hop);
                    kept_packets.push(pending);
                }
            }
        }

        for pending in kept_packets {
            if self.pending_tx.len() >= ETHERNET_MAX_PENDING_PACKETS {
                warn!("Pending packets buffer is full, dropping packet");
            } else {
                self.pending_tx.push_back(pending);
            }
        }
    }
}

impl NetDeviceOps for EthernetDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        Some(self.inner.id())
    }

    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> Option<PacketBuf> {
        loop {
            let rx_buf: NetBufHandle = match self.inner.recv() {
                Ok(buf) => buf,
                Err(err) => {
                    if !matches!(err, DriverError::WouldBlock) {
                        warn!("recv failed: {:?}", err);
                    }
                    return None;
                }
            };
            trace!("RECV {} bytes: {:02X?}", rx_buf.len(), rx_buf.data());

            let packet = self.handle_rx_frame(ifindex, rx_buf.data(), timestamp);
            self.inner.recycle_rx(rx_buf).unwrap();
            if packet.is_some() {
                return packet;
            }
        }
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        if !matches!(next_hop, IpAddress::Ipv4(_)) {
            warn!("Dropping IPv6 packet on IPv4-only Ethernet device");
            return false;
        }

        packet.set_ifindex(ifindex);
        if next_hop.is_broadcast() || self.ip.broadcast().map(IpAddress::Ipv4) == Some(next_hop) {
            self.send_ipv4_packet_to(ifindex, MacAddress::BROADCAST, &packet);
            return false;
        }

        let need_request = match self.neighbors.get(&next_hop) {
            Some(Some(neighbor)) if neighbor.expires_at > timestamp => {
                self.send_ipv4_packet_to(ifindex, neighbor.hardware_address, &packet);
                return false;
            }
            Some(Some(_)) => true,
            // Request already sent
            Some(None) => false,
            None => true,
        };
        // Only send ARP request if we haven't already requested it
        if need_request {
            self.send_arp_request(ifindex, next_hop);
        }
        if self.pending_tx.len() >= ETHERNET_MAX_PENDING_PACKETS {
            warn!("Pending packets buffer is full, dropping packet");
            return false;
        }
        self.pending_tx
            .push_back(PendingTxPacket { next_hop, packet });
        false
    }

    fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        let local_addr = self.mac_addr().0;
        self.inner.recycle_tx().map_err(map_dev_err)?;
        let mut tx_buf = self.inner.alloc_tx_buf(frame.len()).map_err(map_dev_err)?;
        tx_buf.data_mut().copy_from_slice(frame);
        self.inner.send(tx_buf).map_err(map_dev_err)?;
        packet::publish_outgoing_frame(ifindex, frame, local_addr);
        Ok(frame.len())
    }

    fn register_rx_waker(
        &self,
        _source_waker: &core::task::Waker,
        context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        // Ethernet registers each waiting task directly on its IRQ source;
        // the aggregated `source_waker` is only used by single-waker devices.
        if let Some(irq) = self.inner.irq() {
            irq_notify::register_source_waker(irq, NET_RX_IRQ_SOURCE, context)?;
        }
        Ok(())
    }

    fn sync_netlink(
        &mut self,
        name: Option<&str>,
        ipv4_addr: Option<Ipv4Cidr>,
        neighbors: &[(IpAddress, [u8; 6])],
    ) {
        if let Some(name) = name {
            self.name = name.into();
        }

        if let Some(ipv4_addr) = ipv4_addr {
            self.ip = ipv4_addr;
        }

        self.neighbors.clear();
        for (dst_addr, hardware_addr) in neighbors {
            self.neighbors.insert(
                *dst_addr,
                Some(ArpNeighbor {
                    hardware_address: MacAddress(*hardware_addr),
                    expires_at: MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(
                        u64::MAX / 2,
                    )),
                }),
            );
        }
    }
}

fn map_dev_err(err: DriverError) -> KError {
    match err {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::NoMemory => LinuxError::ENOBUFS.into(),
        DriverError::Io => KError::Io,
        _ => KError::BadState,
    }
}
