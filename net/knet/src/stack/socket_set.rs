// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket set wrapper utilities.

use alloc::{vec, vec::Vec};
use core::ops::{Deref, DerefMut};

use event_listener::Event;
use hashbrown::HashMap;
use kerrno::KError;
use ksync::Mutex;
use smoltcp::{
    iface::{Interface, PollIngressSingleResult, SocketHandle, SocketSet},
    socket::{AnySocket, Socket, tcp},
    time::{Duration, Instant},
    wire::{IpEndpoint, IpProtocol, Ipv4Packet, Ipv6Packet, TcpPacket},
};

use super::device::StackDevice;

const ORPHAN_FIN_WAIT2_TIMEOUT: Duration = Duration::from_secs(60);

fn orphan_fin_wait2_deadline(
    state: tcp::State,
    current: Instant,
    deadline: Option<Instant>,
) -> Option<Instant> {
    (state == tcp::State::FinWait2).then(|| deadline.unwrap_or(current + ORPHAN_FIN_WAIT2_TIMEOUT))
}

struct DeferredTcpClose {
    handle: SocketHandle,
    fin_wait2_deadline: Option<Instant>,
}

pub(crate) struct SocketSetState<'a> {
    sockets: SocketSet<'a>,
    deferred_tcp_closes: Vec<DeferredTcpClose>,
    tcp_errors: HashMap<SocketHandle, KError>,
}

impl<'a> SocketSetState<'a> {
    pub(crate) fn new() -> Self {
        Self {
            sockets: SocketSet::new(vec![]),
            deferred_tcp_closes: Vec::new(),
            tcp_errors: HashMap::new(),
        }
    }

    pub(crate) fn add<T: AnySocket<'a>>(&mut self, socket: T) -> SocketHandle {
        let handle = self.sockets.add(socket);
        self.tcp_errors.remove(&handle);
        handle
    }

    pub(crate) fn remove(&mut self, handle: SocketHandle) -> Socket<'a> {
        self.tcp_errors.remove(&handle);
        self.sockets.remove(handle)
    }

    pub(crate) fn with_tcp_socket_mut<R>(
        &mut self,
        handle: SocketHandle,
        f: impl FnOnce(&mut tcp::Socket<'a>, &mut Option<KError>) -> R,
    ) -> R {
        let mut error = self.tcp_errors.remove(&handle);
        let result = f(self.sockets.get_mut(handle), &mut error);
        if let Some(error) = error {
            self.tcp_errors.insert(handle, error);
        }
        result
    }

    /// Records a reset only after protocol validation actually closes its socket.
    pub(crate) fn poll_ingress_single(
        &mut self,
        iface: &mut Interface,
        current: Instant,
        device: &mut StackDevice<'_>,
    ) -> PollIngressSingleResult {
        let candidate = device.pending_ip_packet().and_then(|packet| {
            let (src, dst) = tcp_reset_endpoints(packet)?;
            // Only reset packets need this lookup; ordinary ingress keeps the
            // protocol stack's existing demultiplexing path.
            self.sockets.iter().find_map(|(handle, socket)| {
                let Socket::Tcp(socket) = socket else {
                    return None;
                };
                if socket.local_endpoint() != Some(dst) || socket.remote_endpoint() != Some(src) {
                    return None;
                }
                let error = match socket.state() {
                    tcp::State::Closed | tcp::State::Listen | tcp::State::TimeWait => return None,
                    tcp::State::SynSent => KError::ConnectionRefused,
                    tcp::State::CloseWait => KError::BrokenPipe,
                    _ => KError::ConnectionReset,
                };
                Some((handle, error))
            })
        });
        let result = iface.poll_ingress_single(current, device, &mut self.sockets);
        if let Some((handle, error)) = candidate
            && self.sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed
        {
            // The socket-set lock serializes packet processing, error
            // publication and consumers, including waiters woken by smoltcp.
            self.tcp_errors.entry(handle).or_insert(error);
        }
        result
    }

    pub(crate) fn defer_tcp_close(&mut self, handle: SocketHandle) {
        debug_assert!(
            self.deferred_tcp_closes
                .iter()
                .all(|entry| entry.handle != handle)
        );
        self.deferred_tcp_closes.push(DeferredTcpClose {
            handle,
            fin_wait2_deadline: None,
        });
    }

    pub(crate) fn deferred_tcp_close_deadline(&self) -> Option<Instant> {
        self.deferred_tcp_closes
            .iter()
            .filter_map(|entry| entry.fin_wait2_deadline)
            .min()
    }

    pub(crate) fn reap_deferred_tcp_closes(&mut self, current: Instant) -> Option<Instant> {
        let Self {
            sockets,
            deferred_tcp_closes,
            tcp_errors,
        } = self;
        let mut next_deadline = None;
        deferred_tcp_closes.retain_mut(|entry| {
            let socket = sockets.get_mut::<tcp::Socket>(entry.handle);
            // No reader remains after file close. Data arriving during the
            // FIN handshake must reset the peer instead of filling an orphan.
            if socket.state() != tcp::State::Closed && socket.recv_queue() != 0 {
                socket.abort();
            }
            let state = socket.state();
            // smoltcp keeps the tuple until a CLOSED socket's pending RST has
            // been emitted. The protocol object must remain reachable until
            // that packet has actually been dispatched.
            if state == tcp::State::Closed && socket.local_endpoint().is_none() {
                sockets.remove(entry.handle);
                tcp_errors.remove(&entry.handle);
                return false;
            }

            entry.fin_wait2_deadline =
                orphan_fin_wait2_deadline(state, current, entry.fin_wait2_deadline);
            if let Some(deadline) = entry.fin_wait2_deadline {
                if current >= deadline {
                    sockets.remove(entry.handle);
                    tcp_errors.remove(&entry.handle);
                    return false;
                }
                next_deadline =
                    Some(next_deadline.map_or(deadline, |current: Instant| current.min(deadline)));
            }
            true
        });
        next_deadline
    }
}

fn tcp_reset_endpoints(packet: &[u8]) -> Option<(IpEndpoint, IpEndpoint)> {
    let (src, dst, payload) = match packet.first()? >> 4 {
        4 => {
            let packet = Ipv4Packet::new_checked(packet).ok()?;
            if packet.next_header() != IpProtocol::Tcp {
                return None;
            }
            (
                packet.src_addr().into(),
                packet.dst_addr().into(),
                packet.payload(),
            )
        }
        6 => {
            let packet = Ipv6Packet::new_checked(packet).ok()?;
            if packet.next_header() != IpProtocol::Tcp {
                return None;
            }
            (
                packet.src_addr().into(),
                packet.dst_addr().into(),
                packet.payload(),
            )
        }
        _ => return None,
    };
    let tcp = TcpPacket::new_checked(payload).ok()?;
    tcp.rst().then(|| {
        (
            IpEndpoint::new(src, tcp.src_port()),
            IpEndpoint::new(dst, tcp.dst_port()),
        )
    })
}

impl<'a> Deref for SocketSetState<'a> {
    type Target = SocketSet<'a>;

    fn deref(&self) -> &Self::Target {
        &self.sockets
    }
}

impl DerefMut for SocketSetState<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sockets
    }
}

pub(crate) struct SocketSetWrapper<'a> {
    pub inner: Mutex<SocketSetState<'a>>,
    pub new_socket: Event,
}

impl<'a> SocketSetWrapper<'a> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(SocketSetState::new()),
            new_socket: Event::new(),
        }
    }

    pub(crate) fn add<T: AnySocket<'a>>(&self, socket: T) -> SocketHandle {
        let handle = self.inner.lock().add(socket);
        self.new_socket.notify(1);
        handle
    }

    pub(crate) fn with_socket_mut<T: AnySocket<'a>, R, F>(&self, handle: SocketHandle, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut set = self.inner.lock();
        let socket = set.get_mut(handle);
        f(socket)
    }

    /// Runs `f` on the socket only if `handle` still refers to a live one.
    ///
    /// smoltcp's `get_mut` panics for a removed handle. Most callers run inside
    /// the socket's own lifetime and cannot see that, but an ioctl may be
    /// issued on a descriptor whose transport has already been torn down (a
    /// peer-close race), so those paths need a non-panicking query.
    pub(crate) fn try_with_socket_mut<R>(
        &self,
        handle: SocketHandle,
        f: impl FnOnce(&mut smoltcp::socket::tcp::Socket<'_>) -> R,
    ) -> Option<R> {
        let mut set = self.inner.lock();
        set.iter_mut()
            .find(|(candidate, _)| *candidate == handle)
            .and_then(|(_, socket)| match socket {
                smoltcp::socket::Socket::Tcp(tcp) => Some(f(tcp)),
                _ => None,
            })
    }

    pub(crate) fn remove(&self, handle: SocketHandle) {
        self.inner.lock().remove(handle);
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/stack/socket_set/tests.rs");
