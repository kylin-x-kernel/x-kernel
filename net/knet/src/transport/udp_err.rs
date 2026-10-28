// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP asynchronous error queue support.

use ::core::net::{SocketAddr, SocketAddrV4};
use etherparse::{Icmpv4Slice, Icmpv4Type, UdpHeaderSlice};
use kerrno::LinuxError;

use super::udp::{self, UdpSocketQueuedError};
use crate::{
    SocketErrorInfo, SocketErrorOrigin,
    buf::PacketBuf,
    ipv4::{self, Ipv4Header},
};

const ICMPV4_DST_UNREACHABLE: u8 = 3;
const ICMPV4_TIME_EXCEEDED: u8 = 11;
#[cfg(unittest)]
const ICMPV4_ECHO_REQUEST: u8 = 8;
const ICMPV4_CODE_NET_UNREACHABLE: u8 = 0;
const ICMPV4_CODE_HOST_UNREACHABLE: u8 = 1;
const ICMPV4_CODE_PROTO_UNREACHABLE: u8 = 2;
const ICMPV4_CODE_PORT_UNREACHABLE: u8 = 3;
const ICMPV4_CODE_FRAG_REQUIRED: u8 = 4;
const ICMPV4_CODE_SR_FAILED: u8 = 5;
const ICMPV4_CODE_DST_NET_UNKNOWN: u8 = 6;
const ICMPV4_CODE_DST_HOST_UNKNOWN: u8 = 7;
const ICMPV4_CODE_SRC_HOST_ISOLATED: u8 = 8;
const ICMPV4_CODE_NET_PROHIBITED: u8 = 9;
const ICMPV4_CODE_HOST_PROHIBITED: u8 = 10;
const ICMPV4_CODE_NET_UNREACH_TO_S: u8 = 11;
const ICMPV4_CODE_HOST_UNREACH_TO_S: u8 = 12;
const ICMPV4_CODE_COMM_PROHIBITED: u8 = 13;
const ICMPV4_CODE_HOST_PRECEDENCE: u8 = 14;
const ICMPV4_CODE_PRECEDENCE_CUTOFF: u8 = 15;
const ICMPV4_CODE_TTL_EXPIRED: u8 = 0;
const ICMPV4_CODE_FRAG_EXPIRED: u8 = 1;

pub(crate) type QueuedUdpError = UdpSocketQueuedError;

fn queue_ipv4_error(
    local: SocketAddrV4,
    remote: SocketAddrV4,
    ingress_ifindex: i32,
    error: QueuedUdpError,
) {
    if let Some(state) = udp::lookup_udp_error_state(local.into(), remote.into(), ingress_ifindex) {
        if state.peer_endpoint().is_some() {
            state.record_socket_error(error.ancillary.errno);
        }
        state.enqueue_error(error);
    }
}

fn icmpv4_errno(icmp: &Icmpv4Slice<'_>) -> Option<(LinuxError, u32)> {
    let error_type = icmp.type_u8();
    let error_code = icmp.code_u8();
    match error_type {
        ICMPV4_DST_UNREACHABLE => {
            let result = match error_code {
                ICMPV4_CODE_NET_UNREACHABLE => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_HOST_UNREACHABLE => (LinuxError::EHOSTUNREACH, 0),
                ICMPV4_CODE_PROTO_UNREACHABLE => (LinuxError::ENOPROTOOPT, 0),
                ICMPV4_CODE_PORT_UNREACHABLE => (LinuxError::ECONNREFUSED, 0),
                ICMPV4_CODE_FRAG_REQUIRED => {
                    let mtu = match icmp.icmp_type() {
                        Icmpv4Type::DestinationUnreachable(
                            etherparse::icmpv4::DestUnreachableHeader::FragmentationNeeded {
                                next_hop_mtu,
                            },
                        ) => next_hop_mtu as u32,
                        _ => 0,
                    };
                    (LinuxError::EMSGSIZE, mtu)
                }
                ICMPV4_CODE_SR_FAILED => (LinuxError::EOPNOTSUPP, 0),
                ICMPV4_CODE_DST_NET_UNKNOWN => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_DST_HOST_UNKNOWN => (LinuxError::EHOSTDOWN, 0),
                ICMPV4_CODE_SRC_HOST_ISOLATED => (LinuxError::ENONET, 0),
                ICMPV4_CODE_NET_PROHIBITED => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_HOST_PROHIBITED => (LinuxError::EHOSTUNREACH, 0),
                ICMPV4_CODE_NET_UNREACH_TO_S => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_HOST_UNREACH_TO_S => (LinuxError::EHOSTUNREACH, 0),
                ICMPV4_CODE_COMM_PROHIBITED
                | ICMPV4_CODE_HOST_PRECEDENCE
                | ICMPV4_CODE_PRECEDENCE_CUTOFF => (LinuxError::EHOSTUNREACH, 0),
                _ => (LinuxError::EHOSTUNREACH, 0),
            };
            Some(result)
        }
        ICMPV4_TIME_EXCEEDED if error_code == ICMPV4_CODE_TTL_EXPIRED => {
            Some((LinuxError::EHOSTUNREACH, 0))
        }
        ICMPV4_TIME_EXCEEDED if error_code == ICMPV4_CODE_FRAG_EXPIRED => None,
        ICMPV4_TIME_EXCEEDED => Some((LinuxError::EPROTO, 0)),
        _ => None,
    }
}

/// Inspect an IPv4 ICMP error and queue it for the affected UDP socket.
///
/// ICMP errors quote the packet that triggered the error, so this path parses
/// the ICMP and quoted IPv4/UDP headers after IPv4 input normalization. The
/// quoted UDP 4-tuple and `ingress_ifindex` determine the receiving socket: a
/// connected socket that matches the peer is preferred, and an unconnected
/// socket bound to the local endpoint is used as a fallback.
///
/// This mirrors Linux's UDP ICMP error lookup in
/// `net/ipv4/udp.c::__udp4_lib_err()`.
pub(crate) fn inspect_icmpv4_error(packet: &PacketBuf) {
    let ip_packet = match packet
        .network_packet()
        .and_then(|bytes| Ipv4Header::parse_header_only(bytes).ok())
    {
        Some(header) if header.protocol() == ipv4::PROTOCOL_ICMP => header,
        _ => return,
    };
    let ingress_ifindex = packet.ifindex();
    let icmp_packet = match packet.transport_packet() {
        Some(payload) => payload,
        _ => return,
    };
    let icmp = match Icmpv4Slice::from_slice(icmp_packet) {
        Ok(icmp) => icmp,
        Err(_) => return,
    };
    if icmp.icmp_type().calc_checksum(icmp.payload()) != icmp.checksum() {
        return;
    }
    let error_type = icmp.type_u8();
    let error_code = icmp.code_u8();
    let Some((errno, info)) = icmpv4_errno(&icmp) else {
        return;
    };

    let original_packet = icmp.payload();
    let original_ip = match Ipv4Header::parse_header_only(original_packet) {
        Ok(header) => header,
        Err(_) => return,
    };
    // ICMP quotes may omit the original payload, but must retain a valid header.
    if original_ip.total_len() < original_ip.header_len()
        || original_ip.check_checksum().is_err()
        || original_ip.protocol() != ipv4::PROTOCOL_UDP
    {
        return;
    }
    let original_udp = match original_packet.get(original_ip.header_len()..) {
        Some(payload) => payload,
        _ => return,
    };
    let udp_header = match UdpHeaderSlice::from_slice(original_udp) {
        Ok(header) => header,
        _ => return,
    };

    let local = SocketAddrV4::new(original_ip.src_addr().into(), udp_header.source_port());
    let remote = SocketAddrV4::new(original_ip.dst_addr().into(), udp_header.destination_port());
    // ICMP has no transport-layer port, so Linux reports the offender as the
    // ICMP source address with `sin_port` set to zero.
    let offender = SocketAddr::V4(SocketAddrV4::new(ip_packet.src_addr().into(), 0));
    let error = QueuedUdpError {
        payload: original_udp[udp_header.slice().len()..].to_vec(),
        addr: SocketAddr::V4(remote),
        ancillary: SocketErrorInfo {
            errno,
            origin: SocketErrorOrigin::Icmp,
            error_type,
            error_code,
            info,
            data: 0,
            offender: Some(offender),
        },
    };
    queue_ipv4_error(local, remote, ingress_ifindex, error);
}

unittest::path_mod_test!(tests, "../../ut-tests/transport/udp_err/tests.rs");
