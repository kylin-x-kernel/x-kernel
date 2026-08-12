// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing table and route selection.
use alloc::{boxed::Box, collections::VecDeque, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ktime_types::MonotonicInstant;
use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
    wire::{IpAddress as SmoltcpIpAddress, Ipv6Packet},
};

use super::ipv4::{self, Ipv4Error, Ipv4Header};
use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::NetDevice,
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    netlink::{RT_TABLE_MAIN, RTN_UNICAST, RtnetlinkState},
};

const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;

#[derive(Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
    pub mtu: usize,
}

impl Rule {
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: usize, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            mtu: STANDARD_MTU,
        }
    }
}

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

    pub fn remove_device(&mut self, dev: usize) {
        self.rules.retain(|rule| rule.dev != dev);
        for rule in &mut self.rules {
            if rule.dev > dev {
                rule.dev -= 1;
            }
        }
    }
}

pub struct Router {
    rx_queue: VecDeque<PacketBuf>,
    control_tx_queue: VecDeque<PacketBuf>,
    data_tx_queue: VecDeque<PacketBuf>,
    next_rx_device: usize,
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    next_ipv4_identification: u16,
    pub(crate) devices: Vec<Box<dyn NetDevice>>,
    pub(crate) table: RouteTable,
}

pub(crate) struct RxDrain {
    pub work_done: usize,
    pub has_more: bool,
}
impl Router {
    pub fn new() -> Self {
        Self {
            rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            control_tx_queue: VecDeque::with_capacity(CONTROL_TX_QUEUE_SIZE),
            data_tx_queue: VecDeque::with_capacity(DATA_TX_QUEUE_SIZE),
            next_rx_device: 0,
            local_ipv4_addrs: Vec::new(),
            next_ipv4_identification: 1,
            devices: Vec::new(),
            table: RouteTable::new(),
        }
    }

    pub fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        self.table.lookup(dst).map(|rule| rule.mtu)
    }

    pub fn can_enqueue_tx_packet(&self) -> bool {
        self.data_tx_queue.len() < DATA_TX_QUEUE_SIZE
    }

    pub(crate) fn available_tx_packet_slots(&self) -> usize {
        DATA_TX_QUEUE_SIZE.saturating_sub(self.data_tx_queue.len())
    }

    pub fn can_enqueue_tx_packets(&self, packet_count: usize) -> bool {
        self.data_tx_queue
            .len()
            .checked_add(packet_count)
            .is_some_and(|len| len <= DATA_TX_QUEUE_SIZE)
    }

    pub fn queue_ipv4_packet(&mut self, packet: Vec<u8>) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        if !self.can_enqueue_tx_packets(packets.len()) {
            return Err(KError::WouldBlock);
        }

        self.data_tx_queue.extend(
            packets
                .into_iter()
                .map(|packet| PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)),
        );
        Ok(())
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(device);
        self.devices.len() - 1
    }

    pub fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> bool {
        let Some(pos) = self
            .devices
            .iter()
            .position(|device| device.device_id() == Some(id))
        else {
            return false;
        };
        self.devices.remove(pos);
        self.table.remove_device(pos);
        self.adjust_rx_device_cursor_after_remove(pos);
        true
    }

    pub fn local_ipv4_addrs(&self) -> &[Ipv4Cidr] {
        &self.local_ipv4_addrs
    }

    pub fn sync_netlink(&mut self, state: &RtnetlinkState) {
        for (dev_index, device) in self.devices.iter_mut().enumerate() {
            let ifindex = dev_index as i32 + 1;
            let link = state.links.iter().find(|link| link.index == ifindex);
            let ipv4_addr = state
                .addrs
                .iter()
                .find(|addr| addr.index == ifindex as u32 && addr.family == 2)
                .and_then(|addr_state| match addr_state.address {
                    SmoltcpIpAddress::Ipv4(addr) => {
                        Some(Ipv4Cidr::new(addr.into(), addr_state.prefix_len))
                    }
                    SmoltcpIpAddress::Ipv6(_) => None,
                });
            let neighbors: Vec<_> = state
                .neighs
                .iter()
                .filter(|neigh| neigh.ifindex == ifindex as u32)
                .filter_map(|neigh| {
                    let hardware_addr = neigh.lladdr?;
                    Some((super::from_smoltcp_ip_address(neigh.dst), hardware_addr))
                })
                .collect();
            device.sync_netlink(link.map(|link| link.name.as_str()), ipv4_addr, &neighbors);
        }

        self.table.clear();
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs
            .extend(state.addrs.iter().filter_map(|addr| match addr.address {
                SmoltcpIpAddress::Ipv4(ipv4) => Some(Ipv4Cidr::new(ipv4.into(), addr.prefix_len)),
                SmoltcpIpAddress::Ipv6(_) => None,
            }));
        for route in state.routes.iter().filter(|route| {
            route.family == 2 && route.table == RT_TABLE_MAIN && route.route_type == RTN_UNICAST
        }) {
            let dev = route.oif.saturating_sub(1) as usize;
            if dev >= self.devices.len() {
                continue;
            }

            let filter = match route.dst {
                Some(SmoltcpIpAddress::Ipv4(addr)) => {
                    IpCidr::Ipv4(Ipv4Cidr::new(addr.into(), route.dst_len))
                }
                None => IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, route.dst_len)),
                Some(SmoltcpIpAddress::Ipv6(_)) => continue,
            };
            let src = route
                .prefsrc
                .or_else(|| {
                    state.addrs.iter().find_map(|addr| {
                        (addr.index == route.oif && addr.family == 2).then_some(addr.address)
                    })
                })
                .map(super::from_smoltcp_ip_address)
                .unwrap_or(filter.address());
            let mut rule = Rule::new(
                filter,
                route.gateway.map(super::from_smoltcp_ip_address),
                dev,
                src,
            );
            rule.mtu = state
                .links
                .iter()
                .find(|link| link.index == route.oif as i32)
                .map_or(STANDARD_MTU, |link| link.mtu as usize);
            self.table.add_rule(rule);
        }
    }

    pub fn drain_rx_budgeted_into(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<PacketBuf>,
    ) -> RxDrain {
        packets.clear();
        if budget == 0 || self.devices.is_empty() {
            return RxDrain {
                work_done: 0,
                has_more: self.has_immediate_rx_work(),
            };
        }

        let mut work_done = 0;
        let device_count = self.devices.len();
        if self.next_rx_device >= device_count {
            self.next_rx_device = 0;
        }

        for _ in 0..device_count {
            let dev_index = self.next_rx_device;
            let ifindex = dev_index as i32 + 1;
            loop {
                if work_done >= budget {
                    self.next_rx_device = next_device_index(dev_index, device_count);
                    return RxDrain {
                        work_done,
                        has_more: self.has_immediate_rx_work(),
                    };
                }

                let packet = {
                    let dev = &mut self.devices[dev_index];
                    dev.poll_rx(ifindex, timestamp)
                };
                let Some(packet) = packet else {
                    break;
                };
                work_done += 1;
                packets.push(packet);
            }
            self.next_rx_device = next_device_index(dev_index, device_count);
        }

        RxDrain {
            work_done,
            has_more: false,
        }
    }

    pub fn enqueue_ingress_packets(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(packets.len() <= self.ingress_capacity());
        self.rx_queue.extend(packets.drain(..));
    }

    pub fn ingress_capacity(&self) -> usize {
        SOCKET_BUFFER_SIZE.saturating_sub(self.rx_queue.len())
    }

    pub fn has_pending_ingress(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    pub fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        if ifindex <= 0 {
            return Err(KError::InvalidInput);
        }
        let dev_index = (ifindex - 1) as usize;
        let dev = self
            .devices
            .get_mut(dev_index)
            .ok_or(KError::from(LinuxError::ENODEV))?;
        dev.send_link_frame(ifindex, frame)
    }

    pub fn dispatch_budgeted(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
    ) -> (usize, bool) {
        if budget == 0 {
            return (0, self.has_queued_tx_packets());
        }

        let mut work_done = 0;
        let mut poll_next = false;
        while work_done < budget {
            let Some(mut packet) = self.pop_tx_packet() else {
                break;
            };
            work_done += 1;
            packet.set_owner(PacketOwner::DeviceTx);
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => {
                    poll_next |= self.dispatch_ipv4_packet(packet, timestamp);
                }
                Some(6) => {
                    let (src_addr, dst_addr, is_multicast) = {
                        let ip_packet =
                            Ipv6Packet::new_checked(ip_packet).expect("got invalid IPv6 packet");
                        (
                            IpAddress::Ipv6(ip_packet.src_addr().into()),
                            IpAddress::Ipv6(ip_packet.dst_addr().into()),
                            ip_packet.dst_addr().is_multicast(),
                        )
                    };
                    if is_multicast {
                        for (dev_index, dev) in self.devices.iter_mut().enumerate() {
                            poll_next |= dev.send_ip_packet(
                                dev_index as i32 + 1,
                                dst_addr,
                                packet.clone(),
                                timestamp,
                            );
                        }
                    } else {
                        let Some(rule) = self.table.lookup(&dst_addr) else {
                            warn!("No route found for destination: {}", dst_addr);
                            continue;
                        };
                        assert_eq!(rule.src, src_addr);

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let dev = &mut self.devices[rule.dev];
                        poll_next |=
                            dev.send_ip_packet(rule.dev as i32 + 1, next_hop, packet, timestamp);
                    }
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        (work_done, poll_next || self.has_queued_tx_packets())
    }

    fn dispatch_ipv4_packet(&mut self, mut packet: PacketBuf, timestamp: MonotonicInstant) -> bool {
        let header = match Ipv4Header::prepare_output_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return false,
        };

        let src_addr = IpAddress::Ipv4(header.src_addr());
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_broadcast() {
            if !self.is_valid_ipv4_broadcast_source(header.src_addr()) {
                warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
                return false;
            }

            let device_count = self.devices.len();
            let Some((last_device, preceding_devices)) = self.devices.split_last_mut() else {
                return false;
            };

            let mut poll_next = false;
            for (dev_index, dev) in preceding_devices.iter_mut().enumerate() {
                poll_next |=
                    dev.send_ip_packet(dev_index as i32 + 1, dst_addr, packet.clone(), timestamp);
            }
            poll_next |=
                last_device.send_ip_packet(device_count as i32, dst_addr, packet, timestamp);
            return poll_next;
        }

        let Some(rule) = self.table.lookup(&dst_addr) else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        if !self.is_local_ipv4_source(header.src_addr()) {
            warn!(
                "Dropping IPv4 packet with non-local source {} routed via {}",
                src_addr, rule.src
            );
            return false;
        }

        let next_hop = rule.via.unwrap_or(dst_addr);
        let dev = &mut self.devices[rule.dev];
        dev.send_ip_packet(rule.dev as i32 + 1, next_hop, packet, timestamp)
    }

    fn fragment_ipv4_packet_for_output(&mut self, packet: Vec<u8>) -> KResult<Vec<Vec<u8>>> {
        let header = Ipv4Header::parse_output(&packet).map_err(|_| KError::InvalidInput)?;
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        let Some(mtu) = self.route_mtu(&dst_addr) else {
            return Ok(vec![packet]);
        };
        if packet.len() <= mtu {
            return Ok(vec![packet]);
        }

        let identification = self.next_ipv4_identification;
        self.next_ipv4_identification = self.next_ipv4_identification.wrapping_add(1).max(1);
        ipv4::fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
            ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
            ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
                LinuxError::EMSGSIZE.into()
            }
        })
    }

    fn is_local_ipv4_source(&self, src_addr: crate::ip::Ipv4Address) -> bool {
        self.local_ipv4_addrs
            .iter()
            .any(|addr| addr.address() == src_addr)
    }

    fn is_valid_ipv4_broadcast_source(&self, src_addr: crate::ip::Ipv4Address) -> bool {
        src_addr.is_unspecified() || self.is_local_ipv4_source(src_addr)
    }

    pub fn queue_control_ipv4_packet(&mut self, packet: Vec<u8>) -> KResult {
        if self.control_tx_queue.len() >= CONTROL_TX_QUEUE_SIZE {
            return Err(KError::WouldBlock);
        }
        self.control_tx_queue.push_back(tx_packet_buf(packet));
        Ok(())
    }

    fn pop_tx_packet(&mut self) -> Option<PacketBuf> {
        self.control_tx_queue
            .pop_front()
            .or_else(|| self.data_tx_queue.pop_front())
    }

    fn has_queued_tx_packets(&self) -> bool {
        !self.control_tx_queue.is_empty() || !self.data_tx_queue.is_empty()
    }

    pub(crate) fn has_immediate_rx_work(&self) -> bool {
        self.devices.iter().any(|device| device.has_rx_work())
    }

    fn adjust_rx_device_cursor_after_remove(&mut self, removed_index: usize) {
        if self.devices.is_empty() {
            self.next_rx_device = 0;
        } else if removed_index < self.next_rx_device {
            self.next_rx_device -= 1;
        } else if self.next_rx_device >= self.devices.len() {
            self.next_rx_device = 0;
        }
    }
}

fn next_device_index(dev_index: usize, device_count: usize) -> usize {
    debug_assert!(device_count > 0);
    (dev_index + 1) % device_count
}

fn tx_packet_buf(packet: Vec<u8>) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use ktime_types::MonotonicInstant;
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::device::LoopbackDevice;

    fn router_with_ready_loopback_packet() -> Router {
        let mut router = Router::new();
        let loopback = router.add_device(Box::new(LoopbackDevice::new()));
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
        let _ = router.devices[loopback].send_ip_packet(
            1,
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        );
        router
    }

    #[def_test]
    fn exact_rx_budget_does_not_report_empty_device() {
        let mut router = router_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_more);
    }

    #[def_test]
    fn zero_rx_budget_reports_ready_device() {
        let mut router = router_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 0, &mut packets);

        assert_eq!(drain.work_done, 0);
        assert!(drain.has_more);
    }

    #[def_test]
    fn control_dispatch_does_not_free_data_tx_capacity() {
        let mut router = Router::new();
        router
            .control_tx_queue
            .push_back(tx_packet_buf(vec![0; 20]));
        let capacity_before = router.available_tx_packet_slots();

        let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(router.available_tx_packet_slots(), capacity_before);
    }

    #[def_test]
    fn data_dispatch_frees_data_tx_capacity() {
        let mut router = Router::new();
        router.data_tx_queue.push_back(tx_packet_buf(vec![0; 20]));
        let capacity_before = router.available_tx_packet_slots();

        let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(router.available_tx_packet_slots(), capacity_before + 1);
    }
}

pub struct TxToken<'a>(&'a mut VecDeque<PacketBuf>);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        debug_assert!(self.0.len() < SOCKET_BUFFER_SIZE);

        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        self.0.push_back(PacketBuf::from_ip_packet_vec(
            0,
            ip_packet,
            PacketOwner::Ipv4Stack,
        ));
        result
    }
}

pub struct RxToken(PacketBuf);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self
            .0
            .network_packet()
            .expect("Router RX queue only contains IP packets"))
    }
}

impl smoltcp::phy::Device for Router {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_queue.is_empty() || self.data_tx_queue.len() >= DATA_TX_QUEUE_SIZE {
            None
        } else {
            Some((
                RxToken(self.rx_queue.pop_front().unwrap()),
                TxToken(&mut self.data_tx_queue),
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.data_tx_queue.len() >= DATA_TX_QUEUE_SIZE {
            None
        } else {
            Some(TxToken(&mut self.data_tx_queue))
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
