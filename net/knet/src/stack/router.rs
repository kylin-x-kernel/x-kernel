// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing table and route selection.
use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ktime_types::MonotonicInstant;
use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
    wire::Ipv6Packet,
};

use super::ipv4::{self, Ipv4Error, Ipv4Header};
pub(crate) use crate::netlink::{
    RT_SCOPE_HOST as ROUTE_SCOPE_HOST, RT_SCOPE_LINK as ROUTE_SCOPE_LINK,
    RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE, RT_TABLE_MAIN as ROUTE_TABLE_MAIN,
    RTN_UNICAST as ROUTE_TYPE_UNICAST, RTPROT_BOOT as ROUTE_PROTOCOL_BOOT,
    RTPROT_KERNEL as ROUTE_PROTOCOL_KERNEL,
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::{
        LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate, NetDevice,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
};

const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ipv4AddressRouteKind {
    Local,
    Connected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleSource {
    DevicePrimary,
    Fixed(IpAddress),
}

/// Conditions under which a control-plane neighbor update may be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborUpdatePolicy {
    pub(crate) can_create: bool,
    pub(crate) can_replace: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleOrigin {
    Configured,
    Ipv4Address {
        owner_dev: usize,
        addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    #[expect(dead_code)]
    pub prefsrc: Option<IpAddress>,
    source: RuleSource,
    origin: RuleOrigin,
}

impl Rule {
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: usize, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_BOOT,
            scope: ROUTE_SCOPE_UNIVERSE,
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: Some(src),
            source: RuleSource::Fixed(src),
            origin: RuleOrigin::Configured,
        }
    }

    pub fn with_route_attrs(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: usize,
        src: IpAddress,
        attrs: RouteAttrs,
    ) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            table: attrs.table,
            protocol: attrs.protocol,
            scope: attrs.scope,
            route_type: attrs.route_type,
            prefsrc: attrs.prefsrc,
            source: attrs
                .prefsrc
                .map_or(RuleSource::DevicePrimary, RuleSource::Fixed),
            origin: RuleOrigin::Configured,
        }
    }

    fn for_ipv4_address(
        filter: Ipv4Cidr,
        dev: usize,
        owner: Ipv4AddrEntry,
        kind: Ipv4AddressRouteKind,
        scope: u8,
    ) -> Self {
        let source = IpAddress::Ipv4(owner.addr.address());
        Self {
            filter: filter.into(),
            via: None,
            dev,
            src: source,
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_KERNEL,
            scope,
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: Some(source),
            source: RuleSource::Fixed(source),
            origin: RuleOrigin::Ipv4Address {
                owner_dev: owner.dev,
                addr: owner.addr,
                kind,
            },
        }
    }

    pub(crate) fn preferred_source(self) -> Option<IpAddress> {
        match self.source {
            RuleSource::DevicePrimary => None,
            RuleSource::Fixed(source) => Some(source),
        }
    }

    pub(crate) fn is_configured(self) -> bool {
        matches!(self.origin, RuleOrigin::Configured)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RouteAttrs {
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4AddrEntry {
    pub dev: usize,
    pub addr: Ipv4Cidr,
    pub scope: u8,
    /// Linux `ifa_broadcast` override. `None` means derive from the CIDR.
    pub broadcast: Option<Ipv4Address>,
}

impl Ipv4AddrEntry {
    fn effective_broadcast(self) -> Option<Ipv4Address> {
        self.broadcast.or_else(|| self.addr.broadcast())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Ipv4AddrSnapshot {
    pub entry: Ipv4AddrEntry,
    pub label: String,
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
        let rank = (
            rule.filter.prefix_len(),
            matches!(
                rule.origin,
                RuleOrigin::Ipv4Address {
                    kind: Ipv4AddressRouteKind::Local,
                    ..
                }
            ) as u8,
        );
        let idx = self.rules.partition_point(|existing| {
            let existing_rank = (
                existing.filter.prefix_len(),
                matches!(
                    existing.origin,
                    RuleOrigin::Ipv4Address {
                        kind: Ipv4AddressRouteKind::Local,
                        ..
                    }
                ) as u8,
            );
            existing_rank >= rank
        });
        self.rules.insert(idx, rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn remove_exact_rule(&mut self, route: Rule) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured) || !same_route_rule(*rule, route)
        });
    }

    fn remove_ipv4_address_routes(&mut self) {
        self.rules
            .retain(|rule| matches!(rule.origin, RuleOrigin::Configured));
    }

    fn remove_configured_routes_with_source(&mut self, source: IpAddress) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured)
                || rule.preferred_source() != Some(source)
        });
    }

    pub fn remove_device(&mut self, dev: usize) {
        self.rules.retain(|rule| {
            rule.dev != dev
                && !matches!(
                    rule.origin,
                    RuleOrigin::Ipv4Address { owner_dev, .. } if owner_dev == dev
                )
        });
        for rule in &mut self.rules {
            if rule.dev > dev {
                rule.dev -= 1;
            }
            if let RuleOrigin::Ipv4Address { owner_dev, .. } = &mut rule.origin
                && *owner_dev > dev
            {
                *owner_dev -= 1;
            }
        }
    }
}

pub struct Router {
    rx_queue: VecDeque<PacketBuf>,
    control_tx_queue: VecDeque<PacketBuf>,
    data_tx_queue: VecDeque<PacketBuf>,
    next_rx_device: usize,
    ipv4_addrs: Vec<Ipv4AddrEntry>,
    next_ipv4_identification: u16,
    pub(crate) devices: Vec<Box<dyn NetDevice>>,
    pub(crate) table: RouteTable,
    effective_mtu: usize,
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
            ipv4_addrs: Vec::new(),
            next_ipv4_identification: 1,
            devices: Vec::new(),
            table: RouteTable::new(),
            effective_mtu: STANDARD_MTU,
        }
    }

    pub fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        let rule = self.table.lookup(dst)?;
        self.devices
            .get(rule.dev)
            .map(|device| device.mtu().min(u16::MAX as usize))
    }

    /// Returns the source address for an output route with an active device.
    ///
    /// `oif` is a 1-based bound device index; `0` means the socket is unbound.
    /// Limited broadcasts with a bound device use that device's primary
    /// address (or `0.0.0.0`) without requiring a unicast route.
    ///
    /// # Errors
    ///
    /// Returns `ENETUNREACH` when the route or its device is absent, when
    /// the selected device is administratively down, or when `oif` disagrees
    /// with the looked-up output device.
    pub fn output_route_source(&self, dst: &IpAddress, oif: i32) -> KResult<IpAddress> {
        if dst.is_broadcast() && oif > 0 {
            let device = self
                .devices
                .get((oif - 1) as usize)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            if !device.is_link_up() {
                return Err(LinuxError::ENETUNREACH.into());
            }
            return Ok(self
                .first_ipv4_addr_for_device(oif as u32)
                .map(|cidr| IpAddress::Ipv4(cidr.address()))
                .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)));
        }
        // Unbound limited broadcast still needs a route so the later
        // dispatch flood has a source address.

        let rule = self
            .table
            .lookup(dst)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if oif > 0 && rule.dev != (oif - 1) as usize {
            return Err(KError::from(LinuxError::ENETUNREACH));
        }
        let device = self
            .devices
            .get(rule.dev)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        match rule.source {
            RuleSource::Fixed(source) => Ok(source),
            RuleSource::DevicePrimary => self
                .select_ipv4_source(rule.dev, &rule.via.unwrap_or(*dst), rule.scope)
                .map(IpAddress::Ipv4)
                .ok_or(KError::from(LinuxError::ENETUNREACH)),
        }
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

    pub fn queue_ipv4_packet(&mut self, packet: Vec<u8>, oif: i32) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        if !self.can_enqueue_tx_packets(packets.len()) {
            return Err(KError::WouldBlock);
        }

        self.data_tx_queue.extend(
            packets
                .into_iter()
                .map(|packet| PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack)),
        );
        Ok(())
    }

    /// Returns whether `dst` is routed through a loopback device.
    pub(crate) fn is_loopback_destination(&self, dst: &IpAddress) -> bool {
        self.table.lookup(dst).is_some_and(|rule| {
            self.devices
                .get(rule.dev)
                .is_some_and(|device| device.link_kind() == LinkKind::Loopback)
        })
    }

    /// Transmits an IPv4 packet through the current route without waiting for
    /// the poller TX queue.
    ///
    /// Loopback xmit raises `NetRx` and drains it on `local_bh_enable`, matching
    /// Linux `dev_queue_xmit` → `loopback_xmit` → `__netif_rx`.
    /// A full shared `NetRx` queue drops the packet and still returns `Ok`,
    /// matching `loopback_xmit` returning `NETDEV_TX_OK` after `__netif_rx`
    /// reports `NET_RX_DROP`.
    pub(crate) fn transmit_ipv4_now(
        &mut self,
        packet: Vec<u8>,
        timestamp: MonotonicInstant,
    ) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        for packet in packets {
            let buf = PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack);
            // RX-ready hint for the poller, not send success. This path is
            // not the poller; a dropped loopback packet is still a successful
            // xmit, as documented above.
            let _poll_next = self.dispatch_ipv4_packet(buf, timestamp);
        }
        Ok(())
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(device);
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        self.devices.len() - 1
    }

    pub(crate) fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> Option<()> {
        let pos = self
            .devices
            .iter()
            .position(|device| device.device_id() == Some(id))?;
        let removed_sources: Vec<_> = self
            .ipv4_addrs
            .iter()
            .filter_map(|entry| (entry.dev == pos).then_some(entry.addr.address()))
            .collect();
        self.devices.remove(pos);
        self.table.remove_device(pos);
        self.ipv4_addrs.retain(|entry| entry.dev != pos);
        for entry in &mut self.ipv4_addrs {
            if entry.dev > pos {
                entry.dev -= 1;
            }
        }
        for source in removed_sources {
            if !self.has_ipv4_address(source) {
                self.table
                    .remove_configured_routes_with_source(source.into());
                for device in &mut self.devices {
                    device.remove_pending_ipv4_source(source);
                }
            }
        }
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        self.adjust_rx_device_cursor_after_remove(pos);
        self.recompute_effective_mtu();
        Some(())
    }

    pub fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.ipv4_addrs.iter().map(|entry| entry.addr).collect()
    }

    /// Returns whether `address` is an IPv4 limited or directed broadcast.
    pub(crate) fn ipv4_dest_requires_broadcast(&self, address: &IpAddress) -> bool {
        let IpAddress::Ipv4(address) = *address else {
            return false;
        };
        address.is_broadcast()
            || self
                .ipv4_addrs
                .iter()
                .any(|entry| entry.addr.is_directed_broadcast(address))
    }

    /// Returns the MTU advertised through the smoltcp device capabilities.
    pub fn effective_mtu(&self) -> usize {
        self.effective_mtu
    }

    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.devices
            .iter()
            .enumerate()
            .map(|(dev, device)| device.link_snapshot(dev as i32 + 1))
            .collect()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        Self::device_index(ifindex).is_some_and(|dev_index| self.devices.get(dev_index).is_some())
    }

    pub fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.devices.iter().position(|device| device.name() == name)
    }

    fn first_ipv4_addr_for_device(&self, ifindex: u32) -> Option<Ipv4Cidr> {
        ifindex
            .checked_sub(1)
            .and_then(|dev| {
                self.ipv4_addrs
                    .iter()
                    .find(|entry| entry.dev == dev as usize)
            })
            .map(|entry| entry.addr)
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_send_snapshot())
    }

    pub fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        &self.ipv4_addrs
    }

    pub fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.ipv4_addrs
            .iter()
            .filter_map(|entry| {
                self.devices.get(entry.dev).map(|device| Ipv4AddrSnapshot {
                    entry: *entry,
                    label: String::from(device.name()),
                })
            })
            .collect()
    }

    pub fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if entry.dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if entry.addr.prefix_len() > 32 || !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        if self
            .ipv4_addrs
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            return Ok(());
        }
        if self.ipv4_addrs.len() >= smoltcp::config::IFACE_MAX_ADDR_COUNT {
            return Err(LinuxError::ENOBUFS);
        }

        self.ipv4_addrs.push(entry);
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    /// Updates the primary IPv4 address on `dev` in its owning address table.
    ///
    /// Other addresses on the device remain unchanged. Updating an existing
    /// address resets its scope and broadcast fields like Linux
    /// `SIOCSIFADDR`; setting the same local address is a no-op.
    pub fn set_primary_ipv4_addr(&mut self, dev: usize, addr: Ipv4Cidr) -> Result<(), LinuxError> {
        if dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if addr.prefix_len() > 32 || !addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }

        let Some(primary_index) = self.ipv4_addrs.iter().position(|entry| entry.dev == dev) else {
            return self.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr,
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            });
        };
        let old = self.ipv4_addrs[primary_index];
        if old.addr.address() == addr.address() {
            return Ok(());
        }

        if self
            .ipv4_addrs
            .iter()
            .skip(primary_index + 1)
            .any(|entry| entry.dev == dev && entry.addr == addr)
        {
            return Err(LinuxError::EEXIST);
        }
        self.ipv4_addrs[primary_index] = Ipv4AddrEntry {
            dev,
            addr,
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        self.remove_ipv4_source_state_if_unowned(old.addr.address());
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    /// Removes the primary IPv4 address from `dev`.
    ///
    /// Other addresses on the device remain unchanged. An interface without
    /// an IPv4 address is accepted to match `SIOCSIFADDR 0.0.0.0`.
    pub fn remove_primary_ipv4_addr(&mut self, dev: usize) -> Result<(), LinuxError> {
        if dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        let Some(primary_index) = self.ipv4_addrs.iter().position(|entry| entry.dev == dev) else {
            return Ok(());
        };
        let old = self.ipv4_addrs.remove(primary_index);
        self.remove_ipv4_source_state_if_unowned(old.addr.address());
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    pub fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
        let old_len = self.ipv4_addrs.len();
        self.ipv4_addrs
            .retain(|existing| existing.dev != entry.dev || existing.addr != entry.addr);
        if self.ipv4_addrs.len() == old_len {
            return false;
        }

        self.remove_ipv4_source_state_if_unowned(entry.addr.address());
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        true
    }

    pub fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == address)
    }

    pub fn route_snapshot(&self) -> Vec<Rule> {
        self.table.rules().to_vec()
    }

    pub fn has_ipv4_subnet_for_device(&self, dev: usize, address: IpAddress) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.dev == dev && entry.addr.contains_addr(&address))
    }

    pub fn is_ipv4_directed_broadcast_for_device(&self, dev: usize, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev)
            .any(|entry| entry.addr.is_directed_broadcast(address))
    }

    pub fn add_route_rule(&mut self, rule: Rule) -> Result<(), LinuxError> {
        self.validate_configured_route(rule)?;
        self.table.add_rule(rule);
        Ok(())
    }

    pub fn replace_route_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.validate_configured_route(replacement)?;
        self.table.remove_exact_rule(existing);
        self.table.add_rule(replacement);
        Ok(())
    }

    pub fn remove_exact_route_rule(&mut self, route: Rule) {
        self.table.remove_exact_rule(route);
    }

    pub fn set_ipv4_broadcast(
        &mut self,
        dev: usize,
        broadcast: Ipv4Address,
    ) -> Result<(), LinuxError> {
        let entry = self
            .ipv4_addrs
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        entry.broadcast = Some(broadcast);
        Ok(())
    }

    /// Updates the prefix length of the primary IPv4 address on `dev`.
    ///
    /// Linux `SIOCSIFNETMASK` keeps `ifa_scope` and only recalculates
    /// `ifa_broadcast` when it still matches the old derived mask.
    pub fn set_ipv4_netmask(&mut self, dev: usize, prefix_len: u8) -> Result<(), LinuxError> {
        if prefix_len > 32 {
            return Err(LinuxError::EINVAL);
        }
        let old = self
            .ipv4_addrs
            .iter()
            .find(|entry| entry.dev == dev)
            .copied()
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        if old.addr.prefix_len() == prefix_len {
            return Ok(());
        }

        let old_derived_broadcast = old.addr.broadcast();
        let has_broadcast_flag = self
            .devices
            .get(dev)
            .is_some_and(|device| device.link_kind() == LinkKind::Ethernet);
        let should_recalculate_broadcast = has_broadcast_flag
            && prefix_len < 31
            && old.effective_broadcast() == old_derived_broadcast;

        let entry = self
            .ipv4_addrs
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        entry.addr = Ipv4Cidr::new(old.addr.address(), prefix_len);
        if should_recalculate_broadcast {
            entry.broadcast = None;
        } else if entry.broadcast.is_none() {
            entry.broadcast = old_derived_broadcast;
        }

        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    fn remove_ipv4_source_state_if_unowned(&mut self, address: Ipv4Address) {
        if self.has_ipv4_address(address) {
            return;
        }
        self.table
            .remove_configured_routes_with_source(IpAddress::Ipv4(address));
        for device in &mut self.devices {
            device.remove_pending_ipv4_source(address);
        }
    }

    fn validate_configured_route(&self, rule: Rule) -> Result<(), LinuxError> {
        if !rule.is_configured() {
            return Err(LinuxError::EINVAL);
        }
        if rule.table != ROUTE_TABLE_MAIN || rule.route_type != ROUTE_TYPE_UNICAST {
            return Err(LinuxError::EOPNOTSUPP);
        }
        if !matches!(rule.filter, IpCidr::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        if rule.dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if let Some(prefsrc) = rule.preferred_source()
            && !matches!(prefsrc, IpAddress::Ipv4(address) if self.has_ipv4_address(address))
        {
            return Err(LinuxError::EINVAL);
        }
        if let Some(gateway) = rule.via {
            let IpAddress::Ipv4(gateway) = gateway else {
                return Err(LinuxError::EAFNOSUPPORT);
            };
            if !gateway.is_unicast()
                || self.is_ipv4_directed_broadcast_for_device(rule.dev, gateway)
            {
                return Err(LinuxError::EINVAL);
            }
            if !self.has_ipv4_subnet_for_device(rule.dev, gateway.into()) {
                return Err(LinuxError::ENETUNREACH);
            }
        }
        Ok(())
    }

    /// Applies a validated link update.
    ///
    /// Returns `Ok(true)` when the effective device MTU changed and the
    /// smoltcp interface must be rebuilt to refresh its cached capabilities.
    pub fn update_device_link(
        &mut self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<bool, LinuxError> {
        let dev = Self::device_index(ifindex).ok_or(LinuxError::ENODEV)?;
        let device = self.devices.get(dev).ok_or(LinuxError::ENODEV)?;
        if let Some(name) = update.name.as_deref() {
            crate::device::validate_interface_name(name)?;
            if self
                .devices
                .iter()
                .enumerate()
                .any(|(other_dev, device)| other_dev != dev && device.name() == name)
            {
                return Err(LinuxError::EEXIST);
            }
        }
        if let Some(mtu) = update.mtu {
            device.link_kind().validate_mtu(mtu)?;
        }

        let is_mtu_changed = update.mtu.is_some_and(|mtu| mtu != device.mtu());
        let effective_mtu = self.effective_mtu;

        let LinkConfigUpdate { name, mtu, is_up } = update;
        let device = &mut self.devices[dev];
        if let Some(mtu) = mtu {
            device.set_mtu(mtu)?;
        }
        if let Some(name) = name {
            device.set_name(name);
        }
        if let Some(is_up) = is_up {
            device.set_link_up(is_up);
        }
        if is_mtu_changed {
            self.recompute_effective_mtu();
        }
        Ok(self.effective_mtu != effective_mtu)
    }

    fn device_index(ifindex: i32) -> Option<usize> {
        (ifindex > 0).then_some((ifindex - 1) as usize)
    }

    pub fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        let device = self.devices.get_mut(update.dev).ok_or(LinuxError::ENODEV)?;
        let exists = device.has_neighbor(update.dst);
        if exists && !policy.can_replace {
            return Err(LinuxError::EEXIST);
        }
        if !exists && !policy.can_create {
            return Err(LinuxError::ENOENT);
        }
        device.apply_neighbor_update(update)
    }

    #[cfg(unittest)]
    pub fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.devices
            .get(dev)
            .is_some_and(|device| device.has_neighbor(dst))
    }

    fn select_ipv4_source(
        &self,
        dev: usize,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<Ipv4Address> {
        let mut eligible = self
            .ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev && entry.scope <= route_scope);
        let first = eligible.next()?;
        let selected = if first.addr.contains_addr(next_hop) {
            first
        } else {
            eligible
                .find(|entry| entry.addr.contains_addr(next_hop))
                .unwrap_or(first)
        };
        Some(selected.addr.address())
    }

    fn sync_device_ipv4_addrs(&mut self) {
        let local_addrs: Vec<_> = self.ipv4_addrs.iter().map(|entry| entry.addr).collect();
        let assigned_addrs: Vec<Vec<_>> = (0..self.devices.len())
            .map(|dev| {
                self.ipv4_addrs
                    .iter()
                    .filter_map(|entry| (entry.dev == dev).then_some(entry.addr))
                    .collect()
            })
            .collect();
        for (device, assigned_addrs) in self.devices.iter_mut().zip(assigned_addrs) {
            device.set_ipv4_addrs(&assigned_addrs, &local_addrs);
        }
    }

    fn rebuild_ipv4_address_routes(&mut self) {
        self.table.remove_ipv4_address_routes();
        let loopback_dev = self
            .devices
            .iter()
            .position(|device| device.link_kind() == crate::device::LinkKind::Loopback);
        let entries = self.ipv4_addrs.clone();
        let mut connected = Vec::new();
        for entry in entries {
            if let Some(loopback_dev) = loopback_dev {
                self.table.add_rule(Rule::for_ipv4_address(
                    Ipv4Cidr::new(entry.addr.address(), 32),
                    loopback_dev,
                    entry,
                    Ipv4AddressRouteKind::Local,
                    ROUTE_SCOPE_HOST,
                ));
            }

            let network = entry.addr.network();
            if entry.addr.prefix_len() == 32
                || network.address().is_unspecified()
                || connected.contains(&(entry.dev, network))
            {
                continue;
            }
            connected.push((entry.dev, network));
            let scope = if self.devices[entry.dev].link_kind() == crate::device::LinkKind::Loopback
            {
                ROUTE_SCOPE_HOST
            } else {
                ROUTE_SCOPE_LINK
            };
            self.table.add_rule(Rule::for_ipv4_address(
                network,
                entry.dev,
                entry,
                Ipv4AddressRouteKind::Connected,
                scope,
            ));
        }
    }

    fn recompute_effective_mtu(&mut self) {
        self.effective_mtu = self
            .devices
            .iter()
            .map(|device| device.mtu().min(u16::MAX as usize))
            .min()
            .unwrap_or(STANDARD_MTU);
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
        let dev_index = Self::device_index(ifindex).ok_or(KError::InvalidInput)?;
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
                                src_addr,
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
                        poll_next |= dev.send_ip_packet(
                            rule.dev as i32 + 1,
                            next_hop,
                            src_addr,
                            packet,
                            timestamp,
                        );
                    }
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        (work_done, poll_next || self.has_queued_tx_packets())
    }

    /// Dispatches one IPv4 packet onto the selected device.
    ///
    /// Returns whether the send made RX ready, which [`Self::dispatch_budgeted`]
    /// uses as its `poll_next` hint. Ethernet TX and dropped packets return
    /// `false`.
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

            let bound_if = packet.ifindex();
            let device_count = self.devices.len();
            let Some((last_device, preceding_devices)) = self.devices.split_last_mut() else {
                return false;
            };

            let mut poll_next = false;
            for (dev_index, dev) in preceding_devices.iter_mut().enumerate() {
                let ifindex = dev_index as i32 + 1;
                if bound_if != 0 && bound_if != ifindex {
                    continue;
                }
                poll_next |=
                    dev.send_ip_packet(ifindex, dst_addr, src_addr, packet.clone(), timestamp);
            }
            let last_ifindex = device_count as i32;
            if bound_if == 0 || bound_if == last_ifindex {
                poll_next |=
                    last_device.send_ip_packet(last_ifindex, dst_addr, src_addr, packet, timestamp);
            }
            return poll_next;
        }

        let Some(rule) = self.table.lookup(&dst_addr) else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        let bound_if = packet.ifindex();
        if bound_if != 0 && bound_if != rule.dev as i32 + 1 {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                rule.dev as i32 + 1
            );
            return false;
        }
        if !self.is_local_ipv4_source(header.src_addr()) {
            warn!(
                "Dropping IPv4 packet with non-local source {} routed via {}",
                src_addr, rule.src
            );
            return false;
        }

        let next_hop = rule.via.unwrap_or(dst_addr);
        let dev = &mut self.devices[rule.dev];
        dev.send_ip_packet(rule.dev as i32 + 1, next_hop, src_addr, packet, timestamp)
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
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == src_addr)
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

fn same_route_rule(lhs: Rule, rhs: Rule) -> bool {
    lhs.filter == rhs.filter
        && lhs.via == rhs.via
        && lhs.dev == rhs.dev
        && lhs.table == rhs.table
        && lhs.protocol == rhs.protocol
        && lhs.scope == rhs.scope
        && lhs.route_type == rhs.route_type
        && lhs.preferred_source() == rhs.preferred_source()
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
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        );
        router
    }

    #[def_test(serial)]
    fn exact_rx_budget_does_not_report_empty_device() {
        let mut router = router_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_more);
    }

    #[def_test(serial)]
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

    #[def_test]
    fn ipv4_address_installs_local_and_connected_routes() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();

        let local = router
            .table
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 32);
        assert_eq!(local.dev, dev);
        assert_eq!(local.scope, ROUTE_SCOPE_HOST);
        assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

        let connected = router
            .table
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
            .unwrap();
        assert_eq!(connected.filter.prefix_len(), 24);
        assert_eq!(connected.dev, dev);
        assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
        assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
    }

    #[def_test]
    fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        assert!(router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::BROADCAST)));
        assert!(
            router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255)))
        );
        assert!(
            !router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
        );
    }

    #[def_test]
    fn ipv4_address_capacity_matches_smoltcp_interface_limit() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));

        for index in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
            router
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, index as u8 + 1), 32),
                    scope: ROUTE_SCOPE_HOST,
                    broadcast: None,
                })
                .unwrap();
        }

        assert_eq!(
            router.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 1), 32),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            }),
            Err(LinuxError::ENOBUFS)
        );
    }

    #[def_test]
    fn setting_primary_ipv4_address_keeps_secondary_addresses() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let primary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        let replacement = Ipv4Cidr::new(Ipv4Address::new(203, 0, 113, 7), 24);
        router.set_primary_ipv4_addr(dev, replacement).unwrap();

        assert_eq!(router.ipv4_addr_entries().len(), 2);
        assert_eq!(router.ipv4_addr_entries()[0].addr, replacement);
        assert_eq!(router.ipv4_addr_entries()[0].scope, ROUTE_SCOPE_UNIVERSE);
        assert_eq!(router.ipv4_addr_entries()[0].broadcast, None);
        assert_eq!(router.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn removing_primary_ipv4_address_keeps_secondary_addresses() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let primary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        router.remove_primary_ipv4_addr(dev).unwrap();

        assert_eq!(router.ipv4_addr_entries(), &[secondary]);
    }

    #[def_test]
    fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: Some(custom_broadcast),
            })
            .unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        router.set_ipv4_netmask(dev, 16).unwrap();

        assert_eq!(router.ipv4_addr_entries().len(), 2);
        assert_eq!(router.ipv4_addr_entries()[0].addr.prefix_len(), 16);
        assert_eq!(router.ipv4_addr_entries()[0].scope, ROUTE_SCOPE_HOST);
        assert_eq!(
            router.ipv4_addr_entries()[0].broadcast,
            Some(custom_broadcast)
        );
        assert_eq!(router.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn deleting_ipv4_address_removes_automatic_routes() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();
        assert!(
            router
                .table
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_some()
        );

        assert!(router.remove_ipv4_addr(entry));
        assert!(
            router
                .table
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_none()
        );
    }

    #[def_test]
    fn configured_gateway_must_be_on_device_subnet() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        let unreachable = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        assert_eq!(
            router.add_route_rule(unreachable),
            Err(LinuxError::ENETUNREACH)
        );

        let default = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        router.add_route_rule(default).unwrap();
        router.remove_exact_route_rule(default);
        assert!(
            router
                .route_snapshot()
                .iter()
                .all(|route| !route.is_configured())
        );
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
        caps.max_transmission_unit = self.effective_mtu;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}
