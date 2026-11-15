// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! rtnetlink request parsing and control-plane mutations.

use alloc::{string::String, vec, vec::Vec};

use kerrno::LinuxError;
use smoltcp::wire::IpAddress;

use super::{
    wire::{
        IfAddrMsg, IfInfoMsg, NdMsg, NlAttr, NlMsgHeader, RtMsg, addr as wire_addr, align,
        build_nlmsg, build_nlmsg_done_response, build_nlmsg_error_response,
        build_nlmsg_error_response_with_cap, build_nlmsg_for_port, ip_addr_bytes,
        link as wire_link, neigh as wire_neigh, normalize_route_table, normalize_route_type,
        parse_attrs, parse_attrs_lossy, parse_ip_prefix_by_family, parse_mac_prefix, parse_string,
        push_attr, push_attr_str, read_i32_ne, read_u16_ne, read_u32_ne, read_u32_payload,
        route as wire_route,
    },
    *,
};
use crate::{
    SERVICE, control,
    device::{
        LINK_FLAG_UP, LINK_FLAG_VOLATILE, LinkConfigUpdate, LinkKind, LinkSnapshot,
        NeighborSnapshot, NeighborState, NeighborUpdate, NetDeviceId,
    },
    ip::{IpAddress as KernelIpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    network::{
        event::{NetworkEvent, RouteProvenance},
        interface_address::Ipv4AddrEntry,
        router::{RouteAttrs as RouterRouteAttrs, Rule},
    },
    router::NeighborUpdatePolicy,
    service::ExistingIpv4AddrAction,
    stack::router::Ipv4OutputRoute,
};

#[derive(Clone, Debug)]
pub(super) struct LinkUpdateRequest {
    pub(super) index: i32,
    pub(super) flags: u32,
    pub(super) change: u32,
    pub(super) name: Option<String>,
    pub(super) alt_name: Option<String>,
    pub(super) mtu: Option<u32>,
}

impl LinkUpdateRequest {
    /// Resolves the device selector with Linux `__rtnl_newlink` semantics.
    ///
    /// A positive index is authoritative. Index zero falls back to
    /// `IFLA_IFNAME` and then `IFLA_ALT_IFNAME`; a negative index is invalid.
    fn resolve_index(&self) -> Result<i32, LinuxError> {
        if self.index > 0 {
            return Ok(self.index);
        }
        if self.index < 0 {
            return Err(LinuxError::EINVAL);
        }
        let name = self.name.as_deref().or(self.alt_name.as_deref());
        let name = name.ok_or(LinuxError::ENODEV)?;
        SERVICE
            .link_snapshots()
            .into_iter()
            .find(|link| link.name == name)
            .map(|link| link.ifindex)
            .ok_or(LinuxError::ENODEV)
    }
}

/// One kernel-originated notification produced by a successful mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RtnetlinkNotification {
    pub(super) group: u32,
    pub(super) data: Vec<u8>,
    /// Whether `nlmsg_notify` would unicast an echo and exclude the requester
    /// from multicast for this notification.
    pub(super) echo: bool,
}

/// Header fields that a management notification inherits from its request.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MutationOrigin {
    pub(super) portid: u32,
    pub(super) seq: u32,
    pub(super) echo: bool,
}

impl MutationOrigin {
    fn from_header(header: &NlMsgHeader, requesting_port: u32) -> Self {
        Self {
            portid: requesting_port,
            seq: header.seq,
            echo: header.flags & NLM_F_ECHO != 0,
        }
    }
}

#[derive(Clone, Debug)]
struct LinkQuery {
    index: i32,
    name: Option<String>,
    alt_name: Option<String>,
}

impl LinkQuery {
    /// Linux prefers `IFLA_IFNAME` over `IFLA_ALT_IFNAME` when both are
    /// present; alternate names are only a fallback lookup key.
    fn lookup_name(&self) -> Option<&str> {
        self.name.as_deref().or(self.alt_name.as_deref())
    }
}

#[derive(Clone, Debug)]
pub(super) struct AddrRequest {
    pub(super) index: u32,
    pub(super) family: u8,
    pub(super) prefix_len: u8,
    pub(super) scope: u8,
    pub(super) address: IpAddress,
    pub(super) broadcast: Option<IpAddress>,
    pub(super) flags: u32,
    pub(super) label: Option<[u8; 16]>,
}

/// Selectors accepted by Linux `inet_rtm_deladdr`.
///
/// Deletion is not the inverse of creation: every selector is optional, an
/// unmatched selector is not an error, and the first matching address on the
/// device is removed. `prefix_len` is used only when `address` is present.
#[derive(Clone, Debug, Default)]
pub(super) struct DelAddrRequest {
    pub(super) index: u32,
    pub(super) family: u8,
    pub(super) prefix_len: u8,
    pub(super) local: Option<IpAddress>,
    pub(super) address: Option<IpAddress>,
    pub(super) label: Option<[u8; 16]>,
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
    pub(super) tos: u8,
    pub(super) flags: u32,
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

/// A parsed `RTM_NEWNEIGH` whose family-specific attributes have not been
/// decoded yet.
///
/// Linux resolves a nonzero interface before it interprets `NDA_LLADDR`,
/// `ndm_family`, and `NDA_DST`; keep the raw payloads so error precedence is
/// observable.
#[derive(Clone, Copy, Debug)]
struct NeighWireAddRequest<'a> {
    family: u8,
    ifindex: u32,
    state: u16,
    flags: u8,
    dst: &'a [u8],
    lladdr: Option<&'a [u8]>,
}

/// A parsed `RTM_DELNEIGH` whose destination is still family-encoded.
#[derive(Clone, Copy, Debug)]
struct NeighWireDeleteRequest<'a> {
    family: u8,
    ifindex: u32,
    state: u16,
    flags: u8,
    dst: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
struct NeighGetRequest<'a> {
    family: u8,
    ifindex: i32,
    flags: u8,
    dst: &'a [u8],
}

#[derive(Clone, Copy, Debug, Default)]
struct NeighDumpFilter {
    family: u8,
    ifindex: i32,
    master: i32,
    proxy: bool,
}

#[derive(Clone, Debug, Default)]
struct LinkAttrs {
    name: Option<String>,
    alt_name: Option<String>,
    mtu: Option<u32>,
}

#[derive(Clone, Debug, Default)]
struct LinkQueryAttrs {
    name: Option<String>,
    alt_name: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct AddrAttrs {
    address: Option<IpAddress>,
    local: Option<IpAddress>,
    broadcast: Option<IpAddress>,
    flags: Option<u32>,
    label: Option<[u8; 16]>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RtnetlinkOperation {
    GetLink,
    GetAddr,
    GetRoute,
    GetNeigh,
    NewLink,
    NewAddr,
    DelAddr,
    NewRoute,
    DelRoute,
    NewNeigh,
    DelNeigh,
    Unsupported,
}

impl RtnetlinkOperation {
    fn from_msg_type(msg_type: u16) -> Self {
        match msg_type {
            RTM_GETLINK => Self::GetLink,
            RTM_GETADDR => Self::GetAddr,
            RTM_GETROUTE => Self::GetRoute,
            RTM_GETNEIGH => Self::GetNeigh,
            RTM_NEWLINK => Self::NewLink,
            RTM_NEWADDR => Self::NewAddr,
            RTM_DELADDR => Self::DelAddr,
            RTM_NEWROUTE => Self::NewRoute,
            RTM_DELROUTE => Self::DelRoute,
            RTM_NEWNEIGH => Self::NewNeigh,
            RTM_DELNEIGH => Self::DelNeigh,
            _ => Self::Unsupported,
        }
    }

    fn is_dump_request(self, flags: u16) -> bool {
        match self {
            Self::GetLink | Self::GetAddr | Self::GetRoute | Self::GetNeigh => {
                // Linux tests the combined mask for truthiness, so either
                // `NLM_F_ROOT` or `NLM_F_MATCH` selects the dump operation
                // even without the other bit.
                flags & NLM_F_DUMP != 0
            }
            Self::NewLink
            | Self::NewAddr
            | Self::DelAddr
            | Self::NewRoute
            | Self::DelRoute
            | Self::NewNeigh
            | Self::DelNeigh
            | Self::Unsupported => false,
        }
    }
}

/// Whether `rtnetlink_rcv_msg()` checks `CAP_NET_ADMIN` for this message.
///
/// Linux performs the check for every in-range message whose kind is not
/// `RTNL_KIND_GET`, even when no handler is registered.  Consequently
/// `RTM_SETLINK` is `EPERM` for an unprivileged caller while an out-of-range
/// type is `EOPNOTSUPP` regardless of credentials.
pub(super) fn rtnetlink_request_requires_privilege(msg_type: u16) -> bool {
    if !(RTM_BASE..=RTM_MAX).contains(&msg_type) {
        return false;
    }
    (msg_type - RTM_BASE) & RTNL_KIND_MASK != RTNL_KIND_GET
}

/// Byte offset of `nlmsghdr::nlmsg_pid`.
const NETLINK_HEADER_PID_OFFSET: usize = 12;

pub(super) fn build_error_response(request: &[u8], errno: LinuxError) -> Vec<u8> {
    build_error_response_with_cap(request, errno, false)
}

pub(super) fn build_error_response_with_cap(
    request: &[u8],
    errno: LinuxError,
    cap_ack: bool,
) -> Vec<u8> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    build_nlmsg_error_response_with_cap(&header, -errno.into_raw(), request, cap_ack)
}

pub(super) fn build_ack_response(request: &[u8]) -> Vec<u8> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    build_nlmsg_error_response(&header, 0, request)
}

pub(super) fn handle_rtnetlink_request(request: &[u8], requesting_port: u32) -> Vec<NetlinkPacket> {
    handle_rtnetlink_request_with_cred(request, requesting_port, true, 0, false)
}

/// Dispatches one rtnetlink message with the caller's privilege level.
///
/// `netlink_rcv_skb` filters control messages before a protocol callback is
/// reached: a message without `NLM_F_REQUEST`, or with a type below
/// `NLMSG_MIN_TYPE`, is not dispatched and only produces an acknowledgement
/// when `NLM_F_ACK` is set.  The protocol callback itself checks
/// `CAP_NET_ADMIN` per request, so one batched request cannot turn an
/// unprivileged query into a failure or let a privileged query authorize a
/// following mutation.
pub(super) fn handle_rtnetlink_request_with_cred(
    request: &[u8],
    requesting_port: u32,
    is_privileged: bool,
    requesting_uid: u32,
    cap_ack: bool,
) -> Vec<NetlinkPacket> {
    let packets =
        handle_rtnetlink_request_inner(request, requesting_port, is_privileged, requesting_uid);
    if !cap_ack {
        return packets;
    }
    // `NETLINK_CAP_ACK` only changes error replies.  Success acknowledgements
    // are capped unconditionally by the wire builder.
    normalize_reply_ports(cap_error_replies(packets, request), requesting_port)
}

fn cap_error_replies(mut packets: Vec<NetlinkPacket>, request: &[u8]) -> Vec<NetlinkPacket> {
    let Some(header) = NlMsgHeader::read(request) else {
        return packets;
    };
    for packet in &mut packets {
        if read_u16_ne(&packet.data, 4) != Some(NLMSG_ERROR) {
            continue;
        }
        let Some(errno) = read_i32_ne(&packet.data, NLMSG_HDR_LEN) else {
            continue;
        };
        if errno != 0 {
            packet.data = build_nlmsg_error_response_with_cap(&header, errno, request, true);
        }
    }
    packets
}

fn handle_rtnetlink_request_inner(
    request: &[u8],
    requesting_port: u32,
    is_privileged: bool,
    requesting_uid: u32,
) -> Vec<NetlinkPacket> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    if header.flags & NLM_F_REQUEST == 0 || header.msg_type < NLMSG_MIN_TYPE {
        return if header.flags & NLM_F_ACK != 0 {
            normalize_reply_ports(
                vec![NetlinkPacket {
                    from: NetlinkAddr::default(),
                    data: build_ack_response(request),
                    credentials: None,
                }],
                requesting_port,
            )
        } else {
            Vec::new()
        };
    }

    // `rtnetlink_rcv_msg` rejects an out-of-range type before looking at the
    // payload or credentials.
    if header.msg_type > RTM_MAX {
        return normalize_reply_ports(
            vec![NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_error_response(request, LinuxError::EOPNOTSUPP),
                credentials: None,
            }],
            requesting_port,
        );
    }

    // It then ignores every request shorter than `rtgenmsg` before checking
    // the operation or its privilege.  `struct rtgenmsg` is a single family
    // byte, so a request carrying only the netlink header is short while one
    // with a one-byte payload is long enough to dispatch.
    if (header.len as usize) < NLMSG_HDR_LEN + 1 {
        return if header.flags & NLM_F_ACK != 0 {
            normalize_reply_ports(
                vec![NetlinkPacket {
                    from: NetlinkAddr::default(),
                    data: build_ack_response(request),
                    credentials: None,
                }],
                requesting_port,
            )
        } else {
            Vec::new()
        };
    }

    let payload = request
        .get(NLMSG_HDR_LEN..(header.len as usize).min(request.len()))
        .unwrap_or(&[]);
    let operation = RtnetlinkOperation::from_msg_type(header.msg_type);
    if rtnetlink_request_requires_privilege(header.msg_type) && !is_privileged {
        return normalize_reply_ports(
            vec![NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_error_response(request, LinuxError::EPERM),
                credentials: None,
            }],
            requesting_port,
        );
    }

    let is_dump = operation.is_dump_request(header.flags);
    let mut packets = match operation {
        RtnetlinkOperation::GetLink if is_dump => dump_links(&header, payload),
        RtnetlinkOperation::GetLink => get_link(request, &header, payload),
        RtnetlinkOperation::GetAddr if is_dump => dump_addrs(&header, payload),
        RtnetlinkOperation::GetRoute if is_dump => dump_routes(&header, payload),
        // IPv4 registers `inet_rtm_getroute` as a `doit` callback, so a
        // non-dump route lookup is a real query.  `RTM_GETADDR` remains
        // dump-only, and the route dispatcher selects `doit` from the first
        // payload byte (`rtgen_family`), not from the embedded `rtmsg`.
        RtnetlinkOperation::GetRoute => {
            get_route(request, &header, payload, requesting_port, requesting_uid)
        }
        RtnetlinkOperation::GetAddr => vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, LinuxError::EOPNOTSUPP),
            credentials: None,
        }],
        RtnetlinkOperation::GetNeigh if is_dump => dump_neigh(&header, payload, requesting_port),
        RtnetlinkOperation::GetNeigh => get_neigh(request, &header, payload, requesting_port),
        RtnetlinkOperation::NewLink => apply_newlink(request, &header, payload, requesting_port),
        RtnetlinkOperation::NewAddr => apply_newaddr(request, &header, payload, requesting_port),
        RtnetlinkOperation::DelAddr => apply_deladdr(request, &header, payload, requesting_port),
        RtnetlinkOperation::NewRoute => apply_newroute(request, &header, payload, requesting_port),
        RtnetlinkOperation::DelRoute => apply_delroute(request, &header, payload, requesting_port),
        RtnetlinkOperation::NewNeigh => apply_newneigh(request, &header, payload, requesting_port),
        RtnetlinkOperation::DelNeigh => apply_delneigh(request, &header, payload, requesting_port),
        RtnetlinkOperation::Unsupported => vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, LinuxError::EOPNOTSUPP),
            credentials: None,
        }],
    };
    if is_dump {
        packets.push(NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_done_response(&header),
            credentials: None,
        });
        // A dump that starts successfully is reported to `netlink_rcv_skb` as
        // `-EINTR`, which suppresses the success acknowledgement even when the
        // request set `NLM_F_ACK`.  The dump data and its `NLMSG_DONE` trailer
        // are the complete reply.
    }

    // Linux fills `nlmsg_pid` with `NETLINK_CB(skb).portid`, which is the
    // requesting socket's bound port, for every rtnetlink reply: query
    // results, dump trailers, acknowledgements, and errors.  The individual
    // builders only know the request header, whose `nlmsg_pid` names the
    // destination (normally 0), so normalize the reply headers here.
    normalize_reply_ports(packets, requesting_port)
}

pub(super) fn normalize_reply_ports(
    mut packets: Vec<NetlinkPacket>,
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    for packet in &mut packets {
        if packet.data.len() >= NLMSG_HDR_LEN {
            packet.data[NETLINK_HEADER_PID_OFFSET..NETLINK_HEADER_PID_OFFSET + 4]
                .copy_from_slice(&requesting_port.to_ne_bytes());
        }
    }
    packets
}

fn get_link(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    let result = parse_link_query(payload).and_then(|query| {
        if !SERVICE.is_inited() {
            return Err(LinuxError::ENODEV);
        }
        let link = if query.index > 0 {
            SERVICE.link_snapshot_for_ifindex(query.index)
        } else {
            SERVICE
                .link_snapshots()
                .into_iter()
                .find(|link| query.lookup_name() == Some(link.name.as_str()))
        }
        .ok_or(LinuxError::ENODEV)?;
        Ok(NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_link_message(RTM_NEWLINK, header.seq, 0, link, 0),
            credentials: None,
        })
    });
    match result {
        Ok(packet) => {
            let mut packets = vec![packet];
            packets.extend(ack_packets(request, header));
            packets
        }
        Err(errno) => error_packets(request, errno),
    }
}

/// Non-strict link-dump filters accepted by `rtnl_dump_ifinfo`.
///
/// Linux uses `linkinfo_to_kind_ops` for `IFLA_LINKINFO`, but this kernel
/// registers no `rtnl_link_ops`, so a well-formed kind attribute never
/// narrows the walk.  Master relationships are likewise not modelled: every
/// device has no master, which makes `IFLA_MASTER == -1` match everything and
/// every other nonzero value match nothing.
#[derive(Clone, Copy, Debug, Default)]
struct LinkDumpFilter {
    master_idx: i32,
}

impl LinkDumpFilter {
    fn active(self) -> bool {
        self.master_idx != 0
    }

    fn keeps(self, _link: &LinkSnapshot) -> bool {
        self.master_idx == 0 || self.master_idx == -1
    }
}

/// Parses the liberal link-dump request used when strict checking is off.
///
/// `rtnl_valid_dump_ifinfo_req` treats messages shorter than `ifinfomsg` as
/// the historical `rtgenmsg` form.  `nlmsg_parse_deprecated` aligns the
/// one-byte `rtgenmsg` length to `NLMSG_ALIGN(sizeof(struct rtgenmsg))` before
/// reading attributes, so the first attribute starts four bytes into the
/// payload rather than immediately after the family byte.  Attribute iteration
/// is liberal: a short or overlong attribute ends the walk but preserves the
/// entries already parsed, matching `nla_for_each_attr`.  Only a filter
/// attribute whose payload cannot supply its fixed-width prefix makes the
/// request fall back to an unfiltered walk.  (Linux also warns and accepts a
/// truncated fixed-width policy payload, but this kernel rejects that payload
/// rather than reading across its bounds.)
fn parse_link_dump_filter(payload: &[u8]) -> Option<LinkDumpFilter> {
    let offset = if payload.len() < IfInfoMsg::SIZE {
        align(1)
    } else {
        IfInfoMsg::SIZE
    };
    if payload.len() < offset {
        return Some(LinkDumpFilter::default());
    }
    let mut remaining = &payload[offset..];
    let mut filter = LinkDumpFilter::default();
    // `nla_for_each_attr` stops at a short or overlong attribute header but
    // keeps the entries already parsed; only a policy validation failure makes
    // the whole parse fail.  Reproduce that liberal iteration here instead of
    // using the project's stricter generic parser.
    while remaining.len() >= 4 {
        let len = read_u16_ne(remaining, 0)? as usize;
        let kind = read_u16_ne(remaining, 2)?;
        if len < 4 || len > remaining.len() {
            break;
        }
        let attr = NlAttr {
            kind,
            payload: &remaining[4..len],
        };
        match attr.kind & 0x3fff {
            wire_link::attr::MASTER => {
                // Liberal NLA_U32 validation requires at least four bytes and
                // ignores any trailing bytes.
                filter.master_idx = read_u32_ne(attr.payload, 0)? as i32;
            }
            wire_link::attr::EXT_MASK => {
                // No VF/extended information is exposed, but Linux still
                // requires at least the fixed-width payload.
                read_u32_ne(attr.payload, 0)?;
            }
            // NLA_NESTED accepts an empty payload or a complete nested
            // attribute header; the link kind itself is not a filter.
            wire_link::attr::LINKINFO if !attr.payload.is_empty() && attr.payload.len() < 4 => {
                return None;
            }
            _ => {}
        }
        let consumed = align(len);
        if consumed > remaining.len() {
            break;
        }
        remaining = &remaining[consumed..];
    }
    Some(filter)
}

fn dump_links(request: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    if !SERVICE.is_inited() {
        return Vec::new();
    }
    let filter = parse_link_dump_filter(payload).unwrap_or_default();
    let flags = NLM_F_MULTI
        | if filter.active() {
            NLM_F_DUMP_FILTERED
        } else {
            0
        };
    SERVICE
        .link_snapshots()
        .into_iter()
        .filter(|link| filter.keeps(link))
        .map(|link| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_link_message(RTM_NEWLINK, request.seq, flags, link, 0),
            credentials: None,
        })
        .collect()
}

fn dump_addrs(request: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    // The dump handler is selected from the requested family.  IPv6 has its
    // own dump callback in Linux, so an `AF_INET6` request never reaches the
    // IPv4 dump.  This kernel models only IPv4 addresses and must not leak
    // them into an IPv6 reply.  `AF_UNSPEC` selects `rtnl_dump_all`, whose
    // PF_UNSPEC fallback walks the registered families and therefore reports
    // the IPv4 addresses below.
    if payload.first() == Some(&wire_route::FAMILY_IPV6) {
        return Vec::new();
    }
    // The non-strict `inet_dump_ifaddr` path never looks at `ifaddrmsg` or its
    // attributes.  Family and interface-index filters only exist when strict
    // checking is enabled, which this kernel does not expose.
    if !SERVICE.is_inited() {
        return Vec::new();
    }
    SERVICE
        .ipv4_addr_snapshots()
        .into_iter()
        .map(|snapshot| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_addr_message(
                RTM_NEWADDR,
                request.seq,
                NLM_F_MULTI,
                snapshot.dev,
                snapshot.entry,
                &snapshot.label,
                snapshot.secondary,
            ),
            credentials: None,
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default)]
struct GetRouteAttrs {
    dst: Option<Ipv4Address>,
    src: Option<Ipv4Address>,
    iif: u32,
    oif: u32,
    mark: u32,
    uid: Option<u32>,
    ip_proto: Option<u8>,
    sport: u16,
    dport: u16,
}

/// Handles Linux's non-dump `RTM_GETROUTE` lookup.
///
/// The rtnetlink dispatcher chooses the family `doit` callback from the first
/// byte of the request payload (`rtgen_family`), while the IPv4 callback
/// itself validates a complete `rtmsg` and parses its attributes with the
/// non-strict (`nlmsg_parse_deprecated`) rules.
fn get_route(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
    requesting_uid: u32,
) -> Vec<NetlinkPacket> {
    let result = match payload.first().copied() {
        Some(wire_route::FAMILY_IPV4) => {
            get_route_ipv4(header, payload, requesting_port, requesting_uid)
        }
        _ => Err(LinuxError::EOPNOTSUPP),
    };
    match result {
        Ok(data) => {
            let mut packets = vec![NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data,
                credentials: None,
            }];
            // `netlink_rcv_skb` appends the generic acknowledgement after a
            // successful doer when the request asked for one.
            packets.extend(ack_packets(request, header));
            packets
        }
        Err(error) => vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, error),
            credentials: None,
        }],
    }
}

fn get_route_ipv4(
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
    requesting_uid: u32,
) -> Result<Vec<u8>, LinuxError> {
    if payload.len() < RtMsg::SIZE {
        return Err(LinuxError::EINVAL);
    }
    let request = RtMsg::read(payload)?;
    let attrs = parse_getroute_attrs(payload.get(RtMsg::SIZE..).unwrap_or_default())?;

    // `inet_rtm_getroute` validates `RTA_IP_PROTO` before it resolves the
    // input interface, so an unsupported protocol takes precedence over a
    // missing `RTA_IIF` device.
    let protocol = match attrs.ip_proto {
        None => wire_route::IPPROTO_UDP,
        Some(proto) => match proto {
            wire_route::IPPROTO_TCP | wire_route::IPPROTO_UDP | wire_route::IPPROTO_ICMP => proto,
            _ => return Err(LinuxError::EOPNOTSUPP),
        },
    };
    // Policy routing, marks, UIDs and protocol/port selectors do not affect
    // this stack's single routing table.  Parse and validate them for Linux
    // wire compatibility, but do not fabricate a second lookup key.
    let _ = (protocol, attrs.sport, attrs.dport);

    // Linux has no IPv4 input-route implementation in `inet_rtm_getroute`
    // beyond `ip_route_input_rcu`; this stack does not model input routes.
    // Keep the device-existence error ordering and report the unsupported
    // lookup explicitly rather than fabricating an output route.
    if attrs.iif != 0 {
        return if SERVICE.has_device(attrs.iif as i32) {
            Err(LinuxError::EOPNOTSUPP)
        } else {
            Err(LinuxError::ENODEV)
        };
    }

    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }

    let dst = attrs.dst.unwrap_or(Ipv4Address::UNSPECIFIED);
    let oif = NetDeviceId::from_optional_ifindex(attrs.oif as i32)?;
    let route = SERVICE.lookup_ipv4_output(dst, attrs.src, oif)?;

    let table = if request.flags & wire_route::FLAG_LOOKUP_TABLE != 0 {
        route.table
    } else {
        wire_route::TABLE_MAIN
    };
    let tos = request.tos & wire_route::IPTOS_RT_MASK;
    let uid = attrs
        .uid
        .or(Some(requesting_uid))
        .filter(|uid| *uid != u32::MAX);
    let reply = GetRouteReply {
        header,
        requesting_port,
        attrs: &attrs,
        request_flags: request.flags,
        table,
        tos,
        uid,
    };

    if request.flags & wire_route::FLAG_FIB_MATCH != 0 {
        let Some(rule) = route.fib_entry else {
            return Err(LinuxError::EHOSTUNREACH);
        };
        return Ok(build_getroute_fib_match(reply, rule, route.route_type));
    }

    Ok(build_getroute_reply(reply, &route))
}

fn parse_getroute_attrs(attrs: &[u8]) -> Result<GetRouteAttrs, LinuxError> {
    let mut request = GetRouteAttrs::default();
    let mut offset = 0usize;
    while attrs.len().saturating_sub(offset) >= 4 {
        let Some(len) = read_u16_ne(attrs, offset) else {
            break;
        };
        let Some(kind) = read_u16_ne(attrs, offset + 2) else {
            break;
        };
        let len = len as usize;
        let kind = kind & wire_route::NLA_TYPE_MASK;
        if len < 4 || len > attrs.len() - offset {
            // `nlmsg_parse_deprecated` stops at a malformed trailing
            // attribute; it does not fail the complete request.
            break;
        }
        let value = &attrs[offset + 4..offset + len];
        validate_getroute_attr(kind, value)?;
        match kind {
            wire_route::attr::DST => request.dst = Some(getroute_ipv4_attr(value)?),
            wire_route::attr::SRC => request.src = Some(getroute_ipv4_attr(value)?),
            wire_route::attr::IIF => request.iif = getroute_u32_attr(value)?,
            wire_route::attr::OIF => request.oif = getroute_u32_attr(value)?,
            wire_route::attr::MARK => request.mark = getroute_u32_attr(value)?,
            wire_route::attr::UID => request.uid = Some(getroute_u32_attr(value)?),
            wire_route::attr::IP_PROTO => {
                request.ip_proto = value.first().copied();
            }
            wire_route::attr::SPORT => {
                request.sport = value
                    .get(..2)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u16::from_ne_bytes)
                    .ok_or(LinuxError::EINVAL)?;
            }
            wire_route::attr::DPORT => {
                request.dport = value
                    .get(..2)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u16::from_ne_bytes)
                    .ok_or(LinuxError::EINVAL)?;
            }
            _ => {}
        }
        offset = offset.saturating_add(align(len));
    }
    Ok(request)
}

fn validate_getroute_attr(kind: u16, value: &[u8]) -> Result<(), LinuxError> {
    let too_short = |minimum: usize| {
        if value.len() < minimum {
            Err(LinuxError::EINVAL)
        } else {
            Ok(())
        }
    };
    match kind {
        wire_route::attr::DST
        | wire_route::attr::SRC
        | wire_route::attr::IIF
        | wire_route::attr::OIF
        | wire_route::attr::GATEWAY
        | wire_route::attr::PRIORITY
        | wire_route::attr::PREFSRC
        | wire_route::attr::FLOW
        | wire_route::attr::TABLE
        | wire_route::attr::MARK
        | wire_route::attr::UID => too_short(4),
        wire_route::attr::METRICS | wire_route::attr::ENCAP => {
            if value.is_empty() || value.len() >= 4 {
                Ok(())
            } else {
                Err(LinuxError::EINVAL)
            }
        }
        wire_route::attr::MULTIPATH => too_short(8),
        wire_route::attr::ENCAP_TYPE | wire_route::attr::SPORT | wire_route::attr::DPORT => {
            too_short(2)
        }
        wire_route::attr::IP_PROTO => too_short(1),
        // `RTA_NH_ID` is at `strict_start_type`, so non-strict validation
        // still requires its exact four-byte length.
        wire_route::attr::NH_ID => {
            if value.len() == 4 {
                Ok(())
            } else {
                Err(LinuxError::EINVAL)
            }
        }
        _ => Ok(()),
    }
}

fn getroute_u32_attr(value: &[u8]) -> Result<u32, LinuxError> {
    let bytes: [u8; 4] = value
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(LinuxError::EINVAL)?;
    Ok(u32::from_ne_bytes(bytes))
}

fn getroute_ipv4_attr(value: &[u8]) -> Result<Ipv4Address, LinuxError> {
    value
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(Ipv4Address::from_octets)
        .ok_or(LinuxError::EINVAL)
}

/// Reproduces the high `rt_flags` bits that `rt_fill_info` preserves in
/// `rtm_flags`.
///
/// `RTCF_LOCAL` is set for loopback output and for limited broadcast.
/// `RTCF_BROADCAST` accompanies a broadcast output route.  Multicast output
/// carries `RTCF_MULTICAST`; Linux adds `RTCF_LOCAL` only when `ip_check_mc_rcu`
/// confirms an actual group membership, which this stack does not track, so
/// it is intentionally not invented here.
fn getroute_reply_flags(route: &Ipv4OutputRoute, request_flags: u32) -> u32 {
    let mut flags = wire_route::FLAG_CLONED;
    if request_flags & wire_route::FLAG_NOTIFY != 0 {
        flags |= wire_route::FLAG_NOTIFY;
    }
    match route.route_type {
        wire_route::TYPE_BROADCAST => {
            flags |= wire_route::RTCF_BROADCAST | wire_route::RTCF_LOCAL;
        }
        wire_route::TYPE_MULTICAST => {
            flags |= wire_route::RTCF_MULTICAST;
        }
        _ => {}
    }
    // `__mkroute_output` first marks loopback output local, but multicast
    // clears `RTCF_LOCAL` again unless the host has joined the group.  This
    // stack has no IP multicast membership database, so the honest result is
    // the non-member branch even for loopback output.
    if route.oif == NetDeviceId::LOOPBACK && route.route_type != wire_route::TYPE_MULTICAST {
        flags |= wire_route::RTCF_LOCAL;
    }
    flags
}

#[derive(Clone, Copy)]
struct GetRouteReply<'a> {
    header: &'a NlMsgHeader,
    requesting_port: u32,
    attrs: &'a GetRouteAttrs,
    request_flags: u32,
    table: u8,
    tos: u8,
    uid: Option<u32>,
}

fn build_getroute_reply(reply: GetRouteReply<'_>, route: &Ipv4OutputRoute) -> Vec<u8> {
    let GetRouteReply {
        header,
        requesting_port,
        attrs,
        request_flags,
        table,
        tos,
        uid,
    } = reply;
    let requested_src = attrs.src.filter(|src| !src.is_unspecified());
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 32,
        src_len: if requested_src.is_some() { 32 } else { 0 },
        tos,
        table,
        protocol: 0,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: route.route_type,
        flags: getroute_reply_flags(route, request_flags),
    }
    .write(&mut payload);

    let mut push_u32_attr = |kind: u16, value: u32| {
        push_attr(&mut payload, kind, &value.to_ne_bytes());
    };
    push_u32_attr(wire_route::attr::TABLE, table as u32);
    push_u32_attr(
        wire_route::attr::DST,
        u32::from_ne_bytes(attrs.dst.unwrap_or(Ipv4Address::UNSPECIFIED).octets()),
    );
    if let Some(src) = requested_src {
        push_u32_attr(wire_route::attr::SRC, u32::from_ne_bytes(src.octets()));
    }
    push_u32_attr(wire_route::attr::OIF, route.oif.ifindex() as u32);
    // `rt_fill_info` emits the selected flow source whenever it differs from
    // the source the caller supplied; an absent or explicitly zero source is
    // compared as zero.
    if let Some(source) = route.source
        && source != attrs.src.unwrap_or(Ipv4Address::UNSPECIFIED)
    {
        push_u32_attr(
            wire_route::attr::PREFSRC,
            u32::from_ne_bytes(source.octets()),
        );
    }
    if let Some(gateway) = route.gateway {
        push_u32_attr(
            wire_route::attr::GATEWAY,
            u32::from_ne_bytes(gateway.octets()),
        );
    }
    if attrs.mark != 0 {
        push_u32_attr(wire_route::attr::MARK, attrs.mark);
    }
    if let Some(uid) = uid {
        push_u32_attr(wire_route::attr::UID, uid);
    }
    // `rt_fill_info` always appends `RTA_CACHEINFO`; all eight words are zero
    // for a freshly built route.
    push_attr(&mut payload, wire_route::attr::CACHEINFO, &[0u8; 32]);

    build_nlmsg_for_port(
        wire_route::RTM_NEWROUTE,
        header.seq,
        0,
        requesting_port,
        payload,
    )
}

fn build_getroute_fib_match(reply: GetRouteReply<'_>, rule: Rule, route_type: u8) -> Vec<u8> {
    let GetRouteReply {
        header,
        requesting_port,
        table,
        tos,
        ..
    } = reply;
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: rule.filter.prefix_len(),
        src_len: 0,
        tos,
        table,
        protocol: rule.protocol,
        scope: rule.scope,
        route_type,
        flags: 0,
    }
    .write(&mut payload);

    let mut push_u32_attr = |kind: u16, value: u32| {
        push_attr(&mut payload, kind, &value.to_ne_bytes());
    };
    push_u32_attr(wire_route::attr::TABLE, table as u32);
    let prefix = match rule.filter.address() {
        KernelIpAddress::Ipv4(prefix) => Some(prefix),
        KernelIpAddress::Ipv6(_) => None,
    };
    if rule.filter.prefix_len() != 0
        && let Some(prefix) = prefix
    {
        push_u32_attr(wire_route::attr::DST, u32::from_ne_bytes(prefix.octets()));
    }
    if let Some(prefsrc) = rule.prefsrc.and_then(|addr| match addr {
        KernelIpAddress::Ipv4(addr) => Some(addr),
        KernelIpAddress::Ipv6(_) => None,
    }) && !prefsrc.is_unspecified()
    {
        push_u32_attr(
            wire_route::attr::PREFSRC,
            u32::from_ne_bytes(prefsrc.octets()),
        );
    }
    if let Some(gateway) = rule.via.and_then(|addr| match addr {
        KernelIpAddress::Ipv4(addr) => Some(addr),
        KernelIpAddress::Ipv6(_) => None,
    }) {
        push_u32_attr(
            wire_route::attr::GATEWAY,
            u32::from_ne_bytes(gateway.octets()),
        );
    }
    push_u32_attr(wire_route::attr::OIF, rule.lookup_device().ifindex() as u32);

    build_nlmsg_for_port(
        wire_route::RTM_NEWROUTE,
        header.seq,
        0,
        requesting_port,
        payload,
    )
}

fn dump_routes(request: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    // As with address dumps, an `AF_INET6` request is routed to the IPv6
    // handler, not to the IPv4 dump.  The project has no IPv6 route table, so
    // the reply is an empty dump terminated by `NLMSG_DONE`.
    if payload.first() == Some(&wire_route::FAMILY_IPV6) {
        return Vec::new();
    }
    // `inet_dump_fib` only reads `rtm_flags` from a complete `struct rtmsg`
    // when strict checking is off.  `RTM_F_PREFIX` makes IPv4 return an empty
    // dump; every other field and attribute is ignored (including
    // `RTM_F_CLONED`, which only affects the strict filter).
    if payload.len() >= RtMsg::SIZE
        && RtMsg::read(payload).is_ok_and(|info| info.flags & wire_route::FLAG_PREFIX != 0)
    {
        return Vec::new();
    }
    if !SERVICE.is_inited() {
        return Vec::new();
    }
    SERVICE
        .route_snapshot()
        .into_iter()
        .map(|route| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_route_message(RTM_NEWROUTE, request.seq, NLM_F_MULTI, route),
            credentials: None,
        })
        .collect()
}

fn mutation_response(
    request: &[u8],
    header: &NlMsgHeader,
    result: Result<Vec<NetworkEvent>, LinuxError>,
) -> Vec<NetlinkPacket> {
    match result {
        Ok(_) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newlink(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_link_update(payload).and_then(|mut req| {
            let index = req.resolve_index()?;
            if !SERVICE.is_inited() {
                return Err(LinuxError::ENODEV);
            }
            if SERVICE.link_snapshot_for_ifindex(index).is_some() {
                if header.flags & NLM_F_EXCL != 0 {
                    return Err(LinuxError::EEXIST);
                }
                if header.flags & NLM_F_REPLACE != 0 {
                    return Err(LinuxError::EOPNOTSUPP);
                }
            } else {
                return Err(LinuxError::ENODEV);
            }
            req.index = index;
            apply_link_update_with_origin(req, origin)
        }),
    )
}

fn apply_newaddr(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_addr_request(payload).and_then(|req| add_addr_with_origin(req, header.flags, origin)),
    )
}

fn apply_deladdr(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_del_addr_request(payload).and_then(|req| del_addr_with_origin(req, origin)),
    )
}

fn apply_newroute(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_route_request(payload)
            .and_then(|req| add_route_with_origin(req, header.flags, origin)),
    )
}

fn apply_delroute(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_route_request(payload).and_then(|req| del_route_with_origin(req, origin)),
    )
}

fn apply_newneigh(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_neigh_add_wire_request(payload)
            .and_then(|req| add_neigh_wire_with_origin(req, header.flags, origin)),
    )
}

fn apply_delneigh(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let origin = MutationOrigin::from_header(header, requesting_port);
    mutation_response(
        request,
        header,
        parse_neigh_delete_wire_request(payload)
            .and_then(|req| del_neigh_wire_with_origin(req, origin)),
    )
}

/// Publishes neutral network changes as kernel-originated rtnetlink
/// notifications.
pub(super) fn publish_network_events(events: Vec<NetworkEvent>) {
    for notification in notifications_for_events(events) {
        publish_route_event(notification.group, &notification.data);
    }
}

/// Link notifications only carry the requester port when they also echo.
fn link_portid(origin: MutationOrigin) -> u32 {
    if origin.echo { origin.portid } else { 0 }
}

/// Replaces the header port ID of a built message without touching its
/// message-specific attributes.
fn set_message_port(mut data: Vec<u8>, portid: u32) -> Vec<u8> {
    if data.len() >= NLMSG_HDR_LEN {
        data[12..16].copy_from_slice(&portid.to_ne_bytes());
    }
    data
}

/// Selects the notification origin for a route event.
///
/// Explicit route mutations inherit the request metadata and echo behavior.
/// Address-derived routes are installed or removed by the kernel FIB notifier,
/// whose `fib_magic(..., NULL)` path has no request context, so they are
/// published with port 0, sequence 0, and no echo.
fn route_event_origin(origin: MutationOrigin, provenance: RouteProvenance) -> MutationOrigin {
    match provenance {
        RouteProvenance::Request => origin,
        RouteProvenance::Kernel => MutationOrigin::default(),
    }
}

/// Maps neutral network changes to the rtnetlink messages Linux publishes.
///
/// Data-plane and teardown changes are kernel-originated: their headers use
/// port ID 0 and sequence 0.  Management mutations use
/// [`notifications_for_events_with_origin`] instead.
pub(super) fn notifications_for_events(events: Vec<NetworkEvent>) -> Vec<RtnetlinkNotification> {
    notifications_for_events_with_origin(events, MutationOrigin::default())
}

/// Maps network changes produced by one management request.
///
/// Linux fills address and route notifications from the requesting port and
/// request sequence even without `NLM_F_ECHO`.  Link notifications use those
/// values only with echo; neighbor notifications carry the requesting port but
/// always use sequence 0 and never echo.
fn notifications_for_events_with_origin(
    events: Vec<NetworkEvent>,
    origin: MutationOrigin,
) -> Vec<RtnetlinkNotification> {
    let link_seq = if origin.echo { origin.seq } else { 0 };
    events
        .into_iter()
        .map(|event| match event {
            NetworkEvent::LinkChanged {
                link,
                changed_flags,
            } => RtnetlinkNotification {
                group: RTNLGRP_LINK,
                data: set_message_port(
                    build_link_message(RTM_NEWLINK, link_seq, 0, link, changed_flags),
                    link_portid(origin),
                ),
                echo: origin.echo,
            },
            NetworkEvent::LinkRemoved { link } => RtnetlinkNotification {
                group: RTNLGRP_LINK,
                data: set_message_port(
                    build_link_message(RTM_DELLINK, link_seq, 0, link, u32::MAX),
                    link_portid(origin),
                ),
                echo: origin.echo,
            },
            NetworkEvent::AddrAdded { addr } => RtnetlinkNotification {
                group: RTNLGRP_IPV4_IFADDR,
                data: set_message_port(
                    build_addr_message(
                        RTM_NEWADDR,
                        origin.seq,
                        0,
                        addr.dev,
                        addr.entry,
                        &addr.label,
                        addr.secondary,
                    ),
                    origin.portid,
                ),
                echo: origin.echo,
            },
            NetworkEvent::AddrRemoved { addr } => RtnetlinkNotification {
                group: RTNLGRP_IPV4_IFADDR,
                data: set_message_port(
                    build_addr_message(
                        RTM_DELADDR,
                        origin.seq,
                        0,
                        addr.dev,
                        addr.entry,
                        &addr.label,
                        addr.secondary,
                    ),
                    origin.portid,
                ),
                echo: origin.echo,
            },
            NetworkEvent::RouteAdded { route, provenance } => {
                let origin = route_event_origin(origin, provenance);
                RtnetlinkNotification {
                    group: RTNLGRP_IPV4_ROUTE,
                    data: set_message_port(
                        build_route_message(RTM_NEWROUTE, origin.seq, 0, route),
                        origin.portid,
                    ),
                    echo: origin.echo,
                }
            }
            NetworkEvent::RouteRemoved { route, provenance } => {
                let origin = route_event_origin(origin, provenance);
                RtnetlinkNotification {
                    group: RTNLGRP_IPV4_ROUTE,
                    data: set_message_port(
                        build_route_message(RTM_DELROUTE, origin.seq, 0, route),
                        origin.portid,
                    ),
                    echo: origin.echo,
                }
            }
            NetworkEvent::NeighborAdded { neighbor } => RtnetlinkNotification {
                group: RTNLGRP_NEIGH,
                data: build_neigh_message(
                    RTM_NEWNEIGH,
                    0,
                    0,
                    origin.portid,
                    NeighborSnapshot {
                        dev: neighbor.dev,
                        dst: neighbor.dst,
                        state: neighbor.state,
                    },
                ),
                echo: false,
            },
            NetworkEvent::NeighborRemoved { neighbor } => RtnetlinkNotification {
                group: RTNLGRP_NEIGH,
                data: build_neigh_message(RTM_DELNEIGH, 0, 0, origin.portid, neighbor),
                echo: false,
            },
        })
        .collect()
}

/// Publishes the events produced by a successful mutation.
///
/// Every caller holds `network_config_lock` for both the mutation and this
/// call, which keeps notification order identical to commit order.  The events
/// are returned as well for the handler-specific tests that inspect them.
fn publish_mutation_events(events: Vec<NetworkEvent>, origin: MutationOrigin) -> Vec<NetworkEvent> {
    for notification in notifications_for_events_with_origin(events.clone(), origin) {
        if notification.echo {
            // `nlmsg_notify` sends the multicast copy first with the requester
            // excluded, then unicasts a separate reference as an echo.  The
            // mutation has already committed, so delivery failures do not
            // change its result.
            publish_route_event_excluding(
                notification.group,
                &notification.data,
                Some(origin.portid),
            );
            if let Err(errno) = super::port::deliver_unicast(
                NETLINK_ROUTE,
                origin.portid,
                0,
                super::port::group_bit(notification.group),
                &notification.data,
                None,
                true,
            ) {
                debug!("Failed to echo rtnetlink notification: {:?}", errno);
            }
        } else {
            publish_route_event(notification.group, &notification.data);
        }
    }
    events
}

/// Applies a link update without request-derived notification routing.
pub(super) fn apply_link_update(
    request: LinkUpdateRequest,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    apply_link_update_with_origin(request, MutationOrigin::default())
}

fn apply_link_update_with_origin(
    request: LinkUpdateRequest,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    if request.index <= 0 {
        return Err(LinuxError::ENODEV);
    }
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _network_config_guard = control::network_config_lock();

    let before = SERVICE
        .link_snapshot_for_ifindex(request.index)
        .ok_or(LinuxError::ENODEV)?;
    let current_flags = before.flags;
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
    let events = SERVICE.update_device_link(
        request.index,
        LinkConfigUpdate {
            name: request.name,
            mtu: request.mtu.map(|mtu| mtu as usize),
            is_up: (change & LINK_FLAG_UP != 0).then_some(request.flags & LINK_FLAG_UP != 0),
        },
    )?;
    Ok(publish_mutation_events(events, origin))
}

pub(super) fn add_addr(request: AddrRequest, flags: u16) -> Result<Vec<NetworkEvent>, LinuxError> {
    add_addr_with_origin(request, flags, MutationOrigin::default())
}

fn add_addr_with_origin(
    request: AddrRequest,
    flags: u16,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let (dev, entry) = ipv4_addr_entry_from_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _network_config_guard = control::network_config_lock();
    let existing_action = if flags & NLM_F_REPLACE != 0 && flags & NLM_F_EXCL == 0 {
        ExistingIpv4AddrAction::Keep
    } else {
        ExistingIpv4AddrAction::Reject
    };
    let events = SERVICE.add_ipv4_addr(dev, entry, existing_action)?;
    Ok(publish_mutation_events(events, origin))
}

pub(super) fn del_addr(request: DelAddrRequest) -> Result<Vec<NetworkEvent>, LinuxError> {
    del_addr_with_origin(request, MutationOrigin::default())
}

fn del_addr_with_origin(
    request: DelAddrRequest,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    let ifindex = i32::try_from(request.index).map_err(|_| LinuxError::ENODEV)?;
    let dev = NetDeviceId::new(ifindex).ok_or(LinuxError::ENODEV)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _network_config_guard = control::network_config_lock();
    SERVICE
        .link_snapshot_for_ifindex(ifindex)
        .ok_or(LinuxError::ENODEV)?;
    let addr = SERVICE
        .ipv4_addr_snapshots()
        .into_iter()
        .find(|snapshot| {
            snapshot.dev == dev
                && request.local.is_none_or(|local| {
                    IpAddress::Ipv4(snapshot.entry.addr.address().into()) == local
                })
                && request
                    .label
                    .as_ref()
                    .is_none_or(|label| snapshot.label == *label)
                && request.address.is_none_or(|address| {
                    let IpAddress::Ipv4(address) = address else {
                        return false;
                    };
                    let Some(selector) = Ipv4Cidr::new(address.into(), request.prefix_len) else {
                        return false;
                    };
                    snapshot.entry.addr.prefix_len() == selector.prefix_len()
                        && snapshot.entry.addr.network() == selector.network()
                })
        })
        .map(|snapshot| snapshot.entry.addr)
        .ok_or(LinuxError::EADDRNOTAVAIL)?;
    let events = SERVICE.remove_ipv4_addr(dev, addr)?;
    Ok(publish_mutation_events(events, origin))
}

pub(super) fn add_route(
    request: RouteRequest,
    flags: u16,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    add_route_with_origin(request, flags, MutationOrigin::default())
}

fn add_route_with_origin(
    request: RouteRequest,
    flags: u16,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let rule = route_rule_from_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _network_config_guard = control::network_config_lock();
    let mut matching = SERVICE
        .route_snapshot()
        .into_iter()
        .filter(|route| same_route_key(*route, &request))
        .peekable();
    let events = if matching.peek().is_some() {
        if flags & NLM_F_EXCL != 0 || flags & NLM_F_REPLACE == 0 {
            return Err(LinuxError::EEXIST);
        }
        // Derived routes count as conflicts but cannot be replacement targets.
        let existing = matching
            .find(|route| route.is_configured())
            .ok_or(LinuxError::EEXIST)?;
        // Linux silently keeps an identical route rather than emitting a
        // redundant RTM_NEWROUTE.
        if existing == rule {
            return Ok(Vec::new());
        }
        SERVICE.replace_route_rule(existing, rule)?
    } else if flags & NLM_F_CREATE == 0 {
        return Err(LinuxError::ENOENT);
    } else {
        SERVICE.add_route_rule(rule)?
    };
    Ok(publish_mutation_events(events, origin))
}

pub(super) fn del_route(request: RouteRequest) -> Result<Vec<NetworkEvent>, LinuxError> {
    del_route_with_origin(request, MutationOrigin::default())
}

fn del_route_with_origin(
    request: RouteRequest,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    validate_route_request(request)?;
    if !SERVICE.is_inited() {
        return Err(LinuxError::ENODEV);
    }
    let _network_config_guard = control::network_config_lock();
    let route = SERVICE
        .route_snapshot()
        .into_iter()
        .find(|route| route.is_configured() && route_delete_matches(*route, &request))
        .ok_or(LinuxError::ESRCH)?;
    let events = SERVICE.remove_route_rule(route);
    if events.is_empty() {
        return Err(LinuxError::ESRCH);
    }
    Ok(publish_mutation_events(events, origin))
}

pub(super) fn add_neigh(
    request: NeighRequest,
    flags: u16,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    add_neigh_with_origin(request, flags, MutationOrigin::default())
}

fn add_neigh_with_origin(
    request: NeighRequest,
    flags: u16,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let _network_config_guard = control::network_config_lock();
    // `neigh_add` resolves a nonzero interface before checking the address
    // family, but leaves ifindex zero unresolved; the later "Device not
    // specified" check reports EINVAL rather than ENODEV.
    if request.ifindex != 0 {
        ensure_link_exists(request.ifindex)?;
    }
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    add_neigh_locked(request, flags, origin)
}

fn add_neigh_wire_with_origin(
    request: NeighWireAddRequest<'_>,
    flags: u16,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let _network_config_guard = control::network_config_lock();
    // These checks deliberately follow `neigh_add`: the interface is resolved
    // before the link-layer address, the address family, and the destination
    // payload are examined.
    if request.ifindex != 0 {
        ensure_link_exists(request.ifindex)?;
    }
    // `neigh_add` only saves NDA_LLADDR inside the nonzero-interface branch.
    // With no interface the address is not inspected, so a short payload must
    // not preempt the family/device errors reported later.
    let lladdr = if request.ifindex != 0 {
        request.lladdr.map(parse_mac_prefix).transpose()?
    } else {
        None
    };
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    // `neigh_delete` uses `nlmsg_find_attr` instead of policy parsing and
    // compares the raw destination length with the table key length.  A short
    // NDA_DST is therefore `EINVAL`, not the `ERANGE` that NLA_U32 policy
    // validation would report.
    if request.dst.len() < 4 {
        return Err(LinuxError::EINVAL);
    }
    let dst = parse_ip_prefix_by_family(request.family, request.dst)?;
    add_neigh_locked(
        NeighRequest {
            family: request.family,
            ifindex: request.ifindex,
            state: request.state,
            flags: request.flags,
            dst,
            lladdr,
        },
        flags,
        origin,
    )
}

fn add_neigh_locked(
    request: NeighRequest,
    flags: u16,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let exclusive = flags & NLM_F_EXCL != 0;
    let policy = NeighborUpdatePolicy {
        can_create: flags & NLM_F_CREATE != 0,
        exclusive,
        // Linux clears the override flag when NLM_F_REPLACE is absent (and
        // always when NLM_F_EXCL rejects the request first).
        can_override: flags & NLM_F_REPLACE != 0 && !exclusive,
    };
    let ifindex = i32::try_from(request.ifindex).map_err(|_| LinuxError::ENODEV)?;
    // A zero interface passed the earlier resolution step but is rejected by
    // `neigh_add`'s explicit "Device not specified" check with EINVAL.
    let dev = NetDeviceId::new(ifindex).ok_or(LinuxError::EINVAL)?;
    let dst = to_kernel_ip(request.dst)?;
    let events = SERVICE
        .apply_neighbor_admin_update(dev, dst, policy, |old| {
            neighbor_update_from_request(request, dev, dst, old)
        })?
        .into_iter()
        .collect();
    Ok(publish_mutation_events(events, origin))
}

pub(super) fn del_neigh(request: NeighRequest) -> Result<Vec<NetworkEvent>, LinuxError> {
    del_neigh_with_origin(request, MutationOrigin::default())
}

fn del_neigh_with_origin(
    request: NeighRequest,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let _network_config_guard = control::network_config_lock();
    // `neigh_delete` resolves a nonzero interface before checking the address
    // family. A zero interface stays unresolved for the later EINVAL check.
    if request.ifindex != 0 {
        ensure_link_exists(request.ifindex)?;
    }
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    del_neigh_locked(request, origin)
}

fn del_neigh_wire_with_origin(
    request: NeighWireDeleteRequest<'_>,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    let _network_config_guard = control::network_config_lock();
    if request.ifindex != 0 {
        ensure_link_exists(request.ifindex)?;
    }
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    // `neigh_delete` uses `nlmsg_find_attr` instead of policy parsing and
    // compares the raw destination length with the table key length.  A short
    // NDA_DST is therefore `EINVAL`, not the `ERANGE` that NLA_U32 policy
    // validation would report.
    if request.dst.len() < 4 {
        return Err(LinuxError::EINVAL);
    }
    let dst = parse_ip_prefix_by_family(request.family, request.dst)?;
    del_neigh_locked(
        NeighRequest {
            family: request.family,
            ifindex: request.ifindex,
            state: request.state,
            flags: request.flags,
            dst,
            lladdr: None,
        },
        origin,
    )
}

fn del_neigh_locked(
    request: NeighRequest,
    origin: MutationOrigin,
) -> Result<Vec<NetworkEvent>, LinuxError> {
    // This stack has no proxy-neighbor table. Linux `neigh_delete` consults it
    // before rejecting a missing device, and an absent entry is ENOENT.
    if request.flags & wire_neigh::NTF_PROXY != 0 {
        return Err(LinuxError::ENOENT);
    }
    if request.ifindex == 0 {
        return Err(LinuxError::EINVAL);
    }
    let ifindex = i32::try_from(request.ifindex).map_err(|_| LinuxError::ENODEV)?;
    let dev = NetDeviceId::new(ifindex).ok_or(LinuxError::ENODEV)?;
    let neighbor = SERVICE.remove_neighbor(dev, to_kernel_ip(request.dst)?)?;
    Ok(publish_mutation_events(
        vec![NetworkEvent::NeighborRemoved { neighbor }],
        origin,
    ))
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

fn neighbor_update_from_request(
    request: NeighRequest,
    dev: NetDeviceId,
    dst: KernelIpAddress,
    old: Option<NeighborState>,
) -> Result<NeighborUpdate, LinuxError> {
    if request.flags != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let state = match request.state {
        wire_neigh::STATE_INCOMPLETE => NeighborState::Incomplete,
        wire_neigh::STATE_PERMANENT => NeighborState::Permanent {
            // `__neigh_update` reuses `neigh->ha` when the request omits
            // NDA_LLADDR and the existing entry already has a valid address.
            hardware_addr: request
                .lladdr
                .or_else(|| old.and_then(NeighborState::hardware_addr))
                .ok_or(LinuxError::EINVAL)?,
        },
        _ => return Err(LinuxError::EOPNOTSUPP),
    };
    Ok(NeighborUpdate { dev, dst, state })
}

fn ipv4_addr_entry_from_request(
    request: AddrRequest,
) -> Result<(NetDeviceId, Ipv4AddrEntry), LinuxError> {
    let IpAddress::Ipv4(address) = request.address else {
        return Err(LinuxError::EAFNOSUPPORT);
    };
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    let broadcast = match request.broadcast {
        // `inet_rtm_to_ifa` treats an explicit zero `IFA_BROADCAST` exactly
        // like an absent attribute: the stored `ifa_broadcast` remains zero.
        Some(IpAddress::Ipv4(broadcast)) if !broadcast.is_unspecified() => Some(broadcast.into()),
        Some(IpAddress::Ipv4(_)) | None => None,
        Some(_) => return Err(LinuxError::EAFNOSUPPORT),
    };
    let ifindex = i32::try_from(request.index).map_err(|_| LinuxError::ENODEV)?;
    let dev = NetDeviceId::new(ifindex).ok_or(LinuxError::ENODEV)?;
    Ok((
        dev,
        Ipv4AddrEntry {
            addr: crate::ip::Ipv4Cidr::new(address.into(), request.prefix_len)
                .ok_or(LinuxError::EINVAL)?,
            scope: request.scope,
            broadcast,
            no_prefix_route: request.flags & wire_addr::FLAG_NOPREFIXROUTE != 0,
            label: request.label,
        },
    ))
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
        && (request.oif == 0 || route.dev.ifindex() as u32 == request.oif)
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
    validate_route_insert_request(request)?;
    let dst = request
        .dst
        .unwrap_or(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED));
    let IpAddress::Ipv4(dst) = dst else {
        return Err(LinuxError::EAFNOSUPPORT);
    };
    let filter = IpCidr::Ipv4(
        Ipv4Cidr::new(dst.into(), request.dst_len)
            .expect("route prefix length was validated")
            .network(),
    );
    let ifindex = i32::try_from(request.oif).map_err(|_| LinuxError::EOPNOTSUPP)?;
    let dev = NetDeviceId::new(ifindex).ok_or(LinuxError::EOPNOTSUPP)?;
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
            protocol: request.protocol,
            scope: request.scope,
            route_type: normalize_route_type(request.route_type),
            prefsrc,
        },
    ))
}

/// Validation that only applies when installing a route.
///
/// `fib_create_info()` rejects `RTN_UNSPEC` because its table scope is
/// `RT_SCOPE_NOWHERE`, which is greater than any supplied scope; the same
/// value also falls through every route-type switch as invalid.  Consequently
/// no scope can make an unspecified type insertable.  Routes marked
/// dead/link-down are likewise insertion-only policy.  `RTM_DELROUTE` passes
/// those same bytes to `fib_table_delete()`, which matches the route key and
/// must not inherit the insertion-only policy.
pub(super) fn validate_route_insert_request(request: RouteRequest) -> Result<(), LinuxError> {
    validate_route_request(request)?;
    if request.route_type == 0 {
        return Err(LinuxError::EINVAL);
    }
    if request.flags & (wire_route::RTNH_F_DEAD | wire_route::RTNH_F_LINKDOWN) != 0 {
        return Err(LinuxError::EINVAL);
    }
    Ok(())
}

pub(super) fn validate_route_request(request: RouteRequest) -> Result<(), LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    if request.dst_len > 32 || request.src_len > 32 {
        return Err(LinuxError::EINVAL);
    }
    // `inet_validate_dscp` rejects ECN bits; the remaining DSCP bits are a
    // route-key selector that this stack's single-table model does not store.
    if request.tos & 0x3 != 0 {
        return Err(LinuxError::EINVAL);
    }
    if request.route_type > wire_route::TYPE_MAX {
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
        alt_name: attrs.alt_name,
        mtu: attrs.mtu,
    })
}

fn parse_link_query(payload: &[u8]) -> Result<LinkQuery, LinuxError> {
    let info = IfInfoMsg::read(payload)?;
    let attrs = parse_link_query_attrs(&payload[IfInfoMsg::SIZE..])?;
    if info.index <= 0 && attrs.name.is_none() && attrs.alt_name.is_none() {
        return Err(LinuxError::EINVAL);
    }
    Ok(LinkQuery {
        index: info.index,
        name: attrs.name,
        alt_name: attrs.alt_name,
    })
}

fn parse_addr_request(payload: &[u8]) -> Result<AddrRequest, LinuxError> {
    let info = IfAddrMsg::read(payload)?;
    if info.prefix_len > 32 {
        return Err(LinuxError::EINVAL);
    }
    // Linux derives the legacy IPv4 `ifa_flags` bits from address ordering
    // and lifetimes, so this permanent-address subset accepts the field
    // without treating it as independently mutable state.
    let attrs = parse_addr_attrs(info.family, parse_attrs_lossy(&payload[IfAddrMsg::SIZE..]))?;
    // `inet_rtm_to_ifa` requires IFA_LOCAL. IFA_ADDRESS is the peer address
    // for point-to-point interfaces and defaults to IFA_LOCAL; this kernel
    // has no peer-address model, so a distinct value is rejected rather than
    // silently treated as the local address.
    let local = attrs.local.ok_or(LinuxError::EINVAL)?;
    if attrs.address.is_some_and(|address| address != local) {
        return Err(LinuxError::EOPNOTSUPP);
    }
    // IFA_FLAGS is authoritative when present; otherwise the legacy 8-bit
    // `ifa_flags` field carries the low bits.
    let flags = attrs.flags.unwrap_or(u32::from(info.flags));
    // `IFA_F_SECONDARY` is derived from address ordering and cleared by
    // `__inet_insert_ifa`; accepting it as input does not set independent
    // state.  Other unimplemented lifetimes and flags remain explicitly
    // rejected instead of being silently misrepresented.
    const SUPPORTED_ADDR_FLAGS: u32 = (wire_addr::FLAG_PERMANENT as u32)
        | wire_addr::FLAG_NOPREFIXROUTE
        | wire_addr::FLAG_SECONDARY;
    if flags & !SUPPORTED_ADDR_FLAGS != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    Ok(AddrRequest {
        index: info.index,
        family: info.family,
        prefix_len: info.prefix_len,
        scope: info.scope,
        address: local,
        broadcast: attrs.broadcast,
        flags,
        label: attrs.label,
    })
}

fn parse_del_addr_request(payload: &[u8]) -> Result<DelAddrRequest, LinuxError> {
    let info = IfAddrMsg::read(payload)?;
    let attrs = parse_addr_attrs(info.family, parse_attrs_lossy(&payload[IfAddrMsg::SIZE..]))?;
    Ok(DelAddrRequest {
        index: info.index,
        family: info.family,
        prefix_len: info.prefix_len,
        local: attrs.local,
        address: attrs.address,
        label: attrs.label,
    })
}

pub(super) fn parse_route_request(payload: &[u8]) -> Result<RouteRequest, LinuxError> {
    let info = RtMsg::read(payload)?;
    let attrs = parse_route_attrs(info.family, parse_attrs_lossy(&payload[RtMsg::SIZE..]))?;
    Ok(RouteRequest {
        family: info.family,
        dst_len: info.dst_len,
        src_len: info.src_len,
        table: attrs.table.unwrap_or(info.table),
        protocol: info.protocol,
        scope: info.scope,
        route_type: info.route_type,
        tos: info.tos,
        flags: info.flags,
        oif: attrs.oif,
        dst: attrs.dst,
        src: attrs.src,
        gateway: attrs.gateway,
        prefsrc: attrs.prefsrc,
    })
}

/// Parses `RTM_NEWNEIGH` with the same liberal rules as Linux `neigh_add`.
///
/// Attribute policy is validated before the request is interpreted. A
/// malformed trailing attribute ends iteration without failing the request,
/// while a policy violation aborts it.
fn parse_neigh_add_wire_request(payload: &[u8]) -> Result<NeighWireAddRequest<'_>, LinuxError> {
    let info = NdMsg::read(payload)?;
    let attrs = parse_neigh_attrs_at_liberal(payload);
    for attr in &attrs {
        if !is_valid_neigh_attr(attr) {
            return Err(LinuxError::EINVAL);
        }
    }
    let mut dst = None;
    let mut lladdr = None;
    for attr in attrs {
        match neigh_attr_kind(&attr) {
            wire_neigh::attr::DST => dst = Some(attr.payload),
            wire_neigh::attr::LLADDR => lladdr = Some(attr.payload),
            // `NDA_FLAGS_EXT` carries the internal managed bit. The model has
            // no managed/offloaded neighbor state, so accepting it would
            // silently apply a different update than Linux. `NDA_PROTOCOL`
            // likewise changes the stored protocol and dump output. Reject
            // nonzero values explicitly instead of ignoring state-bearing
            // input; zero is equivalent to omitting the attribute. Attribute
            // policy runs before `neigh_add` interprets the request.
            wire_neigh::attr::FLAGS_EXT => require_zero_u32(attr.payload)?,
            wire_neigh::attr::PROTOCOL => require_zero_u8(attr.payload)?,
            _ => {}
        }
    }
    Ok(NeighWireAddRequest {
        family: info.family,
        ifindex: info.ifindex as u32,
        state: info.state,
        flags: info.flags,
        dst: dst.ok_or(LinuxError::EINVAL)?,
        lladdr,
    })
}

/// Parses `RTM_DELNEIGH`.
///
/// Linux `neigh_delete` does not run `nda_policy`: it only locates `NDA_DST`
/// with `nla_find` and ignores every other attribute. Structural truncation
/// therefore discards attributes after it without turning the request into an
/// attribute-validation error.
fn parse_neigh_delete_wire_request(
    payload: &[u8],
) -> Result<NeighWireDeleteRequest<'_>, LinuxError> {
    let info = NdMsg::read(payload)?;
    let attrs = parse_neigh_attrs_at_liberal(payload);
    let dst = attrs
        .iter()
        .find(|attr| neigh_attr_kind(attr) == wire_neigh::attr::DST)
        .map(|attr| attr.payload)
        .ok_or(LinuxError::EINVAL)?;
    Ok(NeighWireDeleteRequest {
        family: info.family,
        ifindex: info.ifindex as u32,
        state: info.state,
        flags: info.flags,
        dst,
    })
}

fn parse_link_attrs(attrs: Vec<NlAttr<'_>>) -> Result<LinkAttrs, LinuxError> {
    let mut parsed = LinkAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_link::attr::IFNAME => parsed.name = Some(parse_string(attr.payload)?),
            wire_link::attr::ALT_IFNAME => {
                parsed.alt_name = Some(parse_link_query_name(
                    attr.payload,
                    wire_link::ALTIFNAMSIZ - 1,
                )?);
            }
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

fn parse_link_query_attrs(payload: &[u8]) -> Result<LinkQueryAttrs, LinuxError> {
    let mut parsed = LinkQueryAttrs::default();
    let mut remaining = payload;
    while remaining.len() >= 4 {
        let len = read_u16_ne(remaining, 0).ok_or(LinuxError::EINVAL)? as usize;
        let kind = read_u16_ne(remaining, 2).ok_or(LinuxError::EINVAL)?;
        if len < 4 || len > remaining.len() {
            // Non-strict netlink parsing stops at a malformed attribute and
            // ignores the remaining bytes instead of failing the request.
            break;
        }
        let attr = &remaining[4..len];
        match kind & wire_route::NLA_TYPE_MASK {
            wire_link::attr::IFNAME => {
                parsed.name = Some(parse_link_query_name(attr, wire_link::IFNAMSIZ - 1)?);
            }
            wire_link::attr::ALT_IFNAME => {
                parsed.alt_name = Some(parse_link_query_name(attr, wire_link::ALTIFNAMSIZ - 1)?);
            }
            wire_link::attr::EXT_MASK => {
                let _ = read_u32_payload(attr)?;
            }
            wire_link::attr::LINKINFO if !attr.is_empty() && attr.len() < 4 => {
                // The link-info policy is `NLA_NESTED`: an empty payload is
                // valid, but a nonempty one must contain a complete child
                // attribute header.
                return Err(LinuxError::EINVAL);
            }
            wire_link::attr::NET_NS_PID | wire_link::attr::NET_NS_FD => {
                // `rtnl_ensure_unique_netns(..., true)` rejects these two
                // selectors; `IFLA_TARGET_NETNSID` is not covered by that
                // check, but this stack has no namespace-id registry.
                return Err(LinuxError::EOPNOTSUPP);
            }
            wire_link::attr::TARGET_NETNSID => return Err(LinuxError::EINVAL),
            wire_link::attr::PERM_ADDRESS => return Err(LinuxError::EINVAL),
            // Linux's non-strict GETLINK parser accepts every other
            // in-range attribute and the lookup ignores it.
            _ => {}
        }
        let consumed = align(len);
        if consumed > remaining.len() {
            break;
        }
        remaining = &remaining[consumed..];
    }
    Ok(parsed)
}

fn parse_link_query_name(payload: &[u8], max_len: usize) -> Result<String, LinuxError> {
    if payload.is_empty() {
        return Err(LinuxError::EINVAL);
    }
    let len_without_terminator = payload.len() - usize::from(payload.ends_with(&[0]));
    if len_without_terminator > max_len {
        return Err(LinuxError::EINVAL);
    }
    parse_string(payload)
}

/// Parses an `IFA_LABEL` using `NLA_STRING` semantics.
///
/// Validation requires at least one byte and limits the string length to
/// `IFNAMSIZ - 1`; bytes after the first NUL are ignored by `nla_strscpy`.
fn parse_addr_label(payload: &[u8]) -> Result<[u8; 16], LinuxError> {
    if payload.is_empty() {
        return Err(LinuxError::ERANGE);
    }
    // `NLA_STRING` policy validation removes at most one trailing NUL before
    // enforcing the configured maximum.  A payload with an earlier NUL is
    // still rejected when its raw length exceeds the limit; `nla_strscpy`
    // only truncates after policy validation has succeeded.
    let policy_len = payload.len() - usize::from(payload.ends_with(&[0]));
    if policy_len > 15 {
        return Err(LinuxError::ERANGE);
    }
    let string_len = payload
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(policy_len);
    let mut label = [0; 16];
    label[..string_len].copy_from_slice(&payload[..string_len]);
    Ok(label)
}

fn parse_addr_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<AddrAttrs, LinuxError> {
    let mut parsed = AddrAttrs::default();
    for attr in attrs {
        match attr.kind & wire_route::NLA_TYPE_MASK {
            wire_addr::attr::LOCAL => {
                parsed.local = Some(parse_ip_prefix_by_family(family, attr.payload)?)
            }
            wire_addr::attr::ADDRESS => {
                parsed.address = Some(parse_ip_prefix_by_family(family, attr.payload)?)
            }
            wire_addr::attr::LABEL => {
                // Linux `ifa_ipv4_policy[IFA_LABEL]` is an `NLA_STRING`
                // limited to `IFNAMSIZ - 1` bytes.  Labels are raw bytes, not
                // necessarily UTF-8, so they stay fixed-size all the way to
                // notification construction.
                parsed.label = Some(parse_addr_label(attr.payload)?);
            }
            wire_addr::attr::BROADCAST => {
                parsed.broadcast = Some(parse_ip_prefix_by_family(family, attr.payload)?)
            }
            wire_addr::attr::FLAGS => parsed.flags = Some(read_u32_prefix(attr.payload)? as u32),
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_route_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<RouteAttrs, LinuxError> {
    let mut parsed = RouteAttrs::default();
    for attr in attrs {
        match attr.kind & wire_route::NLA_TYPE_MASK {
            wire_route::attr::DST => {
                parsed.dst = Some(parse_ip_prefix_by_family(family, attr.payload)?)
            }
            wire_route::attr::OIF => parsed.oif = read_u32_payload(attr.payload)?,
            wire_route::attr::SRC => {
                parsed.src = Some(parse_ip_prefix_by_family(family, attr.payload)?)
            }
            wire_route::attr::GATEWAY => {
                parsed.gateway = Some(parse_ip_prefix_by_family(family, attr.payload)?)
            }
            wire_route::attr::PREFSRC => {
                parsed.prefsrc = Some(parse_ip_prefix_by_family(family, attr.payload)?)
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

fn require_zero_u32(payload: &[u8]) -> Result<(), LinuxError> {
    if read_u32_payload(payload)? == 0 {
        Ok(())
    } else {
        Err(LinuxError::EOPNOTSUPP)
    }
}

fn require_zero_u8(payload: &[u8]) -> Result<(), LinuxError> {
    if payload.first().is_none_or(|value| *value == 0) {
        Ok(())
    } else {
        Err(LinuxError::EOPNOTSUPP)
    }
}

const NEIGH_ATTR_TYPE_MASK: u16 = 0x3fff;
/// `NLA_F_NESTED`, checked only for attributes covered by a strict-start
/// policy entry.
const NLA_F_NESTED: u16 = 0x8000;
/// `NTF_EXT_MASK`, the only bits accepted by `NDA_FLAGS_EXT`.
const NTF_EXT_MASK: u32 = 1;

fn neigh_attr_kind(attr: &NlAttr<'_>) -> u16 {
    attr.kind & NEIGH_ATTR_TYPE_MASK
}

fn parse_neigh_attrs_at_strict(payload: &[u8]) -> Result<Vec<NlAttr<'_>>, LinuxError> {
    let attrs_payload = payload.get(NdMsg::SIZE..).unwrap_or(&[]);
    let attrs = parse_attrs(attrs_payload)?;
    // `__nla_validate_parse` reports positive leftover bytes only. An
    // unaligned final attribute consumes past the buffer, leaving a negative
    // remainder, and is accepted without a trailing-data error.
    let mut remaining = attrs_payload.len() as isize;
    for attr in &attrs {
        remaining -= align(attr.payload.len() + 4) as isize;
    }
    if remaining > 0 {
        return Err(LinuxError::EINVAL);
    }
    Ok(attrs)
}

/// Parses attributes with the liberal structural rules of `nlmsg_parse`.
///
/// A truncated or malformed attribute ends iteration without failing the
/// request, so filters parsed before it remain effective. Policy violations
/// are reported separately because they abort validation before any filter is
/// applied.
fn parse_neigh_attrs_at_liberal(payload: &[u8]) -> Vec<NlAttr<'_>> {
    let mut attrs_payload = payload.get(NdMsg::SIZE..).unwrap_or(&[]);
    let mut attrs = Vec::new();
    while attrs_payload.len() >= 4 {
        let Some(len) = read_u16_ne(attrs_payload, 0) else {
            break;
        };
        let Some(kind) = read_u16_ne(attrs_payload, 2) else {
            break;
        };
        let len = usize::from(len);
        if len < 4 || len > attrs_payload.len() {
            break;
        }
        attrs.push(NlAttr {
            kind,
            payload: &attrs_payload[4..len],
        });
        let consumed = align(len);
        if consumed > attrs_payload.len() {
            break;
        }
        attrs_payload = &attrs_payload[consumed..];
    }
    attrs
}

/// Applies `nda_policy` to one structurally valid attribute.
///
/// The base policy rules are identical for liberal `neigh_add` parsing and
/// strict GETNEIGH parsing: minimum lengths and `NLA_BINARY` maximums are
/// enforced, but fixed-width attributes may still be longer than their
/// nominal size because `NL_VALIDATE_STRICT_ATTRS` is not set. Attributes at
/// or above `strict_start_type` additionally use `NL_VALIDATE_STRICT`.
fn is_valid_neigh_attr(attr: &NlAttr<'_>) -> bool {
    let len = attr.payload.len();
    let nested = attr.kind & NLA_F_NESTED != 0;
    match neigh_attr_kind(attr) {
        wire_neigh::attr::DST | wire_neigh::attr::LLADDR => len <= 32,
        wire_neigh::attr::CACHEINFO => len >= 8,
        wire_neigh::attr::PROBES
        | wire_neigh::attr::VNI
        | wire_neigh::attr::IFINDEX
        | wire_neigh::attr::MASTER => len >= 4,
        wire_neigh::attr::VLAN | wire_neigh::attr::PORT => len >= 2,
        wire_neigh::attr::PROTOCOL => !attr.payload.is_empty(),
        // `nda_policy[NDA_UNSPEC].strict_start_type = NDA_NH_ID` makes every
        // attribute from `NDA_NH_ID` onwards use `NL_VALIDATE_STRICT`.
        wire_neigh::attr::NH_ID => len == 4 && !nested,
        wire_neigh::attr::FDB_EXT_ATTRS => nested && (len == 0 || len >= 4),
        wire_neigh::attr::FLAGS_EXT => {
            len == 4
                && !nested
                && read_u32_ne(attr.payload, 0).is_some_and(|value| value & !NTF_EXT_MASK == 0)
        }
        // These policy entries are NLA_UNSPEC, which strict validation rejects.
        wire_neigh::attr::NDM_STATE_MASK | wire_neigh::attr::NDM_FLAGS_MASK => false,
        _ => true,
    }
}

/// Parses the strict GETNEIGH form documented by `neigh_valid_get_req`.
///
/// A get request is always validated strictly, irrespective of the socket's
/// `NETLINK_GET_STRICT_CHK` setting: Linux only makes dump validation
/// conditional on that option.
fn parse_neigh_get_request(payload: &[u8]) -> Result<NeighGetRequest<'_>, LinuxError> {
    let header = NdMsg::read(payload)?;
    if header.pad1 != 0 || header.pad2 != 0 || header.state != 0 || header.neigh_type != 0 {
        return Err(LinuxError::EINVAL);
    }
    if header.flags & !wire_neigh::NTF_PROXY != 0 {
        return Err(LinuxError::EINVAL);
    }
    // `neigh_valid_get_req()` checks this before attribute parsing: a
    // non-proxy request without a device is invalid even when its attributes
    // would themselves fail validation.
    if header.flags & wire_neigh::NTF_PROXY == 0 && header.ifindex == 0 {
        return Err(LinuxError::EINVAL);
    }

    let attrs = parse_neigh_attrs_at_strict(payload)?;
    // `nlmsg_parse_deprecated_strict` enables `NL_VALIDATE_MAXTYPE` and runs
    // the attribute policy before `neigh_find_table`.
    for attr in &attrs {
        let kind = neigh_attr_kind(attr);
        // `NL_VALIDATE_MAXTYPE` rejects both the unspecified type zero and
        // every type above the policy maximum.
        if kind == 0 || kind > wire_neigh::attr::MAX || !is_valid_neigh_attr(attr) {
            return Err(LinuxError::EINVAL);
        }
    }

    let mut dst = None;
    for attr in attrs {
        match neigh_attr_kind(&attr) {
            wire_neigh::attr::DST => dst = Some(attr.payload),
            _ => return Err(LinuxError::EINVAL),
        }
    }
    // `neigh_valid_get_req()` requires NDA_DST before resolving the family or
    // device.  The address-family key length is checked later, after the
    // family's neighbor table has been selected.
    let dst = dst.ok_or(LinuxError::EINVAL)?;
    Ok(NeighGetRequest {
        family: header.family,
        ifindex: header.ifindex,
        flags: header.flags,
        dst,
    })
}

/// Parses a neighbor dump without requiring strict-check validation.
///
/// `NDA_IFINDEX` and `NDA_MASTER` are the only filters Linux applies in
/// liberal mode; all other attributes are ignored. A structural parse failure
/// ends attribute iteration at that point, while a policy failure makes the
/// caller fall back to an empty filter.
fn parse_neigh_dump_request(payload: &[u8]) -> Result<NeighDumpFilter, LinuxError> {
    let family = payload.first().copied().unwrap_or(0);
    let proxy = NdMsg::read(payload).is_ok_and(|header| header.flags == wire_neigh::NTF_PROXY);
    let mut filter = NeighDumpFilter {
        family,
        proxy,
        ..NeighDumpFilter::default()
    };
    for attr in parse_neigh_attrs_at_liberal(payload) {
        let kind = neigh_attr_kind(&attr);
        if kind == 0 || kind > wire_neigh::attr::MAX {
            continue;
        }
        if !is_valid_neigh_attr(&attr) {
            return Err(LinuxError::EINVAL);
        }
        match kind {
            wire_neigh::attr::IFINDEX => filter.ifindex = read_u32_prefix(attr.payload)?,
            wire_neigh::attr::MASTER => filter.master = read_u32_prefix(attr.payload)?,
            _ => {}
        }
    }
    Ok(filter)
}

/// Reads the first four bytes of an attribute.
///
/// `NLA_U32` validation requires at least four bytes but does not reject
/// longer attributes, matching `nla_get_u32()`'s prefix read.
fn read_u32_prefix(payload: &[u8]) -> Result<i32, LinuxError> {
    let bytes: [u8; 4] = payload
        .get(..4)
        .ok_or(LinuxError::ERANGE)?
        .try_into()
        .map_err(|_| LinuxError::ERANGE)?;
    Ok(i32::from_ne_bytes(bytes))
}

fn get_neigh(
    request: &[u8],
    header: &NlMsgHeader,
    payload: &[u8],
    requesting_port: u32,
) -> Vec<NetlinkPacket> {
    let result = parse_neigh_get_request(payload).and_then(|query| {
        // This stack registers only the IPv4 neighbor table.  Linux selects
        // the table first, so an unsupported family wins over a malformed
        // address length.
        if query.family != wire_route::FAMILY_IPV4 {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        // `tbl->key_len` for `arp_tbl` is four bytes.
        let dst = <[u8; 4]>::try_from(query.dst).map_err(|_| LinuxError::EINVAL)?;
        let dst = KernelIpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3]));
        if query.ifindex != 0 {
            if !SERVICE.is_inited() {
                return Err(LinuxError::ENODEV);
            }
            SERVICE
                .link_snapshot_for_ifindex(query.ifindex)
                .ok_or(LinuxError::ENODEV)?;
        }
        if query.flags & wire_neigh::NTF_PROXY != 0 {
            return Err(LinuxError::ENOENT);
        }
        let dev = NetDeviceId::new(query.ifindex).ok_or(LinuxError::ENODEV)?;
        let neighbor = SERVICE
            .neighbor_snapshot(dev, dst)
            .ok_or(LinuxError::ENOENT)?;
        Ok(NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_neigh_message(RTM_NEWNEIGH, header.seq, 0, requesting_port, neighbor),
            credentials: None,
        })
    });
    match result {
        Ok(packet) => {
            let mut packets = vec![packet];
            packets.extend(ack_packets(request, header));
            packets
        }
        Err(errno) => error_packets(request, errno),
    }
}

fn dump_neigh(header: &NlMsgHeader, payload: &[u8], requesting_port: u32) -> Vec<NetlinkPacket> {
    // Liberal dump parsing reports malformed attributes but continues with an
    // empty filter.  The family and proxy mode are read before attribute
    // parsing, so a malformed attribute list must not discard them.
    let family = payload.first().copied().unwrap_or(0);
    let proxy = NdMsg::read(payload).is_ok_and(|msg| msg.flags == wire_neigh::NTF_PROXY);
    let mut filter = parse_neigh_dump_request(payload).unwrap_or(NeighDumpFilter {
        family,
        proxy,
        ..NeighDumpFilter::default()
    });
    filter.family = family;
    filter.proxy = proxy;
    if filter.proxy
        || !matches!(filter.family, 0 | wire_route::FAMILY_IPV4)
        // The stack has no master/upper-device relationships.  A positive
        // master filter can therefore never match; `-1` explicitly selects
        // devices without a master and matches every device.
        || !matches!(filter.master, 0 | -1)
        || !SERVICE.is_inited()
    {
        return Vec::new();
    }
    let mut flags = NLM_F_MULTI;
    if filter.ifindex != 0 || filter.master != 0 {
        flags |= NLM_F_DUMP_FILTERED;
    }
    SERVICE
        .neighbor_snapshots()
        .into_iter()
        .filter(|neighbor| filter.ifindex == 0 || neighbor.dev.ifindex() == filter.ifindex)
        .map(|neighbor| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_neigh_message(RTM_NEWNEIGH, header.seq, flags, requesting_port, neighbor),
            credentials: None,
        })
        .collect()
}

fn build_link_message(
    msg_type: u16,
    seq: u32,
    flags: u16,
    link: LinkSnapshot,
    change: u32,
) -> Vec<u8> {
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
        change,
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

    build_nlmsg(msg_type, seq, flags, payload)
}

fn build_addr_message(
    msg_type: u16,
    seq: u32,
    flags: u16,
    dev: NetDeviceId,
    addr: Ipv4AddrEntry,
    label: &[u8],
    secondary: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    let ifa_flags = u32::from(wire_addr::FLAG_PERMANENT)
        | if addr.no_prefix_route {
            wire_addr::FLAG_NOPREFIXROUTE
        } else {
            0
        }
        | if secondary {
            wire_addr::FLAG_SECONDARY
        } else {
            0
        };
    IfAddrMsg {
        family: wire_route::FAMILY_IPV4,
        prefix_len: addr.addr.prefix_len(),
        flags: (ifa_flags & u32::from(u8::MAX)) as u8,
        scope: addr.scope,
        index: dev.ifindex() as u32,
    }
    .write(&mut payload);

    let ip_bytes = ip_addr_bytes(IpAddress::Ipv4(addr.addr.address().into()));
    push_attr(&mut payload, wire_addr::attr::ADDRESS, &ip_bytes);
    push_attr(&mut payload, wire_addr::attr::LOCAL, &ip_bytes);
    if let Some(broadcast) = addr.broadcast {
        push_attr(
            &mut payload,
            wire_addr::attr::BROADCAST,
            &ip_addr_bytes(IpAddress::Ipv4(broadcast.into())),
        );
    }
    // `inet_fill_ifaddr()` emits `IFA_LABEL` only when `ifa_label[0]` is
    // nonzero.  Otherwise the attribute is omitted entirely; it is not an
    // empty NUL-terminated string.
    if label.first().is_some_and(|byte| *byte != 0) {
        let label_len = label
            .iter()
            .position(|byte| *byte == 0)
            .map_or(label.len(), |nul| nul + 1);
        push_attr(&mut payload, wire_addr::attr::LABEL, &label[..label_len]);
    }
    push_attr(
        &mut payload,
        wire_addr::attr::FLAGS,
        &ifa_flags.to_ne_bytes(),
    );

    build_nlmsg(msg_type, seq, flags, payload)
}

fn build_route_message(msg_type: u16, seq: u32, flags: u16, route: Rule) -> Vec<u8> {
    let mut payload = Vec::new();
    let family = route_family(route);
    let dst = route_dst(route);
    let (table, route_type, dev) = route.notification_route();
    RtMsg {
        family,
        dst_len: route.filter.prefix_len(),
        src_len: 0,
        tos: 0,
        table,
        protocol: route.protocol,
        scope: route.scope,
        route_type,
        flags: 0,
    }
    .write(&mut payload);

    // `fib_dump_info` always emits `RTA_TABLE`, even for the main and local
    // tables.  Route dumps and route notifications both go through it, so
    // the compact `rtm_table` byte is never the only table identifier.
    push_attr(
        &mut payload,
        wire_route::attr::TABLE,
        &u32::from(table).to_ne_bytes(),
    );
    if let Some(dst) = dst {
        let dst = ip_addr_bytes(dst);
        push_attr(&mut payload, wire_route::attr::DST, &dst);
    }
    push_attr(
        &mut payload,
        wire_route::attr::OIF,
        &dev.ifindex().to_ne_bytes(),
    );
    if let Some(gateway) = route.via.map(to_smoltcp_ip) {
        let gateway = ip_addr_bytes(gateway);
        push_attr(&mut payload, wire_route::attr::GATEWAY, &gateway);
    }
    if let Some(prefsrc) = route.preferred_source().map(to_smoltcp_ip) {
        let prefsrc = ip_addr_bytes(prefsrc);
        push_attr(&mut payload, wire_route::attr::PREFSRC, &prefsrc);
    }

    build_nlmsg(msg_type, seq, flags, payload)
}

fn build_neigh_message(
    msg_type: u16,
    seq: u32,
    flags: u16,
    portid: u32,
    neighbor: NeighborSnapshot,
) -> Vec<u8> {
    let state = match neighbor.state {
        NeighborState::Incomplete => wire_neigh::STATE_INCOMPLETE,
        NeighborState::Reachable { .. } => wire_neigh::STATE_REACHABLE,
        NeighborState::Stale { .. } => wire_neigh::STATE_STALE,
        NeighborState::Failed => wire_neigh::STATE_FAILED,
        NeighborState::Permanent { .. } => wire_neigh::STATE_PERMANENT,
    };
    let mut payload = Vec::new();
    NdMsg {
        family: wire_route::FAMILY_IPV4,
        pad1: 0,
        pad2: 0,
        ifindex: neighbor.dev.ifindex(),
        state,
        flags: 0,
        // IPv4 ARP neighbors are unicast routes.  Linux stores
        // `inet_addr_type_dev_table()` here, which is RTN_UNICAST for a normal
        // resolved address.
        neigh_type: RTN_UNICAST,
    }
    .write(&mut payload);
    let dst = ip_addr_bytes(to_smoltcp_ip(neighbor.dst));
    push_attr(&mut payload, wire_neigh::attr::DST, &dst);
    if let Some(lladdr) = neighbor.state.hardware_addr() {
        push_attr(&mut payload, wire_neigh::attr::LLADDR, &lladdr);
    }
    build_nlmsg_for_port(msg_type, seq, flags, portid, payload)
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
            credentials: None,
        }]
    }
}

fn error_packets(request: &[u8], errno: LinuxError) -> Vec<NetlinkPacket> {
    vec![NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: build_error_response(request, errno),
        credentials: None,
    }]
}
