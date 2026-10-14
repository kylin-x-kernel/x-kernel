// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Route rules, attributes, and table lookup.

use alloc::vec::Vec;

pub(crate) use crate::netlink::{
    RT_SCOPE_HOST as ROUTE_SCOPE_HOST, RT_SCOPE_LINK as ROUTE_SCOPE_LINK,
    RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE, RT_TABLE_MAIN as ROUTE_TABLE_MAIN,
    RTN_UNICAST as ROUTE_TYPE_UNICAST, RTPROT_BOOT as ROUTE_PROTOCOL_BOOT,
    RTPROT_KERNEL as ROUTE_PROTOCOL_KERNEL,
};
use crate::{
    device::NetDeviceId,
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

    pub(crate) fn remove_ipv4_address_routes(&mut self) {
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
