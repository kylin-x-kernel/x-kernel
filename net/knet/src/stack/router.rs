// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Router state and packet dispatch.

use alloc::{collections::VecDeque, vec::Vec};

use kalloc_types::KBox;
use kerrno::{KError, KResult, LinuxError};
use kpoll::{PollContext, PollRegisterError, PollSet};
use ktime_types::MonotonicInstant;
use smoltcp::wire::Ipv6Packet;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    device::{
        DeviceRegistry, IF_OPER_DOWN, Ipv4AddrChange, LINK_FLAG_LOWER_UP, LINK_FLAG_RUNNING,
        LINK_FLAG_UP, LinkKind, NeighborEvent, NeighborSnapshot, NeighborState, NeighborUpdate,
        NetDevice, NetDeviceId, RxPoll, net_rx::RxPacket,
    },
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    netfilter::FilterConfig,
    network::{
        event::{NetworkEvent, RouteProvenance},
        interface_address::{Ipv4AddrEntry, Ipv4AddrSnapshot},
        ipv4::{
            self, Ipv4Header,
            forward::{Ipv4ForwardFailure, Ipv4ForwardInput, PreparedIpv4Forward},
        },
        router::{
            ROUTE_SCOPE_HOST, ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE, ROUTE_TYPE_BROADCAST,
            ROUTE_TYPE_LOCAL, ROUTE_TYPE_MULTICAST, ROUTE_TYPE_UNICAST, RouteTable, Rule,
        },
    },
    runtime::tx_queue::DataTxQueue,
    stack::device::StackDevice,
};

const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;

/// Conditions under which a control-plane neighbor update may be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborUpdatePolicy {
    /// `NLM_F_CREATE` allows installing a missing entry.
    pub(crate) can_create: bool,
    /// `NLM_F_EXCL` rejects an existing entry with `EEXIST`.
    pub(crate) exclusive: bool,
    /// `NLM_F_REPLACE` allows changing a valid entry's link-layer address.
    pub(crate) can_override: bool,
}

/// Result of the output-route lookup performed for `RTM_GETROUTE`.
///
/// `fib_entry` is absent only for Linux's source-specified multicast
/// shortcut, zero-destination loopback rewrite, and on-link synthesis.  The
/// source shortcut bypasses the FIB; the other two paths start with an empty
/// `fib_result`.  FIB hits keep their entry even when final output turns the
/// route into a broadcast or multicast route, because `RTM_F_FIB_MATCH`
/// reports the matched FIB entry rather than the constructed output type.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ipv4OutputRoute {
    /// Effective destination after resolving an unspecified target.
    pub(crate) destination: Ipv4Address,
    pub(crate) fib_entry: Option<Rule>,
    pub(crate) table: u8,
    pub(crate) route_type: u8,
    pub(crate) oif: NetDeviceId,
    pub(crate) source: Option<Ipv4Address>,
    pub(crate) gateway: Option<Ipv4Address>,
}

/// The selected path stays with a forward while its fragments await dispatch.
#[derive(Debug)]
struct Ipv4ForwardRoute {
    device: NetDeviceId,
    next_hop: Ipv4Address,
    neighbor_source: Ipv4Address,
}

struct PendingIpv4Forward {
    route: Ipv4ForwardRoute,
    packets: alloc::vec::IntoIter<PacketBuf>,
}

fn ipv4_address(addr: IpAddress) -> Option<Ipv4Address> {
    match addr {
        IpAddress::Ipv4(addr) => Some(addr),
        IpAddress::Ipv6(_) => None,
    }
}

pub(crate) struct Router {
    rx_queue: VecDeque<PacketBuf>,
    forward_queue: VecDeque<Ipv4ForwardInput>,
    pending_forward: Option<PendingIpv4Forward>,
    ipv4_forwarding: i32,
    next_tx_is_forward: bool,
    control_tx_queue: VecDeque<PacketBuf>,
    data_tx_queue: DataTxQueue,
    next_rx_device: usize,
    next_ipv4_identification: u16,
    /// Data-plane neighbor transitions waiting to be published after the
    /// Router lock is released.
    neighbor_events: Vec<NeighborEvent>,
    pub(crate) devices: DeviceRegistry,
    pub(crate) table: RouteTable,
}

pub(crate) struct RxDrain {
    /// Work items reported by successful `RxPoll::Consumed` polls,
    /// including handled or dropped frames absent from the batch.
    /// Failed polls are excluded and reported separately in `error`.
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
            forward_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            pending_forward: None,
            ipv4_forwarding: 0,
            next_tx_is_forward: true,
            control_tx_queue: VecDeque::with_capacity(CONTROL_TX_QUEUE_SIZE),
            data_tx_queue: DataTxQueue::with_capacity(DATA_TX_QUEUE_SIZE),
            next_rx_device: 0,
            next_ipv4_identification: 1,
            neighbor_events: Vec::new(),
            devices: DeviceRegistry::new(smoltcp::config::IFACE_MAX_ADDR_COUNT),
            table: RouteTable::new(),
        }
    }

    /// Borrows only the packet queues and capabilities required by smoltcp.
    ///
    /// The enclosing Router guard still serializes queue access. The adapter
    /// cannot access routing, device state, or dispatch packets to a device.
    pub(crate) fn stack_device(&mut self) -> StackDevice<'_> {
        StackDevice::new(
            &mut self.rx_queue,
            &mut self.data_tx_queue,
            self.devices.effective_mtu(),
        )
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

        // A local address route is stored with loopback as its output device
        // but is owned by the address's interface.  Constrain and validate
        // the lookup through `lookup_device`, as Linux does for `RTA_OIF`,
        // rather than comparing the internal output rewrite.
        let oif = NetDeviceId::new(oif).filter(|id| id.ifindex() > 0);
        let rule = self
            .table
            .lookup_for_output(dst, oif)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        // An explicit `RTA_OIF` is validated against the FIB device before the
        // lookup, matching `ip_route_output_key_hash_rcu`.  A local /32 has
        // its FIB device on the address owner but is transmitted through
        // loopback; when no OIF was requested Linux still delivers to the
        // local address while the owner is down, so only the output device's
        // link state gates that path.
        if oif.is_some() {
            let fib_device = self
                .devices
                .get_by_id(rule.lookup_device())
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            if !fib_device.is_link_up() {
                return Err(LinuxError::ENETUNREACH.into());
            }
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
                .devices
                .ipv4(rule.dev)
                .and_then(|addrs| {
                    addrs.select_source(
                        &IpAddress::Ipv4(
                            rule.via
                                .and_then(ipv4_address)
                                .unwrap_or(Ipv4Address::UNSPECIFIED),
                        ),
                        rule.scope,
                    )
                })
                .map(IpAddress::Ipv4)
                .ok_or(KError::from(LinuxError::ENETUNREACH)),
        }
    }

    /// Resolves the effective IPv4 destination, source, and output route.
    ///
    /// This follows Linux's output lookup order: validate an explicit source,
    /// apply the output-interface shortcuts, handle the zero-destination
    /// loopback rewrite, and only then consult the routing table.  The lookup
    /// itself has no `FLOWI_FLAG_ANYSRC`; an explicit source must be local.
    ///
    /// An explicit `oif` constrains the FIB lookup and, when no route matches,
    /// synthesizes an on-link route exactly as `ip_route_output_key_hash_rcu`
    /// does.  The project does not model policy rules, marks, UIDs, protocol
    /// or port based routing, input routes, multipath, or metrics; those
    /// selectors are accepted but do not change the matched route.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for a multicast, limited-broadcast, or zero-network
    /// source, `ENETUNREACH` for a non-local source or an unreachable route,
    /// and `ENODEV` when an explicit output interface does not exist.
    pub(crate) fn lookup_ipv4_output(
        &self,
        dst: Ipv4Address,
        src: Option<Ipv4Address>,
        oif: Option<NetDeviceId>,
    ) -> KResult<Ipv4OutputRoute> {
        let requested = src.filter(|src| !src.is_unspecified());
        if let Some(src) = requested {
            if src.is_multicast() || src.is_broadcast() || src.is_zeronet() {
                return Err(LinuxError::EINVAL.into());
            }
            if self.ipv4_address_owner(src).is_none() {
                return Err(LinuxError::ENETUNREACH.into());
            }
        }

        let output_oif = oif;
        if let Some(id) = output_oif {
            let device = self.devices.get_by_id(id).ok_or(LinuxError::ENODEV)?;
            if !device.is_link_up() || self.devices.ipv4(id).is_none() {
                return Err(LinuxError::ENETUNREACH.into());
            }
        }

        if let Some(route) = self.ipv4_output_shortcut(dst, requested, output_oif)? {
            return self.finish_output_route(route, dst);
        }

        // Linux preselects an output-interface source only for the zero
        // destination (host scope) and multicast (universe scope).  Ordinary
        // output routes leave `fl4.saddr` unset until `fib_select_path`, so a
        // route's preferred source is not shadowed by a device-primary source.
        let mut flow_source = requested;
        if flow_source.is_none()
            && let Some(output) = output_oif
        {
            if dst.is_unspecified() {
                flow_source =
                    self.select_ipv4_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_HOST);
            } else if dst.is_multicast() {
                flow_source =
                    self.select_ipv4_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_UNIVERSE);
            }
        }

        let zero_destination = dst.is_unspecified();
        let lookup_dst = if zero_destination {
            flow_source.unwrap_or(Ipv4Address::LOOPBACK)
        } else {
            dst
        };

        let rule = self
            .table
            .lookup_for_output(&IpAddress::Ipv4(lookup_dst), output_oif);

        let route = if zero_destination {
            Ipv4OutputRoute {
                destination: lookup_dst,
                fib_entry: None,
                table: 0,
                route_type: ROUTE_TYPE_LOCAL,
                oif: NetDeviceId::LOOPBACK,
                source: flow_source.or(Some(lookup_dst)),
                gateway: None,
            }
        } else if let Some(rule) = rule {
            self.output_route_from_fib(*rule, dst, flow_source)
        } else if let Some(output) = output_oif {
            Ipv4OutputRoute {
                destination: dst,
                fib_entry: None,
                table: 0,
                route_type: ROUTE_TYPE_UNICAST,
                oif: output,
                source: flow_source.or_else(|| {
                    self.select_ipv4_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_LINK)
                }),
                gateway: None,
            }
        } else {
            return Err(LinuxError::ENETUNREACH.into());
        };

        self.finish_output_route(route, dst)
    }

    /// Applies the two output lookups that Linux resolves without consulting
    /// the FIB: an explicit multicast/broadcast source selects the owning
    /// device, and an explicit output interface selects a local multicast or
    /// limited-broadcast route.  IGMP is unreachable because this lookup has
    /// no protocol selector.
    fn ipv4_output_shortcut(
        &self,
        dst: Ipv4Address,
        requested: Option<Ipv4Address>,
        output_oif: Option<NetDeviceId>,
    ) -> KResult<Option<Ipv4OutputRoute>> {
        if output_oif.is_none()
            && (dst.is_multicast() || dst.is_broadcast())
            && let Some(source) = requested
        {
            let output = self
                .ipv4_address_owner(source)
                .ok_or(LinuxError::ENETUNREACH)?;
            return Ok(Some(Ipv4OutputRoute {
                destination: dst,
                fib_entry: None,
                table: 0,
                route_type: 0,
                oif: output,
                source: Some(source),
                gateway: None,
            }));
        }

        if let Some(output) = output_oif
            && (dst.is_local_multicast() || dst.is_broadcast())
        {
            let source = requested
                .or_else(|| {
                    self.select_ipv4_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_LINK)
                })
                .unwrap_or(Ipv4Address::UNSPECIFIED);
            return Ok(Some(Ipv4OutputRoute {
                destination: dst,
                fib_entry: None,
                table: 0,
                route_type: 0,
                oif: output,
                source: Some(source),
                gateway: None,
            }));
        }

        Ok(None)
    }

    /// Builds the output route for a matched FIB rule.  A local rule is served
    /// by the loopback device; other rules select their configured preferred
    /// source before falling back to address selection.
    fn output_route_from_fib(
        &self,
        rule: Rule,
        dst: Ipv4Address,
        flow_source: Option<Ipv4Address>,
    ) -> Ipv4OutputRoute {
        let (table, route_type, _) = rule.notification_route();
        let mut output_oif = rule.lookup_device();
        let mut selected_source = flow_source;
        if route_type == ROUTE_TYPE_LOCAL {
            if selected_source.is_none() {
                selected_source = rule.preferred_source().and_then(ipv4_address).or(Some(dst));
            }
            output_oif = NetDeviceId::LOOPBACK;
        } else if selected_source.is_none() {
            selected_source = rule.preferred_source().and_then(ipv4_address).or_else(|| {
                self.select_ipv4_source(
                    output_oif,
                    rule.via
                        .and_then(ipv4_address)
                        .unwrap_or(Ipv4Address::UNSPECIFIED),
                    rule.scope,
                )
            });
        }
        Ipv4OutputRoute {
            destination: dst,
            fib_entry: Some(rule),
            table,
            route_type,
            oif: output_oif,
            source: selected_source,
            gateway: if route_type == ROUTE_TYPE_LOCAL {
                None
            } else {
                rule.via.and_then(ipv4_address)
            },
        }
    }

    fn finish_output_route(
        &self,
        mut route: Ipv4OutputRoute,
        requested_dst: Ipv4Address,
    ) -> KResult<Ipv4OutputRoute> {
        if let Some(source) = route.source
            && source.is_loopback()
            && route.oif != NetDeviceId::LOOPBACK
        {
            return Err(LinuxError::EINVAL.into());
        }
        if requested_dst.is_multicast() {
            route.route_type = ROUTE_TYPE_MULTICAST;
            // `__mkroute_output` drops a multicast FIB result shorter than
            // the IPv4 multicast prefix, so the packet goes out on the link
            // without a gateway.  The FIB entry itself survives for
            // `RTM_F_FIB_MATCH` replies.
            if route
                .fib_entry
                .is_some_and(|fib| fib.filter.prefix_len() < 4)
            {
                route.gateway = None;
            }
        } else if requested_dst.is_broadcast() {
            route.route_type = ROUTE_TYPE_BROADCAST;
            // Linux unconditionally clears `fi` for limited broadcast.
            route.gateway = None;
        }
        Ok(route)
    }

    fn ipv4_address_owner(&self, addr: Ipv4Address) -> Option<NetDeviceId> {
        self.devices
            .iter_ipv4_addrs()
            .find(|(_, entry)| entry.addr.address() == addr)
            .map(|(dev, _)| dev)
    }

    fn select_ipv4_source(
        &self,
        dev: NetDeviceId,
        next_hop: Ipv4Address,
        scope: u8,
    ) -> Option<Ipv4Address> {
        self.devices
            .ipv4(dev)
            .and_then(|addrs| addrs.select_source(&IpAddress::Ipv4(next_hop), scope))
            .or_else(|| {
                // Linux continues through every device in `dev_base` order
                // when the output device has no eligible primary address.
                self.devices
                    .iter_ipv4_addrs()
                    .find_map(|(dev, _)| self.devices.ipv4(dev)?.select_fallback_source(scope))
            })
    }

    /// Prepares host-unicast forwarding without applying local-source restrictions.
    ///
    /// Requires task context under the Router lock. Errors retain the original
    /// packet for an ICMP reply with a separately selected local source.
    fn prepare_ipv4_forward(
        &self,
        input: Ipv4ForwardInput,
    ) -> Result<(PreparedIpv4Forward, Ipv4ForwardRoute), Ipv4ForwardFailure> {
        let destination = input.destination();
        let Some(route) = self.table.lookup(&IpAddress::Ipv4(destination)) else {
            return Err(input.no_route());
        };
        let device = self
            .devices
            .get_by_id(route.dev)
            .expect("route device must be registered");
        if route.route_type != ROUTE_TYPE_UNICAST || device.link_kind() == LinkKind::Loopback {
            return Err(input.discard());
        }
        if !device.is_link_up() {
            return Err(input.no_route());
        }
        let next_hop = match route.via {
            Some(IpAddress::Ipv4(gateway)) => gateway,
            None => destination,
            Some(_) => return Err(input.no_route()),
        };
        let Some(neighbor_source) = self.devices.ipv4(route.dev).and_then(|addresses| {
            addresses.select_source(&IpAddress::Ipv4(next_hop), ROUTE_SCOPE_LINK)
        }) else {
            return Err(input.discard());
        };
        let packet = input.prepare(device.mtu().min(u16::MAX as usize) as u16)?;
        Ok((
            packet,
            Ipv4ForwardRoute {
                device: route.dev,
                next_hop,
                neighbor_source,
            },
        ))
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

    pub(crate) fn queue_ipv4_packet(&mut self, packet: Vec<u8>, oif: i32) -> KResult {
        self.check_ipv4_output_size(&packet)?;
        self.data_tx_queue
            .try_extend(core::iter::once(PacketBuf::from_ip_packet_vec(
                oif,
                packet,
                PacketOwner::Ipv4Stack,
            )))
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
        self.check_ipv4_output_size(&packet)?;
        self.dispatch_ipv4_output(tx_packet_buf(packet), timestamp, crate::netfilter::global());
        Ok(())
    }

    pub(crate) fn add_device(
        &mut self,
        device: KBox<dyn NetDevice>,
    ) -> Result<NetDeviceId, LinuxError> {
        self.devices.register(device)
    }

    /// Test-only compatibility lookup for legacy DeviceId-based fixtures.
    #[cfg(unittest)]
    pub(crate) fn remove_device_by_model_id(
        &mut self,
        id: kdevice::DeviceId,
    ) -> Option<Vec<NetworkEvent>> {
        let publication = self
            .devices
            .iter()
            .find(|device| device.device_id() == Some(id))
            .and_then(|device| device.publication_id())?;
        self.remove_device_by_publication(publication)
    }

    /// Removes a device and returns the notifications describing the state it
    /// owned.  Address, route, and neighbor removals precede the link removal
    /// so a subscriber can still resolve the interface while processing them.
    pub(crate) fn remove_device_by_publication(
        &mut self,
        publication: kdevice::PublicationId,
    ) -> Option<Vec<NetworkEvent>> {
        let pos = self
            .devices
            .iter()
            .position(|device| device.publication_id() == Some(publication))?;
        let dev = self.devices.id_at(pos);
        let mut link = self.devices.get_by_id(dev)?.link_snapshot(dev.ifindex());
        let neighbors = self.devices.get_by_id(dev)?.neighbor_snapshots(dev);
        let addresses: Vec<_> = self
            .ipv4_addr_snapshots()
            .into_iter()
            .filter(|snapshot| snapshot.dev == dev)
            .collect();
        let routes_before = self.table.snapshot();

        let removed_sources = self.devices.remove_slot(pos);
        self.reconcile_forward_output();
        self.table.remove_device(dev);
        for source in removed_sources {
            if !self.has_ipv4_address(source) {
                self.table
                    .remove_configured_routes_with_source(source.into());
                for device in self.devices.iter_mut() {
                    device.remove_pending_ipv4_source(source);
                }
            }
        }
        self.table.sync_ipv4_address_routes(&self.devices);
        self.adjust_rx_device_cursor_after_remove(pos);

        let routes_after = self.table.snapshot();
        let mut remaining = routes_after;
        let mut routes = Vec::new();
        for route in routes_before {
            if let Some(index) = remaining.iter().position(|candidate| *candidate == route) {
                remaining.swap_remove(index);
            } else {
                routes.push(route);
            }
        }
        let mut events = Vec::with_capacity(addresses.len() + routes.len() + neighbors.len() + 2);
        // `unregister_netdevice_many` calls `dev_close_many(..., true)` first.
        // For an active device that publishes RTM_NEWLINK with IFF_UP and
        // IFF_RUNNING cleared (and already reflects the down operstate) before
        // any address, route, or neighbor is removed.
        if link.flags & LINK_FLAG_UP != 0 {
            link.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            link.operstate = IF_OPER_DOWN;
            events.push(NetworkEvent::LinkChanged {
                changed_flags: LINK_FLAG_UP | LINK_FLAG_RUNNING,
                link: link.clone(),
            });
        }
        // Linux destroys an interface's addresses one at a time and flushes the
        // routes derived from each address before moving to the next one.  Do
        // not publish every address first: a listener applying the events in
        // order must never observe an address whose connected/local routes are
        // still present (or routes whose source address is already gone).
        let mut remaining_routes = routes;
        for addr in addresses {
            let entry = addr.entry;
            events.push(NetworkEvent::AddrRemoved { addr });
            events.extend(
                remaining_routes
                    .extract_if(.., |route| {
                        Self::route_derived_from_address(*route, dev, entry)
                    })
                    .map(|route| NetworkEvent::RouteRemoved {
                        route,
                        provenance: RouteProvenance::Kernel,
                    }),
            );
        }
        // Routes configured directly on the departing device (including routes
        // whose preferred source belongs to another interface) are flushed only
        // after all address-derived state is gone.
        events.extend(
            remaining_routes
                .into_iter()
                .map(|route| NetworkEvent::RouteRemoved {
                    route,
                    provenance: RouteProvenance::Kernel,
                }),
        );
        events.extend(
            neighbors
                .into_iter()
                .map(|neighbor| NetworkEvent::NeighborRemoved { neighbor }),
        );
        events.push(NetworkEvent::LinkRemoved { link });
        Some(events)
    }

    /// Returns whether `route` was created from `addr` on `dev`, either as its
    /// local host route, its connected-network route, or a configured route
    /// pinned to the address as preferred source.
    fn route_derived_from_address(route: Rule, dev: NetDeviceId, entry: Ipv4AddrEntry) -> bool {
        route.is_derived_from_address(dev, entry)
            || route.preferred_source() == Some(IpAddress::Ipv4(entry.addr.address()))
    }

    /// Reconciles the kernel-derived IPv4 routes with device and address
    /// state.  Link changes add or remove prefix and broadcast routes while
    /// the local host route remains installed.
    pub(crate) fn sync_ipv4_address_routes(&mut self) {
        self.table.sync_ipv4_address_routes(&self.devices);
    }

    pub(crate) fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.devices
            .iter_ipv4_addrs()
            .map(|(_, entry)| entry.addr)
            .collect()
    }

    /// Returns whether `address` is an IPv4 limited or directed broadcast.
    pub(crate) fn ipv4_dest_requires_broadcast(&self, address: &IpAddress) -> bool {
        let IpAddress::Ipv4(address) = *address else {
            return false;
        };
        address.is_broadcast()
            || self
                .devices
                .iter_ipv4_addrs()
                .any(|(_, entry)| entry.addr.is_directed_broadcast(address))
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
        let dev = self
            .devices
            .device_id_by_name(name)
            .ok_or(LinuxError::ENODEV)?;
        self.first_ipv4_entry(dev)
            .map(|entry| entry.addr)
            .ok_or(LinuxError::EADDRNOTAVAIL)
    }

    fn first_ipv4_addr_for_device(&self, ifindex: i32) -> Option<Ipv4Cidr> {
        let dev = NetDeviceId::new(ifindex)?;
        self.first_ipv4_entry(dev).map(|entry| entry.addr)
    }

    /// Selects the address used by legacy interface queries and updates.
    fn first_ipv4_entry(&self, dev: NetDeviceId) -> Option<Ipv4AddrEntry> {
        self.devices.ipv4(dev)?.as_slice().first().copied()
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addr_entries(&self, dev: NetDeviceId) -> &[Ipv4AddrEntry] {
        self.devices.ipv4(dev).map_or(&[], |addrs| addrs.as_slice())
    }

    pub(crate) fn has_ipv4_addr_entry(&self, dev: NetDeviceId, addr: Ipv4Cidr) -> bool {
        self.devices
            .ipv4(dev)
            .is_some_and(|addrs| addrs.contains_entry(addr))
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.devices
            .iter_ipv4_addrs()
            .filter_map(|(dev, entry)| self.ipv4_addr_snapshot_for(dev, entry))
            .collect()
    }

    /// Returns the notification projection of one address entry.
    ///
    /// The entry and its owning device come from the same registry, so this
    /// succeeds for every entry obtained from [`Self::ipv4_addr_snapshots`]
    /// while the enclosing Router lock is held.
    pub(crate) fn ipv4_addr_snapshot(
        &self,
        dev: NetDeviceId,
        addr: Ipv4Cidr,
    ) -> Option<Ipv4AddrSnapshot> {
        let entry = self
            .devices
            .ipv4(dev)?
            .as_slice()
            .iter()
            .find(|entry| entry.addr == addr)?;
        self.ipv4_addr_snapshot_for(dev, entry)
    }

    fn ipv4_addr_snapshot_for(
        &self,
        dev: NetDeviceId,
        entry: &Ipv4AddrEntry,
    ) -> Option<Ipv4AddrSnapshot> {
        let device = self.devices.get_by_id(dev)?;
        let secondary = self.devices.ipv4(dev)?.is_secondary(entry.addr);
        let label = entry.label.unwrap_or_else(|| {
            let mut label = [0; 16];
            let name = device.name().as_bytes();
            let len = name.len().min(15);
            label[..len].copy_from_slice(&name[..len]);
            label
        });
        Some(Ipv4AddrSnapshot {
            dev,
            entry: *entry,
            label,
            secondary,
        })
    }

    pub(crate) fn add_ipv4_addr(
        &mut self,
        dev: NetDeviceId,
        entry: Ipv4AddrEntry,
    ) -> Result<(), LinuxError> {
        let change = self.devices.add_ipv4_addr(dev, entry)?;
        self.reconcile_ipv4_change(change);
        Ok(())
    }

    /// Sets the first address; the same local address preserves existing attributes.
    pub(crate) fn set_primary_ipv4_addr(
        &mut self,
        dev: NetDeviceId,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        if self.devices.slot(dev).is_none() {
            return Err(LinuxError::ENODEV);
        }
        let current = self.first_ipv4_entry(dev);
        if current.is_some_and(|entry| entry.addr.address() == addr.address()) {
            return Ok(());
        }
        let device = self
            .devices
            .get_by_id(dev)
            .expect("address device must be registered");
        // `SIOCSIFADDR` stores the computed broadcast for a broadcast-capable
        // interface with a prefix shorter than `/31`. The stack models that
        // capability as an Ethernet link.
        let broadcast = if device.link_kind() == LinkKind::Ethernet && addr.prefix_len() < 31 {
            addr.broadcast()
        } else {
            None
        };
        // `SIOCSIFADDR` reuses an existing `in_ifaddr` and therefore keeps
        // its flags, including `IFA_F_NOPREFIXROUTE`.  A newly created
        // address starts without that suppression.
        let entry = Ipv4AddrEntry {
            addr,
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast,
            no_prefix_route: current.is_some_and(|entry| entry.no_prefix_route),
            label: current.and_then(|entry| entry.label),
        };
        let Some(current) = current else {
            return self.add_ipv4_addr(dev, entry);
        };
        let change = self.devices.replace_ipv4_addr(dev, current.addr, entry)?;
        self.reconcile_ipv4_change(change);
        Ok(())
    }

    /// Removes the first address; an addressless registered interface is a no-op.
    pub(crate) fn remove_primary_ipv4_addr(
        &mut self,
        dev: NetDeviceId,
    ) -> Result<Vec<Ipv4Address>, LinuxError> {
        if self.devices.slot(dev).is_none() {
            return Err(LinuxError::ENODEV);
        }
        Ok(self
            .first_ipv4_entry(dev)
            .map_or_else(Vec::new, |entry| self.remove_ipv4_addr(dev, entry.addr)))
    }

    /// Removes an address and returns the removed sources in notification order.
    ///
    /// A primary removal cascades to its secondaries, and Linux announces each
    /// secondary before the primary.  Passing that order back to the caller
    /// keeps neutral events and rtnetlink notifications in the same sequence
    /// instead of reconstructing it from an unordered before/after diff.
    pub(crate) fn remove_ipv4_addr(
        &mut self,
        dev: NetDeviceId,
        addr: Ipv4Cidr,
    ) -> Vec<Ipv4Address> {
        let change = self.devices.remove_ipv4_addr(dev, addr);
        self.reconcile_ipv4_change(change)
    }

    pub(crate) fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.devices
            .iter_ipv4_addrs()
            .any(|(_, entry)| entry.addr.address() == address)
    }

    pub(crate) fn add_route_rule(&mut self, rule: Rule) -> Result<(), LinuxError> {
        self.table.add_configured_rule(rule, &self.devices)
    }

    pub(crate) fn replace_route_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.table
            .replace_configured_rule(existing, replacement, &self.devices)
    }

    /// Updates the stored broadcast address of the first IPv4 address.
    ///
    /// `SIOCSIFBRDADDR` replaces the address even when the requested value
    /// equals the CIDR-derived broadcast, because the comparison is against
    /// the stored `ifa_broadcast`. The zero address clears the stored value.
    pub(crate) fn set_ipv4_broadcast(
        &mut self,
        dev: NetDeviceId,
        broadcast: Ipv4Address,
    ) -> Result<(), LinuxError> {
        let mut entry = self
            .first_ipv4_entry(dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        let broadcast = (!broadcast.is_unspecified()).then_some(broadcast);
        if entry.broadcast == broadcast {
            return Ok(());
        }
        let old_addr = entry.addr;
        entry.broadcast = broadcast;
        let change = self.devices.replace_ipv4_addr(dev, old_addr, entry)?;
        self.reconcile_ipv4_change(change);
        Ok(())
    }

    /// Changes the first address prefix, retaining scope and custom broadcast values.
    pub(crate) fn set_ipv4_netmask(
        &mut self,
        dev: NetDeviceId,
        prefix_len: u8,
    ) -> Result<(), LinuxError> {
        if prefix_len > 32 {
            return Err(LinuxError::EINVAL);
        }
        let mut entry = self
            .first_ipv4_entry(dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        if entry.addr.prefix_len() == prefix_len {
            return Ok(());
        }
        let old_addr = entry.addr;
        let addr =
            Ipv4Cidr::new(entry.addr.address(), prefix_len).expect("prefix length was validated");
        if self.has_ipv4_addr_entry(dev, addr) {
            // Linux drops the old entry when netmask reinsertion collides,
            // and SIOCSIFNETMASK still reports success.
            self.remove_ipv4_addr(dev, old_addr);
            return Ok(());
        }
        let has_broadcast_flag = self
            .devices
            .get_by_id(dev)
            .is_some_and(|device| device.link_kind() == LinkKind::Ethernet);
        let old_derived_broadcast = entry.addr.broadcast();
        // Linux retains the stored broadcast when the new prefix is `/31` or
        // `/32`, and only rewrites it when it still equals the broadcast
        // computed from the old mask. A zero stored value stays zero.
        if has_broadcast_flag && prefix_len < 31 && entry.broadcast == old_derived_broadcast {
            entry.broadcast = addr.broadcast();
        }
        entry.addr = addr;
        let change = self.devices.replace_ipv4_addr(dev, old_addr, entry)?;
        self.reconcile_ipv4_change(change);
        Ok(())
    }

    /// Reconciles cross-object state before releasing the enclosing Router lock.
    fn reconcile_ipv4_change(&mut self, change: Ipv4AddrChange) -> Vec<Ipv4Address> {
        let Ipv4AddrChange::Updated { old_sources } = change else {
            return Vec::new();
        };
        for source in &old_sources {
            self.remove_ipv4_source_state_if_unowned(*source);
        }
        self.reconcile_forward_output();
        self.table.sync_ipv4_address_routes(&self.devices);
        old_sources
    }

    /// Retired devices and neighbor sources cannot keep pending output alive.
    fn reconcile_forward_output(&mut self) {
        if self.pending_forward.as_ref().is_some_and(|pending| {
            !self
                .devices
                .ipv4(pending.route.device)
                .is_some_and(|addresses| {
                    addresses
                        .as_slice()
                        .iter()
                        .any(|entry| entry.addr.address() == pending.route.neighbor_source)
                })
        }) {
            self.pending_forward = None;
        }
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

    #[cfg(unittest)]
    pub(crate) fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<Option<NetworkEvent>, LinuxError> {
        let (dev, dst) = (update.dev, update.dst);
        self.apply_neighbor_admin_update(dev, dst, policy, |_| Ok(update))
    }

    /// Applies a control-plane neighbor update after resolving its existence
    /// policy.
    ///
    /// Linux `neigh_add` performs the `NLM_F_EXCL`, `NLM_F_CREATE`, and
    /// existing-entry checks before it interprets the requested state, flags,
    /// or missing link-layer address.  Building the update only after those
    /// checks lets the rtnetlink layer preserve the same error precedence
    /// without duplicating the table policy here.
    pub(crate) fn apply_neighbor_admin_update<F>(
        &mut self,
        dev: NetDeviceId,
        dst: IpAddress,
        policy: NeighborUpdatePolicy,
        build: F,
    ) -> Result<Option<NetworkEvent>, LinuxError>
    where
        F: FnOnce(Option<NeighborState>) -> Result<NeighborUpdate, LinuxError>,
    {
        let device = self.devices.get_mut_by_id(dev).ok_or(LinuxError::ENODEV)?;
        let exists = device.has_neighbor(dst);
        if exists && policy.exclusive {
            return Err(LinuxError::EEXIST);
        }
        if !exists && !policy.can_create {
            return Err(LinuxError::ENOENT);
        }
        let before = device.neighbor_state(dst);
        let update = build(before)?;
        debug_assert_eq!(update.dev, dev);
        debug_assert_eq!(update.dst, dst);
        device.apply_neighbor_update(update, policy.can_override)?;
        let after = device.neighbor_state(dst);
        if before == after {
            return Ok(None);
        }
        Ok(Some(NetworkEvent::NeighborAdded { neighbor: update }))
    }

    /// Removes one neighbor entry, matching `RTM_DELNEIGH` key semantics.
    ///
    /// Linux deletes by device and destination address; state and link-layer
    /// attributes in the request are not part of the lookup.  A missing device
    /// or entry is reported as `ENODEV` or `ENOENT`.
    pub(crate) fn remove_neighbor(
        &mut self,
        dev: NetDeviceId,
        dst: IpAddress,
    ) -> Result<NeighborSnapshot, LinuxError> {
        let device = self.devices.get_mut_by_id(dev).ok_or(LinuxError::ENODEV)?;
        let state = device.neighbor_state(dst).ok_or(LinuxError::ENOENT)?;
        if !device.remove_neighbor(dst) {
            return Err(LinuxError::ENOENT);
        }
        Ok(NeighborSnapshot { dev, dst, state })
    }

    /// Returns one stable snapshot per neighbor currently held by the stack.
    ///
    /// Device registration order is preserved so neighbor dumps are
    /// deterministic without exposing the mutable device registry.
    pub(crate) fn neighbor_snapshots(&self) -> Vec<NeighborSnapshot> {
        self.devices
            .iter_with_ids()
            .flat_map(|(dev, device)| device.neighbor_snapshots(dev))
            .collect()
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: NetDeviceId, dst: IpAddress) -> bool {
        self.devices
            .get_by_id(dev)
            .is_some_and(|device| device.has_neighbor(dst))
    }

    pub(crate) fn drain_rx_budgeted_into(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<RxPacket>,
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
                    self.collect_neighbor_events();
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
                    Ok(RxPoll::Consumed(packet)) => packet,
                    Ok(RxPoll::Empty) => break,
                    Err(error) => {
                        self.collect_neighbor_events();
                        self.next_rx_device = next_device_index(dev_index, device_count);
                        return RxDrain {
                            work_done,
                            has_more: self.has_immediate_rx_work(),
                            error: Some((ifindex, error)),
                        };
                    }
                };
                work_done += 1;
                if let Some(packet) = packet {
                    packets.push(packet);
                }
            }
            self.next_rx_device = next_device_index(dev_index, device_count);
        }

        self.collect_neighbor_events();
        RxDrain {
            work_done,
            has_more: false,
            error: None,
        }
    }

    /// Moves neighbor changes from every device into the Router queue.
    ///
    /// Called only after a completed device operation, so the device-local
    /// queue is drained at the same point that the Router releases its lock.
    fn collect_neighbor_events(&mut self) {
        let mut events = Vec::new();
        for device in self.devices.iter_mut() {
            events.append(&mut device.take_neighbor_events());
        }
        self.neighbor_events.extend(events);
    }

    /// Drains data-plane neighbor changes for publication outside the Router
    /// lock.  Control-plane mutations continue to return their events directly.
    pub(crate) fn take_neighbor_events(&mut self) -> Vec<NeighborEvent> {
        core::mem::take(&mut self.neighbor_events)
    }

    pub(crate) fn enqueue_ingress_packets(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(packets.len() <= self.ingress_capacity());
        self.rx_queue.extend(packets.drain(..));
    }

    pub(crate) fn ingress_capacity(&self) -> usize {
        SOCKET_BUFFER_SIZE.saturating_sub(self.rx_queue.len().max(self.forward_queue.len()))
    }

    pub(crate) fn ipv4_forwarding(&self) -> i32 {
        self.ipv4_forwarding
    }

    pub(crate) fn set_ipv4_forwarding(&mut self, value: i32) {
        self.ipv4_forwarding = value;
        if value == 0 {
            self.forward_queue.clear();
            self.pending_forward = None;
        }
    }

    /// Batch sizes are limited by `ingress_capacity` before draining devices.
    pub(crate) fn enqueue_forward_packets(&mut self, packets: &mut Vec<Ipv4ForwardInput>) {
        if self.ipv4_forwarding != 0 {
            debug_assert!(packets.len() <= SOCKET_BUFFER_SIZE - self.forward_queue.len());
            self.forward_queue.extend(packets.drain(..));
        } else {
            packets.clear();
        }
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
        self.dispatch_output_budgeted(timestamp, budget, crate::netfilter::global())
    }

    pub(crate) fn dispatch_output_budgeted(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
        filter: &FilterConfig,
    ) -> (usize, bool) {
        if budget == 0 {
            return (0, self.has_queued_tx_packets());
        }

        let mut work_done = 0;
        let mut poll_next = false;
        while work_done < budget {
            if (self.next_tx_is_forward
                || self.control_tx_queue.is_empty() && self.data_tx_queue.is_empty())
                && (self.pending_forward.is_some() || !self.forward_queue.is_empty())
            {
                self.next_tx_is_forward = false;
                work_done += 1;
                poll_next |= self.dispatch_ipv4_forward(timestamp, filter);
                continue;
            }
            let Some(mut packet) = self.pop_tx_packet() else {
                break;
            };
            self.next_tx_is_forward = true;
            work_done += 1;
            packet.set_owner(PacketOwner::DeviceTx);
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => {
                    poll_next |= self.dispatch_ipv4_output(packet, timestamp, filter);
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
        self.collect_neighbor_events();
        (work_done, poll_next || self.has_queued_tx_packets())
    }

    fn dispatch_ipv4_forward(
        &mut self,
        timestamp: MonotonicInstant,
        filter: &FilterConfig,
    ) -> bool {
        if self.pending_forward.is_none() {
            let input = self
                .forward_queue
                .pop_front()
                .expect("pending forwarding work");
            let (packet, route) = match self.prepare_ipv4_forward(input) {
                Ok(prepared) => prepared,
                Err(failure) => {
                    self.reply_forward_error(failure);
                    return false;
                }
            };
            let Some(indev) = NetDeviceId::new(packet.packet().ifindex())
                .and_then(|id| self.devices.get_by_id(id))
            else {
                return false;
            };
            let outdev = self
                .devices
                .get_by_id(route.device)
                .expect("selected route device");
            if !filter.accept_forward(packet.packet(), indev.name(), outdev.name()) {
                return false;
            }
            let Ok(packets) = packet.into_packets() else {
                return false;
            };
            self.pending_forward = Some(PendingIpv4Forward {
                route,
                packets: packets.into_iter(),
            });
        }

        let pending = self
            .pending_forward
            .as_mut()
            .expect("prepared forwarding output");
        let device = self
            .devices
            .get_mut_by_id(pending.route.device)
            .expect("device removal discards pending forwarding output");
        let mut packet = pending.packets.next().expect("pending forwarding fragment");
        packet.set_ifindex(pending.route.device.ifindex());
        let poll_next = device.send_ip_packet(
            pending.route.device.ifindex(),
            IpAddress::Ipv4(pending.route.next_hop),
            IpAddress::Ipv4(pending.route.neighbor_source),
            packet,
            timestamp,
        );
        if pending.packets.len() == 0 {
            self.pending_forward = None;
        }
        poll_next
    }

    fn reply_forward_error(&mut self, failure: ipv4::forward::Ipv4ForwardFailure) {
        let Some(destination) = failure.reply_destination() else {
            return;
        };
        let Ok(IpAddress::Ipv4(source)) =
            self.output_route_source(&IpAddress::Ipv4(destination), 0)
        else {
            return;
        };
        if let Some(reply) = failure.icmp_reply(source) {
            let _ = self.queue_control_ipv4_packet(reply);
        }
    }

    /// Dispatches one IPv4 packet onto the selected device.
    ///
    /// Returns whether the send made RX ready, which [`Self::dispatch_budgeted`]
    /// uses as its `poll_next` hint. Ethernet TX and dropped packets return
    /// `false`.
    pub(crate) fn dispatch_ipv4_output(
        &mut self,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
        filter: &FilterConfig,
    ) -> bool {
        if crate::ipv4::prepare_output_packet(&mut packet).is_err() {
            return false;
        }
        let header =
            Ipv4Header::parse_header_only(packet.network_packet().expect("finalized IPv4 packet"))
                .expect("finalized IPv4 header");

        let ipv4_src_addr = header.src_addr();
        let src_addr = IpAddress::Ipv4(ipv4_src_addr);
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
                poll_next |= send_ipv4_output(
                    dev,
                    ifindex,
                    dst_addr,
                    packet.clone(),
                    timestamp,
                    &mut self.next_ipv4_identification,
                    filter,
                );
            }
            let last_ifindex = last_id.ifindex();
            if bound_if == 0 || bound_if == last_ifindex {
                poll_next |= send_ipv4_output(
                    last_device,
                    last_ifindex,
                    dst_addr,
                    packet,
                    timestamp,
                    &mut self.next_ipv4_identification,
                    filter,
                );
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
        if !self.is_local_ipv4_source(ipv4_src_addr) {
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
        send_ipv4_output(
            dev,
            rule.dev.ifindex(),
            next_hop,
            packet,
            timestamp,
            &mut self.next_ipv4_identification,
            filter,
        )
    }

    fn check_ipv4_output_size(&self, packet: &[u8]) -> KResult {
        if packet.len() > usize::from(u16::MAX) {
            return Err(KError::InvalidInput);
        }
        let header = Ipv4Header::parse_header_only(packet).map_err(|_| KError::InvalidInput)?;
        if let Some(mtu) = self.route_mtu(&IpAddress::Ipv4(header.dst_addr()))
            && packet.len() > mtu
            && (header.dont_fragment() || mtu < header.header_len() + 8)
        {
            return Err(LinuxError::EMSGSIZE.into());
        }
        Ok(())
    }

    fn is_local_ipv4_source(&self, src_addr: crate::ip::Ipv4Address) -> bool {
        self.has_ipv4_address(src_addr)
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
        !self.control_tx_queue.is_empty()
            || !self.data_tx_queue.is_empty()
            || !self.forward_queue.is_empty()
            || self.pending_forward.is_some()
    }

    pub(crate) fn has_immediate_rx_work(&self) -> bool {
        self.devices.iter().any(|device| device.has_rx_work())
    }

    /// Returns the earliest device timer deadline that must wake the network
    /// poller even when no frame arrives.
    pub(crate) fn next_neighbor_deadline(&self) -> Option<MonotonicInstant> {
        self.devices
            .iter()
            .filter_map(|device| device.next_neighbor_deadline())
            .min()
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

/// Applies OUTPUT to the complete datagram for this device before fragmentation.
fn send_ipv4_output(
    dev: &mut dyn NetDevice,
    ifindex: i32,
    next_hop: IpAddress,
    packet: PacketBuf,
    timestamp: MonotonicInstant,
    identification: &mut u16,
    filter: &FilterConfig,
) -> bool {
    if !filter.accept_output(&packet, dev.name()) {
        return false;
    }
    let bytes = packet.network_packet().expect("finalized IPv4 packet");
    let header = Ipv4Header::parse_header_only(bytes).expect("finalized IPv4 header");
    let source = IpAddress::Ipv4(header.src_addr());
    if bytes.len() <= dev.mtu() {
        return dev.send_ip_packet(ifindex, next_hop, source, packet, timestamp);
    }
    let id = *identification;
    *identification = identification.wrapping_add(1).max(1);
    let Ok(fragments) = ipv4::fragment_output_packet(bytes, dev.mtu(), id) else {
        return false;
    };
    let mut poll_next = false;
    for bytes in fragments {
        let fragment = PacketBuf::from_ip_packet_vec(ifindex, bytes, PacketOwner::DeviceTx);
        poll_next |= dev.send_ip_packet(ifindex, next_hop, source, fragment, timestamp);
    }
    poll_next
}

fn tx_packet_buf(packet: Vec<u8>) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)
}

unittest::path_mod_test!(tests, "../../ut-tests/stack/router/tests.rs");
unittest::path_mod_test!(
    forwarding_tests,
    "../../ut-tests/stack/router/forwarding.rs"
);
