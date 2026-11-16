// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux netlink ABI carrier structures and byte-level codecs.

use alloc::{string::String, vec::Vec};

use kerrno::LinuxError;
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};

use super::*;

pub(crate) mod link {
    pub(crate) const IFNAMSIZ: usize = 16;
    pub(crate) const ALTIFNAMSIZ: usize = 128;

    pub(crate) mod attr {
        pub(crate) const ADDRESS: u16 = 1;
        pub(crate) const BROADCAST: u16 = 2;
        pub(crate) const IFNAME: u16 = 3;
        pub(crate) const MTU: u16 = 4;
        pub(crate) const LINK: u16 = 5;
        pub(crate) const MASTER: u16 = 10;
        pub(crate) const OPERSTATE: u16 = 16;
        pub(crate) const LINKINFO: u16 = 18;
        pub(crate) const NET_NS_PID: u16 = 19;
        pub(crate) const NET_NS_FD: u16 = 28;
        pub(crate) const EXT_MASK: u16 = 29;
        pub(crate) const TARGET_NETNSID: u16 = 46;
        pub(crate) const ALT_IFNAME: u16 = 53;
        pub(crate) const PERM_ADDRESS: u16 = 54;
    }
}

pub(crate) mod addr {
    pub(crate) const FLAG_SECONDARY: u32 = 0x01;
    pub(crate) const FLAG_PERMANENT: u8 = 0x80;
    /// Do not install a connected-prefix route for this address.
    pub(crate) const FLAG_NOPREFIXROUTE: u32 = 0x200;

    pub(crate) mod attr {
        pub(crate) const ADDRESS: u16 = 1;
        pub(crate) const LOCAL: u16 = 2;
        pub(crate) const LABEL: u16 = 3;
        pub(crate) const BROADCAST: u16 = 4;
        pub(crate) const FLAGS: u16 = 8;
    }
}

pub(crate) mod route {
    pub(crate) const FAMILY_IPV4: u8 = 2;
    pub(crate) const FAMILY_IPV6: u8 = 10;
    pub(crate) const RTM_NEWROUTE: u16 = 24;
    pub(crate) const IPPROTO_ICMP: u8 = 1;
    pub(crate) const IPPROTO_TCP: u8 = 6;
    pub(crate) const IPPROTO_UDP: u8 = 17;
    pub(crate) const IPTOS_RT_MASK: u8 = 0x1c;
    /// Mask selecting the attribute type when `NLA_F_NESTED` /
    /// `NLA_F_NET_BYTEORDER` flags are present.
    pub(crate) const NLA_TYPE_MASK: u16 = 0x3fff;
    pub(crate) const TABLE_MAIN: u8 = 254;
    pub(crate) const TABLE_LOCAL: u8 = 255;
    pub(crate) const PROTOCOL_KERNEL: u8 = 2;
    pub(crate) const PROTOCOL_BOOT: u8 = 3;
    pub(crate) const SCOPE_UNIVERSE: u8 = 0;
    pub(crate) const SCOPE_LINK: u8 = 253;
    pub(crate) const SCOPE_HOST: u8 = 254;
    #[cfg(unittest)]
    pub(crate) const SCOPE_NOWHERE: u8 = 255;
    pub(crate) const TYPE_UNICAST: u8 = 1;
    pub(crate) const TYPE_LOCAL: u8 = 2;
    pub(crate) const TYPE_BROADCAST: u8 = 3;
    pub(crate) const TYPE_MULTICAST: u8 = 5;
    /// Highest `RTN_*` type: `RTN_XRESOLVE`.
    pub(crate) const TYPE_MAX: u8 = 11;
    /// Route is dead or has a link-down nexthop and cannot be inserted.
    pub(crate) const RTNH_F_DEAD: u32 = 0x1;
    pub(crate) const RTNH_F_LINKDOWN: u32 = 0x2;
    pub(crate) const FLAG_NOTIFY: u32 = 0x100;
    pub(crate) const FLAG_CLONED: u32 = 0x200;
    pub(crate) const FLAG_LOOKUP_TABLE: u32 = 0x1000;
    pub(crate) const FLAG_FIB_MATCH: u32 = 0x2000;
    /// IPv4 has no prefix routes; a dump carrying this flag returns no routes.
    pub(crate) const FLAG_PREFIX: u32 = 0x800;
    pub(crate) const RTCF_BROADCAST: u32 = 0x1000_0000;
    pub(crate) const RTCF_MULTICAST: u32 = 0x2000_0000;
    pub(crate) const RTCF_LOCAL: u32 = 0x8000_0000;

    pub(crate) mod attr {
        pub(crate) const DST: u16 = 1;
        pub(crate) const SRC: u16 = 2;
        pub(crate) const IIF: u16 = 3;
        pub(crate) const OIF: u16 = 4;
        pub(crate) const GATEWAY: u16 = 5;
        pub(crate) const PRIORITY: u16 = 6;
        pub(crate) const PREFSRC: u16 = 7;
        pub(crate) const METRICS: u16 = 8;
        pub(crate) const MULTIPATH: u16 = 9;
        pub(crate) const FLOW: u16 = 11;
        pub(crate) const CACHEINFO: u16 = 12;
        pub(crate) const TABLE: u16 = 15;
        pub(crate) const MARK: u16 = 16;
        pub(crate) const ENCAP_TYPE: u16 = 21;
        pub(crate) const ENCAP: u16 = 22;
        pub(crate) const UID: u16 = 25;
        pub(crate) const IP_PROTO: u16 = 27;
        pub(crate) const SPORT: u16 = 28;
        pub(crate) const DPORT: u16 = 29;
        pub(crate) const NH_ID: u16 = 30;
    }
}

pub(crate) mod neigh {
    pub(crate) const STATE_INCOMPLETE: u16 = 0x01;
    pub(crate) const STATE_REACHABLE: u16 = 0x02;
    pub(crate) const STATE_STALE: u16 = 0x04;
    pub(crate) const STATE_FAILED: u16 = 0x20;
    pub(crate) const STATE_PERMANENT: u16 = 0x80;

    /// Neighbor entry flag selecting the proxy table.
    pub(crate) const NTF_PROXY: u8 = 0x08;

    pub(crate) mod attr {
        pub(crate) const DST: u16 = 1;
        pub(crate) const LLADDR: u16 = 2;
        pub(crate) const CACHEINFO: u16 = 3;
        pub(crate) const PROBES: u16 = 4;
        pub(crate) const VLAN: u16 = 5;
        pub(crate) const PORT: u16 = 6;
        pub(crate) const VNI: u16 = 7;
        /// Device filter accepted by non-strict neighbor dumps.
        pub(crate) const IFINDEX: u16 = 8;
        /// Upper-device filter accepted by non-strict neighbor dumps.
        pub(crate) const MASTER: u16 = 9;
        pub(crate) const PROTOCOL: u16 = 12;
        pub(crate) const NH_ID: u16 = 13;
        pub(crate) const FDB_EXT_ATTRS: u16 = 14;
        pub(crate) const FLAGS_EXT: u16 = 15;
        /// Mask attribute selecting the state bits to change.
        pub(crate) const NDM_STATE_MASK: u16 = 16;
        /// Mask attribute selecting the flag bits to change.
        pub(crate) const NDM_FLAGS_MASK: u16 = 17;
        /// Largest neighbor attribute type defined by the Linux UAPI.
        pub(crate) const MAX: u16 = 17;
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub(super) struct NlMsgHeader {
    pub(super) len: u32,
    pub(super) msg_type: u16,
    pub(super) flags: u16,
    pub(super) seq: u32,
    pub(super) pid: u32,
}

impl NlMsgHeader {
    pub(super) fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < NLMSG_HDR_LEN {
            return None;
        }
        let header = Self {
            len: read_u32_ne(buf, 0)?,
            msg_type: read_u16_ne(buf, 4)?,
            flags: read_u16_ne(buf, 6)?,
            seq: read_u32_ne(buf, 8)?,
            pid: read_u32_ne(buf, 12)?,
        };
        let len = header.len as usize;
        (NLMSG_HDR_LEN..=buf.len()).contains(&len).then_some(header)
    }
}

impl NlMsgHeader {
    pub(super) fn build_payload_response(
        &self,
        msg_type: u16,
        flags: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let len = NLMSG_HDR_LEN + payload.len();
        let mut out = Vec::with_capacity(align(len));
        push_u32_ne(&mut out, len as u32);
        push_u16_ne(&mut out, msg_type);
        push_u16_ne(&mut out, flags);
        push_u32_ne(&mut out, self.seq);
        push_u32_ne(&mut out, self.pid);
        out.extend_from_slice(payload);
        while out.len() % NLMSG_ALIGNTO != 0 {
            out.push(0);
        }
        out
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub(super) struct IfInfoMsg {
    pub(super) family: u8,
    pub(super) pad: u8,
    pub(super) link_type: u16,
    pub(super) index: i32,
    pub(super) flags: u32,
    pub(super) change: u32,
}

impl IfInfoMsg {
    pub(super) const SIZE: usize = 16;

    pub(super) fn read(payload: &[u8]) -> Result<Self, LinuxError> {
        if payload.len() < Self::SIZE {
            return Err(LinuxError::EINVAL);
        }
        Ok(Self {
            family: payload[0],
            pad: payload[1],
            link_type: read_u16_ne(payload, 2).ok_or(LinuxError::EINVAL)?,
            index: read_i32_ne(payload, 4).ok_or(LinuxError::EINVAL)?,
            flags: read_u32_ne(payload, 8).ok_or(LinuxError::EINVAL)?,
            change: read_u32_ne(payload, 12).ok_or(LinuxError::EINVAL)?,
        })
    }

    pub(super) fn write(&self, buf: &mut Vec<u8>) {
        push_u8(buf, self.family);
        push_u8(buf, self.pad);
        push_u16_ne(buf, self.link_type);
        push_i32_ne(buf, self.index);
        push_u32_ne(buf, self.flags);
        push_u32_ne(buf, self.change);
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub(super) struct IfAddrMsg {
    pub(super) family: u8,
    pub(super) prefix_len: u8,
    pub(super) flags: u8,
    pub(super) scope: u8,
    pub(super) index: u32,
}

impl IfAddrMsg {
    pub(super) const SIZE: usize = 8;

    pub(super) fn read(payload: &[u8]) -> Result<Self, LinuxError> {
        if payload.len() < Self::SIZE {
            return Err(LinuxError::EINVAL);
        }
        Ok(Self {
            family: payload[0],
            prefix_len: payload[1],
            flags: payload[2],
            scope: payload[3],
            index: read_u32_ne(payload, 4).ok_or(LinuxError::EINVAL)?,
        })
    }

    pub(super) fn write(&self, buf: &mut Vec<u8>) {
        push_u8(buf, self.family);
        push_u8(buf, self.prefix_len);
        push_u8(buf, self.flags);
        push_u8(buf, self.scope);
        push_u32_ne(buf, self.index);
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub(super) struct RtMsg {
    pub(super) family: u8,
    pub(super) dst_len: u8,
    pub(super) src_len: u8,
    pub(super) tos: u8,
    pub(super) table: u8,
    pub(super) protocol: u8,
    pub(super) scope: u8,
    pub(super) route_type: u8,
    pub(super) flags: u32,
}

impl RtMsg {
    pub(super) const SIZE: usize = 12;

    pub(super) fn read(payload: &[u8]) -> Result<Self, LinuxError> {
        if payload.len() < Self::SIZE {
            return Err(LinuxError::EINVAL);
        }
        Ok(Self {
            family: payload[0],
            dst_len: payload[1],
            src_len: payload[2],
            tos: payload[3],
            table: payload[4],
            protocol: payload[5],
            scope: payload[6],
            route_type: payload[7],
            flags: read_u32_ne(payload, 8).ok_or(LinuxError::EINVAL)?,
        })
    }

    pub(super) fn write(&self, buf: &mut Vec<u8>) {
        push_u8(buf, self.family);
        push_u8(buf, self.dst_len);
        push_u8(buf, self.src_len);
        push_u8(buf, self.tos);
        push_u8(buf, self.table);
        push_u8(buf, self.protocol);
        push_u8(buf, self.scope);
        push_u8(buf, self.route_type);
        push_u32_ne(buf, self.flags);
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub(super) struct NdMsg {
    pub(super) family: u8,
    pub(super) pad1: u8,
    pub(super) pad2: u16,
    pub(super) ifindex: i32,
    pub(super) state: u16,
    pub(super) flags: u8,
    pub(super) neigh_type: u8,
}

impl NdMsg {
    pub(super) const SIZE: usize = 12;

    pub(super) fn read(payload: &[u8]) -> Result<Self, LinuxError> {
        if payload.len() < Self::SIZE {
            return Err(LinuxError::EINVAL);
        }
        Ok(Self {
            family: payload[0],
            pad1: payload[1],
            pad2: read_u16_ne(payload, 2).ok_or(LinuxError::EINVAL)?,
            ifindex: read_i32_ne(payload, 4).ok_or(LinuxError::EINVAL)?,
            state: read_u16_ne(payload, 8).ok_or(LinuxError::EINVAL)?,
            flags: payload[10],
            neigh_type: payload[11],
        })
    }

    pub(super) fn write(self, buf: &mut Vec<u8>) {
        push_u8(buf, self.family);
        push_u8(buf, self.pad1);
        push_u16_ne(buf, self.pad2);
        push_i32_ne(buf, self.ifindex);
        push_u16_ne(buf, self.state);
        push_u8(buf, self.flags);
        push_u8(buf, self.neigh_type);
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NlAttr<'a> {
    pub(super) kind: u16,
    pub(super) payload: &'a [u8],
}

/// Parses attributes with the liberal structural rules of `nlmsg_parse`.
///
/// A truncated or malformed attribute ends iteration without failing the
/// request, preserving attributes parsed before it.
pub(super) fn parse_attrs_lossy(mut payload: &[u8]) -> Vec<NlAttr<'_>> {
    let mut attrs = Vec::new();
    while payload.len() >= 4 {
        let Some(len) = read_u16_ne(payload, 0) else {
            break;
        };
        let Some(kind) = read_u16_ne(payload, 2) else {
            break;
        };
        let len = usize::from(len);
        if len < 4 || len > payload.len() {
            break;
        }
        attrs.push(NlAttr {
            kind,
            payload: &payload[4..len],
        });
        let consumed = align(len);
        if consumed > payload.len() {
            break;
        }
        payload = &payload[consumed..];
    }
    attrs
}

pub(super) fn parse_attrs(mut payload: &[u8]) -> Result<Vec<NlAttr<'_>>, LinuxError> {
    let mut attrs = Vec::new();
    while payload.len() >= 4 {
        let len = read_u16_ne(payload, 0).ok_or(LinuxError::EINVAL)? as usize;
        let kind = read_u16_ne(payload, 2).ok_or(LinuxError::EINVAL)?;
        if len < 4 || len > payload.len() {
            return Err(LinuxError::EINVAL);
        }
        attrs.push(NlAttr {
            kind,
            payload: &payload[4..len],
        });
        let consumed = align(len);
        if consumed > payload.len() {
            break;
        }
        payload = &payload[consumed..];
    }
    Ok(attrs)
}

pub(super) fn build_nlmsg(msg_type: u16, seq: u32, flags: u16, payload: Vec<u8>) -> Vec<u8> {
    build_nlmsg_for_port(msg_type, seq, flags, 0, payload)
}

/// Builds a reply whose `nlmsg_pid` names the socket that issued the request.
pub(super) fn build_nlmsg_for_port(
    msg_type: u16,
    seq: u32,
    flags: u16,
    portid: u32,
    payload: Vec<u8>,
) -> Vec<u8> {
    let len = NLMSG_HDR_LEN + payload.len();
    let mut out = Vec::with_capacity(align(len));
    push_u32_ne(&mut out, len as u32);
    push_u16_ne(&mut out, msg_type);
    push_u16_ne(&mut out, flags);
    push_u32_ne(&mut out, seq);
    push_u32_ne(&mut out, portid);
    out.extend_from_slice(&payload);
    while out.len() % NLMSG_ALIGNTO != 0 {
        out.push(0);
    }
    out
}

// NLMSG_DONE carries a zero status when the dump finishes successfully.
pub(super) fn build_nlmsg_done_response(request: &NlMsgHeader) -> Vec<u8> {
    request.build_payload_response(NLMSG_DONE, NLM_F_MULTI, &0i32.to_ne_bytes())
}

pub(super) fn build_nlmsg_error_response(
    request: &NlMsgHeader,
    errno: i32,
    original_request: &[u8],
) -> Vec<u8> {
    build_nlmsg_error_response_with_cap(request, errno, original_request, false)
}

/// Builds an `NLMSG_ERROR` response with Linux's `NETLINK_CAP_ACK` behavior.
///
/// A successful acknowledgement never carries the request payload and is
/// marked `NLM_F_CAPPED`.  An error normally echoes the original message, but
/// a socket that enabled `NETLINK_CAP_ACK` asks the kernel to omit that echo;
/// the fixed `nlmsgerr.msg` header is always present in either case.
pub(super) fn build_nlmsg_error_response_with_cap(
    request: &NlMsgHeader,
    errno: i32,
    original_request: &[u8],
    cap_ack: bool,
) -> Vec<u8> {
    let capped = errno == 0 || cap_ack;
    let max_len = request.len as usize;
    let max_len = max_len.min(original_request.len());
    let copy_len = if capped {
        max_len.min(NLMSG_HDR_LEN)
    } else {
        max_len
    };
    let mut payload = Vec::with_capacity(core::mem::size_of::<i32>() + copy_len);
    push_i32_ne(&mut payload, errno);
    payload.extend_from_slice(&original_request[..copy_len]);
    let flags = if capped { NLM_F_CAPPED } else { 0 };
    request.build_payload_response(NLMSG_ERROR, flags, &payload)
}

pub(super) fn push_attr(buf: &mut Vec<u8>, attr_type: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    push_u16_ne(buf, len as u16);
    push_u16_ne(buf, attr_type);
    buf.extend_from_slice(payload);
    while !buf.len().is_multiple_of(NLMSG_ALIGNTO) {
        buf.push(0);
    }
}

pub(super) fn push_attr_str(buf: &mut Vec<u8>, attr_type: u16, payload: &str) {
    let mut string = payload.as_bytes().to_vec();
    string.push(0);
    push_attr(buf, attr_type, &string);
}

pub(super) fn parse_string(payload: &[u8]) -> Result<String, LinuxError> {
    let end = payload
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(payload.len());
    let string = core::str::from_utf8(&payload[..end]).map_err(|_| LinuxError::EINVAL)?;
    Ok(String::from(string))
}

/// Parses the key-length prefix of an address attribute.
///
/// Linux attributes such as `NDA_DST` are typed as binary blobs: only the
/// address-family key length is required, and any trailing bytes are ignored.
pub(super) fn parse_ip_prefix_by_family(
    family: u8,
    payload: &[u8],
) -> Result<IpAddress, LinuxError> {
    match family {
        route::FAMILY_IPV4 => {
            let bytes: [u8; 4] = payload
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(LinuxError::ERANGE)?;
            Ok(IpAddress::Ipv4(Ipv4Address::from_octets(bytes)))
        }
        route::FAMILY_IPV6 => {
            let bytes: [u8; 16] = payload
                .get(..16)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(LinuxError::ERANGE)?;
            Ok(IpAddress::Ipv6(Ipv6Address::from_octets(bytes)))
        }
        _ => Err(LinuxError::EAFNOSUPPORT),
    }
}

/// Parses the leading Ethernet address from a longer link-layer attribute.
pub(super) fn parse_mac_prefix(payload: &[u8]) -> Result<[u8; 6], LinuxError> {
    payload
        .get(..6)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(LinuxError::EINVAL)
}

pub(super) fn read_u32_payload(payload: &[u8]) -> Result<u32, LinuxError> {
    // `NLA_U32` validation checks a minimum length; `nla_get_u32` reads the
    // first four bytes and ignores any trailing payload.
    let bytes: [u8; 4] = payload
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(LinuxError::ERANGE)?;
    Ok(u32::from_ne_bytes(bytes))
}

pub(super) fn ip_addr_bytes(addr: IpAddress) -> Vec<u8> {
    match addr {
        IpAddress::Ipv4(addr) => addr.octets().to_vec(),
        IpAddress::Ipv6(addr) => addr.octets().to_vec(),
    }
}

pub(super) fn align(len: usize) -> usize {
    (len + NLMSG_ALIGNTO - 1) & !(NLMSG_ALIGNTO - 1)
}

pub(super) fn normalize_route_table(table: u8) -> u8 {
    if table == 0 { route::TABLE_MAIN } else { table }
}

pub(super) fn normalize_route_type(route_type: u8) -> u8 {
    if route_type == 0 {
        route::TYPE_UNICAST
    } else {
        route_type
    }
}

pub(super) fn push_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

pub(super) fn read_u16_ne(buf: &[u8], offset: usize) -> Option<u16> {
    let bytes = buf.get(offset..offset + 2)?;
    Some(u16::from_ne_bytes([bytes[0], bytes[1]]))
}

pub(super) fn read_u32_ne(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes = buf.get(offset..offset + 4)?;
    Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

pub(super) fn read_i32_ne(buf: &[u8], offset: usize) -> Option<i32> {
    let bytes = buf.get(offset..offset + 4)?;
    Some(i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

pub(super) fn push_u16_ne(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_ne_bytes());
}

pub(super) fn push_u32_ne(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_ne_bytes());
}

pub(super) fn push_i32_ne(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_ne_bytes());
}
