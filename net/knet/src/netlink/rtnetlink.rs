// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! rtnetlink request parsing and control-plane mutations.

use alloc::{string::String, vec, vec::Vec};

use kerrno::LinuxError;
use smoltcp::wire::IpAddress;

use super::{
    wire::{
        IfAddrMsg, IfInfoMsg, NdMsg, NlAttr, NlMsgHeader, RtMsg, addr as wire_addr, build_nlmsg,
        build_nlmsg_done_response, build_nlmsg_error_response, ip_addr_bytes, link as wire_link,
        neigh as wire_neigh, normalize_neigh_state, normalize_route_protocol,
        normalize_route_table, normalize_route_type, parse_attrs, parse_ip_by_family, parse_mac,
        parse_string, push_attr, push_attr_str, read_u32_payload, route as wire_route,
    },
    *,
};
use crate::{
    SERVICE,
    device::{
        LINK_FLAG_UP, LINK_FLAG_VOLATILE, LinkConfigUpdate, LinkKind, LinkSnapshot, NeighborState,
        NeighborUpdate,
    },
    ip::{IpAddress as KernelIpAddress, IpCidr, Ipv4Cidr},
    router::{Ipv4AddrEntry, NeighborUpdatePolicy, RouteAttrs as RouterRouteAttrs, Rule},
    service::ExistingIpv4AddrAction,
};

#[derive(Clone, Debug)]
pub(super) struct LinkUpdateRequest {
    pub(super) index: i32,
    pub(super) flags: u32,
    pub(super) change: u32,
    pub(super) name: Option<String>,
    pub(super) mtu: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct AddrRequest {
    pub(super) index: u32,
    pub(super) family: u8,
    pub(super) prefix_len: u8,
    pub(super) scope: u8,
    pub(super) address: IpAddress,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RouteRequest {
    pub(super) family: u8,
    pub(super) dst_len: u8,
    pub(super) src_len: u8,
    pub(super) table: u8,
    pub(super) protocol: u8,
    pub(super) scope: u8,
    pub(super) route_type: u8,
    pub(super) oif: u32,
    pub(super) dst: Option<IpAddress>,
    pub(super) src: Option<IpAddress>,
    pub(super) gateway: Option<IpAddress>,
    pub(super) prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NeighRequest {
    pub(super) family: u8,
    pub(super) ifindex: u32,
    pub(super) state: u16,
    pub(super) flags: u8,
    pub(super) dst: IpAddress,
    pub(super) lladdr: Option<[u8; 6]>,
}

#[derive(Clone, Debug, Default)]
struct LinkAttrs {
    name: Option<String>,
    mtu: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct AddrAttrs {
    address: Option<IpAddress>,
    local: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RouteAttrs {
    oif: u32,
    dst: Option<IpAddress>,
    src: Option<IpAddress>,
    gateway: Option<IpAddress>,
    prefsrc: Option<IpAddress>,
    table: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct NeighAttrs {
    dst: Option<IpAddress>,
    lladdr: Option<[u8; 6]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RtnetlinkOperation {
    GetLink,
    GetAddr,
    GetRoute,
    NewLink,
    NewAddr,
    DelAddr,
    NewRoute,
    DelRoute,
    NewNeigh,
    Unsupported,
}

impl RtnetlinkOperation {
    fn from_msg_type(msg_type: u16) -> Self {
        match msg_type {
            RTM_GETLINK => Self::GetLink,
            RTM_GETADDR => Self::GetAddr,
            RTM_GETROUTE => Self::GetRoute,
            RTM_NEWLINK => Self::NewLink,
            RTM_NEWADDR => Self::NewAddr,
            RTM_DELADDR => Self::DelAddr,
            RTM_NEWROUTE => Self::NewRoute,
            RTM_DELROUTE => Self::DelRoute,
            RTM_NEWNEIGH => Self::NewNeigh,
            _ => Self::Unsupported,
        }
    }

    fn is_dump(self) -> bool {
        match self {
            Self::GetLink | Self::GetAddr | Self::GetRoute => true,
            Self::NewLink
            | Self::NewAddr
            | Self::DelAddr
            | Self::NewRoute
            | Self::DelRoute
            | Self::NewNeigh
            | Self::Unsupported => false,
        }
    }

    fn requires_privilege(self) -> bool {
        match self {
            Self::NewLink
            | Self::NewAddr
            | Self::DelAddr
            | Self::NewRoute
            | Self::DelRoute
            | Self::NewNeigh => true,
            Self::GetLink | Self::GetAddr | Self::GetRoute | Self::Unsupported => false,
        }
    }
}

pub(super) fn rtnetlink_request_requires_privilege(msg_type: u16) -> bool {
    RtnetlinkOperation::from_msg_type(msg_type).requires_privilege()
}

pub(super) fn build_error_response(request: &[u8], errno: LinuxError) -> Vec<u8> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    build_nlmsg_error_response(&header, -errno.into_raw(), request)
}

pub(super) fn build_ack_response(request: &[u8]) -> Vec<u8> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    build_nlmsg_error_response(&header, 0, request)
}

pub(super) fn handle_rtnetlink_request(request: &[u8]) -> Vec<NetlinkPacket> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    if header.flags & NLM_F_REQUEST == 0 {
        return vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, LinuxError::EINVAL),
        }];
    }

    let payload = request
        .get(NLMSG_HDR_LEN..(header.len as usize).min(request.len()))
        .unwrap_or(&[]);
    let operation = RtnetlinkOperation::from_msg_type(header.msg_type);
    let mut packets = match operation {
        RtnetlinkOperation::GetLink => dump_links(&header, payload),
        RtnetlinkOperation::GetAddr => dump_addrs(&header, payload),
        RtnetlinkOperation::GetRoute => dump_routes(&header, payload),
        RtnetlinkOperation::NewLink => apply_newlink(request, &header, payload),
        RtnetlinkOperation::NewAddr => apply_newaddr(request, &header, payload),
        RtnetlinkOperation::DelAddr => apply_deladdr(request, &header, payload),
        RtnetlinkOperation::NewRoute => apply_newroute(request, &header, payload),
        RtnetlinkOperation::DelRoute => apply_delroute(request, &header, payload),
        RtnetlinkOperation::NewNeigh => apply_newneigh(request, &header, payload),
        RtnetlinkOperation::Unsupported => vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, LinuxError::EOPNOTSUPP),
        }],
    };
    if operation.is_dump() {
        packets.push(NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_done_response(&header),
        });
    }
    packets
}

fn dump_links(request: &NlMsgHeader, _payload: &[u8]) -> Vec<NetlinkPacket> {
    if !SERVICE.is_inited() {
        return Vec::new();
    }
    SERVICE
        .link_snapshots()
        .into_iter()
        .map(|link| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_link_message(request, link),
        })
        .collect()
}

fn dump_addrs(request: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(wire_route::FAMILY_IPV4);
    if family != wire_route::FAMILY_IPV4 || !SERVICE.is_inited() {
        return Vec::new();
    }
    SERVICE
        .ipv4_addr_snapshots()
        .into_iter()
        .map(|snapshot| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_addr_message(request, snapshot.entry, &snapshot.label),
        })
        .collect()
}

fn dump_routes(request: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(wire_route::FAMILY_IPV4);
    if !SERVICE.is_inited() {
        return Vec::new();
    }
    SERVICE
        .route_snapshot()
        .into_iter()
        .filter(|route| route_family(*route) == family)
        .map(|route| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_route_message(request, route),
        })
        .collect()
}

fn apply_newlink(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_link_update(payload).and_then(apply_link_update) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newaddr(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_addr_request(payload).and_then(|req| add_addr(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_deladdr(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_addr_request(payload).and_then(del_addr) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newroute(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_route_request(payload).and_then(|req| add_route(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_delroute(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_route_request(payload).and_then(del_route) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newneigh(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_neigh_request(payload).and_then(|req| add_neigh(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

pub(super) fn apply_link_update(request: LinkUpdateRequest) -> Result<(), LinuxError> {
    if request.index <= 0 {
        return Err(LinuxError::ENODEV);
    }
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _rtnl = rtnl_lock();

    let current_flags = SERVICE
        .link_snapshot_for_ifindex(request.index)
        .ok_or(LinuxError::ENODEV)?
        .flags;
    // Linux treats a zero change mask with nonzero flags as an all-bits mask
    // for backward compatibility. Device-owned volatile flags are preserved.
    let change = if request.change == 0 && request.flags != 0 {
        u32::MAX
    } else {
        request.change
    };
    let changed_flags = change & (request.flags ^ current_flags);
    if changed_flags & !(LINK_FLAG_UP | LINK_FLAG_VOLATILE) != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    SERVICE.update_device_link(
        request.index,
        LinkConfigUpdate {
            name: request.name,
            mtu: request.mtu.map(|mtu| mtu as usize),
            is_up: (change & LINK_FLAG_UP != 0).then_some(request.flags & LINK_FLAG_UP != 0),
        },
    )
}

pub(super) fn add_addr(request: AddrRequest, flags: u16) -> Result<(), LinuxError> {
    let entry = ipv4_addr_entry_from_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _rtnl = rtnl_lock();
    let existing_action = if flags & NLM_F_REPLACE != 0 && flags & NLM_F_EXCL == 0 {
        ExistingIpv4AddrAction::Keep
    } else {
        ExistingIpv4AddrAction::Reject
    };
    SERVICE.add_ipv4_addr(entry, existing_action)
}

pub(super) fn del_addr(request: AddrRequest) -> Result<(), LinuxError> {
    let entry = ipv4_addr_entry_from_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _rtnl = rtnl_lock();
    SERVICE.remove_ipv4_addr(entry)?;
    Ok(())
}

pub(super) fn add_route(request: RouteRequest, flags: u16) -> Result<(), LinuxError> {
    let _rtnl = rtnl_lock();
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let rule = route_rule_from_request(request)?;
    let existing = SERVICE
        .route_snapshot()
        .into_iter()
        .find(|route| same_route_key(*route, &request));
    if let Some(existing) = existing {
        if flags & NLM_F_EXCL != 0 || flags & NLM_F_REPLACE == 0 || !existing.is_configured() {
            return Err(LinuxError::EEXIST);
        }
        SERVICE.replace_route_rule(existing, rule)?;
    } else if flags & NLM_F_CREATE == 0 {
        return Err(LinuxError::ENOENT);
    } else {
        SERVICE.add_route_rule(rule)?;
    }
    Ok(())
}

pub(super) fn del_route(request: RouteRequest) -> Result<(), LinuxError> {
    let _rtnl = rtnl_lock();
    validate_route_request(request)?;
    if normalize_route_table(request.table) != RT_TABLE_MAIN
        || normalize_route_type(request.route_type) != RTN_UNICAST
    {
        return Err(LinuxError::EOPNOTSUPP);
    }
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    if request.oif != 0 {
        ensure_link_exists(request.oif)?;
    }
    let route = SERVICE
        .route_snapshot()
        .into_iter()
        .find(|route| route.is_configured() && route_delete_matches(*route, &request))
        .ok_or(LinuxError::ESRCH)?;
    SERVICE.remove_route_rule(route);
    Ok(())
}

pub(super) fn add_neigh(request: NeighRequest, flags: u16) -> Result<(), LinuxError> {
    let _rtnl = rtnl_lock();
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    ensure_link_exists(request.ifindex)?;
    let update = neighbor_update_from_request(request)?;
    let policy = NeighborUpdatePolicy {
        can_create: flags & NLM_F_CREATE != 0,
        can_replace: flags & NLM_F_REPLACE != 0 && flags & NLM_F_EXCL == 0,
    };
    SERVICE.apply_neighbor_update(update, policy)
}

fn ensure_link_exists(ifindex: u32) -> Result<(), LinuxError> {
    let ifindex = i32::try_from(ifindex).map_err(|_| LinuxError::ENODEV)?;
    let exists = if SERVICE.is_inited() {
        SERVICE.link_snapshot_for_ifindex(ifindex).is_some()
    } else {
        false
    };
    exists.then_some(()).ok_or(LinuxError::ENODEV)
}

fn neighbor_update_from_request(request: NeighRequest) -> Result<NeighborUpdate, LinuxError> {
    if request.flags != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let state = match normalize_neigh_state(request.state) {
        wire_neigh::STATE_INCOMPLETE => NeighborState::Incomplete,
        wire_neigh::STATE_PERMANENT => NeighborState::Permanent {
            hardware_addr: request.lladdr.ok_or(LinuxError::EINVAL)?,
        },
        _ => return Err(LinuxError::EOPNOTSUPP),
    };
    let dev = request.ifindex.checked_sub(1).ok_or(LinuxError::ENODEV)? as usize;
    Ok(NeighborUpdate {
        dev,
        dst: to_kernel_ip(request.dst)?,
        state,
    })
}

fn ipv4_addr_entry_from_request(request: AddrRequest) -> Result<Ipv4AddrEntry, LinuxError> {
    let IpAddress::Ipv4(address) = request.address else {
        return Err(LinuxError::EAFNOSUPPORT);
    };
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    if request.prefix_len > 32 {
        return Err(LinuxError::EINVAL);
    }
    let dev = request.index.checked_sub(1).ok_or(LinuxError::ENODEV)? as usize;
    Ok(Ipv4AddrEntry {
        dev,
        addr: crate::ip::Ipv4Cidr::new(address.into(), request.prefix_len),
        scope: request.scope,
    })
}

fn same_route_key(route: Rule, request: &RouteRequest) -> bool {
    same_route_destination(route, request)
        && route.route_type == normalize_route_type(request.route_type)
}

fn same_route_destination(route: Rule, request: &RouteRequest) -> bool {
    route_family(route) == request.family
        && route.filter.prefix_len() == request.dst_len
        && u32::from(route.table) == u32::from(normalize_route_table(request.table))
        && route_dst(route) == normalized_route_request_dst(request)
}

fn route_delete_matches(route: Rule, request: &RouteRequest) -> bool {
    same_route_destination(route, request)
        && (request.route_type == 0 || route.route_type == request.route_type)
        && (request.protocol == 0 || route.protocol == request.protocol)
        && (request.scope == u8::MAX || route.scope == request.scope)
        && (request.oif == 0 || route.dev as u32 + 1 == request.oif)
        && request.gateway.is_none_or(|gateway| {
            to_kernel_ip(gateway).is_ok_and(|gateway| route.via == Some(gateway))
        })
        && request.prefsrc.is_none_or(|prefsrc| {
            to_kernel_ip(prefsrc).is_ok_and(|prefsrc| route.preferred_source() == Some(prefsrc))
        })
}

fn normalized_route_request_dst(request: &RouteRequest) -> Option<IpAddress> {
    let dst = request
        .dst
        .unwrap_or(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED));
    let dst = match dst {
        IpAddress::Ipv4(addr) => IpAddress::Ipv4(
            smoltcp::wire::Ipv4Cidr::new(addr, request.dst_len)
                .network()
                .address(),
        ),
        IpAddress::Ipv6(addr) => IpAddress::Ipv6(addr),
    };
    (!dst.is_unspecified() || request.dst_len != 0).then_some(dst)
}

fn route_rule_from_request(request: RouteRequest) -> Result<Rule, LinuxError> {
    validate_route_request(request)?;
    let dst = request
        .dst
        .unwrap_or(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED));
    let IpAddress::Ipv4(dst) = dst else {
        return Err(LinuxError::EAFNOSUPPORT);
    };
    let filter = IpCidr::Ipv4(Ipv4Cidr::new(dst.into(), request.dst_len).network());
    let dev = request.oif.checked_sub(1).ok_or(LinuxError::EOPNOTSUPP)? as usize;
    let gateway = request.gateway.map(to_kernel_ip).transpose()?;
    let prefsrc = request.prefsrc.map(to_kernel_ip).transpose()?;
    let source = prefsrc.unwrap_or(filter.address());
    Ok(Rule::with_route_attrs(
        filter,
        gateway,
        dev,
        source,
        RouterRouteAttrs {
            table: normalize_route_table(request.table),
            protocol: normalize_route_protocol(request.protocol),
            scope: request.scope,
            route_type: normalize_route_type(request.route_type),
            prefsrc,
        },
    ))
}

fn validate_route_request(request: RouteRequest) -> Result<(), LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    if request.dst_len > 32 || request.src_len > 32 {
        return Err(LinuxError::EINVAL);
    }
    match (request.src_len, request.src) {
        (0, None) => Ok(()),
        (0, Some(_)) => Err(LinuxError::EINVAL),
        _ => Err(LinuxError::EOPNOTSUPP),
    }
}

fn route_family(route: Rule) -> u8 {
    match route.filter.address() {
        KernelIpAddress::Ipv4(_) => wire_route::FAMILY_IPV4,
        KernelIpAddress::Ipv6(_) => wire_route::FAMILY_IPV6,
    }
}

fn route_dst(route: Rule) -> Option<IpAddress> {
    if route.filter.prefix_len() == 0 && route.filter.address().is_unspecified() {
        None
    } else {
        Some(to_smoltcp_ip(route.filter.address()))
    }
}

fn to_kernel_ip(address: IpAddress) -> Result<KernelIpAddress, LinuxError> {
    Ok(match address {
        IpAddress::Ipv4(address) => KernelIpAddress::Ipv4(address.into()),
        IpAddress::Ipv6(_) => return Err(LinuxError::EAFNOSUPPORT),
    })
}

fn to_smoltcp_ip(address: KernelIpAddress) -> IpAddress {
    match address {
        KernelIpAddress::Ipv4(address) => IpAddress::Ipv4(address.into()),
        KernelIpAddress::Ipv6(address) => IpAddress::Ipv6(address.into()),
    }
}

pub(super) fn parse_link_update(payload: &[u8]) -> Result<LinkUpdateRequest, LinuxError> {
    let info = IfInfoMsg::read(payload)?;
    let attrs = parse_link_attrs(parse_attrs(&payload[IfInfoMsg::SIZE..])?)?;
    Ok(LinkUpdateRequest {
        index: info.index,
        flags: info.flags,
        change: info.change,
        name: attrs.name,
        mtu: attrs.mtu,
    })
}

fn parse_addr_request(payload: &[u8]) -> Result<AddrRequest, LinuxError> {
    let info = IfAddrMsg::read(payload)?;
    // Linux derives the legacy IPv4 `ifa_flags` bits from address ordering
    // and lifetimes, so this permanent-address subset accepts the field
    // without treating it as independently mutable state.
    let attrs = parse_addr_attrs(info.family, parse_attrs(&payload[IfAddrMsg::SIZE..])?)?;
    if let (Some(address), Some(local)) = (attrs.address, attrs.local)
        && address != local
    {
        return Err(LinuxError::EOPNOTSUPP);
    }
    Ok(AddrRequest {
        index: info.index,
        family: info.family,
        prefix_len: info.prefix_len,
        scope: info.scope,
        address: attrs.local.or(attrs.address).ok_or(LinuxError::EINVAL)?,
    })
}

fn parse_route_request(payload: &[u8]) -> Result<RouteRequest, LinuxError> {
    let info = RtMsg::read(payload)?;
    let attrs = parse_route_attrs(info.family, parse_attrs(&payload[RtMsg::SIZE..])?)?;
    Ok(RouteRequest {
        family: info.family,
        dst_len: info.dst_len,
        src_len: info.src_len,
        table: attrs.table.unwrap_or(info.table),
        protocol: info.protocol,
        scope: info.scope,
        route_type: info.route_type,
        oif: attrs.oif,
        dst: attrs.dst,
        src: attrs.src,
        gateway: attrs.gateway,
        prefsrc: attrs.prefsrc,
    })
}

fn parse_neigh_request(payload: &[u8]) -> Result<NeighRequest, LinuxError> {
    let info = NdMsg::read(payload)?;
    let attrs = parse_neigh_attrs(info.family, parse_attrs(&payload[NdMsg::SIZE..])?)?;
    Ok(NeighRequest {
        family: info.family,
        ifindex: info.ifindex as u32,
        state: info.state,
        flags: info.flags,
        dst: attrs.dst.ok_or(LinuxError::EINVAL)?,
        lladdr: attrs.lladdr,
    })
}

fn parse_link_attrs(attrs: Vec<NlAttr<'_>>) -> Result<LinkAttrs, LinuxError> {
    let mut parsed = LinkAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_link::attr::IFNAME => parsed.name = Some(parse_string(attr.payload)?),
            wire_link::attr::MTU => parsed.mtu = Some(read_u32_payload(attr.payload)?),
            wire_link::attr::EXT_MASK
            | wire_link::attr::ADDRESS
            | wire_link::attr::BROADCAST
            | wire_link::attr::LINK
            | wire_link::attr::OPERSTATE => return Err(LinuxError::EOPNOTSUPP),
            _ => return Err(LinuxError::EOPNOTSUPP),
        }
    }
    Ok(parsed)
}

fn parse_addr_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<AddrAttrs, LinuxError> {
    let mut parsed = AddrAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_addr::attr::LOCAL => {
                parsed.local = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_addr::attr::ADDRESS => {
                parsed.address = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_addr::attr::LABEL => {
                let _ = parse_string(attr.payload)?;
            }
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_route_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<RouteAttrs, LinuxError> {
    let mut parsed = RouteAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_route::attr::DST => parsed.dst = Some(parse_ip_by_family(family, attr.payload)?),
            wire_route::attr::OIF => parsed.oif = read_u32_payload(attr.payload)?,
            wire_route::attr::SRC => parsed.src = Some(parse_ip_by_family(family, attr.payload)?),
            wire_route::attr::GATEWAY => {
                parsed.gateway = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::PREFSRC => {
                parsed.prefsrc = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::TABLE => {
                let table = read_u32_payload(attr.payload)?;
                parsed.table = Some(u8::try_from(table).map_err(|_| LinuxError::EOPNOTSUPP)?);
            }
            // Linux `rtm_to_fib_config` skips unmatched RTA types after nla
            // validation so newer iproute2 attributes still work on older kernels.
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_neigh_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<NeighAttrs, LinuxError> {
    let mut parsed = NeighAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_neigh::attr::DST => parsed.dst = Some(parse_ip_by_family(family, attr.payload)?),
            wire_neigh::attr::LLADDR => parsed.lladdr = Some(parse_mac(attr.payload)?),
            _ => {}
        }
    }
    Ok(parsed)
}

fn build_link_message(request: &NlMsgHeader, link: LinkSnapshot) -> Vec<u8> {
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: match link.kind {
            LinkKind::Loopback => ARPHRD_LOOPBACK,
            LinkKind::Ethernet => ARPHRD_ETHER,
        },
        index: link.ifindex,
        flags: link.flags,
        change: 0,
    }
    .write(&mut payload);

    push_attr(&mut payload, wire_link::attr::ADDRESS, &link.hardware_addr);
    push_attr(
        &mut payload,
        wire_link::attr::BROADCAST,
        &link.broadcast_addr,
    );
    push_attr_str(&mut payload, wire_link::attr::IFNAME, &link.name);
    push_attr(
        &mut payload,
        wire_link::attr::MTU,
        &(link.mtu as u32).to_ne_bytes(),
    );
    push_attr(&mut payload, wire_link::attr::OPERSTATE, &[link.operstate]);

    build_nlmsg(RTM_NEWLINK, request.seq, NLM_F_MULTI, payload)
}

fn build_addr_message(request: &NlMsgHeader, addr: Ipv4AddrEntry, label: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    IfAddrMsg {
        family: wire_route::FAMILY_IPV4,
        prefix_len: addr.addr.prefix_len(),
        flags: 0,
        scope: addr.scope,
        index: addr.dev as u32 + 1,
    }
    .write(&mut payload);

    let ip_bytes = ip_addr_bytes(IpAddress::Ipv4(addr.addr.address().into()));
    push_attr(&mut payload, wire_addr::attr::ADDRESS, &ip_bytes);
    push_attr(&mut payload, wire_addr::attr::LOCAL, &ip_bytes);
    push_attr_str(&mut payload, wire_addr::attr::LABEL, label);

    build_nlmsg(RTM_NEWADDR, request.seq, NLM_F_MULTI, payload)
}

fn build_route_message(request: &NlMsgHeader, route: Rule) -> Vec<u8> {
    let mut payload = Vec::new();
    let family = route_family(route);
    let dst = route_dst(route);
    RtMsg {
        family,
        dst_len: route.filter.prefix_len(),
        src_len: 0,
        tos: 0,
        table: route.table,
        protocol: route.protocol,
        scope: route.scope,
        route_type: route.route_type,
        flags: 0,
    }
    .write(&mut payload);

    if let Some(dst) = dst {
        let dst = ip_addr_bytes(dst);
        push_attr(&mut payload, wire_route::attr::DST, &dst);
    }
    push_attr(
        &mut payload,
        wire_route::attr::OIF,
        &(route.dev as u32 + 1).to_ne_bytes(),
    );
    if let Some(gateway) = route.via.map(to_smoltcp_ip) {
        let gateway = ip_addr_bytes(gateway);
        push_attr(&mut payload, wire_route::attr::GATEWAY, &gateway);
    }
    if let Some(prefsrc) = route.preferred_source().map(to_smoltcp_ip) {
        let prefsrc = ip_addr_bytes(prefsrc);
        push_attr(&mut payload, wire_route::attr::PREFSRC, &prefsrc);
    }

    build_nlmsg(RTM_NEWROUTE, request.seq, NLM_F_MULTI, payload)
}

fn build_done_response(request: &NlMsgHeader) -> Vec<u8> {
    build_nlmsg_done_response(request)
}

fn ack_packets(request: &[u8], header: &NlMsgHeader) -> Vec<NetlinkPacket> {
    if header.flags & NLM_F_ACK == 0 {
        Vec::new()
    } else {
        vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_ack_response(request),
        }]
    }
}

fn error_packets(request: &[u8], errno: LinuxError) -> Vec<NetlinkPacket> {
    vec![NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: build_error_response(request, errno),
    }]
}
