// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Route rules, configuration validation, address-derived routes, and lookup.

use alloc::vec::Vec;
use core::cmp::Reverse;

use kerrno::LinuxError;

pub(crate) use crate::netlink::{
    RT_SCOPE_HOST as ROUTE_SCOPE_HOST, RT_SCOPE_LINK as ROUTE_SCOPE_LINK,
    RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE, RT_TABLE_LOCAL as ROUTE_TABLE_LOCAL,
    RT_TABLE_MAIN as ROUTE_TABLE_MAIN, RTN_BROADCAST as ROUTE_TYPE_BROADCAST,
    RTN_LOCAL as ROUTE_TYPE_LOCAL, RTN_MULTICAST as ROUTE_TYPE_MULTICAST,
    RTN_UNICAST as ROUTE_TYPE_UNICAST, RTPROT_BOOT as ROUTE_PROTOCOL_BOOT,
    RTPROT_KERNEL as ROUTE_PROTOCOL_KERNEL,
};
use crate::{
    device::{DeviceRegistry, LinkKind, NetDeviceId},
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    network::interface_address::Ipv4AddrEntry,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ipv4AddressRouteKind {
    Local,
    Connected,
    Broadcast,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleSource {
    DevicePrimary,
    Fixed(IpAddress),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleOrigin {
    Configured,
    Ipv4Address {
        owner_dev: NetDeviceId,
        owner_addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
    },
}

/// Matches an address-derived route by origin and destination prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AddressRouteId {
    owner_dev: NetDeviceId,
    owner_addr: Ipv4Cidr,
    filter: IpCidr,
    kind: Ipv4AddressRouteKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: NetDeviceId,
    pub src: IpAddress,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
    source: RuleSource,
    origin: RuleOrigin,
}

impl Rule {
    pub(crate) fn new(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: NetDeviceId,
        src: IpAddress,
    ) -> Self {
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

    pub(crate) fn with_route_attrs(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: NetDeviceId,
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

    pub(crate) fn for_local_ipv4_address(
        loopback_dev: NetDeviceId,
        owner_dev: NetDeviceId,
        local_addr: Ipv4Cidr,
        prefsrc: Ipv4Cidr,
    ) -> Self {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: 32 is a valid IPv4 prefix length for \
                      the local host route"
        )]
        let filter = Ipv4Cidr::new(local_addr.address(), 32).unwrap();
        let mut rule = Self::for_ipv4_address(
            filter,
            loopback_dev,
            owner_dev,
            local_addr,
            prefsrc,
            Ipv4AddressRouteKind::Local,
            ROUTE_SCOPE_HOST,
        );
        rule.table = ROUTE_TABLE_LOCAL;
        rule.route_type = ROUTE_TYPE_LOCAL;
        rule
    }

    pub(crate) fn for_connected_ipv4_address(
        dev: NetDeviceId,
        owner_addr: Ipv4Cidr,
        scope: u8,
    ) -> Self {
        let mut rule = Self::for_ipv4_address(
            owner_addr.network(),
            dev,
            dev,
            owner_addr,
            owner_addr,
            Ipv4AddressRouteKind::Connected,
            scope,
        );
        // A loopback prefix route is a local-table RTN_LOCAL entry.  Other
        // connected prefixes are ordinary main-table unicast routes.
        if scope == ROUTE_SCOPE_HOST {
            rule.table = ROUTE_TABLE_LOCAL;
            rule.route_type = ROUTE_TYPE_LOCAL;
        }
        rule
    }

    pub(crate) fn for_broadcast_ipv4_address(
        dev: NetDeviceId,
        owner_addr: Ipv4Cidr,
        prefsrc: Ipv4Cidr,
        broadcast: Ipv4Address,
    ) -> Self {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: IPv4 broadcast routes are /32"
        )]
        let filter = Ipv4Cidr::new(broadcast, 32).unwrap();
        let mut rule = Self::for_ipv4_address(
            filter,
            dev,
            dev,
            owner_addr,
            prefsrc,
            Ipv4AddressRouteKind::Broadcast,
            ROUTE_SCOPE_LINK,
        );
        rule.table = ROUTE_TABLE_LOCAL;
        rule.route_type = ROUTE_TYPE_BROADCAST;
        rule
    }

    fn for_ipv4_address(
        filter: Ipv4Cidr,
        dev: NetDeviceId,
        owner_dev: NetDeviceId,
        owner_addr: Ipv4Cidr,
        prefsrc: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
        scope: u8,
    ) -> Self {
        let source = IpAddress::Ipv4(prefsrc.address());
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
                owner_dev,
                owner_addr,
                kind,
            },
        }
    }

    /// Returns the table, type, and output device to report for an rtnetlink
    /// route notification.
    ///
    /// The local /32 route is stored with the loopback output device so that
    /// lookups can select it, but Linux installs it in the local table as
    /// `RTN_LOCAL` on the address's own device.  Notification consumers see
    /// the FIB representation, not the internal lookup shortcut.
    pub(crate) fn notification_route(self) -> (u8, u8, NetDeviceId) {
        if let RuleOrigin::Ipv4Address {
            owner_dev,
            kind: Ipv4AddressRouteKind::Local,
            ..
        } = self.origin
        {
            (ROUTE_TABLE_LOCAL, ROUTE_TYPE_LOCAL, owner_dev)
        } else {
            (self.table, self.route_type, self.dev)
        }
    }

    /// Returns the device that owns this route in the FIB.
    ///
    /// A local address route is stored with `dev` set to loopback so that
    /// destination lookups select it, while Linux owns the corresponding
    /// local-table entry on the interface carrying the address.  Output
    /// lookup constraints such as `RTA_OIF` are checked against the FIB
    /// device, not the internal output rewrite.
    pub(crate) fn lookup_device(self) -> NetDeviceId {
        if let RuleOrigin::Ipv4Address {
            owner_dev,
            kind: Ipv4AddressRouteKind::Local,
            ..
        } = self.origin
        {
            owner_dev
        } else {
            self.dev
        }
    }

    /// Returns whether this route was generated from `entry` on `dev`.
    ///
    /// Address-derived routes are the local /32 host route, the connected
    /// network route, and both the explicit and computed broadcast /32 routes.
    /// Configured routes pinned to the address's preferred source are handled
    /// separately by the caller, since they may live on a different device.
    pub(crate) fn is_derived_from_address(self, dev: NetDeviceId, entry: Ipv4AddrEntry) -> bool {
        let RuleOrigin::Ipv4Address {
            owner_dev,
            owner_addr,
            kind,
        } = self.origin
        else {
            return false;
        };
        if owner_dev != dev || owner_addr != entry.addr {
            return false;
        }
        let addr = entry.addr;
        match kind {
            Ipv4AddressRouteKind::Local => {
                self.filter.prefix_len() == 32
                    && self.filter.address() == IpAddress::Ipv4(addr.address())
            }
            Ipv4AddressRouteKind::Connected => {
                self.filter.prefix_len() == addr.prefix_len()
                    && self.filter.address() == IpAddress::Ipv4(addr.network().address())
            }
            Ipv4AddressRouteKind::Broadcast => {
                self.filter.prefix_len() == 32
                    && matches!(
                        self.filter.address(),
                        IpAddress::Ipv4(broadcast)
                            if Some(broadcast) == entry.broadcast
                                || Some(broadcast) == addr.broadcast()
                    )
            }
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

    /// Returns the order in which Linux announces one address's derived
    /// routes.  `fib_add_ifaddr()` installs the local host route, an explicit
    /// broadcast, the connected prefix, and finally the computed broadcast.
    /// `fib_del_ifaddr()` removes the connected prefix first, then the
    /// broadcasts, and the local host route last.
    pub(crate) fn address_route_transition_rank(
        self,
        entry: Ipv4AddrEntry,
        deleting: bool,
    ) -> Option<u8> {
        let RuleOrigin::Ipv4Address { kind, .. } = self.origin else {
            return None;
        };
        let explicit_broadcast = matches!(
            self.filter,
            IpCidr::Ipv4(broadcast)
                if entry.broadcast == Some(broadcast.address())
        );
        Some(match (deleting, kind) {
            (false, Ipv4AddressRouteKind::Local) => 0,
            (false, Ipv4AddressRouteKind::Broadcast) if explicit_broadcast => 1,
            (false, Ipv4AddressRouteKind::Connected) => 2,
            (false, Ipv4AddressRouteKind::Broadcast) => 3,
            (true, Ipv4AddressRouteKind::Connected) => 0,
            (true, Ipv4AddressRouteKind::Broadcast) if explicit_broadcast => 1,
            (true, Ipv4AddressRouteKind::Broadcast) => 2,
            (true, Ipv4AddressRouteKind::Local) => 3,
        })
    }

    fn address_route_id(self) -> Option<AddressRouteId> {
        match self.origin {
            RuleOrigin::Configured => None,
            RuleOrigin::Ipv4Address {
                owner_dev,
                owner_addr,
                kind,
            } => Some(AddressRouteId {
                owner_dev,
                owner_addr,
                filter: self.filter,
                kind,
            }),
        }
    }

    fn same_route_as(self, other: Self) -> bool {
        self.filter == other.filter
            && self.via == other.via
            && self.dev == other.dev
            && self.table == other.table
            && self.protocol == other.protocol
            && self.scope == other.scope
            && self.route_type == other.route_type
            && self.preferred_source() == other.preferred_source()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteAttrs {
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
}

struct RouteEntry {
    rule: Rule,
    insertion_order: u64,
}

impl RouteEntry {
    // Larger keys win: longest prefix, local host route, then oldest entry.
    fn selection_key(&self) -> (u8, bool, Reverse<u64>) {
        (
            self.rule.filter.prefix_len(),
            matches!(
                self.rule.origin,
                RuleOrigin::Ipv4Address {
                    kind: Ipv4AddressRouteKind::Local,
                    ..
                }
            ),
            Reverse(self.insertion_order),
        )
    }
}

/// Owns route precedence through explicit selection keys.
///
/// Entries are sorted by selection key for lookup; their indices are not identities.
pub(crate) struct RouteTable {
    entries: Vec<RouteEntry>,
    next_insertion_order: u64,
}
impl RouteTable {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_insertion_order: 0,
        }
    }

    /// Adds a route after all existing routes of equal selection priority.
    pub(crate) fn add_rule(&mut self, rule: Rule) {
        self.push_rule(rule);
        self.sort_entries();
    }

    fn push_rule(&mut self, rule: Rule) {
        if self.next_insertion_order == u64::MAX {
            // Compact live sequence numbers before reuse, preserving precedence.
            self.entries
                .sort_unstable_by_key(|entry| entry.insertion_order);
            for (index, entry) in self.entries.iter_mut().enumerate() {
                entry.insertion_order = index as u64;
            }
            self.next_insertion_order = self.entries.len() as u64;
        }
        self.entries.push(RouteEntry {
            rule,
            insertion_order: self.next_insertion_order,
        });
        self.next_insertion_order += 1;
    }

    /// Validates a configured route against the current device and address inventory.
    pub(crate) fn add_configured_rule(
        &mut self,
        rule: Rule,
        devices: &DeviceRegistry,
    ) -> Result<(), LinuxError> {
        Self::validate_configured_route(rule, devices)?;
        self.add_rule(rule);
        Ok(())
    }

    /// Validates the replacement before removing the existing route.
    /// A successful replacement receives a new insertion order, like delete/add.
    pub(crate) fn replace_configured_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
        devices: &DeviceRegistry,
    ) -> Result<(), LinuxError> {
        Self::validate_configured_route(replacement, devices)?;
        self.remove_exact_rule(existing);
        self.add_rule(replacement);
        Ok(())
    }

    fn validate_configured_route(rule: Rule, devices: &DeviceRegistry) -> Result<(), LinuxError> {
        if !rule.is_configured() {
            return Err(LinuxError::EINVAL);
        }
        if rule.table != ROUTE_TABLE_MAIN || rule.route_type != ROUTE_TYPE_UNICAST {
            return Err(LinuxError::EOPNOTSUPP);
        }
        if !matches!(rule.filter, IpCidr::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        let addrs = devices.ipv4(rule.dev).ok_or(LinuxError::ENODEV)?;
        if let Some(prefsrc) = rule.preferred_source()
            && !matches!(prefsrc, IpAddress::Ipv4(address) if devices.iter_ipv4_addrs().any(|(_, entry)| entry.addr.address() == address))
        {
            return Err(LinuxError::EINVAL);
        }
        if let Some(gateway) = rule.via {
            let IpAddress::Ipv4(gateway) = gateway else {
                return Err(LinuxError::EAFNOSUPPORT);
            };
            if !gateway.is_unicast()
                || addrs
                    .as_slice()
                    .iter()
                    .any(|entry| entry.addr.is_directed_broadcast(gateway))
            {
                return Err(LinuxError::EINVAL);
            }
            if !addrs
                .as_slice()
                .iter()
                .any(|entry| entry.addr.contains_addr(&gateway.into()))
            {
                return Err(LinuxError::ENETUNREACH);
            }
        }
        Ok(())
    }

    /// Synchronizes derived routes by identity while retaining their precedence.
    ///
    /// Every address must refer to a registered device. Callers retain the
    /// enclosing Router lock across address mutation and route refresh. The
    /// registry preserves address order within each device to select the first
    /// supporting source for shared routes. New identities receive new orders.
    /// Existing route identities retain their order even if their source changes.
    pub(crate) fn sync_ipv4_address_routes(&mut self, devices: &DeviceRegistry) {
        let loopback_dev = devices
            .iter()
            .position(|device| device.link_kind() == LinkKind::Loopback)
            .map(|slot| devices.id_at(slot));
        let mut desired = Vec::new();
        for (dev, entry) in devices.iter_ipv4_addrs() {
            let addrs = devices
                .ipv4(dev)
                .expect("address device must be registered");
            let primary = addrs.primary_for(entry).unwrap_or(*entry);
            let secondary = addrs.is_secondary(entry.addr);
            if let Some(loopback_dev) = loopback_dev {
                Self::push_address_route(
                    &mut desired,
                    Rule::for_local_ipv4_address(loopback_dev, dev, entry.addr, primary.addr),
                );
            }

            let network = primary.addr.network();
            let device = devices
                .get_by_id(dev)
                .expect("address device must be registered");
            // Linux installs prefix and broadcast routes only while the
            // interface is up; the local host route above survives link-down.
            if !device.is_link_up() {
                continue;
            }
            // `fib_add_ifaddr` derives a primary address's prefix and
            // computed broadcast only for the primary itself.  A secondary
            // contributes a local /32 and an explicitly assigned broadcast,
            // both using the primary as their preferred source.
            let prefix_route_eligible = !secondary
                && !primary.no_prefix_route
                && primary.addr.prefix_len() < 32
                && !network.address().is_unspecified();
            if prefix_route_eligible {
                let scope = if device.link_kind() == LinkKind::Loopback {
                    ROUTE_SCOPE_HOST
                } else {
                    ROUTE_SCOPE_LINK
                };
                Self::push_address_route(
                    &mut desired,
                    Rule::for_connected_ipv4_address(dev, primary.addr, scope),
                );
            }
            // An explicitly assigned broadcast is installed first, while the
            // link is up, unless it is the limited broadcast address.  This
            // route is independent of IFA_F_NOPREFIXROUTE and of a secondary
            // address's prefix-route eligibility.
            if let Some(broadcast) = entry.broadcast
                && broadcast != Ipv4Address::BROADCAST
            {
                Self::push_address_route(
                    &mut desired,
                    Rule::for_broadcast_ipv4_address(dev, entry.addr, primary.addr, broadcast),
                );
            }
            // The computed network broadcast route is independent of
            // IFA_F_NOPREFIXROUTE, exists only for primaries with prefixes
            // shorter than /31, and a zeronet has no broadcast route.  An
            // identical explicit broadcast was already added above and
            // deduplicates by identity.
            if !secondary && primary.addr.prefix_len() < 31 && !network.address().is_zeronet() {
                Self::push_address_route(
                    &mut desired,
                    Rule::for_broadcast_ipv4_address(
                        dev,
                        primary.addr,
                        primary.addr,
                        primary.addr.broadcast().unwrap_or(Ipv4Address::BROADCAST),
                    ),
                );
            }
        }

        self.entries.retain(|entry| {
            entry.rule.is_configured()
                || desired
                    .iter()
                    .any(|rule| rule.address_route_id() == entry.rule.address_route_id())
        });
        for rule in desired {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.rule.address_route_id() == rule.address_route_id())
            {
                entry.rule = rule;
            } else {
                self.push_rule(rule);
            }
        }
        self.sort_entries();
    }

    fn push_address_route(routes: &mut Vec<Rule>, route: Rule) {
        // Two addresses on the same device can derive the same FIB route (for
        // example two local addresses that resolve to one prefix).  The route
        // table has one entry for that destination, while provenance still
        // identifies which address generated it.  Keep one representative and
        // let a later synchronization reattribute it after that address goes
        // away; do not create duplicate reachable routes.
        if !routes.iter().any(|existing| {
            existing.address_route_id() == route.address_route_id() || existing.same_route_as(route)
        }) {
            routes.push(route);
        }
    }

    fn sort_entries(&mut self) {
        self.entries
            .sort_unstable_by_key(|entry| Reverse(entry.selection_key()));
    }

    /// Selects the first matching route in selection-key order.
    pub(crate) fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.entries
            .iter()
            .map(|entry| &entry.rule)
            .find(|rule| rule.filter.contains_addr(dst))
    }

    /// Selects the first matching route in selection-key order, optionally
    /// constrained by the FIB device that an output lookup requested.
    pub(crate) fn lookup_for_output(
        &self,
        dst: &IpAddress,
        oif: Option<NetDeviceId>,
    ) -> Option<&Rule> {
        self.entries.iter().map(|entry| &entry.rule).find(|rule| {
            rule.filter.contains_addr(dst) && oif.is_none_or(|oif| rule.lookup_device() == oif)
        })
    }

    /// Projects a control-plane snapshot in route selection order.
    pub(crate) fn snapshot(&self) -> Vec<Rule> {
        self.entries.iter().map(|entry| entry.rule).collect()
    }

    pub(crate) fn remove_exact_rule(&mut self, route: Rule) {
        let _ = self.take_exact_rule(route);
    }

    /// Removes the first configured route matching `route` and returns the
    /// entry that was actually removed.
    pub(crate) fn take_exact_rule(&mut self, route: Rule) -> Option<Rule> {
        let index = self.entries.iter().position(|entry| {
            let rule = entry.rule;
            rule.is_configured() && rule.same_route_as(route)
        })?;
        Some(self.entries.remove(index).rule)
    }

    pub(crate) fn remove_configured_routes_with_source(&mut self, source: IpAddress) {
        self.entries.retain(|entry| {
            let rule = entry.rule;
            !matches!(rule.origin, RuleOrigin::Configured)
                || rule.preferred_source() != Some(source)
        });
    }

    pub(crate) fn remove_device(&mut self, dev: NetDeviceId) {
        self.entries.retain(|entry| {
            let rule = entry.rule;
            rule.dev != dev
                && !matches!(
                    rule.origin,
                    RuleOrigin::Ipv4Address { owner_dev, .. } if owner_dev == dev
                )
        });
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/network/router/tests.rs");
