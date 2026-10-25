// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP socket implementation.
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{
    net::{Ipv4Addr, SocketAddr},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use hashbrown::HashMap;
use kerrno::{KError, KResult, k_bail, k_err_type};
use kio::prelude::*;
use klazy::lazy_static;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, static_lock};
use smoltcp::{
    iface::SocketHandle,
    socket::tcp as smol,
    time::Duration,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint},
};

use crate::{
    AcceptOptions, ConnectOptions, LISTEN_TABLE, RecvFlags, RecvOptions, SERVICE, SOCKET_SET,
    SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    consts::{TCP_KEEPIDLE_DEFAULT_SECS, TCP_KEEPIDLE_MAX_SECS, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    poller::{PollReason, network_poller},
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
    handle: SocketHandle,
    bound_endpoint: Mutex<IpListenEndpoint>,
    accepted_remote_endpoint: Option<IpEndpoint>,
    bound_registered: AtomicBool,

    general: GeneralOptions,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
    poll_rx_closed: Arc<PollSet>,
    /// TCP_KEEPIDLE in seconds (Linux default: tcp_keepalive_time).
    /// TCP_KEEPINTVL/TCP_KEEPCNT are rejected with ENOPROTOOPT: the
    /// transport models a single keep-alive interval with no probe-count
    /// knob, and silently accepting inert options would hide that gap.
    keep_idle_secs: AtomicU32,
}

impl TcpSocket {
    /// Creates a new TCP socket.
    pub fn new() -> Self {
        let handle = SOCKET_SET.add(new_tcp_socket());
        Self {
            state: StateLock::new(State::Idle),
            handle,
            bound_endpoint: Mutex::new(empty_endpoint()),
            accepted_remote_endpoint: None,
            bound_registered: AtomicBool::new(false),

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            poll_rx_closed: Arc::new(PollSet::new()),
            keep_idle_secs: AtomicU32::new(TCP_KEEPIDLE_DEFAULT_SECS),
        }
    }

    /// Creates a new TCP socket that is already connected,
    /// using the given local and remote endpoints.
    fn new_connected(
        handle: SocketHandle,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
    ) -> Self {
        let result = Self {
            state: StateLock::new(State::Connected),
            handle,
            bound_endpoint: Mutex::new(empty_endpoint()),
            accepted_remote_endpoint: Some(remote_endpoint),
            bound_registered: AtomicBool::new(false),

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            poll_rx_closed: Arc::new(PollSet::new()),
            keep_idle_secs: AtomicU32::new(TCP_KEEPIDLE_DEFAULT_SECS),
        };
        let bound_endpoint = endpoint_from_ip_endpoint(local_endpoint);
        *result.bound_endpoint.lock() = bound_endpoint;

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

    /// Applies SO_KEEPALIVE to the transport using the configured
    /// TCP_KEEPIDLE interval. Disabling clears the timer.
    fn apply_keep_alive(&self, enabled: bool) {
        self.with_smol_socket(|socket| {
            if enabled {
                let idle_secs = u64::from(self.keep_idle_secs.load(Ordering::Relaxed));
                // Enabling reads the stored interval. While keep-alive is
                // enabled, smoltcp does not expose a safe deadline reset, so
                // TCP_KEEPIDLE updates are rejected by the setter below.
                socket.set_keep_alive(Some(Duration::from_secs(idle_secs)));
            } else {
                socket.set_keep_alive(None);
            }
        });
    }

    /// Stores TCP_KEEPIDLE before keep-alive is enabled. Once smoltcp has an
    /// active idle deadline, changing the interval is not immediately
    /// observable through its public API, so that update is rejected.
    fn set_keep_idle_secs(&self, idle_secs: u32) -> KResult {
        self.with_smol_socket(|socket| {
            if socket.keep_alive().is_some() {
                return Err(KError::OperationNotSupported);
            }
            self.keep_idle_secs.store(idle_secs, Ordering::Relaxed);
            Ok(())
        })
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        SOCKET_SET.with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }

    // File close must inspect the receive queue before any protocol packet is
    // dispatched so unread data can select the TCP reset path.
    fn shutdown_inner(
        &self,
        how: Shutdown,
        close_protocol: bool,
        progress_immediately: bool,
    ) -> KResult {
        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
            self.poll_rx_closed.wake();
        }

        // stream
        if self.state() == State::Connected {
            if how.has_write() {
                self.tx_closed.store(true, Ordering::Release);
                if close_protocol {
                    self.with_smol_socket(|socket| {
                        socket.close();
                    });
                }
                if progress_immediately {
                    network_poller().notify(PollReason::Tx);
                }
            }
            if how == Shutdown::Both {
                self.state.set(State::Closed);
                self.unregister_bound_endpoint();
                *self.bound_endpoint.lock() = empty_endpoint();
            }
            if progress_immediately {
                network_poller().assist_once();
            }
        }

        // listener
        if let Ok(guard) = self.state.lock(State::Listening) {
            guard.transit(State::Closed, || {
                let bound_endpoint = self.bound_endpoint()?;
                LISTEN_TABLE.unlisten(bound_endpoint);
                self.unregister_bound_endpoint();
                *self.bound_endpoint.lock() = empty_endpoint();
                if progress_immediately {
                    network_poller().assist_once();
                }
                Ok(())
            })?;
        }

        // ignore for other states
        Ok(())
    }

    fn bound_endpoint(&self) -> KResult<IpListenEndpoint> {
        let endpoint = *self.bound_endpoint.lock();
        if endpoint.port == 0 {
            k_bail!(InvalidInput, "not bound");
        }
        Ok(endpoint)
    }

    /// Copies the next chunk of `src` into the socket's transmit buffer.
    ///
    /// Returns `BrokenPipe` after the send side is closed, the state error when
    /// the connection cannot send, and `WouldBlock` while the buffer is full.
    fn send_to_socket(&self, socket: &mut smol::Socket<'_>, src: &mut impl Read) -> KResult<usize> {
        if self.tx_closed.load(Ordering::Acquire) {
            return Err(KError::BrokenPipe);
        }
        if !socket.may_send() {
            return Err(Self::send_state_error(socket.state()));
        }
        if !socket.can_send() {
            return Err(KError::WouldBlock);
        }
        socket
            .send(|buffer| {
                let result = src.read(buffer);
                let len = result.as_ref().map_or(0, |len| *len);
                (len, result)
            })
            .map_err(|_| k_err_type!(NotConnected, "not connected?"))?
    }

    /// Copies queued data from the socket into `dst`, or peeks it when `peek`.
    ///
    /// Sets `should_poll_rx_window` when a dequeue reopens enough receive
    /// window to be worth advertising. Returns `Ok(0)` at end of stream and
    /// `WouldBlock` while no data is queued.
    fn recv_from_socket(
        socket: &mut smol::Socket<'_>,
        dst: &mut (impl Write + IoBufMut),
        peek: bool,
        should_poll_rx_window: &mut bool,
    ) -> KResult<usize> {
        if !socket.can_recv() {
            return if socket.may_recv() {
                Err(KError::WouldBlock)
            } else {
                Ok(0)
            };
        }
        if peek {
            return dst.write(
                socket
                    .peek(dst.remaining_mut())
                    .map_err(|_| k_err_type!(NotConnected, "not connected?"))?,
            );
        }
        let receive_capacity_bytes = socket.recv_capacity();
        let available_before_bytes = receive_capacity_bytes.saturating_sub(socket.recv_queue());
        let result = socket
            .recv(|buf| {
                let result = dst.write(buf);
                let len = result.unwrap_or(0);
                (len, result)
            })
            .map_err(|_| k_err_type!(NotConnected, "not connected?"))?;
        if let Ok(received) = result {
            *should_poll_rx_window = should_poll_receive_window(
                receive_capacity_bytes,
                available_before_bytes,
                received,
            );
        }
        result
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
                    debug!("TCP socket {}: connected to {}", self.handle, remote);
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
            .and_then(|endpoint| {
                let sockets = SOCKET_SET.inner.lock();
                LISTEN_TABLE.can_accept(endpoint, &sockets).ok()
            })
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
            O::KeepIdle(secs) => {
                **secs = self.keep_idle_secs.load(Ordering::Relaxed);
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
                // TODO(mivik): implement TCP diagnostics
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
                network_poller().notify(PollReason::Tx);
            }
            O::KeepAlive(keep_alive) => {
                // TCP_KEEPIDLE is enforced, not merely recorded: enabling
                // keep-alive installs the configured idle time as the
                // transport's (single) keep-alive interval.
                // TCP_KEEPINTVL/TCP_KEEPCNT are rejected with ENOPROTOOPT:
                // the transport has no per-phase knobs, and silently
                // accepting inert options would hide that gap.
                self.apply_keep_alive(*keep_alive);
                network_poller().notify(PollReason::Timer);
            }
            O::KeepIdle(secs) => {
                // Linux (net/ipv4/tcp.c) rejects TCP_KEEPIDLE outside
                // 1..=TCP_KEEPIDLE_MAX_SECS with EINVAL.
                if *secs == 0 || *secs > TCP_KEEPIDLE_MAX_SECS {
                    return Err(KError::InvalidInput);
                }
                self.set_keep_idle_secs(*secs)?;
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}
impl SocketOps for TcpSocket {
    /// Returns the `SO_TYPE` reported for this socket.
    fn socket_type(&self) -> i32 {
        linux_raw_sys::net::SOCK_STREAM as i32
    }

    fn socket_family(&self) -> i32 {
        linux_raw_sys::net::AF_INET as i32
    }

    fn socket_protocol(&self) -> i32 {
        linux_raw_sys::net::IPPROTO_TCP as i32
    }

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

                Ok(())
            })
    }

    fn connect(&self, remote_addr: SocketAddrEx, options: ConnectOptions) -> KResult {
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
                        Some(SERVICE.get_smoltcp_source_address(&remote_endpoint.addr)?);
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
                    let mut sockets = SOCKET_SET.inner.lock();
                    let mut iface = crate::SERVICE.iface.lock();
                    let context = iface.context();
                    let socket = sockets.get_mut::<smol::Socket>(self.handle);
                    socket
                        .connect(context, remote_endpoint, bound_endpoint)
                        .map_err(|e| match e {
                            smol::ConnectError::InvalidState => k_err_type!(AlreadyConnected),
                            smol::ConnectError::Unaddressable => {
                                k_err_type!(ConnectionRefused, "unaddressable")
                            }
                        })?;
                    Ok::<(), KError>(())
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

                Ok(())
            })?;

        network_poller().notify(PollReason::Tx);

        // Hack: let the server listen
        ktask::yield_now();

        // Here our state must be `CONNECTING`, and only one thread can run here.
        let is_nonblocking = self.general.nonblocking() || options.nonblocking;
        let result = self
            .general
            .send_poller_with_nonblocking(self, is_nonblocking, || {
                network_poller().assist_once();
                let events = self.poll_connect();
                if !events.contains(IoEvents::OUT) {
                    Err(KError::WouldBlock)
                } else if self.state() == State::Connected {
                    Ok(())
                } else {
                    Err(k_err_type!(ConnectionRefused, "connection refused"))
                }
            });
        match result {
            Err(KError::WouldBlock) if is_nonblocking => {
                // The one-shot nonblocking poll does not register a waker.
                // Republish work so TCP keeps progressing after connect
                // returns `EINPROGRESS`.
                network_poller().notify(PollReason::Tx);
                Err(KError::InProgress)
            }
            _ => result,
        }
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

                Ok(())
            })?;
        } else {
            // ignore simultaneous `listen`s.
        }
        Ok(())
    }

    fn accept(&self, options: AcceptOptions) -> KResult<Socket> {
        if !self.is_listening() {
            k_bail!(InvalidInput, "not listening");
        }

        let bound_endpoint = self.bound_endpoint()?;
        self.general
            .recv_poller_with_nonblocking(self, options.nonblocking, || {
                network_poller().assist_once();
                let accepted = {
                    let mut sockets = SOCKET_SET.inner.lock();
                    LISTEN_TABLE.accept(bound_endpoint, &mut sockets)?
                };
                Ok({
                    Socket::Tcp(Box::new(TcpSocket::new_connected(
                        accepted.handle,
                        accepted.local_endpoint,
                        accepted.remote_endpoint,
                    )))
                })
            })
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        let mut total_sent = 0;
        let nonblocking = options.flags.nonblocking();

        while src.remaining() > 0 {
            let result = self
                .general
                .send_poller_with_nonblocking(self, nonblocking, || {
                    self.with_smol_socket(|socket| self.send_to_socket(socket, &mut src))
                });

            match result {
                Ok(0) => break,
                Ok(len) => {
                    network_poller().notify(PollReason::Tx);
                    total_sent += len;
                }
                Err(_) if total_sent > 0 => return Ok(total_sent),
                Err(err) => return Err(err),
            }
        }

        Ok(total_sent)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        if self.rx_closed.load(Ordering::Acquire) {
            return Err(KError::NotConnected);
        }
        let mut should_poll_rx_window = false;
        let received =
            self.general
                .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                    network_poller().assist_once();
                    self.with_smol_socket(|socket| {
                        Self::recv_from_socket(
                            socket,
                            &mut dst,
                            options.flags.contains(RecvFlags::PEEK),
                            &mut should_poll_rx_window,
                        )
                    })
                })?;
        if should_poll_rx_window {
            network_poller().notify(PollReason::RxWindow);
        }
        Ok(received)
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
                socket
                    .remote_endpoint()
                    .or(self.accepted_remote_endpoint)
                    .ok_or(KError::NotConnected)?
                    .into(),
            ))
        })
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        self.shutdown_inner(how, true, true)
    }
}

fn should_poll_receive_window(
    receive_capacity_bytes: usize,
    available_before_bytes: usize,
    received_bytes: usize,
) -> bool {
    // Keep this calculation aligned with smoltcp Socket::new so dependency
    // upgrades cannot silently narrow the zero-window range covered here.
    let capacity_bit_len = usize::BITS as usize - receive_capacity_bytes.leading_zeros() as usize;
    let max_window_scale_shift = capacity_bit_len.saturating_sub(16);
    let max_window_scale_quantum_bytes = 1usize << max_window_scale_shift;

    // A peer without window scaling needs the first notification after a full
    // buffer is consumed. A scaled peer can still see a zero window until one
    // complete scale quantum is free, so each read in that range must poll.
    received_bytes > 0 && available_before_bytes < max_window_scale_quantum_bytes
}

impl Pollable for TcpSocket {
    fn poll(&self) -> IoEvents {
        network_poller().assist_once();
        let mut events = match self.state() {
            State::Connecting => self.poll_connect(),
            State::Connected | State::Idle | State::Closed => self.poll_stream(),
            State::Listening => self.poll_listener(),
            State::Busy => IoEvents::empty(),
        };
        events.set(IoEvents::RDHUP, self.rx_closed.load(Ordering::Acquire));
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::OUT) {
            let source_waker = self.general.register_tx_waker(context)?;
            self.with_smol_socket(|socket| socket.register_send_waker(&source_waker));
        } else if events.intersects(IoEvents::IN | IoEvents::RDHUP) {
            self.general.register_rx_waker(context)?;
        }
        if self.is_listening()
            && events.contains(IoEvents::IN)
            && let Ok(endpoint) = self.bound_endpoint()
        {
            let sockets = SOCKET_SET.inner.lock();
            LISTEN_TABLE.register_accept_waker(endpoint, &sockets, context)?;
        }
        if events.contains(IoEvents::RDHUP) {
            context.register(&self.poll_rx_closed)?;
        }
        Ok(())
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        if let Err(err) = self.shutdown_inner(Shutdown::Both, false, false) {
            warn!("TCP socket {}: shutdown failed: {}", self.handle, err);
        }
        self.unregister_bound_endpoint();

        let mut sockets = SOCKET_SET.inner.lock();
        let socket = sockets.get_mut::<smol::Socket>(self.handle);
        let has_unread_data = socket.can_recv();
        let is_protocol_closed = {
            // Linux aborts a TCP close with unread receive data. Keep the
            // handle until smoltcp dispatches the resulting reset packet.
            if has_unread_data {
                socket.abort();
            } else {
                socket.close();
            }
            socket.state() == smol::State::Closed
        };
        if is_protocol_closed && !has_unread_data && socket.local_endpoint().is_none() {
            sockets.remove(self.handle);
            return;
        }

        sockets.defer_tcp_close(self.handle);
        drop(sockets);
        network_poller().notify(PollReason::Tx);
        network_poller().assist_once();
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
    static_lock! {
        static CURR: Mutex<u16> = Mutex::new(PORT_START);
    }

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

unittest::path_mod_test!(tests, "../../ut-tests/transport/tcp/tests.rs");
