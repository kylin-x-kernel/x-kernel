// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux netlink ABI carrier structures and byte-level codecs.

use alloc::{string::String, vec::Vec};

use kerrno::LinuxError;
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};

use super::*;

pub(crate) mod link {
    pub(crate) mod attr {
        pub(crate) const ADDRESS: u16 = 1;
        pub(crate) const BROADCAST: u16 = 2;
        pub(crate) const IFNAME: u16 = 3;
        pub(crate) const MTU: u16 = 4;
        pub(crate) const LINK: u16 = 5;
        pub(crate) const OPERSTATE: u16 = 16;
        pub(crate) const EXT_MASK: u16 = 29;
    }
}

pub(crate) mod addr {
    pub(crate) mod attr {
        pub(crate) const ADDRESS: u16 = 1;
        pub(crate) const LOCAL: u16 = 2;
        pub(crate) const LABEL: u16 = 3;
    }
}

pub(crate) mod route {
    pub(crate) const FAMILY_IPV4: u8 = 2;
    pub(crate) const FAMILY_IPV6: u8 = 10;
    pub(crate) const TABLE_MAIN: u8 = 254;
    pub(crate) const PROTOCOL_BOOT: u8 = 3;
    pub(crate) const SCOPE_UNIVERSE: u8 = 0;
    // Reserved for future route deletion / nowhere-scope handling.
    // pub(crate) const SCOPE_NOWHERE: u8 = 255;
    pub(crate) const SCOPE_HOST: u8 = 254;
    pub(crate) const TYPE_UNICAST: u8 = 1;

    pub(crate) mod attr {
        pub(crate) const DST: u16 = 1;
        pub(crate) const SRC: u16 = 2;
        pub(crate) const OIF: u16 = 4;
        pub(crate) const GATEWAY: u16 = 5;
        pub(crate) const PREFSRC: u16 = 7;
        pub(crate) const TABLE: u16 = 15;
    }
}

pub(crate) mod neigh {
    pub(crate) const STATE_PERMANENT: u16 = 0x80;

    pub(crate) mod attr {
        pub(crate) const DST: u16 = 1;
        pub(crate) const LLADDR: u16 = 2;
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
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NlAttr<'a> {
    pub(super) kind: u16,
    pub(super) payload: &'a [u8],
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
    let len = NLMSG_HDR_LEN + payload.len();
    let mut out = Vec::with_capacity(align(len));
    push_u32_ne(&mut out, len as u32);
    push_u16_ne(&mut out, msg_type);
    push_u16_ne(&mut out, flags);
    push_u32_ne(&mut out, seq);
    push_u32_ne(&mut out, 0);
    out.extend_from_slice(&payload);
    while out.len() % NLMSG_ALIGNTO != 0 {
        out.push(0);
    }
    out
}

pub(super) fn build_nlmsg_done_response(request: &NlMsgHeader) -> Vec<u8> {
    request.build_payload_response(NLMSG_DONE, NLM_F_MULTI, &[])
}

pub(super) fn build_nlmsg_error_response(
    request: &NlMsgHeader,
    errno: i32,
    original_request: &[u8],
) -> Vec<u8> {
    let copy_len = request.len as usize;
    let copy_len = copy_len.min(original_request.len());
    let mut payload = Vec::with_capacity(core::mem::size_of::<i32>() + copy_len);
    push_i32_ne(&mut payload, errno);
    payload.extend_from_slice(&original_request[..copy_len]);
    request.build_payload_response(NLMSG_ERROR, 0, &payload)
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

pub(super) fn parse_ip_by_family(family: u8, payload: &[u8]) -> Result<IpAddress, LinuxError> {
    match family {
        route::FAMILY_IPV4 => {
            let bytes: [u8; 4] = payload.try_into().map_err(|_| LinuxError::EINVAL)?;
            Ok(IpAddress::Ipv4(Ipv4Address::from_octets(bytes)))
        }
        route::FAMILY_IPV6 => {
            let bytes: [u8; 16] = payload.try_into().map_err(|_| LinuxError::EINVAL)?;
            Ok(IpAddress::Ipv6(Ipv6Address::from_octets(bytes)))
        }
        _ => Err(LinuxError::EAFNOSUPPORT),
    }
}

pub(super) fn parse_mac(payload: &[u8]) -> Result<[u8; 6], LinuxError> {
    payload.try_into().map_err(|_| LinuxError::EINVAL)
}

pub(super) fn read_u32_payload(payload: &[u8]) -> Result<u32, LinuxError> {
    let bytes: [u8; 4] = payload.try_into().map_err(|_| LinuxError::EINVAL)?;
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

pub(super) fn normalize_route_protocol(protocol: u8) -> u8 {
    if protocol == 0 {
        route::PROTOCOL_BOOT
    } else {
        protocol
    }
}

pub(super) fn normalize_route_type(route_type: u8) -> u8 {
    if route_type == 0 {
        route::TYPE_UNICAST
    } else {
        route_type
    }
}

pub(super) fn normalize_neigh_state(state: u16) -> u16 {
    if state == 0 {
        neigh::STATE_PERMANENT
    } else {
        state
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
