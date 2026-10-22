// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Route rules, configuration validation, address-derived routes, and lookup.

use alloc::vec::Vec;
use core::cmp::Reverse;

use kerrno::LinuxError;

pub(crate) use crate::netlink::{
    RT_SCOPE_HOST as ROUTE_SCOPE_HOST, RT_SCOPE_LINK as ROUTE_SCOPE_LINK,
    RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE, RT_TABLE_MAIN as ROUTE_TABLE_MAIN,
    RTN_UNICAST as ROUTE_TYPE_UNICAST, RTPROT_BOOT as ROUTE_PROTOCOL_BOOT,
    RTPROT_KERNEL as ROUTE_PROTOCOL_KERNEL,
};
use crate::{
    device::{DeviceRegistry, LinkKind, NetDeviceId},
    ip::{IpAddress, IpCidr, Ipv4Cidr},
};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleOrigin {
    Configured,
    Ipv4Address {
        owner_dev: NetDeviceId,
        kind: Ipv4AddressRouteKind,
    },
}

/// Matches an address-derived route by origin and destination prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AddressRouteId {
    owner_dev: NetDeviceId,
    filter: IpCidr,
    kind: Ipv4AddressRouteKind,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: NetDeviceId,
    pub src: IpAddress,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: Retain the route preferred-source attribute \
                  for source-address selection"
    )]
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
        owner_addr: Ipv4Cidr,
    ) -> Self {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: 32 is a valid IPv4 prefix length for \
                      the local host route"
        )]
        let filter = Ipv4Cidr::new(owner_addr.address(), 32).unwrap();
        Self::for_ipv4_address(
            filter,
            loopback_dev,
            owner_dev,
            owner_addr,
            Ipv4AddressRouteKind::Local,
            ROUTE_SCOPE_HOST,
        )
    }

    pub(crate) fn for_connected_ipv4_address(
        dev: NetDeviceId,
        owner_addr: Ipv4Cidr,
        scope: u8,
    ) -> Self {
        Self::for_ipv4_address(
            owner_addr.network(),
            dev,
            dev,
            owner_addr,
            Ipv4AddressRouteKind::Connected,
            scope,
        )
    }

    fn for_ipv4_address(
        filter: Ipv4Cidr,
        dev: NetDeviceId,
        owner_dev: NetDeviceId,
        owner_addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
        scope: u8,
    ) -> Self {
        let source = IpAddress::Ipv4(owner_addr.address());
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
            origin: RuleOrigin::Ipv4Address { owner_dev, kind },
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

    fn address_route_id(self) -> Option<AddressRouteId> {
        match self.origin {
            RuleOrigin::Configured => None,
            RuleOrigin::Ipv4Address { owner_dev, kind } => Some(AddressRouteId {
                owner_dev,
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
            if let Some(loopback_dev) = loopback_dev {
                Self::push_address_route(
                    &mut desired,
                    Rule::for_local_ipv4_address(loopback_dev, dev, entry.addr),
                );
            }

            let network = entry.addr.network();
            if entry.addr.prefix_len() == 32 || network.address().is_unspecified() {
                continue;
            }
            let device = devices
                .get_by_id(dev)
                .expect("address device must be registered");
            let scope = if device.link_kind() == LinkKind::Loopback {
                ROUTE_SCOPE_HOST
            } else {
                ROUTE_SCOPE_LINK
            };
            Self::push_address_route(
                &mut desired,
                Rule::for_connected_ipv4_address(dev, entry.addr, scope),
            );
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
        if !routes
            .iter()
            .any(|existing| existing.address_route_id() == route.address_route_id())
        {
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

    /// Projects a control-plane snapshot in route selection order.
    pub(crate) fn snapshot(&self) -> Vec<Rule> {
        self.entries.iter().map(|entry| entry.rule).collect()
    }

    pub(crate) fn remove_exact_rule(&mut self, route: Rule) {
        if let Some(index) = self.entries.iter().position(|entry| {
            let rule = entry.rule;
            rule.is_configured() && rule.same_route_as(route)
        }) {
            self.entries.remove(index);
        }
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
