// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP asynchronous error queue support.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    net::{SocketAddr, SocketAddrV4},
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
};

use kerrno::LinuxError;
use kpoll::PollSet;
use ksync::{Mutex, RwLock};
use lazyinit::LazyInit;
use smoltcp::{
    iface::SocketHandle,
    wire::{
        Icmpv4DstUnreachable, Icmpv4Message, Icmpv4Packet, Icmpv4TimeExceeded, IpAddress,
        IpEndpoint, IpProtocol, Ipv4Packet, UdpPacket,
    },
};

use crate::{SocketErrorInfo, SocketErrorOrigin};

static UDP_REGISTRY: LazyInit<RwLock<Vec<Arc<UdpErrorState>>>> = LazyInit::new();

pub(crate) fn init_udp_error_registry() {
    UDP_REGISTRY.call_once(|| RwLock::new(Vec::new()));
}

#[derive(Clone)]
pub(crate) struct QueuedUdpError {
    pub(crate) payload: Vec<u8>,
    pub(crate) addr: SocketAddr,
    pub(crate) ancillary: SocketErrorInfo,
}

#[derive(Clone, Copy)]
struct UdpErrorAddr {
    local: Option<IpEndpoint>,
    peer: Option<(IpEndpoint, IpAddress)>,
}

pub(crate) struct UdpErrorState {
    dispatch_irq: SocketHandle,
    addr: RwLock<UdpErrorAddr>,
    recv_err: AtomicBool,
    socket_error: AtomicI32,
    error_queue: Mutex<VecDeque<QueuedUdpError>>,
    error_poll: PollSet,
}

impl UdpErrorState {
    pub(crate) fn new(dispatch_irq: SocketHandle) -> Self {
        Self {
            dispatch_irq,
            addr: RwLock::new(UdpErrorAddr {
                local: None,
                peer: None,
            }),
            recv_err: AtomicBool::new(false),
            socket_error: AtomicI32::new(0),
            error_queue: Mutex::new(VecDeque::new()),
            error_poll: PollSet::new(),
        }
    }

    pub(crate) fn set_local_addr(&self, local_addr: Option<IpEndpoint>) {
        self.addr.write().local = local_addr;
    }

    pub(crate) fn set_peer_addr(&self, peer_addr: Option<(IpEndpoint, IpAddress)>) {
        self.addr.write().peer = peer_addr;
    }

    pub(crate) fn recv_err_enabled(&self) -> bool {
        self.recv_err.load(Ordering::Relaxed)
    }

    pub(crate) fn set_recv_err(&self, enabled: bool) {
        self.recv_err.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn has_pending_error(&self) -> bool {
        !self.error_queue.lock().is_empty()
    }

    pub(crate) fn peek_error(&self) -> Option<QueuedUdpError> {
        self.error_queue.lock().front().cloned()
    }

    pub(crate) fn pop_error(&self) -> Option<QueuedUdpError> {
        let mut queue = self.error_queue.lock();
        let error = queue.pop_front();
        self.refresh_socket_error(&queue);
        error
    }

    pub(crate) fn consume_socket_error(&self) -> i32 {
        self.socket_error.swap(0, Ordering::AcqRel)
    }

    pub(crate) fn register_error_waker(&self, waker: &core::task::Waker) {
        self.error_poll.register(waker);
    }

    fn enqueue_error(&self, error: QueuedUdpError) {
        if !self.recv_err_enabled() {
            return;
        }

        let mut queue = self.error_queue.lock();
        const MAX_UDP_ERROR_QUEUE: usize = 32;
        if queue.len() >= MAX_UDP_ERROR_QUEUE {
            queue.pop_front();
        }
        queue.push_back(error);
        self.refresh_socket_error(&queue);
        drop(queue);
        self.error_poll.wake();
    }

    fn refresh_socket_error(&self, queue: &VecDeque<QueuedUdpError>) {
        let errno = queue
            .front()
            .map(|error| error.ancillary.errno.into_raw())
            .unwrap_or(0);
        self.socket_error.store(errno, Ordering::Release);
    }
}

pub(crate) fn register_udp_error_state(state: Arc<UdpErrorState>) {
    UDP_REGISTRY.write().push(state);
}

pub(crate) fn unregister_udp_error_state(dispatch_irq: SocketHandle) {
    UDP_REGISTRY
        .write()
        .retain(|state| state.dispatch_irq != dispatch_irq);
}

fn local_endpoint_matches(bound: IpEndpoint, local: SocketAddrV4) -> bool {
    if bound.port != local.port() {
        return false;
    }
    bound.addr.is_unspecified() || bound.addr == IpAddress::Ipv4(*local.ip())
}

fn peer_endpoint_matches(bound: IpEndpoint, remote: SocketAddrV4) -> bool {
    (bound.addr.is_unspecified() || bound.addr == IpAddress::Ipv4(*remote.ip()))
        && (bound.port == 0 || bound.port == remote.port())
}

fn queue_ipv4_error(local: SocketAddrV4, remote: SocketAddrV4, error: QueuedUdpError) {
    let selected = {
        let registry = UDP_REGISTRY.read();
        let mut selected = None;

        for state in registry.iter() {
            let addr = *state.addr.read();
            let Some(bound) = addr.local else {
                continue;
            };
            if !local_endpoint_matches(bound, local) {
                continue;
            }

            let peer_match = match addr.peer {
                Some((peer, _)) if peer_endpoint_matches(peer, remote) => Some(true),
                Some(_) => Some(false),
                None => None,
            };
            match peer_match {
                Some(true) => {
                    selected = Some(state.clone());
                    break;
                }
                Some(false) => continue,
                None => {
                    if selected.is_none() {
                        selected = Some(state.clone());
                    }
                }
            }
        }

        selected
    };

    if let Some(state) = selected {
        state.enqueue_error(error);
    }
}

fn icmpv4_errno(packet: &Icmpv4Packet<&[u8]>) -> Option<(LinuxError, u32)> {
    match packet.msg_type() {
        Icmpv4Message::DstUnreachable => {
            let reason = Icmpv4DstUnreachable::from(packet.msg_code());
            let result = match reason {
                Icmpv4DstUnreachable::PortUnreachable => (LinuxError::ECONNREFUSED, 0),
                Icmpv4DstUnreachable::FragRequired => {
                    let mtu_bytes: [u8; 2] = packet.as_ref().get(6..8)?.try_into().unwrap();
                    let mtu = u16::from_be_bytes(mtu_bytes) as u32;
                    (LinuxError::EMSGSIZE, mtu)
                }
                Icmpv4DstUnreachable::NetUnreachable
                | Icmpv4DstUnreachable::DstNetUnknown
                | Icmpv4DstUnreachable::NetProhibited
                | Icmpv4DstUnreachable::NetUnreachToS => (LinuxError::ENETUNREACH, 0),
                Icmpv4DstUnreachable::CommProhibited => (LinuxError::EACCES, 0),
                Icmpv4DstUnreachable::ProtoUnreachable => (LinuxError::EPROTO, 0),
                _ => (LinuxError::EHOSTUNREACH, 0),
            };
            Some(result)
        }
        Icmpv4Message::TimeExceeded => {
            let reason = Icmpv4TimeExceeded::from(packet.msg_code());
            let errno = match reason {
                Icmpv4TimeExceeded::TtlExpired | Icmpv4TimeExceeded::FragExpired => {
                    LinuxError::EHOSTUNREACH
                }
                Icmpv4TimeExceeded::Unknown(_) => LinuxError::EPROTO,
            };
            Some((errno, 0))
        }
        _ => None,
    }
}

/// Inspect an IPv4 ICMP error and queue it for the affected UDP socket.
///
/// ICMP errors quote the packet that triggered the error, so this path parses
/// the outer IPv4 and ICMP headers, then the quoted IPv4 and UDP headers. The
/// quoted UDP 4-tuple determines the receiving socket: a connected socket that
/// matches the peer is preferred, and an unconnected socket bound to the local
/// endpoint is used as a fallback.
///
/// This mirrors Linux's ICMP-to-socket error delivery path in `net/ipv4/icmp.c`
/// and `ip_icmp_error()` in `net/ipv4/ip_sockglue.c`.
pub(crate) fn inspect_icmpv4_error(packet: &[u8]) {
    let ip_packet = match Ipv4Packet::new_checked(packet) {
        Ok(packet) if packet.next_header() == IpProtocol::Icmp => packet,
        _ => return,
    };
    let icmp_packet = match Icmpv4Packet::new_checked(ip_packet.payload()) {
        Ok(packet) => packet,
        Err(_) => return,
    };
    let Some((errno, info)) = icmpv4_errno(&icmp_packet) else {
        return;
    };

    let original_ip = match Ipv4Packet::new_checked(icmp_packet.data()) {
        Ok(packet) if packet.next_header() == IpProtocol::Udp => packet,
        _ => return,
    };
    let original_udp = match UdpPacket::new_checked(original_ip.payload()) {
        Ok(packet) => packet,
        Err(_) => return,
    };

    let local = SocketAddrV4::new(original_ip.src_addr(), original_udp.src_port());
    let remote = SocketAddrV4::new(original_ip.dst_addr(), original_udp.dst_port());
    // ICMP has no transport-layer port, so Linux reports the offender as the
    // ICMP source address with `sin_port` set to zero.
    let offender = SocketAddr::V4(SocketAddrV4::new(ip_packet.src_addr(), 0));
    let error = QueuedUdpError {
        payload: original_udp.payload().to_vec(),
        addr: SocketAddr::V4(remote),
        ancillary: SocketErrorInfo {
            errno,
            origin: SocketErrorOrigin::Icmp,
            error_type: ip_packet.payload()[0],
            error_code: ip_packet.payload()[1],
            info,
            data: 0,
            offender: Some(offender),
        },
    };
    queue_ipv4_error(local, remote, error);
}

#[cfg(unittest)]
mod tests {
    extern crate alloc;

    use alloc::{sync::Arc, vec};
    use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use smoltcp::wire::{Icmpv4DstUnreachable, Icmpv4Message, Icmpv4TimeExceeded};
    use unittest::def_test;

    use super::*;
    use crate::SocketErrorOrigin;

    fn endpoint(addr: Ipv4Addr, port: u16) -> IpEndpoint {
        SocketAddrV4::new(addr, port).into()
    }

    fn icmp_packet(ty: Icmpv4Message, code: u8, info: u16) -> [u8; 8] {
        let mut bytes = [0; 8];
        {
            let mut packet = Icmpv4Packet::new_unchecked(&mut bytes[..]);
            packet.set_msg_type(ty);
            packet.set_msg_code(code);
        }
        bytes[6..8].copy_from_slice(&info.to_be_bytes());
        bytes
    }

    fn queued_error(errno: LinuxError, payload_byte: u8) -> QueuedUdpError {
        QueuedUdpError {
            payload: vec![payload_byte],
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234)),
            ancillary: SocketErrorInfo {
                errno,
                origin: SocketErrorOrigin::Icmp,
                error_type: 3,
                error_code: 3,
                info: 0,
                data: 0,
                offender: Some(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(192, 0, 2, 254),
                    0,
                ))),
            },
        }
    }

    fn clear_registry() {
        init_udp_error_registry();
        UDP_REGISTRY.write().clear();
    }

    #[def_test]
    fn test_icmpv4_errno_maps_error_types() {
        let bytes = icmp_packet(
            Icmpv4Message::DstUnreachable,
            Icmpv4DstUnreachable::PortUnreachable.into(),
            0,
        );
        let packet = Icmpv4Packet::new_checked(&bytes[..]).unwrap();
        assert_eq!(icmpv4_errno(&packet), Some((LinuxError::ECONNREFUSED, 0)));

        let bytes = icmp_packet(
            Icmpv4Message::DstUnreachable,
            Icmpv4DstUnreachable::FragRequired.into(),
            1400,
        );
        let packet = Icmpv4Packet::new_checked(&bytes[..]).unwrap();
        assert_eq!(icmpv4_errno(&packet), Some((LinuxError::EMSGSIZE, 1400)));

        let bytes = icmp_packet(
            Icmpv4Message::TimeExceeded,
            Icmpv4TimeExceeded::TtlExpired.into(),
            0,
        );
        let packet = Icmpv4Packet::new_checked(&bytes[..]).unwrap();
        assert_eq!(icmpv4_errno(&packet), Some((LinuxError::EHOSTUNREACH, 0)));

        let bytes = icmp_packet(Icmpv4Message::EchoRequest, 0, 0);
        let packet = Icmpv4Packet::new_checked(&bytes[..]).unwrap();
        assert_eq!(icmpv4_errno(&packet), None);
    }

    #[def_test]
    fn test_udp_error_endpoint_matching() {
        let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
        assert!(local_endpoint_matches(
            endpoint(Ipv4Addr::UNSPECIFIED, 8080),
            local
        ));
        assert!(local_endpoint_matches(
            endpoint(Ipv4Addr::new(10, 0, 0, 2), 8080),
            local
        ));
        assert!(!local_endpoint_matches(
            endpoint(Ipv4Addr::new(10, 0, 0, 3), 8080),
            local
        ));
        assert!(!local_endpoint_matches(
            endpoint(Ipv4Addr::UNSPECIFIED, 9090),
            local
        ));

        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
        assert!(peer_endpoint_matches(
            endpoint(Ipv4Addr::UNSPECIFIED, 0),
            remote
        ));
        assert!(peer_endpoint_matches(
            endpoint(Ipv4Addr::new(192, 0, 2, 1), 0),
            remote
        ));
        assert!(peer_endpoint_matches(
            endpoint(Ipv4Addr::new(192, 0, 2, 1), 5353),
            remote
        ));
        assert!(!peer_endpoint_matches(
            endpoint(Ipv4Addr::new(192, 0, 2, 2), 5353),
            remote
        ));
        assert!(!peer_endpoint_matches(
            endpoint(Ipv4Addr::new(192, 0, 2, 1), 5354),
            remote
        ));
    }

    #[def_test]
    fn test_connected_udp_error_state_takes_precedence() {
        clear_registry();

        let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
        let bound = Arc::new(UdpErrorState::new(Default::default()));
        bound.set_recv_err(true);
        bound.set_local_addr(Some(endpoint(*local.ip(), local.port())));
        let connected = Arc::new(UdpErrorState::new(Default::default()));
        connected.set_recv_err(true);
        connected.set_local_addr(Some(endpoint(*local.ip(), local.port())));
        connected.set_peer_addr(Some((
            endpoint(*remote.ip(), remote.port()),
            IpAddress::Ipv4(*local.ip()),
        )));

        register_udp_error_state(bound.clone());
        register_udp_error_state(connected.clone());
        queue_ipv4_error(local, remote, queued_error(LinuxError::ECONNREFUSED, 1));

        assert!(!bound.has_pending_error());
        assert!(connected.has_pending_error());

        clear_registry();
    }

    #[def_test]
    fn test_udp_error_queue_drops_oldest_entry_when_full() {
        let state = UdpErrorState::new(Default::default());
        state.set_recv_err(true);

        for byte in 0..33 {
            state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, byte));
        }

        let first = state.pop_error().unwrap();
        assert_eq!(first.payload, vec![1]);
    }

    #[def_test]
    fn test_udp_socket_error_is_read_and_clear() {
        let state = UdpErrorState::new(Default::default());
        state.set_recv_err(true);
        state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));

        assert_eq!(
            state.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );
        assert_eq!(state.consume_socket_error(), 0);
    }
}
