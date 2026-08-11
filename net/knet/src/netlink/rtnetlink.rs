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

impl From<AddrRequest> for AddrState {
    fn from(value: AddrRequest) -> Self {
        Self {
            index: value.index,
            family: value.family,
            prefix_len: value.prefix_len,
            scope: value.scope,
            address: value.address,
        }
    }
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
    route_state()
        .links
        .into_iter()
        .map(|link| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_link_message(request, &link),
        })
        .collect()
}

fn dump_addrs(request: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(wire_route::FAMILY_IPV4);
    let RtnetlinkState { links, addrs, .. } = route_state();
    addrs
        .into_iter()
        .filter(|addr| addr.family == family)
        .map(|addr| {
            let label = links
                .iter()
                .find(|link| link.index as u32 == addr.index)
                .map(|link| link.name.as_str())
                .unwrap_or("");
            NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_addr_message(request, addr, label),
            }
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
    mutate_route_state(|state| {
        if let Some(name) = request.name.as_ref()
            && state
                .links
                .iter()
                .any(|other| other.index != request.index && other.name == *name)
        {
            return Err(LinuxError::EEXIST);
        }
        let link = state
            .links
            .iter_mut()
            .find(|link| link.index == request.index)
            .ok_or(LinuxError::ENODEV)?;

        if let Some(name) = request.name {
            link.name = name;
        }
        if let Some(mtu) = request.mtu {
            link.mtu = mtu;
        }
        if request.change & IFF_UP != 0 {
            if request.flags & IFF_UP != 0 {
                link.flags |= IFF_UP | IFF_RUNNING | IFF_LOWER_UP;
                if link.link_type == ARPHRD_ETHER {
                    link.operstate = 6;
                }
            } else {
                link.flags &= !(IFF_UP | IFF_RUNNING | IFF_LOWER_UP);
                link.operstate = 2;
            }
        }
        Ok(())
    })
}

pub(super) fn add_addr_state(request: AddrRequest, flags: u16) -> Result<(), LinuxError> {
    mutate_route_state(|state| {
        ensure_link_exists(state, request.index)?;
        let existing = state
            .addrs
            .iter()
            .position(|addr| same_addr(addr, &request));
        if let Some(pos) = existing {
            if flags & NLM_F_REPLACE != 0 {
                state.addrs[pos] = request.into();
                return Ok(());
            }
            return Err(LinuxError::EEXIST);
        }
        if flags & NLM_F_CREATE == 0 && flags & NLM_F_EXCL != 0 {
            return Err(LinuxError::ENOENT);
        }
        state.addrs.push(request.into());
        Ok(())
    })
}

pub(super) fn del_addr_state(request: AddrRequest) -> Result<(), LinuxError> {
    mutate_route_state(|state| {
        let before = state.addrs.len();
        state.addrs.retain(|addr| !same_addr(addr, &request));
        if state.addrs.len() == before {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        Ok(())
    })
}

pub(super) fn add_route_state(request: RouteRequest, flags: u16) -> Result<(), LinuxError> {
    mutate_route_state(|state| {
        if request.oif != 0 {
            ensure_link_exists(state, request.oif)?;
        }
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
    mutate_route_state(|state| {
        ensure_link_exists(state, request.ifindex)?;
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

fn ensure_link_exists(state: &RtnetlinkState, ifindex: u32) -> Result<(), LinuxError> {
    state
        .links
        .iter()
        .any(|link| link.index as u32 == ifindex)
        .then_some(())
        .ok_or(LinuxError::ENODEV)
}

fn same_addr(addr: &AddrState, request: &AddrRequest) -> bool {
    addr.index == request.index
        && addr.family == request.family
        && addr.prefix_len == request.prefix_len
        && addr.address == request.address
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
    let attrs = parse_addr_attrs(info.family, parse_attrs(&payload[IfAddrMsg::SIZE..])?)?;
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
            | wire_link::attr::OPERSTATE => {}
            _ => {}
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

fn build_link_message(request: &NlMsgHeader, link: &LinkState) -> Vec<u8> {
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 17,
        pad: 0,
        link_type: link.link_type,
        index: link.index,
        flags: link.flags,
        change: u32::MAX,
    }
    .write(&mut payload);

    push_attr(&mut payload, wire_link::attr::ADDRESS, &link.mac);
    push_attr(&mut payload, wire_link::attr::BROADCAST, &link.broadcast);
    push_attr_str(&mut payload, wire_link::attr::IFNAME, &link.name);
    push_attr(&mut payload, wire_link::attr::MTU, &link.mtu.to_ne_bytes());
    if link.index > 1 {
        push_attr(
            &mut payload,
            wire_link::attr::LINK,
            &(link.index as u32 - 1).to_ne_bytes(),
        );
    }
    push_attr(&mut payload, wire_link::attr::OPERSTATE, &[link.operstate]);

    build_nlmsg(RTM_NEWLINK, request.seq, NLM_F_MULTI, payload)
}

fn build_addr_message(request: &NlMsgHeader, addr: AddrState, label: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    IfAddrMsg {
        family: addr.family,
        prefix_len: addr.prefix_len,
        flags: 0,
        scope: addr.scope,
        index: addr.index,
    }
    .write(&mut payload);

    let ip_bytes = ip_addr_bytes(addr.address);
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

pub(crate) fn link_state_for_ifindex(ifindex: i32) -> Option<LinkState> {
    if ROUTE_STATE.is_inited() {
        ROUTE_STATE
            .read()
            .links
            .iter()
            .find(|link| link.index == ifindex)
            .cloned()
    } else {
        None
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
        SERVICE.lock().sync_netlink(&new_state);
    }
    Ok(())
}

pub(crate) fn build_initial_state(
    lo_ip: IpCidr,
    eth0_ip: Option<IpCidr>,
    eth0_mac: Option<[u8; 6]>,
    gateway: Option<IpAddress>,
    standard_mtu: u32,
) -> RtnetlinkState {
    let mut state = RtnetlinkState::default();
    state.links.push(LinkState {
        index: 1,
        name: String::from("lo"),
        flags: IFF_UP | IFF_RUNNING | IFF_LOOPBACK | IFF_LOWER_UP,
        mtu: 65_536,
        operstate: 0,
        link_type: ARPHRD_LOOPBACK,
        mac: [0; 6],
        broadcast: [0; 6],
    });
    state.addrs.push(AddrState {
        index: 1,
        family: match lo_ip.address() {
            IpAddress::Ipv4(_) => wire_route::FAMILY_IPV4,
            IpAddress::Ipv6(_) => wire_route::FAMILY_IPV6,
        },
        prefix_len: lo_ip.prefix_len(),
        scope: wire_route::SCOPE_HOST,
        address: lo_ip.address(),
    });
    state.routes.push(RouteState {
        family: wire_route::FAMILY_IPV4,
        dst_len: lo_ip.prefix_len(),
        table: wire_route::TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_HOST,
        route_type: wire_route::TYPE_UNICAST,
        oif: 1,
        dst: Some(lo_ip.address()),
        gateway: None,
        prefsrc: Some(lo_ip.address()),
    });

    if let Some(eth0_ip) = eth0_ip {
        state.links.push(LinkState {
            index: 2,
            name: String::from("eth0"),
            flags: IFF_UP | IFF_RUNNING | IFF_BROADCAST | IFF_MULTICAST | IFF_LOWER_UP,
            mtu: standard_mtu,
            operstate: 6,
            link_type: ARPHRD_ETHER,
            mac: eth0_mac.unwrap_or([0; 6]),
            broadcast: [0xff; 6],
        });
        state.addrs.push(AddrState {
            index: 2,
            family: match eth0_ip.address() {
                IpAddress::Ipv4(_) => wire_route::FAMILY_IPV4,
                IpAddress::Ipv6(_) => wire_route::FAMILY_IPV6,
            },
            prefix_len: eth0_ip.prefix_len(),
            scope: wire_route::SCOPE_UNIVERSE,
            address: eth0_ip.address(),
        });
        state.routes.push(RouteState {
            family: wire_route::FAMILY_IPV4,
            dst_len: 32,
            table: wire_route::TABLE_MAIN,
            protocol: wire_route::PROTOCOL_BOOT,
            scope: wire_route::SCOPE_HOST,
            route_type: wire_route::TYPE_UNICAST,
            oif: 1,
            dst: Some(eth0_ip.address()),
            gateway: None,
            prefsrc: Some(eth0_ip.address()),
        });
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
