// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network control-plane coordination and legacy socket ioctl helpers.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use kerrno::LinuxError;
use ksync::{Mutex, MutexGuard, static_lock};

use crate::{
    SERVICE,
    device::{
        ARPHRD_ETHER, ARPHRD_LOOPBACK, LINK_FLAG_UP, LinkConfigUpdate, LinkKind, NetDeviceId,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    network::{
        event::NetworkEvent,
        interface_address::Ipv4AddrEntry,
        router::{
            ROUTE_PROTOCOL_BOOT, ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE, ROUTE_TABLE_MAIN,
            ROUTE_TYPE_UNICAST, RouteAttrs, Rule,
        },
    },
};

static_lock! {
    static NETWORK_CONFIG_MUTATION_LOCK: Mutex<()> = Mutex::new(());
}

/// Serializes network configuration mutations across control-plane front ends
/// and device teardown.
///
/// This lock may sleep and must only be acquired from task context. Callers
/// hold it while resolving mutation targets and updating their owning network
/// objects so legacy socket ioctls, rtnetlink requests, and device removal do
/// not interleave.
pub(crate) fn network_config_lock() -> MutexGuard<'static, ()> {
    NETWORK_CONFIG_MUTATION_LOCK.lock()
}

/// Nonblocking variant used by the data-plane poller.
///
/// Neighbor events are consumed from [`crate::network::router::Router`] and
/// published to netlink.  Taking this lock first serializes that pair with
/// control-plane mutations, which hold it across their own Router update and
/// notification.  A caller that cannot take it must leave the events pending
/// so they cannot be republished after a later `NeighborRemoved`.
pub(crate) fn try_network_config_lock() -> Option<MutexGuard<'static, ()>> {
    NETWORK_CONFIG_MUTATION_LOCK.try_lock()
}

/// Publishes neutral network changes to netlink.
///
/// Callers invoke this while still holding [`network_config_lock`], so the
/// notification order is the same as the order in which mutations commit.
pub(crate) fn publish_events(events: Vec<NetworkEvent>) {
    if !events.is_empty() {
        crate::netlink::publish_network_events(events);
    }
}

/// Snapshot of interface state required by network socket ioctls.
#[derive(Clone, Debug)]
pub struct NetInterfaceInfo {
    /// 1-based interface index.
    pub ifindex: i32,
    /// Flags bitmask (`IFF_UP`, `IFF_BROADCAST`, ...).
    pub flags: u32,
    /// Hardware address (MAC for Ethernet).
    pub hardware_addr: [u8; 6],
    /// Device type (`ARPHRD_ETHER`, `ARPHRD_LOOPBACK`, ...).
    pub arphrd: u16,
}

/// Finds a network interface by name.
pub fn find_interface(name: &str) -> Option<NetInterfaceInfo> {
    if !SERVICE.is_inited() {
        return None;
    }
    let link = SERVICE
        .link_snapshots()
        .into_iter()
        .find(|link| link.name == name)?;
    let arphrd = match link.kind {
        LinkKind::Loopback => ARPHRD_LOOPBACK,
        LinkKind::Ethernet => ARPHRD_ETHER,
    };
    Some(NetInterfaceInfo {
        ifindex: link.ifindex,
        flags: link.flags,
        hardware_addr: link.hardware_addr,
        arphrd,
    })
}

/// Returns the primary IPv4 address of the specified interface.
///
/// Device resolution and address lookup run under a single Router lock.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown and `EADDRNOTAVAIL` when the
/// interface has no IPv4 address.
pub fn interface_ipv4_addr(name: &str) -> Result<Ipv4Addr, LinuxError> {
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    SERVICE
        .interface_ipv4_addr(name)
        .map(|cidr| Ipv4Addr::from(cidr.address().octets()))
}

/// Updates the primary IPv4 address on `name`.
///
/// Other IPv4 addresses assigned to the interface remain unchanged.
pub fn set_interface_ipv4_addr(
    name: &str,
    addr: Ipv4Addr,
    prefix_len: u8,
) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let dev = device_id_by_name(name)?;
    let events = if addr.is_unspecified() {
        SERVICE.remove_primary_ipv4_addr(dev)?
    } else {
        let cidr = Ipv4Cidr::new(Ipv4Address::from_octets(addr.octets()), prefix_len)
            .ok_or(LinuxError::EINVAL)?;
        SERVICE.set_primary_ipv4_addr(dev, cidr)?
    };
    publish_events(events);
    Ok(())
}

/// Updates the prefix length of the first IPv4 address assigned to `name`.
///
/// Preserves other IPv4 addresses. If the resulting CIDR already exists on the
/// interface, removes the updated entry and preserves the existing target's
/// attributes, returning success. Otherwise, retains the updated entry's scope
/// and custom broadcast address.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown, `EADDRNOTAVAIL` when the
/// interface has no IPv4 address, or `EINVAL` when `prefix_len` is invalid.
pub fn set_interface_ipv4_netmask(name: &str, prefix_len: u8) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let dev = device_id_by_name(name)?;
    let events = SERVICE.set_ipv4_netmask(dev, prefix_len)?;
    publish_events(events);
    Ok(())
}

/// Sets the IPv4 broadcast address stored on the first address of `name`.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown, or `EADDRNOTAVAIL` when the
/// interface has no IPv4 address.
pub fn set_interface_ipv4_broadcast(name: &str, broadcast: Ipv4Addr) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let dev = device_id_by_name(name)?;
    let events = SERVICE.set_ipv4_broadcast(dev, Ipv4Address::from_octets(broadcast.octets()))?;
    publish_events(events);
    Ok(())
}

/// Applies the `IFF_UP` state requested through `SIOCSIFFLAGS`.
///
/// # Errors
///
/// Returns `ENODEV` when `name` is unknown.
pub fn set_interface_flags(name: &str, flags: u16) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let ifindex = device_id_by_name(name)?.ifindex();
    let events = SERVICE.update_device_link(
        ifindex,
        LinkConfigUpdate {
            name: None,
            mtu: None,
            is_up: Some(flags as u32 & LINK_FLAG_UP != 0),
        },
    )?;
    publish_events(events);
    Ok(())
}

/// Adds an IPv4 route owned by `Router`.
///
/// Linux `SIOCADDRT` sets `NLM_F_CREATE` without `NLM_F_EXCL`, so a second
/// route to the same destination is rejected only when the nexthop also
/// matches (`oif` and gateway). A different gateway is a distinct route.
///
/// # Errors
///
/// Returns `ENODEV` when `oif_name` is unknown, `EINVAL` when the prefix or
/// gateway is illegal, `ENETUNREACH` when no output interface is supplied or
/// the gateway is outside its subnet, or `EEXIST` for a duplicate route.
pub fn add_ipv4_route(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let rule = ipv4_configured_route_rule(dst, dst_len, gateway, oif_name)?;
    if SERVICE.route_snapshot().iter().any(|existing| {
        existing.is_configured()
            && existing.filter == rule.filter
            && existing.via == rule.via
            && existing.dev == rule.dev
    }) {
        return Err(LinuxError::EEXIST);
    }
    let events = SERVICE.add_route_rule(rule)?;
    publish_events(events);
    Ok(())
}

/// Deletes an IPv4 route owned by `Router`. Missing entries return `ESRCH`.
///
/// A missing output interface and gateway are wildcards, matching Linux
/// `SIOCDELRT` / `fib_nh_match` when `rt_dev` or `RTF_GATEWAY` is omitted.
/// `route del default` therefore removes the first matching default route.
///
/// # Errors
///
/// Returns `ENODEV` when `oif_name` is unknown, `EINVAL` when the prefix is
/// illegal, or `ESRCH` when no matching configured route exists.
pub fn del_ipv4_route(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<(), LinuxError> {
    let _network_config_guard = network_config_lock();
    let oif = match oif_name {
        Some(name) => Some(device_id_by_name(name)?),
        None => None,
    };
    let filter = ipv4_route_filter(dst, dst_len)?;
    let via = gateway.map(|addr| IpAddress::Ipv4(Ipv4Address::from_octets(addr.octets())));
    let existing = SERVICE
        .route_snapshot()
        .into_iter()
        .find(|existing| {
            existing.is_configured()
                && existing.filter == filter
                && oif.is_none_or(|oif| existing.dev == oif)
                && (via.is_none() || existing.via == via)
        })
        .ok_or(LinuxError::ESRCH)?;
    let events = SERVICE.remove_route_rule(existing);
    if events.is_empty() {
        return Err(LinuxError::ESRCH);
    }
    publish_events(events);
    Ok(())
}

fn ipv4_route_filter(dst: Option<Ipv4Addr>, dst_len: u8) -> Result<IpCidr, LinuxError> {
    let dst_addr = dst
        .map(|addr| Ipv4Address::from_octets(addr.octets()))
        .unwrap_or(Ipv4Address::UNSPECIFIED);
    Ok(Ipv4Cidr::new(dst_addr, dst_len)
        .ok_or(LinuxError::EINVAL)?
        .network()
        .into())
}

fn ipv4_configured_route_rule(
    dst: Option<Ipv4Addr>,
    dst_len: u8,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<&str>,
) -> Result<Rule, LinuxError> {
    let dev = device_id_by_name(oif_name.ok_or(LinuxError::ENETUNREACH)?)?;
    let filter = ipv4_route_filter(dst, dst_len)?;
    let via = gateway.map(|addr| IpAddress::Ipv4(Ipv4Address::from_octets(addr.octets())));
    let source = ipv4_entries_for_device(dev)
        .first()
        .map(|entry| IpAddress::Ipv4(entry.addr.address()))
        .unwrap_or(filter.address());
    Ok(Rule::with_route_attrs(
        filter,
        via,
        dev,
        source,
        RouteAttrs {
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_BOOT,
            scope: if via.is_some() {
                ROUTE_SCOPE_UNIVERSE
            } else {
                ROUTE_SCOPE_LINK
            },
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: None,
        },
    ))
}

fn device_id_by_name(name: &str) -> Result<NetDeviceId, LinuxError> {
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    SERVICE.device_id_by_name(name).ok_or(LinuxError::ENODEV)
}

fn ipv4_entries_for_device(dev: NetDeviceId) -> Vec<Ipv4AddrEntry> {
    SERVICE
        .ipv4_addr_snapshots()
        .into_iter()
        .filter(|snapshot| snapshot.dev == dev)
        .map(|snapshot| snapshot.entry)
        .collect()
}
