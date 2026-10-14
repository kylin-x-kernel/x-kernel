// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Router state and packet dispatch.

use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{PollContext, PollRegisterError, PollSet};
use ktime_types::MonotonicInstant;
use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
    wire::Ipv6Packet,
};

#[cfg(unittest)]
use crate::network::router::ROUTE_PROTOCOL_KERNEL;
use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::{
        DeviceRegistry, LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate,
        NetDevice, NetDeviceId,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    network::{
        ipv4::{self, Ipv4Error, Ipv4Header},
        router::{
            ROUTE_SCOPE_HOST, ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE, ROUTE_TABLE_MAIN,
            ROUTE_TYPE_UNICAST, RouteTable, Rule,
        },
    },
    runtime::tx_queue::DataTxQueue,
};

const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;

/// Conditions under which a control-plane neighbor update may be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborUpdatePolicy {
    pub(crate) can_create: bool,
    pub(crate) can_replace: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4AddrEntry {
    pub dev: NetDeviceId,
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

pub(crate) struct Router {
    rx_queue: VecDeque<PacketBuf>,
    control_tx_queue: VecDeque<PacketBuf>,
    data_tx_queue: DataTxQueue,
    next_rx_device: usize,
    ipv4_addrs: Vec<Ipv4AddrEntry>,
    next_ipv4_identification: u16,
    pub(crate) devices: DeviceRegistry,
    pub(crate) table: RouteTable,
    effective_mtu: usize,
}

pub(crate) struct RxDrain {
    pub work_done: usize,
    pub has_more: bool,
    /// A failed device poll, distinct from an empty queue. Already collected
    /// packets remain in the caller's batch and must still be processed.
    pub error: Option<(i32, KError)>,
}
impl Router {
    pub(crate) fn new() -> Self {
        Self {
            rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            control_tx_queue: VecDeque::with_capacity(CONTROL_TX_QUEUE_SIZE),
            data_tx_queue: DataTxQueue::with_capacity(DATA_TX_QUEUE_SIZE),
            next_rx_device: 0,
            ipv4_addrs: Vec::new(),
            next_ipv4_identification: 1,
            devices: DeviceRegistry::new(),
            table: RouteTable::new(),
            effective_mtu: STANDARD_MTU,
        }
    }

    pub(crate) fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        let rule = self.table.lookup(dst)?;
        self.devices
            .get_by_id(rule.dev)
            .map(|device| device.mtu().min(u16::MAX as usize))
    }

    /// Returns the source address for an output route with an active device.
    ///
    /// `oif` is a stable interface index; `0` means the socket is unbound.
    /// Limited broadcasts with a bound device use that device's primary
    /// address (or `0.0.0.0`) without requiring a unicast route.
    ///
    /// # Errors
    ///
    /// Returns `ENETUNREACH` when the route or its device is absent, when
    /// the selected device is administratively down, or when `oif` disagrees
    /// with the looked-up output device.
    pub(crate) fn output_route_source(&self, dst: &IpAddress, oif: i32) -> KResult<IpAddress> {
        if dst.is_broadcast() && oif > 0 {
            let device = self
                .devices
                .get_by_id(NetDeviceId::new(oif).ok_or(LinuxError::ENETUNREACH)?)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            if !device.is_link_up() {
                return Err(LinuxError::ENETUNREACH.into());
            }
            return Ok(self
                .first_ipv4_addr_for_device(oif)
                .map(|cidr| IpAddress::Ipv4(cidr.address()))
                .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)));
        }
        // Unbound limited broadcast still needs a route so the later
        // dispatch flood has a source address.

        let rule = self
            .table
            .lookup(dst)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if oif > 0 && rule.dev.ifindex() != oif {
            return Err(KError::from(LinuxError::ENETUNREACH));
        }
        let device = self
            .devices
            .get_by_id(rule.dev)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        match rule.preferred_source() {
            Some(source) => Ok(source),
            None => self
                .select_ipv4_source(rule.dev, &rule.via.unwrap_or(*dst), rule.scope)
                .map(IpAddress::Ipv4)
                .ok_or(KError::from(LinuxError::ENETUNREACH)),
        }
    }

    pub(crate) fn can_enqueue_tx_packet(&self) -> bool {
        !self.data_tx_queue.is_full()
    }

    pub(crate) fn available_tx_packet_slots(&self) -> usize {
        self.data_tx_queue.available()
    }

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.data_tx_queue.register_capacity_waker(context)
    }

    pub(crate) fn tx_capacity_poll(&self) -> PollSet {
        self.data_tx_queue.capacity_poll()
    }

    pub(crate) fn can_enqueue_tx_packets(&self, packet_count: usize) -> bool {
        self.data_tx_queue.can_push(packet_count)
    }

    pub(crate) fn queue_ipv4_packet(&mut self, packet: Vec<u8>, oif: i32) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        self.data_tx_queue
            .try_extend(
                packets.into_iter().map(|packet| {
                    PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack)
                }),
            )
            .map_err(|_| KError::WouldBlock)
    }

    /// Returns whether `dst` is routed through a loopback device.
    pub(crate) fn is_loopback_destination(&self, dst: &IpAddress) -> bool {
        self.table.lookup(dst).is_some_and(|rule| {
            self.devices
                .get_by_id(rule.dev)
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

    pub(crate) fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub(crate) fn add_device(
        &mut self,
        device: Box<dyn NetDevice>,
    ) -> Result<NetDeviceId, LinuxError> {
        let id = self.devices.register(device)?;
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        Ok(id)
    }

    pub(crate) fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> Option<()> {
        let pos = self
            .devices
            .iter()
            .position(|device| device.device_id() == Some(id))?;
        let dev = self.devices.id_at(pos);
        let removed_sources: Vec<_> = self
            .ipv4_addrs
            .iter()
            .filter_map(|entry| (entry.dev == dev).then_some(entry.addr.address()))
            .collect();
        self.devices.remove_slot(pos);
        self.table.remove_device(dev);
        self.ipv4_addrs.retain(|entry| entry.dev != dev);
        for source in removed_sources {
            if !self.has_ipv4_address(source) {
                self.table
                    .remove_configured_routes_with_source(source.into());
                for device in self.devices.iter_mut() {
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

    pub(crate) fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
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
    pub(crate) fn effective_mtu(&self) -> usize {
        self.effective_mtu
    }

    pub(crate) fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.devices
            .iter()
            .enumerate()
            .map(|(slot, device)| device.link_snapshot(self.devices.id_at(slot).ifindex()))
            .collect()
    }

    pub(crate) fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        let dev_index = self.device_slot(ifindex)?;
        self.devices
            .get_by_slot(dev_index)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub(crate) fn has_device(&self, ifindex: i32) -> bool {
        self.device_slot(ifindex).is_some()
    }

    pub(crate) fn device_id_by_name(&self, name: &str) -> Option<NetDeviceId> {
        self.devices
            .iter()
            .position(|device| device.name() == name)
            .map(|slot| self.devices.id_at(slot))
    }

    /// Returns the primary IPv4 address of the device named `name`.
    ///
    /// Device resolution and address lookup happen under one Router borrow so
    /// a concurrent device removal cannot invalidate the resolved ID between
    /// the two steps, matching Linux `devinet_ioctl` holding `rtnl_lock`.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` for an unknown device and `EADDRNOTAVAIL` when the
    /// device has no IPv4 address.
    pub(crate) fn interface_ipv4_addr(&self, name: &str) -> Result<Ipv4Cidr, LinuxError> {
        let dev = self.device_id_by_name(name).ok_or(LinuxError::ENODEV)?;
        self.ipv4_addrs
            .iter()
            .find(|entry| entry.dev == dev)
            .map(|entry| entry.addr)
            .ok_or(LinuxError::EADDRNOTAVAIL)
    }

    fn first_ipv4_addr_for_device(&self, ifindex: i32) -> Option<Ipv4Cidr> {
        let dev = NetDeviceId::new(ifindex)?;
        self.ipv4_addrs
            .iter()
            .find(|entry| entry.dev == dev)
            .map(|entry| entry.addr)
    }

    pub(crate) fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        let dev_index = self.device_slot(ifindex)?;
        self.devices
            .get_by_slot(dev_index)
            .map(|device| device.link_send_snapshot())
    }

    pub(crate) fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        &self.ipv4_addrs
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.ipv4_addrs
            .iter()
            .filter_map(|entry| {
                self.devices
                    .get_by_id(entry.dev)
                    .map(|device| Ipv4AddrSnapshot {
                        entry: *entry,
                        label: String::from(device.name()),
                    })
            })
            .collect()
    }

    pub(crate) fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if self.devices.slot(entry.dev).is_none() {
            return Err(LinuxError::ENODEV);
        }
        if !entry.addr.address().is_unicast() {
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
    pub(crate) fn set_primary_ipv4_addr(
        &mut self,
        dev: NetDeviceId,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        if self.devices.slot(dev).is_none() {
            return Err(LinuxError::ENODEV);
        }
        if !addr.address().is_unicast() {
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
    pub(crate) fn remove_primary_ipv4_addr(&mut self, dev: NetDeviceId) -> Result<(), LinuxError> {
        if self.devices.slot(dev).is_none() {
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

    pub(crate) fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
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

    pub(crate) fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == address)
    }

    pub(crate) fn route_snapshot(&self) -> Vec<Rule> {
        self.table.rules().to_vec()
    }

    pub(crate) fn has_ipv4_subnet_for_device(&self, dev: NetDeviceId, address: IpAddress) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.dev == dev && entry.addr.contains_addr(&address))
    }

    pub(crate) fn is_ipv4_directed_broadcast_for_device(
        &self,
        dev: NetDeviceId,
        address: Ipv4Address,
    ) -> bool {
        self.ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev)
            .any(|entry| entry.addr.is_directed_broadcast(address))
    }

    pub(crate) fn add_route_rule(&mut self, rule: Rule) -> Result<(), LinuxError> {
        self.validate_configured_route(rule)?;
        self.table.add_rule(rule);
        Ok(())
    }

    pub(crate) fn replace_route_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.validate_configured_route(replacement)?;
        self.table.remove_exact_rule(existing);
        self.table.add_rule(replacement);
        Ok(())
    }

    pub(crate) fn remove_exact_route_rule(&mut self, route: Rule) {
        self.table.remove_exact_rule(route);
    }

    pub(crate) fn set_ipv4_broadcast(
        &mut self,
        dev: NetDeviceId,
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
    pub(crate) fn set_ipv4_netmask(
        &mut self,
        dev: NetDeviceId,
        prefix_len: u8,
    ) -> Result<(), LinuxError> {
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
            .get_by_id(dev)
            .is_some_and(|device| device.link_kind() == LinkKind::Ethernet);
        let should_recalculate_broadcast = has_broadcast_flag
            && prefix_len < 31
            && old.effective_broadcast() == old_derived_broadcast;

        let entry = self
            .ipv4_addrs
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        entry.addr =
            Ipv4Cidr::new(old.addr.address(), prefix_len).expect("prefix length was validated");
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
        for device in self.devices.iter_mut() {
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
        if self.devices.slot(rule.dev).is_none() {
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
    pub(crate) fn update_device_link(
        &mut self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<bool, LinuxError> {
        let dev = self.device_slot(ifindex).ok_or(LinuxError::ENODEV)?;
        let device = self.devices.get_by_slot(dev).ok_or(LinuxError::ENODEV)?;
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
        let device = self
            .devices
            .get_mut_by_slot(dev)
            .expect("validated device slot");
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

    fn device_slot(&self, ifindex: i32) -> Option<usize> {
        self.devices.slot(NetDeviceId::new(ifindex)?)
    }

    pub(crate) fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        let device = self
            .devices
            .get_mut_by_id(update.dev)
            .ok_or(LinuxError::ENODEV)?;
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
    pub(crate) fn has_neighbor(&self, dev: NetDeviceId, dst: IpAddress) -> bool {
        self.devices
            .get_by_id(dev)
            .is_some_and(|device| device.has_neighbor(dst))
    }

    fn select_ipv4_source(
        &self,
        dev: NetDeviceId,
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
            .map(|slot| {
                let dev = self.devices.id_at(slot);
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
            .position(|device| device.link_kind() == crate::device::LinkKind::Loopback)
            .map(|slot| self.devices.id_at(slot));
        let entries = self.ipv4_addrs.clone();
        let mut connected = Vec::new();
        for entry in entries {
            if let Some(loopback_dev) = loopback_dev {
                self.table.add_rule(Rule::for_local_ipv4_address(
                    loopback_dev,
                    entry.dev,
                    entry.addr,
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
            let device = self
                .devices
                .get_by_id(entry.dev)
                .expect("address device must be registered");
            let scope = if device.link_kind() == crate::device::LinkKind::Loopback {
                ROUTE_SCOPE_HOST
            } else {
                ROUTE_SCOPE_LINK
            };
            self.table.add_rule(Rule::for_connected_ipv4_address(
                entry.dev, entry.addr, scope,
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

    pub(crate) fn drain_rx_budgeted_into(
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
                error: None,
            };
        }

        let mut work_done = 0;
        let device_count = self.devices.len();
        if self.next_rx_device >= device_count {
            self.next_rx_device = 0;
        }

        for _ in 0..device_count {
            let dev_index = self.next_rx_device;
            let ifindex = self.devices.id_at(dev_index).ifindex();
            loop {
                if work_done >= budget {
                    self.next_rx_device = next_device_index(dev_index, device_count);
                    return RxDrain {
                        work_done,
                        has_more: self.has_immediate_rx_work(),
                        error: None,
                    };
                }

                let packet = {
                    let dev = self
                        .devices
                        .get_mut_by_slot(dev_index)
                        .expect("valid RX device slot");
                    dev.poll_rx(ifindex, timestamp)
                };
                let packet = match packet {
                    Ok(Some(packet)) => packet,
                    Ok(None) => break,
                    Err(error) => {
                        self.next_rx_device = next_device_index(dev_index, device_count);
                        return RxDrain {
                            work_done,
                            has_more: self.has_immediate_rx_work(),
                            error: Some((ifindex, error)),
                        };
                    }
                };
                work_done += 1;
                packets.push(packet);
            }
            self.next_rx_device = next_device_index(dev_index, device_count);
        }

        RxDrain {
            work_done,
            has_more: false,
            error: None,
        }
    }

    pub(crate) fn enqueue_ingress_packets(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(packets.len() <= self.ingress_capacity());
        self.rx_queue.extend(packets.drain(..));
    }

    pub(crate) fn ingress_capacity(&self) -> usize {
        SOCKET_BUFFER_SIZE.saturating_sub(self.rx_queue.len())
    }

    pub(crate) fn has_pending_ingress(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    pub(crate) fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        let dev_id = NetDeviceId::new(ifindex).ok_or(KError::InvalidInput)?;
        let dev = self
            .devices
            .get_mut_by_id(dev_id)
            .ok_or(KError::from(LinuxError::ENODEV))?;
        dev.send_link_frame(ifindex, frame)
    }

    pub(crate) fn dispatch_budgeted(
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
                        for (id, dev) in self.devices.iter_mut_with_ids() {
                            poll_next |= dev.send_ip_packet(
                                id.ifindex(),
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
                        let dev = self
                            .devices
                            .get_mut_by_id(rule.dev)
                            .expect("route device must be registered");
                        poll_next |= dev.send_ip_packet(
                            rule.dev.ifindex(),
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
            let mut devices = self.devices.iter_mut_with_ids();
            let Some((last_id, last_device)) = devices.next_back() else {
                return false;
            };

            let mut poll_next = false;
            for (id, dev) in devices {
                let ifindex = id.ifindex();
                if bound_if != 0 && bound_if != ifindex {
                    continue;
                }
                poll_next |=
                    dev.send_ip_packet(ifindex, dst_addr, src_addr, packet.clone(), timestamp);
            }
            let last_ifindex = last_id.ifindex();
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
        if bound_if != 0 && bound_if != rule.dev.ifindex() {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                rule.dev.ifindex()
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
        let dev = self
            .devices
            .get_mut_by_id(rule.dev)
            .expect("route device must be registered");
        dev.send_ip_packet(rule.dev.ifindex(), next_hop, src_addr, packet, timestamp)
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

    pub(crate) fn queue_control_ipv4_packet(&mut self, packet: Vec<u8>) -> KResult {
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

#[unittest::mod_test]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use ktime_types::MonotonicInstant;
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::device::LoopbackDevice;

    fn valid_ipv4_cidr(address: Ipv4Address, prefix_len: u8) -> Ipv4Cidr {
        Ipv4Cidr::new(address, prefix_len).unwrap()
    }

    fn router_with_ready_loopback_packet() -> Router {
        let mut router = Router::new();
        let loopback = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
        let _ = router
            .devices
            .get_mut_by_id(loopback)
            .unwrap()
            .send_ip_packet(
                1,
                IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
                IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
                packet,
                MonotonicInstant::ORIGIN,
            );
        router
    }

    struct FailingRxDevice {
        link: LoopbackDevice,
        packet: Option<PacketBuf>,
    }

    impl NetDevice for FailingRxDevice {
        fn name(&self) -> &str {
            self.link.name()
        }

        fn link_kind(&self) -> LinkKind {
            LinkKind::Ethernet
        }

        fn mtu(&self) -> usize {
            self.link.mtu()
        }

        fn is_link_up(&self) -> bool {
            true
        }

        fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
            self.link.link_snapshot(ifindex)
        }

        fn link_send_snapshot(&self) -> LinkSendSnapshot {
            self.link.link_send_snapshot()
        }

        fn poll_rx(
            &mut self,
            _ifindex: i32,
            _timestamp: MonotonicInstant,
        ) -> KResult<Option<PacketBuf>> {
            self.packet.take().map(Some).ok_or(KError::Io)
        }

        fn has_rx_work(&self) -> bool {
            self.packet.is_some()
        }

        fn send_ip_packet(
            &mut self,
            _ifindex: i32,
            _next_hop: IpAddress,
            _source_addr: IpAddress,
            _packet: PacketBuf,
            _timestamp: MonotonicInstant,
        ) -> bool {
            false
        }

        fn register_rx_waker(
            &self,
            source_waker: &core::task::Waker,
            context: &mut PollContext<'_>,
        ) -> Result<(), PollRegisterError> {
            self.link.register_rx_waker(source_waker, context)
        }
    }

    #[def_test]
    fn rx_failure_preserves_partial_batch_and_advances_device_cursor() {
        let mut router = Router::new();
        for _ in 0..2 {
            router
                .add_device(Box::new(FailingRxDevice {
                    link: LoopbackDevice::new(),
                    packet: Some(tx_packet_buf(vec![0x45, 0, 0, 20])),
                }))
                .unwrap();
        }
        let mut packets = Vec::new();
        let first = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
        assert_eq!(first.work_done, 1);
        assert_eq!(packets.len(), 1);
        assert_eq!(first.error, Some((2, KError::Io)));
        assert!(first.has_more);

        let second = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
        assert_eq!(second.work_done, 1);
        assert_eq!(packets.len(), 1);
        assert_eq!(second.error, Some((3, KError::Io)));
        assert!(!second.has_more);
    }

    #[def_test(serial)]
    fn exact_rx_budget_does_not_report_empty_device() {
        let mut router = router_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

        assert_eq!(drain.work_done, 1);
        assert_eq!(drain.error, None);
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
        assert!(
            router
                .data_tx_queue
                .try_push(tx_packet_buf(vec![0; 20]))
                .is_ok()
        );
        let capacity_before = router.available_tx_packet_slots();

        let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(router.available_tx_packet_slots(), capacity_before + 1);
    }

    #[def_test]
    fn ipv4_address_installs_local_and_connected_routes() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let entry = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
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
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
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
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();

        for index in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
            router
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, index as u8 + 1), 32),
                    scope: ROUTE_SCOPE_HOST,
                    broadcast: None,
                })
                .unwrap();
        }

        assert_eq!(
            router.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 1), 32),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            }),
            Err(LinuxError::ENOBUFS)
        );
    }

    #[def_test]
    fn setting_primary_ipv4_address_keeps_secondary_addresses() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let primary = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        let replacement = valid_ipv4_cidr(Ipv4Address::new(203, 0, 113, 7), 24);
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
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let primary = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
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
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
        let secondary = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
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
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let entry = Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
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
        let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        let unreachable = Rule::new(
            valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        assert_eq!(
            router.add_route_rule(unreachable),
            Err(LinuxError::ENETUNREACH)
        );

        let default = Rule::new(
            valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
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

pub(crate) struct TxToken<'a>(&'a mut DataTxQueue);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        assert!(
            self.0
                .try_push(PacketBuf::from_ip_packet_vec(
                    0,
                    ip_packet,
                    PacketOwner::Ipv4Stack,
                ))
                .is_ok(),
            "smoltcp consumed a TX token without queue capacity"
        );
        result
    }
}

pub(crate) struct RxToken(PacketBuf);

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
        if self.data_tx_queue.is_full() {
            return None;
        }
        let packet = self.rx_queue.pop_front()?;
        Some((RxToken(packet), TxToken(&mut self.data_tx_queue)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.data_tx_queue.is_full() {
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
