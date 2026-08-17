// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! rtnetlink request parsing and control-plane state updates.

use alloc::{string::String, vec, vec::Vec};

use kerrno::LinuxError;
use smoltcp::wire::{IpAddress, IpCidr};

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
    device::{LINK_FLAG_UP, LINK_FLAG_VOLATILE, LinkConfigUpdate, LinkKind, LinkSnapshot},
    router::Ipv4AddrEntry,
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
    pub(super) table: u8,
    pub(super) protocol: u8,
    pub(super) scope: u8,
    pub(super) route_type: u8,
    pub(super) oif: u32,
    pub(super) dst: Option<IpAddress>,
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
    gateway: Option<IpAddress>,
    prefsrc: Option<IpAddress>,
    table: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct NeighAttrs {
    dst: Option<IpAddress>,
    lladdr: Option<[u8; 6]>,
}

impl From<RouteRequest> for RouteState {
    fn from(value: RouteRequest) -> Self {
        Self {
            family: value.family,
            dst_len: value.dst_len,
            table: normalize_route_table(value.table),
            protocol: normalize_route_protocol(value.protocol),
            scope: value.scope,
            route_type: normalize_route_type(value.route_type),
            oif: value.oif,
            dst: value.dst,
            gateway: value.gateway,
            prefsrc: value.prefsrc,
        }
    }
}

impl From<NeighRequest> for NeighState {
    fn from(value: NeighRequest) -> Self {
        Self {
            family: value.family,
            ifindex: value.ifindex,
            state: normalize_neigh_state(value.state),
            flags: value.flags,
            dst: value.dst,
            lladdr: value.lladdr,
        }
    }
}

pub(crate) fn init_route_state(state: RtnetlinkState) {
    ROUTE_STATE.init_once(RwLock::new(state));
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
    route_state()
        .routes
        .into_iter()
        .filter(|route| route.family == family)
        .map(|route| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_route_message(request, route),
        })
        .collect()
}

fn apply_newlink(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_link_update(payload).and_then(update_link_state) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newaddr(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_addr_request(payload).and_then(|req| add_addr_state(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_deladdr(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_addr_request(payload).and_then(del_addr_state) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newroute(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_route_request(payload).and_then(|req| add_route_state(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_delroute(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_route_request(payload).and_then(del_route_state) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newneigh(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_neigh_request(payload).and_then(|req| add_neigh_state(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

pub(super) fn update_link_state(request: LinkUpdateRequest) -> Result<(), LinuxError> {
    if request.index <= 0 {
        return Err(LinuxError::ENODEV);
    }
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();

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

pub(super) fn add_addr_state(request: AddrRequest, flags: u16) -> Result<(), LinuxError> {
    let entry = ipv4_addr_entry_from_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();
    let existing_action = if flags & NLM_F_REPLACE != 0 && flags & NLM_F_EXCL == 0 {
        ExistingIpv4AddrAction::Keep
    } else {
        ExistingIpv4AddrAction::Reject
    };
    SERVICE.add_ipv4_addr(entry, existing_action)
}

pub(super) fn del_addr_state(request: AddrRequest) -> Result<(), LinuxError> {
    let entry = ipv4_addr_entry_from_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();
    let new_state = {
        let mut state = ROUTE_STATE.write();
        if !SERVICE.remove_ipv4_addr(entry)? {
            return Ok(());
        }
        state
            .routes
            .retain(|route| route.prefsrc != Some(request.address));
        state.clone()
    };
    SERVICE.sync_netlink(&new_state);
    Ok(())
}

pub(crate) fn remove_device_state(id: kdevice::DeviceId) -> bool {
    if !SERVICE.is_inited() {
        return false;
    }
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();
    if !ROUTE_STATE.is_inited() {
        return SERVICE.remove_device_by_model_id(id).is_some();
    }

    let new_state = {
        let mut state = ROUTE_STATE.write();
        let Some(removal) = SERVICE.remove_device_by_model_id(id) else {
            return false;
        };
        state.routes.retain(|route| {
            route.oif != removal.ifindex
                && !removal
                    .orphaned_ipv4_sources
                    .iter()
                    .any(|source| route.prefsrc == Some(IpAddress::Ipv4((*source).into())))
        });
        for route in &mut state.routes {
            if route.oif > removal.ifindex {
                route.oif -= 1;
            }
        }
        state
            .neighs
            .retain(|neigh| neigh.ifindex != removal.ifindex);
        for neigh in &mut state.neighs {
            if neigh.ifindex > removal.ifindex {
                neigh.ifindex -= 1;
            }
        }
        state.clone()
    };
    SERVICE.sync_netlink(&new_state);
    true
}

pub(super) fn add_route_state(request: RouteRequest, flags: u16) -> Result<(), LinuxError> {
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();
    if request.oif != 0 {
        ensure_link_exists(request.oif)?;
    }
    mutate_route_state(|state| {
        let existing = state
            .routes
            .iter()
            .position(|route| same_route(route, &request));
        if let Some(pos) = existing {
            if flags & NLM_F_REPLACE != 0 {
                state.routes[pos] = request.into();
                return Ok(());
            }
            return Err(LinuxError::EEXIST);
        }
        state.routes.push(request.into());
        Ok(())
    })
}

pub(super) fn del_route_state(request: RouteRequest) -> Result<(), LinuxError> {
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();
    mutate_route_state(|state| {
        let before = state.routes.len();
        state.routes.retain(|route| !same_route(route, &request));
        if state.routes.len() == before {
            return Err(LinuxError::ESRCH);
        }
        Ok(())
    })
}

pub(super) fn add_neigh_state(request: NeighRequest, flags: u16) -> Result<(), LinuxError> {
    let _mutation_guard = RTNETLINK_MUTATION_LOCK.lock();
    ensure_link_exists(request.ifindex)?;
    mutate_route_state(|state| {
        let existing = state
            .neighs
            .iter()
            .position(|neigh| neigh.ifindex == request.ifindex && neigh.dst == request.dst);
        if let Some(pos) = existing {
            if flags & NLM_F_REPLACE != 0 {
                state.neighs[pos] = request.into();
                return Ok(());
            }
            return Err(LinuxError::EEXIST);
        }
        state.neighs.push(request.into());
        Ok(())
    })
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

fn same_route(route: &RouteState, request: &RouteRequest) -> bool {
    route.family == request.family
        && route.dst_len == request.dst_len
        && route.table == normalize_route_table(request.table)
        && route.protocol == normalize_route_protocol(request.protocol)
        && route.scope == request.scope
        && route.route_type == normalize_route_type(request.route_type)
        && route.oif == request.oif
        && route.dst == request.dst
        && route.gateway == request.gateway
        && route.prefsrc == request.prefsrc
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
        table: attrs.table.unwrap_or(info.table),
        protocol: info.protocol,
        scope: info.scope,
        route_type: info.route_type,
        oif: attrs.oif,
        dst: attrs.dst,
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
            wire_route::attr::GATEWAY => {
                parsed.gateway = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::PREFSRC | wire_route::attr::SRC => {
                parsed.prefsrc = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::TABLE => parsed.table = Some(read_u32_payload(attr.payload)? as u8),
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

fn build_route_message(request: &NlMsgHeader, route: RouteState) -> Vec<u8> {
    let mut payload = Vec::new();
    RtMsg {
        family: route.family,
        dst_len: route.dst_len,
        src_len: 0,
        tos: 0,
        table: route.table,
        protocol: route.protocol,
        scope: route.scope,
        route_type: route.route_type,
        flags: 0,
    }
    .write(&mut payload);

    if let Some(dst) = route.dst {
        let dst = ip_addr_bytes(dst);
        push_attr(&mut payload, wire_route::attr::DST, &dst);
    }
    push_attr(
        &mut payload,
        wire_route::attr::OIF,
        &route.oif.to_ne_bytes(),
    );
    if let Some(gateway) = route.gateway {
        let gateway = ip_addr_bytes(gateway);
        push_attr(&mut payload, wire_route::attr::GATEWAY, &gateway);
    }
    if let Some(prefsrc) = route.prefsrc {
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

pub(crate) fn route_state() -> RtnetlinkState {
    if ROUTE_STATE.is_inited() {
        (*ROUTE_STATE.read()).clone()
    } else {
        Default::default()
    }
}

fn mutate_route_state<F>(f: F) -> Result<(), LinuxError>
where
    F: FnOnce(&mut RtnetlinkState) -> Result<(), LinuxError>,
{
    let new_state = {
        let mut state = ROUTE_STATE.write();
        f(&mut state)?;
        state.clone()
    };
    if SERVICE.is_inited() {
        SERVICE.sync_netlink(&new_state);
    }
    Ok(())
}

pub(crate) fn build_initial_state(
    eth0_ip: Option<IpCidr>,
    gateway: Option<IpAddress>,
) -> RtnetlinkState {
    let mut state = RtnetlinkState::default();
    if let Some(eth0_ip) = eth0_ip {
        state.routes.push(RouteState {
            family: wire_route::FAMILY_IPV4,
            dst_len: 0,
            table: wire_route::TABLE_MAIN,
            protocol: wire_route::PROTOCOL_BOOT,
            scope: wire_route::SCOPE_UNIVERSE,
            route_type: wire_route::TYPE_UNICAST,
            oif: 2,
            dst: None,
            gateway,
            prefsrc: Some(eth0_ip.address()),
        });
    }

    state
}
