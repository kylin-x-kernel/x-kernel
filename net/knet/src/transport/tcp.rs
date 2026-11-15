// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP socket implementation.
use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    net::{Ipv4Addr, SocketAddr},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::Waker,
};

use hashbrown::HashMap;
use kalloc_types::KBox;
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
    AcceptOptions, ConnectOptions, LISTEN_TABLE, QueueLengths, RecvFlags, RecvOptions, SERVICE,
    SOCKET_SET, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    consts::{
        TCP_KEEPCNT_DEFAULT, TCP_KEEPCNT_MAX, TCP_KEEPIDLE_DEFAULT_SECS, TCP_KEEPIDLE_MAX_SECS,
        TCP_KEEPINTVL_DEFAULT_SECS, TCP_KEEPINTVL_MAX_SECS, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN,
    },
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

/// TCP keep-alive parameters recorded per socket.
///
/// This is deliberately an ABI-only layer: Linux defaults and ranges are
/// enforced, and every accepted value is reported verbatim by `getsockopt`.
/// smoltcp exposes a single keep-alive interval and no probe-count control, so
/// the transport applies the idle value as that interval. `interval_secs` and
/// `probe_count` are recorded configuration, not transport timing.
///
/// # Accepted ABI divergence
///
/// Accepting `TCP_KEEPINTVL` / `TCP_KEEPCNT` while smoltcp keeps using
/// `idle_secs` is an explicit trade-off, not an implementation of Linux probe
/// timing. Callers get a successful `setsockopt`, verbatim `getsockopt`, and
/// Linux defaults/ranges, but probe spacing and the probe-count limit remain
/// those of smoltcp's single interval. This divergence is accepted so the A14
/// ABI surface is complete; returning `ENOPROTOOPT` would instead reject
/// options Linux accepts. Programs that depend on Linux probe timing must not
/// rely on these values until smoltcp exposes separate probe controls.
struct KeepAliveConfig {
    idle_secs: AtomicU32,
    interval_secs: AtomicU32,
    probe_count: AtomicU32,
}

impl Default for KeepAliveConfig {
    fn default() -> Self {
        Self {
            idle_secs: AtomicU32::new(TCP_KEEPIDLE_DEFAULT_SECS),
            interval_secs: AtomicU32::new(TCP_KEEPINTVL_DEFAULT_SECS),
            probe_count: AtomicU32::new(TCP_KEEPCNT_DEFAULT),
        }
    }
}

/// Validates a Linux keep-alive knob; all three accept `1..=max` and reject
/// other values with `EINVAL`.
fn validate_keepalive_range(value: u32, max: u32) -> KResult<()> {
    if value == 0 || value > max {
        return Err(KError::InvalidInput);
    }
    Ok(())
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
    poll_rx: Arc<PollSet>,
    poll_tx: Arc<PollSet>,
    keep_alive: KeepAliveConfig,
    /// Whether a smoltcp keep-alive deadline may still be armed.
    ///
    /// smoltcp does not clear an armed deadline when keep-alive is disabled, so
    /// this stays set for the lifetime of the socket after the first enable.
    keep_alive_deadline_armed: AtomicBool,
    is_abortive_close: AtomicBool,
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
            poll_rx: Arc::new(PollSet::new()),
            poll_tx: Arc::new(PollSet::new()),
            keep_alive: KeepAliveConfig::default(),
            keep_alive_deadline_armed: AtomicBool::new(false),
            is_abortive_close: AtomicBool::new(false),
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
            poll_rx: Arc::new(PollSet::new()),
            poll_tx: Arc::new(PollSet::new()),
            keep_alive: KeepAliveConfig::default(),
            keep_alive_deadline_armed: AtomicBool::new(false),
            is_abortive_close: AtomicBool::new(false),
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

    /// Applies `SO_KEEPALIVE` using the recorded `TCP_KEEPIDLE`.
    ///
    /// smoltcp uses one interval for both the initial idle delay and probe
    /// spacing, and cannot recalculate a deadline that is already pending.
    /// Enabling keep-alive on an established socket also arms smoltcp's first
    /// probe at `Instant::ZERO`, so that probe may be sent on the next poll
    /// instead of after `TCP_KEEPIDLE`; later probes use the recorded idle
    /// interval. Live `TCP_KEEPIDLE` updates are therefore rejected by
    /// [`Self::set_keep_idle_secs`] instead of accepting a value that would not
    /// take effect.
    fn apply_keep_alive(&self, enabled: bool) {
        self.with_smol_socket(|socket| {
            if enabled {
                self.keep_alive_deadline_armed
                    .store(true, Ordering::Relaxed);
                let idle_secs = u64::from(self.keep_alive.idle_secs.load(Ordering::Relaxed));
                socket.set_keep_alive(Some(Duration::from_secs(idle_secs)));
            } else {
                // smoltcp clears the option but leaves an already armed
                // deadline in place, so one stale probe may still be sent.
                // Re-enabling before that deadline fires reuses it instead of
                // restarting the idle period.
                socket.set_keep_alive(None);
            }
        });
    }

    /// Stores `TCP_KEEPIDLE` before keep-alive is first enabled.
    ///
    /// smoltcp only arms its keep-alive deadline when none is pending and
    /// exposes no way to recalculate an existing one. Accepting a live update
    /// would therefore silently keep the old interval. The rejection also
    /// covers a later disable, because smoltcp can keep the old deadline armed
    /// and a subsequent enable would otherwise reuse it. Keeping the recorded
    /// value unchanged is intentional: `getsockopt` then still describes the
    /// interval that the pending deadline was armed with, instead of reporting
    /// a new value that will not take effect.
    fn set_keep_idle_secs(&self, idle_secs: u32) -> KResult {
        self.with_smol_socket(|socket| {
            if self.keep_alive_deadline_armed.load(Ordering::Relaxed)
                || socket.keep_alive().is_some()
            {
                return Err(KError::OperationNotSupported);
            }
            self.keep_alive
                .idle_secs
                .store(idle_secs, Ordering::Relaxed);
            Ok(())
        })
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        SOCKET_SET.with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }

    fn with_smol_socket_error<R>(
        &self,
        f: impl FnOnce(&mut smol::Socket, &mut Option<KError>) -> R,
    ) -> R {
        SOCKET_SET.inner.lock().with_tcp_socket_mut(self.handle, f)
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
            self.poll_rx.wake();
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
    /// Sets `should_poll_rx_window` after consuming data so protocol processing
    /// can decide whether to advertise the enlarged window. Returns `Ok(0)` at
    /// end of stream and `WouldBlock` while no data is queued.
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
        let result = socket
            .recv(|buf| {
                let result = dst.write(buf);
                let len = result.unwrap_or(0);
                (len, result)
            })
            .map_err(|_| k_err_type!(NotConnected, "not connected?"))?;
        if let Ok(received) = result {
            // A positive advertised window can still be too small for the peer
            // to send. Reevaluate it after every read that frees buffer space.
            *should_poll_rx_window = received > 0;
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
        self.with_smol_socket_error(|socket, error| {
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
            events.set(IoEvents::ERR, error.is_some());
            events.set(
                IoEvents::HUP,
                self.state() == State::Connected && socket.state() == smol::State::Closed,
            );
            events.set(
                IoEvents::RDHUP,
                matches!(
                    socket.state(),
                    smol::State::CloseWait
                        | smol::State::LastAck
                        | smol::State::Closing
                        | smol::State::TimeWait
                        | smol::State::Closed
                ) && self.state() == State::Connected,
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

        if let O::Error(error) = option {
            **error = self.with_smol_socket_error(|_, pending| {
                pending
                    .take()
                    .map_or(0, |error| kerrno::LinuxError::from(error).into_raw())
            });
            return Ok(OptionHandled::Yes);
        }
        if let O::AcceptConn(listening) = option {
            **listening = self.is_listening();
            return Ok(OptionHandled::Yes);
        }
        if self.general.get_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::Linger(linger) => {
                **linger = self
                    .is_abortive_close
                    .load(Ordering::Relaxed)
                    .then_some(ktime_types::TimeSpan::ZERO);
            }
            O::Ttl(ttl) => {
                **ttl = self.with_smol_socket(|socket| socket.hop_limit().unwrap_or(64));
            }
            O::NoDelay(no_delay) => {
                **no_delay = self.with_smol_socket(|socket| !socket.nagle_enabled());
            }
            O::KeepAlive(keep_alive) => {
                **keep_alive = self.with_smol_socket(|socket| socket.keep_alive().is_some());
            }
            O::KeepIdle(secs) => {
                **secs = self.keep_alive.idle_secs.load(Ordering::Relaxed);
            }
            O::KeepInterval(secs) => {
                **secs = self.keep_alive.interval_secs.load(Ordering::Relaxed);
            }
            O::KeepCount(count) => {
                **count = self.keep_alive.probe_count.load(Ordering::Relaxed);
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
            O::Linger(linger) => {
                if linger.is_some_and(|duration| !duration.is_zero()) {
                    return Err(KError::OperationNotSupported);
                }
                // This independent close policy does not publish other state.
                self.is_abortive_close
                    .store(linger.is_some(), Ordering::Relaxed);
            }
            O::Ttl(ttl) => {
                if *ttl == 0 {
                    return Err(KError::InvalidInput);
                }
                self.with_smol_socket(|socket| socket.set_hop_limit(Some(*ttl)));
            }
            O::NoDelay(no_delay) => {
                self.with_smol_socket(|socket| {
                    socket.set_nagle_enabled(!no_delay);
                });
                network_poller().notify(PollReason::Tx);
            }
            O::KeepAlive(keep_alive) => {
                // TCP_KEEPIDLE is applied to smoltcp's single keep-alive
                // interval when keep-alive is enabled. TCP_KEEPINTVL and
                // TCP_KEEPCNT are recorded for readback, but the transport has
                // no per-phase timing or probe-count controls.
                self.apply_keep_alive(*keep_alive);
                network_poller().notify(PollReason::Timer);
            }
            O::KeepIdle(secs) => {
                // Linux (net/ipv4/tcp.c) rejects TCP_KEEPIDLE outside
                // 1..=TCP_KEEPIDLE_MAX_SECS with EINVAL.
                validate_keepalive_range(*secs, TCP_KEEPIDLE_MAX_SECS)?;
                self.set_keep_idle_secs(*secs)?;
            }
            O::KeepInterval(secs) => {
                // Linux rejects TCP_KEEPINTVL outside
                // 1..=TCP_KEEPINTVL_MAX_SECS with EINVAL. Recording without
                // changing smoltcp timing is the accepted ABI divergence
                // documented on KeepAliveConfig.
                validate_keepalive_range(*secs, TCP_KEEPINTVL_MAX_SECS)?;
                self.keep_alive
                    .interval_secs
                    .store(*secs, Ordering::Relaxed);
            }
            O::KeepCount(count) => {
                // Linux rejects TCP_KEEPCNT outside 1..=TCP_KEEPCNT_MAX with
                // EINVAL. Recording without changing smoltcp timing is the
                // accepted ABI divergence documented on KeepAliveConfig.
                validate_keepalive_range(*count, TCP_KEEPCNT_MAX)?;
                self.keep_alive.probe_count.store(*count, Ordering::Relaxed);
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

    fn is_listening(&self) -> bool {
        TcpSocket::is_listening(self)
    }

    fn queue_lengths(&self) -> KResult<Option<QueueLengths>> {
        // `tcp_ioctl(SIOCINQ)` reports `tcp_inq()` -- the bytes queued for
        // reading -- and answers EINVAL to a listening socket, which holds no
        // data of its own. `SIOCOUTQ` reports `tp->write_seq - tp->snd_una`,
        // the bytes handed to the peer but not yet acknowledged; smoltcp's
        // `send_queue()` is the unsent transmit buffer instead, so it is the
        // closest quantity this stack exposes and is reported as such.
        if self.is_listening() {
            return Err(KError::InvalidInput);
        }
        // A closed or time-wait socket still answers: `tcp_inq` reports its
        // (usually empty) receive queue rather than an error, and only the
        // listening state is refused. `recv_queue()`/`send_queue()` remain
        // readable in those states, so no state check beyond listening is
        // needed here.
        //
        // An ioctl can arrive after the transport was torn down (the caller
        // still holds the descriptor while the peer-close path removed the
        // smoltcp socket), and smoltcp's accessors panic for a dead handle.
        // A missing socket reports an idle connection instead.
        let Some((received, queued)) = SOCKET_SET.try_with_socket_mut(self.handle, |socket| {
            (socket.recv_queue(), socket.send_queue())
        }) else {
            return Ok(Some(QueueLengths::default()));
        };
        Ok(Some(QueueLengths::from_bytes(queued, received)))
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
                let mut remote_endpoint = IpEndpoint::from(remote_addr);
                let mut bound_endpoint = *self.bound_endpoint.lock();
                let (source, destination) = SERVICE.resolve_ip_output(
                    crate::ip::from_smoltcp_ip_address(remote_endpoint.addr),
                    bound_endpoint.addr.map(crate::ip::from_smoltcp_ip_address),
                    self.general.bound_dev_if(),
                )?;
                remote_endpoint.addr = crate::ip::to_smoltcp_ip_address(destination);
                bound_endpoint.addr = Some(crate::ip::to_smoltcp_ip_address(source));
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
                if let Err(err) = LISTEN_TABLE.listen(bound_endpoint, backlog, self.handle) {
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
            // Existing interests must switch from stream to accept sources.
            self.poll_rx.wake();
            self.poll_tx.wake();
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
        let storage = KBox::<Self>::new_uninit()?;
        let accepted =
            self.general
                .recv_poller_with_nonblocking(self, options.nonblocking, || {
                    network_poller().assist_once();
                    let accepted = {
                        let mut sockets = SOCKET_SET.inner.lock();
                        LISTEN_TABLE.accept(bound_endpoint, &mut sockets)?
                    };
                    Ok(accepted)
                })?;
        let connected = TcpSocket::new_connected(
            accepted.handle,
            accepted.local_endpoint,
            accepted.remote_endpoint,
        );
        connected.is_abortive_close.store(
            self.is_abortive_close.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        Ok(Socket::Tcp(KBox::write(storage, connected)))
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        let mut total_sent = 0;
        let nonblocking = options.flags.nonblocking();

        while src.remaining() > 0 {
            let result = self
                .general
                .send_poller_with_nonblocking(self, nonblocking, || {
                    self.with_smol_socket_error(|socket, error| {
                        if let Some(error) = error.take() {
                            return Err(error);
                        }
                        self.send_to_socket(socket, &mut src)
                    })
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
                    self.with_smol_socket_error(|socket, error| {
                        // Deliver queued bytes before the pending reset; a
                        // read, write or SO_ERROR consumes that error once.
                        if !socket.can_recv()
                            && !matches!(socket.peek(0), Err(smol::RecvError::Finished))
                            && let Some(error) = error.take()
                        {
                            return Err(error);
                        }
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

impl Pollable for TcpSocket {
    fn poll(&self) -> IoEvents {
        network_poller().assist_once();
        let mut events = match self.state() {
            State::Connecting => self.poll_connect(),
            State::Connected | State::Idle | State::Closed => self.poll_stream(),
            State::Listening => self.poll_listener(),
            State::Busy => IoEvents::empty(),
        };
        if self.rx_closed.load(Ordering::Acquire) {
            events.insert(IoEvents::RDHUP);
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        let read_events = IoEvents::IN | IoEvents::RDHUP | IoEvents::ERR | IoEvents::HUP;
        if events.intersects(read_events) {
            context.register(&self.poll_rx)?;
        }
        if events.contains(IoEvents::OUT) {
            context.register(&self.poll_tx)?;
        }
        if self.is_listening() {
            if events.contains(IoEvents::IN)
                && let Ok(endpoint) = self.bound_endpoint()
            {
                LISTEN_TABLE.register_accept_waker(endpoint, context)?;
            }
        } else {
            // smoltcp has one waker slot per direction. Aggregate all waiters
            // for this socket without subscribing them to unrelated traffic.
            self.with_smol_socket(|socket| {
                if events.intersects(read_events) {
                    socket.register_recv_waker(&Waker::from(self.poll_rx.clone()));
                }
                if events.contains(IoEvents::OUT) {
                    socket.register_send_waker(&Waker::from(self.poll_tx.clone()));
                }
            });
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
            if has_unread_data || self.is_abortive_close.load(Ordering::Relaxed) {
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
