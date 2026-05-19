// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP socket implementation.
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{
    net::{Ipv4Addr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use hashbrown::HashMap;
use kerrno::{KError, KResult, k_bail, k_err_type};
use kio::prelude::*;
use kpoll::{IoEvents, PollSet, Pollable};
use ksync::Mutex;
use lazy_static::lazy_static;
use smoltcp::{
    iface::SocketHandle,
    socket::tcp as smol,
    time::Duration,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint},
};

use super::{LISTEN_TABLE, SOCKET_SET};
use crate::{
    RecvFlags, RecvOptions, SERVICE, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    consts::{TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    poll_interfaces,
    state::*,
};

pub(crate) fn new_tcp_socket() -> smol::Socket<'static> {
    smol::Socket::new(
        smol::SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
        smol::SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
    )
}

/// A TCP socket that provides POSIX-like APIs.
pub struct TcpSocket {
    state: StateLock,
    dispatch_irq: SocketHandle,
    bound_endpoint: Mutex<IpListenEndpoint>,
    bound_registered: AtomicBool,

    general: GeneralOptions,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
    poll_rx_closed: Arc<PollSet>,
}

unsafe impl Sync for TcpSocket {}

impl TcpSocket {
    /// Creates a new TCP socket.
    pub fn new() -> Self {
        let dispatch_irq = SOCKET_SET.add(new_tcp_socket());
        Self {
            state: StateLock::new(State::Idle),
            dispatch_irq,
            bound_endpoint: Mutex::new(empty_endpoint()),
            bound_registered: AtomicBool::new(false),

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            poll_rx_closed: Arc::new(PollSet::new()),
        }
    }

    /// Creates a new TCP socket that is already connected.
    fn new_connected(dispatch_irq: SocketHandle) -> Self {
        let result = Self {
            state: StateLock::new(State::Connected),
            dispatch_irq,
            bound_endpoint: Mutex::new(empty_endpoint()),
            bound_registered: AtomicBool::new(false),

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            poll_rx_closed: Arc::new(PollSet::new()),
        };
        let (remote_endpoint, bound_endpoint) = result
            .with_smol_socket(|socket| (socket.remote_endpoint(), socket_bound_endpoint(socket)));
        *result.bound_endpoint.lock() = bound_endpoint;
        let service = SERVICE.lock();
        let device_mask = remote_endpoint.map_or_else(
            || service.device_mask_for(&bound_endpoint),
            |remote_endpoint| service.device_mask_for_addr(&remote_endpoint.addr),
        );
        result.general.set_device_mask(device_mask);
        result
    }
}

impl Default for TcpSocket {
    fn default() -> Self {
        Self::new()
    }
}

/// Private methods
impl TcpSocket {
    fn state(&self) -> State {
        self.state.get()
    }

    #[inline]
    fn is_listening(&self) -> bool {
        self.state() == State::Listening
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        SOCKET_SET.with_socket_mut::<smol::Socket, _, _>(self.dispatch_irq, f)
    }

    fn bound_endpoint(&self) -> KResult<IpListenEndpoint> {
        let endpoint = *self.bound_endpoint.lock();
        if endpoint.port == 0 {
            k_bail!(InvalidInput, "not bound");
        }
        Ok(endpoint)
    }

    fn send_state_error(state: smol::State) -> KError {
        match state {
            smol::State::Listen | smol::State::SynSent | smol::State::SynReceived => {
                KError::NotConnected
            }
            smol::State::Closed
            | smol::State::FinWait1
            | smol::State::FinWait2
            | smol::State::Closing
            | smol::State::LastAck
            | smol::State::TimeWait => KError::BrokenPipe,
            smol::State::Established | smol::State::CloseWait => unreachable!(),
        }
    }

    fn poll_connect(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let writable = self.with_smol_socket(|socket| match socket.state() {
            smol::State::SynSent => false, // wait for connection
            smol::State::Established => {
                self.state.set(State::Connected); // connected
                if let Some(remote) = socket.remote_endpoint() {
                    debug!("TCP socket {}: connected to {}", self.dispatch_irq, remote);
                }
                true
            }
            _ => {
                self.state.set(State::Closed); // connection failed
                true
            }
        });
        events.set(IoEvents::OUT, writable);
        events
    }

    fn poll_stream(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        self.with_smol_socket(|socket| {
            events.set(
                IoEvents::IN,
                !self.rx_closed.load(Ordering::Acquire)
                    && (!socket.may_recv() || socket.can_recv()),
            );
            events.set(
                IoEvents::OUT,
                !self.tx_closed.load(Ordering::Acquire)
                    && (!socket.may_send() || socket.can_send()),
            );
        });
        events
    }

    fn poll_listener(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let readable = self
            .bound_endpoint()
            .ok()
            .and_then(|endpoint| LISTEN_TABLE.can_accept(endpoint).ok())
            .unwrap_or(false);
        events.set(IoEvents::IN, readable);
        events
    }
}

impl Configurable for TcpSocket {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.general.get_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::NoDelay(no_delay) => {
                **no_delay = self.with_smol_socket(|socket| !socket.nagle_enabled());
            }
            O::KeepAlive(keep_alive) => {
                **keep_alive = self.with_smol_socket(|socket| socket.keep_alive().is_some());
            }
            O::MaxSegment(max_segment) => {
                // TODO(mivik): get actual MSS
                **max_segment = 1460;
            }
            O::SendBuffer(size) => {
                **size = TCP_TX_BUF_LEN;
            }
            O::ReceiveBuffer(size) => {
                **size = TCP_RX_BUF_LEN;
            }
            O::TcpInfo(_) => {
                // TODO(mivik): implement TCP_INFO
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        if self.general.set_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::NoDelay(no_delay) => {
                self.with_smol_socket(|socket| {
                    socket.set_nagle_enabled(!no_delay);
                });
            }
            O::KeepAlive(keep_alive) => {
                self.with_smol_socket(|socket| {
                    socket.set_keep_alive(keep_alive.then(|| Duration::from_secs(75)));
                });
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}
impl SocketOps for TcpSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let mut local_addr = local_addr.into_ip()?;
        self.state
            .lock(State::Idle)
            .map_err(|_| k_err_type!(InvalidInput, "already bound"))?
            .transit(State::Idle, || {
                // TODO: check addr is available
                if local_addr.port() == 0 {
                    local_addr.set_port(get_ephemeral_port(local_addr.ip().into())?);
                }

                let endpoint = IpListenEndpoint {
                    addr: if local_addr.ip().is_unspecified() {
                        None
                    } else {
                        Some(local_addr.ip().into())
                    },
                    port: local_addr.port(),
                };
                if !self.general.reuse_address() && !LISTEN_TABLE.can_listen(endpoint) {
                    return Err(KError::AddrInUse);
                }
                if self.bound_endpoint.lock().port != 0 {
                    return Err(KError::InvalidInput);
                }
                self.register_bound_endpoint(endpoint)?;
                *self.bound_endpoint.lock() = endpoint;
                self.general
                    .set_device_mask(SERVICE.lock().device_mask_for(&endpoint));
                Ok(())
            })
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        let remote_addr = remote_addr.into_ip()?;
        self.state
            .lock(State::Idle)
            .map_err(|state| {
                if state == State::Connecting {
                    KError::InProgress
                } else {
                    // TODO(mivik): error code
                    k_err_type!(AlreadyConnected)
                }
            })?
            .transit(State::Connecting, || {
                // TODO: check remote addr unreachable
                // let (bound_endpoint, remote_endpoint) = self.get_endpoint_pair(remote_addr)?;
                let remote_endpoint = IpEndpoint::from(remote_addr);
                let mut bound_endpoint = *self.bound_endpoint.lock();
                if bound_endpoint.addr.is_none() {
                    bound_endpoint.addr =
                        Some(SERVICE.lock().get_source_address(&remote_endpoint.addr)?);
                }
                if bound_endpoint.port == 0 {
                    let local_addr = bound_endpoint
                        .addr
                        .expect("source address must be resolved before ephemeral bind");
                    bound_endpoint.port = get_ephemeral_port(local_addr)?;
                }
                let should_register = !self.bound_registered.load(Ordering::Acquire);
                if should_register {
                    register_tcp_bound(bound_endpoint)?;
                }

                let result = {
                    let mut service = crate::SERVICE.lock();
                    let context = service.iface.context();
                    self.with_smol_socket(|socket| {
                        socket
                            .connect(context, remote_endpoint, bound_endpoint)
                            .map_err(|e| match e {
                                smol::ConnectError::InvalidState => k_err_type!(AlreadyConnected),
                                smol::ConnectError::Unaddressable => {
                                    k_err_type!(ConnectionRefused, "unaddressable")
                                }
                            })?;
                        Ok::<(), KError>(())
                    })
                };
                if let Err(err) = result {
                    if should_register {
                        unregister_tcp_bound(bound_endpoint);
                    }
                    return Err(err);
                }

                *self.bound_endpoint.lock() = bound_endpoint;
                if should_register {
                    self.bound_registered.store(true, Ordering::Release);
                }
                self.general
                    .set_device_mask(SERVICE.lock().device_mask_for_addr(&remote_endpoint.addr));
                Ok(())
            })?;

        // Hack: let the server listen
        ktask::yield_now();

        // Here our state must be `CONNECTING`, and only one thread can run here.
        self.general.send_poller(self, || {
            poll_interfaces();
            let events = self.poll_connect();
            if !events.contains(IoEvents::OUT) {
                Err(KError::WouldBlock)
            } else if self.state() == State::Connected {
                Ok(())
            } else {
                Err(k_err_type!(ConnectionRefused, "connection refused"))
            }
        })
    }

    fn listen(&self, backlog: usize) -> KResult {
        if let Ok(guard) = self.state.lock(State::Idle) {
            guard.transit(State::Listening, || {
                let mut bound_endpoint = *self.bound_endpoint.lock();
                if bound_endpoint.port == 0 {
                    let local_addr = bound_endpoint
                        .addr
                        .unwrap_or(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED));
                    bound_endpoint.port = get_ephemeral_port(local_addr)?;
                }
                let should_register = !self.bound_registered.load(Ordering::Acquire);
                if should_register {
                    register_tcp_bound(bound_endpoint)?;
                }
                if let Err(err) = LISTEN_TABLE.listen(bound_endpoint, backlog) {
                    if should_register {
                        unregister_tcp_bound(bound_endpoint);
                    }
                    return Err(err);
                }
                *self.bound_endpoint.lock() = bound_endpoint;
                if should_register {
                    self.bound_registered.store(true, Ordering::Release);
                }
                self.general
                    .set_device_mask(SERVICE.lock().device_mask_for(&bound_endpoint));
                Ok(())
            })?;
        } else {
            // ignore simultaneous `listen`s.
        }
        Ok(())
    }

    fn accept(&self) -> KResult<Socket> {
        if !self.is_listening() {
            k_bail!(InvalidInput, "not listening");
        }

        let bound_endpoint = self.bound_endpoint()?;
        self.general.recv_poller(self, || {
            poll_interfaces();
            LISTEN_TABLE
                .accept(bound_endpoint)
                .map(|dispatch_irq| Socket::Tcp(Box::new(TcpSocket::new_connected(dispatch_irq))))
        })
    }

    fn send(&self, mut src: impl Read, _options: SendOptions) -> KResult<usize> {
        // SAFETY: `self.dispatch_irq` should be initialized in a connected socket.
        self.general.send_poller(self, || {
            poll_interfaces();
            self.with_smol_socket(|socket| {
                if self.tx_closed.load(Ordering::Acquire) {
                    Err(KError::BrokenPipe)
                } else if !socket.may_send() {
                    Err(Self::send_state_error(socket.state()))
                } else if !socket.can_send() {
                    Err(KError::WouldBlock)
                } else {
                    // connected, and the tx buffer is not full
                    let len = socket
                        .send(|buffer| {
                            let result = src.read(buffer);
                            let len = result.unwrap_or(0);
                            (len, result)
                        })
                        .map_err(|_| k_err_type!(NotConnected, "not connected?"))??;
                    Ok(len)
                }
            })
        })
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        if self.rx_closed.load(Ordering::Acquire) {
            return Err(KError::NotConnected);
        }
        self.general.recv_poller(self, || {
            poll_interfaces();
            self.with_smol_socket(|socket| {
                if socket.can_recv() {
                    if options.flags.contains(RecvFlags::PEEK) {
                        dst.write(
                            socket
                                .peek(dst.remaining_mut())
                                .map_err(|_| k_err_type!(NotConnected, "not connected?"))?,
                        )
                    } else {
                        socket
                            .recv(|buf| {
                                let result = dst.write(buf);
                                let len = result.unwrap_or(0);
                                (len, result)
                            })
                            .map_err(|_| k_err_type!(NotConnected, "not connected?"))?
                    }
                } else if !socket.may_recv() {
                    Ok(0)
                } else {
                    Err(KError::WouldBlock)
                }
            })
        })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        let endpoint = self
            .with_smol_socket(|socket| socket.local_endpoint().map(endpoint_from_ip_endpoint))
            .unwrap_or_else(|| *self.bound_endpoint.lock());
        Ok(SocketAddrEx::Ip(SocketAddr::new(
            endpoint
                .addr
                .map_or_else(|| Ipv4Addr::UNSPECIFIED.into(), Into::into),
            endpoint.port,
        )))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        self.with_smol_socket(|socket| {
            Ok(SocketAddrEx::Ip(
                socket.remote_endpoint().ok_or(KError::NotConnected)?.into(),
            ))
        })
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
            self.poll_rx_closed.wake();
        }

        // stream
        if self.state() == State::Connected {
            if how.has_write() {
                self.tx_closed.store(true, Ordering::Release);
                self.with_smol_socket(|socket| {
                    socket.close();
                });
            }
            if how == Shutdown::Both {
                self.state.set(State::Closed);
                self.unregister_bound_endpoint();
                *self.bound_endpoint.lock() = empty_endpoint();
            }
            poll_interfaces();
        }

        // listener
        if let Ok(guard) = self.state.lock(State::Listening) {
            guard.transit(State::Closed, || {
                let bound_endpoint = self.bound_endpoint()?;
                LISTEN_TABLE.unlisten(bound_endpoint);
                self.unregister_bound_endpoint();
                *self.bound_endpoint.lock() = empty_endpoint();
                poll_interfaces();
                Ok(())
            })?;
        }

        // ignore for other states
        Ok(())
    }
}

impl Pollable for TcpSocket {
    fn poll(&self) -> IoEvents {
        poll_interfaces();
        let mut events = match self.state() {
            State::Connecting => self.poll_connect(),
            State::Connected | State::Idle | State::Closed => self.poll_stream(),
            State::Listening => self.poll_listener(),
            State::Busy => IoEvents::empty(),
        };
        events.set(IoEvents::RDHUP, self.rx_closed.load(Ordering::Acquire));
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.intersects(IoEvents::IN | IoEvents::OUT | IoEvents::RDHUP) {
            self.general.register_rx_waker(context.waker());
        }
        if self.is_listening()
            && events.contains(IoEvents::IN)
            && let Ok(endpoint) = self.bound_endpoint()
        {
            let _ = LISTEN_TABLE.register_accept_waker(endpoint, context.waker());
        }
        if events.contains(IoEvents::RDHUP) {
            self.poll_rx_closed.register(context.waker());
        }
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        if let Err(err) = self.shutdown(Shutdown::Both) {
            warn!("TCP socket {}: shutdown failed: {}", self.dispatch_irq, err);
        }
        self.unregister_bound_endpoint();
        SOCKET_SET.remove(self.dispatch_irq);
        // This is crucial for the close messages to be sent.
        poll_interfaces();
    }
}

const fn empty_endpoint() -> IpListenEndpoint {
    IpListenEndpoint {
        addr: None,
        port: 0,
    }
}

fn endpoint_from_ip_endpoint(endpoint: IpEndpoint) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: Some(endpoint.addr),
        port: endpoint.port,
    }
}

fn socket_bound_endpoint(socket: &smol::Socket<'_>) -> IpListenEndpoint {
    socket
        .local_endpoint()
        .map(endpoint_from_ip_endpoint)
        .unwrap_or_else(|| socket.listen_endpoint())
}

impl TcpSocket {
    fn register_bound_endpoint(&self, endpoint: IpListenEndpoint) -> KResult {
        if !self.bound_registered.load(Ordering::Acquire) {
            register_tcp_bound(endpoint)?;
            self.bound_registered.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn unregister_bound_endpoint(&self) {
        if self.bound_registered.swap(false, Ordering::AcqRel) {
            unregister_tcp_bound(*self.bound_endpoint.lock());
        }
    }
}

lazy_static! {
    static ref TCP_BOUND_ENDPOINTS: Mutex<HashMap<u16, Vec<Option<IpAddress>>>> =
        Mutex::new(HashMap::new());
}

fn register_tcp_bound(endpoint: IpListenEndpoint) -> KResult {
    if endpoint.port == 0 {
        return Ok(());
    }

    let mut bound_endpoints = TCP_BOUND_ENDPOINTS.lock();
    let bound_addrs = bound_endpoints.entry(endpoint.port).or_default();
    if bound_addrs
        .iter()
        .any(|&addr| listen_addrs_conflict(addr, endpoint.addr))
    {
        return Err(KError::AddrInUse);
    }
    bound_addrs.push(endpoint.addr);
    Ok(())
}

fn unregister_tcp_bound(endpoint: IpListenEndpoint) {
    if endpoint.port == 0 {
        return;
    }

    let mut bound_endpoints = TCP_BOUND_ENDPOINTS.lock();
    let Some(bound_addrs) = bound_endpoints.get_mut(&endpoint.port) else {
        return;
    };
    if let Some(index) = bound_addrs.iter().position(|&addr| addr == endpoint.addr) {
        bound_addrs.swap_remove(index);
    }
    if bound_addrs.is_empty() {
        bound_endpoints.remove(&endpoint.port);
    }
}

fn tcp_port_available(endpoint: IpListenEndpoint) -> bool {
    LISTEN_TABLE.can_listen(endpoint)
        && !TCP_BOUND_ENDPOINTS
            .lock()
            .get(&endpoint.port)
            .is_some_and(|bound_addrs| {
                bound_addrs
                    .iter()
                    .any(|&addr| listen_addrs_conflict(addr, endpoint.addr))
            })
}

fn listen_addrs_conflict(a: Option<IpAddress>, b: Option<IpAddress>) -> bool {
    a.is_none() || b.is_none() || a == b
}

fn get_ephemeral_port(local_addr: smoltcp::wire::IpAddress) -> KResult<u16> {
    const PORT_START: u16 = 0xc000;
    const PORT_END: u16 = 0xffff;
    static CURR: Mutex<u16> = Mutex::new(PORT_START);

    let mut curr = CURR.lock();
    let mut tries = 0;
    // TODO: more robust
    while tries <= PORT_END - PORT_START {
        let port = *curr;
        if *curr == PORT_END {
            *curr = PORT_START;
        } else {
            *curr += 1;
        }
        let listen_endpoint = IpListenEndpoint {
            addr: (!local_addr.is_unspecified()).then_some(local_addr),
            port,
        };
        if tcp_port_available(listen_endpoint) {
            return Ok(port);
        }
        tries += 1;
    }
    k_bail!(AddrInUse, "no available ports");
}
