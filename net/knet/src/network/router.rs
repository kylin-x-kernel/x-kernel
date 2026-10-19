// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Route rules, configuration validation, address-derived routes, and lookup.

use alloc::vec::Vec;

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
    network::interface_address::Ipv4AddrEntry,
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
        addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
    },
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
            origin: RuleOrigin::Ipv4Address {
                owner_dev,
                addr: owner_addr,
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

// TODO(mivik): optimize
pub(crate) struct RouteTable {
    rules: Vec<Rule>,
}
impl RouteTable {
    pub(crate) fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub(crate) fn add_rule(&mut self, rule: Rule) {
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

    /// Validates a configured route against the current device and address inventory.
    pub(crate) fn add_configured_rule(
        &mut self,
        rule: Rule,
        devices: &DeviceRegistry,
        addrs: &[Ipv4AddrEntry],
    ) -> Result<(), LinuxError> {
        Self::validate_configured_route(rule, devices, addrs)?;
        self.add_rule(rule);
        Ok(())
    }

    /// Validates the replacement before removing the existing route.
    pub(crate) fn replace_configured_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
        devices: &DeviceRegistry,
        addrs: &[Ipv4AddrEntry],
    ) -> Result<(), LinuxError> {
        Self::validate_configured_route(replacement, devices, addrs)?;
        self.remove_exact_rule(existing);
        self.add_rule(replacement);
        Ok(())
    }

    fn validate_configured_route(
        rule: Rule,
        devices: &DeviceRegistry,
        addrs: &[Ipv4AddrEntry],
    ) -> Result<(), LinuxError> {
        if !rule.is_configured() {
            return Err(LinuxError::EINVAL);
        }
        if rule.table != ROUTE_TABLE_MAIN || rule.route_type != ROUTE_TYPE_UNICAST {
            return Err(LinuxError::EOPNOTSUPP);
        }
        if !matches!(rule.filter, IpCidr::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        if devices.slot(rule.dev).is_none() {
            return Err(LinuxError::ENODEV);
        }
        if let Some(prefsrc) = rule.preferred_source()
            && !matches!(prefsrc, IpAddress::Ipv4(address) if addrs.iter().any(|entry| entry.addr.address() == address))
        {
            return Err(LinuxError::EINVAL);
        }
        if let Some(gateway) = rule.via {
            let IpAddress::Ipv4(gateway) = gateway else {
                return Err(LinuxError::EAFNOSUPPORT);
            };
            if !gateway.is_unicast()
                || addrs
                    .iter()
                    .any(|entry| entry.dev == rule.dev && entry.addr.is_directed_broadcast(gateway))
            {
                return Err(LinuxError::EINVAL);
            }
            if !addrs
                .iter()
                .any(|entry| entry.dev == rule.dev && entry.addr.contains_addr(&gateway.into()))
            {
                return Err(LinuxError::ENETUNREACH);
            }
        }
        Ok(())
    }

    /// Refreshes local and connected routes while retaining configured routes.
    ///
    /// Every address must refer to a registered device. Callers retain the
    /// enclosing Router lock across address mutation and route refresh.
    pub(crate) fn rebuild_ipv4_address_routes(
        &mut self,
        devices: &DeviceRegistry,
        addrs: &[Ipv4AddrEntry],
    ) {
        self.remove_ipv4_address_routes();
        let loopback_dev = devices
            .iter()
            .position(|device| device.link_kind() == LinkKind::Loopback)
            .map(|slot| devices.id_at(slot));
        let mut connected = Vec::new();
        for entry in addrs {
            if let Some(loopback_dev) = loopback_dev {
                self.add_rule(Rule::for_local_ipv4_address(
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
            let device = devices
                .get_by_id(entry.dev)
                .expect("address device must be registered");
            let scope = if device.link_kind() == LinkKind::Loopback {
                ROUTE_SCOPE_HOST
            } else {
                ROUTE_SCOPE_LINK
            };
            self.add_rule(Rule::for_connected_ipv4_address(
                entry.dev, entry.addr, scope,
            ));
        }
    }

    pub(crate) fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub(crate) fn remove_exact_rule(&mut self, route: Rule) {
        if let Some(index) = self.rules.iter().position(|rule| {
            matches!(rule.origin, RuleOrigin::Configured) && rule.same_route_as(route)
        }) {
            self.rules.remove(index);
        }
    }

    fn remove_ipv4_address_routes(&mut self) {
        self.rules
            .retain(|rule| matches!(rule.origin, RuleOrigin::Configured));
    }

    pub(crate) fn remove_configured_routes_with_source(&mut self, source: IpAddress) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured)
                || rule.preferred_source() != Some(source)
        });
    }

    pub(crate) fn remove_device(&mut self, dev: NetDeviceId) {
        self.rules.retain(|rule| {
            rule.dev != dev
                && !matches!(
                    rule.origin,
                    RuleOrigin::Ipv4Address { owner_dev, .. } if owner_dev == dev
                )
        });
    }
}
