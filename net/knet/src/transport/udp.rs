// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP socket implementation.
use alloc::{boxed::Box, sync::Arc, vec};
use core::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    task::Context,
};

use kerrno::{KError, KResult, k_bail, k_err_type};
use kio::prelude::*;
use kpoll::{IoEvents, Pollable};
use ksync::{Mutex, RwLock, static_lock};
use smoltcp::{
    iface::SocketHandle,
    phy::PacketMeta,
    socket::udp::{self as smol, UdpMetadata},
    storage::PacketMetadata,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint},
};

use crate::{
    KernelAncillaryData, RecvFlags, RecvOptions, SERVICE, SOCKET_SET, SendOptions, Shutdown,
    SocketAddrEx, SocketOps,
    consts::{UDP_RX_BUF_LEN, UDP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    poll_interfaces,
    udp_err::{
        QueuedUdpError, UdpErrorState, register_udp_error_state, unregister_udp_error_state,
    },
};

pub(crate) fn new_udp_socket() -> smol::Socket<'static> {
    // TODO(mivik): buffer size
    smol::Socket::new(
        smol::PacketBuffer::new(vec![PacketMetadata::EMPTY; 256], vec![0; UDP_RX_BUF_LEN]),
        smol::PacketBuffer::new(vec![PacketMetadata::EMPTY; 256], vec![0; UDP_TX_BUF_LEN]),
    )
}

/// A UDP socket that provides POSIX-like APIs.
pub struct UdpSocket {
    dispatch_irq: SocketHandle,
    local_addr: RwLock<Option<IpEndpoint>>,
    peer_addr: RwLock<Option<(IpEndpoint, IpAddress)>>,
    err_state: Arc<UdpErrorState>,
    general: GeneralOptions,
}

impl UdpSocket {
    /// Creates a new UDP socket.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let socket = new_udp_socket();
        let dispatch_irq = SOCKET_SET.add(socket);
        let err_state = Arc::new(UdpErrorState::new(dispatch_irq));

        Self {
            dispatch_irq,
            local_addr: RwLock::new(None),
            peer_addr: RwLock::new(None),
            err_state,
            general: GeneralOptions::new(),
        }
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        SOCKET_SET.with_socket_mut::<smol::Socket, _, _>(self.dispatch_irq, f)
    }

    fn remote_endpoint(&self) -> KResult<(IpEndpoint, IpAddress)> {
        match self.peer_addr.try_read() {
            Some(addr) => addr.ok_or(KError::NotConnected),
            None => Err(KError::NotConnected),
        }
    }
}

impl Configurable for UdpSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        if let GetSocketOption::Error(error) = opt {
            // Drive pending RX work before reading the socket error. UDP asynchronous
            // errors are discovered from incoming ICMP packets while polling
            // devices, and x-kernel currently advances the network stack from
            // explicit poll sites rather than a background softirq.
            poll_interfaces();
            **error = self.err_state.consume_socket_error();
            return Ok(OptionHandled::Yes);
        }
        if self.general.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }
        match opt {
            GetSocketOption::Ttl(ttl) => {
                self.with_smol_socket(|socket| {
                    **ttl = socket.hop_limit().unwrap_or(64);
                });
            }
            GetSocketOption::SendBuffer(size) => {
                **size = UDP_TX_BUF_LEN;
            }
            GetSocketOption::ReceiveBuffer(size) => {
                **size = UDP_RX_BUF_LEN;
            }
            GetSocketOption::RecvErr(recv_err) => {
                **recv_err = self.err_state.recv_err_enabled();
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        if self.general.set_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }
        match opt {
            SetSocketOption::Ttl(ttl) => {
                self.with_smol_socket(|socket| {
                    socket.set_hop_limit(Some(*ttl));
                });
            }
            SetSocketOption::RecvErr(recv_err) => {
                self.err_state.set_recv_err(*recv_err);
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}
impl SocketOps for UdpSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let mut local_addr = local_addr.into_ip()?;
        let mut guard = self.local_addr.write();

        if local_addr.port() == 0 {
            local_addr.set_port(get_ephemeral_port()?);
        }
        if guard.is_some() {
            k_bail!(InvalidInput, "already bound");
        }

        let local_endpoint = IpEndpoint::from(local_addr);
        let endpoint = IpListenEndpoint {
            addr: (!local_endpoint.addr.is_unspecified()).then_some(local_endpoint.addr),
            port: local_endpoint.port,
        };

        if !self.general.reuse_address() {
            // Check if the address is already in use
            SOCKET_SET.udp_bind_check(local_endpoint.addr, local_endpoint.port)?;
        }

        self.with_smol_socket(|socket| {
            socket.bind(endpoint).map_err(|e| match e {
                smol::BindError::InvalidState => k_err_type!(InvalidInput, "already bound"),
                smol::BindError::Unaddressable => k_err_type!(ConnectionRefused, "unaddressable"),
            })
        })?;
        self.general
            .set_device_mask(SERVICE.lock().device_mask_for(&endpoint));

        *guard = Some(local_endpoint);
        self.err_state.set_local_addr(Some(local_endpoint));
        register_udp_error_state(self.err_state.clone());
        info!("UDP socket {}: bound on {}", self.dispatch_irq, endpoint);
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        let remote_addr = remote_addr.into_ip()?;
        let mut guard = self.peer_addr.write();
        if self.local_addr.read().is_none() {
            self.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))?;
        }

        let remote_addr = IpEndpoint::from(remote_addr);
        let src = SERVICE.lock().get_source_address(&remote_addr.addr)?;
        *guard = Some((remote_addr, src));
        self.general
            .set_device_mask(SERVICE.lock().device_mask_for_addr(&remote_addr.addr));
        self.err_state.set_peer_addr(Some((remote_addr, src)));
        debug!(
            "UDP socket {}: connected to {}",
            self.dispatch_irq, remote_addr
        );
        Ok(())
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        let (remote_addr, source_addr) = match options.to {
            Some(addr) => {
                let addr = IpEndpoint::from(addr.into_ip()?);
                let src = SERVICE.lock().get_source_address(&addr.addr)?;
                (addr, src)
            }
            None => self.remote_endpoint()?,
        };
        if remote_addr.port == 0 || remote_addr.addr.is_unspecified() {
            k_bail!(InvalidInput, "invalid address");
        }

        if self.local_addr.read().is_none() {
            self.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))?;
        }
        self.general
            .send_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.is_open() {
                        // not connected
                        Err(k_err_type!(NotConnected))
                    } else if !socket.can_send() {
                        Err(KError::WouldBlock)
                    } else {
                        let buf = socket
                            .send(
                                src.remaining(),
                                UdpMetadata {
                                    endpoint: remote_addr,
                                    local_address: Some(source_addr),
                                    meta: PacketMeta::default(),
                                },
                            )
                            .map_err(|e| match e {
                                smol::SendError::BufferFull => KError::WouldBlock,
                                smol::SendError::Unaddressable => {
                                    k_err_type!(ConnectionRefused, "unaddressable")
                                }
                            })?;
                        let read = src.read(buf)?;
                        assert_eq!(read, buf.len());
                        Ok(read)
                    }
                })
            })
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> KResult<usize> {
        if options.flags.contains(RecvFlags::ERROR_QUEUE) {
            return self.general.recv_poller(self, || {
                poll_interfaces();
                let error = if options.flags.contains(RecvFlags::PEEK) {
                    self.err_state.peek_error()
                } else {
                    self.err_state.pop_error()
                };
                let Some(error) = error else {
                    return Err(KError::WouldBlock);
                };
                let QueuedUdpError {
                    payload,
                    addr,
                    ancillary: recv_error,
                } = error;

                if let Some(from) = options.from.as_deref_mut() {
                    *from = SocketAddrEx::Ip(addr);
                }
                if let Some(ancillary) = options.ancillary.as_deref_mut() {
                    ancillary.push(Box::new(KernelAncillaryData::IpError(recv_error)));
                }

                let read = dst.write(&payload)?;
                if read < payload.len() {
                    warn!(
                        "UDP error payload truncated: {} -> {} bytes",
                        payload.len(),
                        read
                    );
                }

                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    payload.len()
                } else {
                    read
                })
            });
        }

        if self.local_addr.read().is_none() {
            k_bail!(NotConnected);
        }

        enum ExpectedRemote<'a> {
            Any(&'a mut SocketAddrEx),
            Expecting(IpEndpoint),
        }
        let mut expected_remote = match options.from {
            Some(addr) => ExpectedRemote::Any(addr),
            None => ExpectedRemote::Expecting(self.remote_endpoint()?.0),
        };

        self.general.recv_poller(self, || {
            poll_interfaces();
            self.with_smol_socket(|socket| {
                if !socket.is_open() {
                    // not bound
                    Err(k_err_type!(NotConnected))
                } else if !socket.can_recv() {
                    Err(KError::WouldBlock)
                } else {
                    let result = if options.flags.contains(RecvFlags::PEEK) {
                        socket.peek().map(|(data, meta)| (data, *meta))
                    } else {
                        socket.recv()
                    };
                    match result {
                        Ok((src, meta)) => {
                            match &mut expected_remote {
                                ExpectedRemote::Any(remote_addr) => {
                                    **remote_addr = SocketAddrEx::Ip(meta.endpoint.into());
                                }
                                ExpectedRemote::Expecting(expected) => {
                                    if (!expected.addr.is_unspecified()
                                        && expected.addr != meta.endpoint.addr)
                                        || (expected.port != 0
                                            && expected.port != meta.endpoint.port)
                                    {
                                        return Err(KError::WouldBlock);
                                    }
                                }
                            }

                            let read = dst.write(src)?;
                            if read < src.len() {
                                warn!("UDP message truncated: {} -> {} bytes", src.len(), read);
                            }

                            Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                                src.len()
                            } else {
                                read
                            })
                        }
                        Err(smol::RecvError::Exhausted) => Err(KError::WouldBlock),
                        Err(smol::RecvError::Truncated) => {
                            unreachable!("UDP socket recv never returns Err(Truncated)")
                        }
                    }
                }
            })
        })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        match self.local_addr.try_read() {
            Some(addr) => addr
                .map(Into::into)
                .map(SocketAddrEx::Ip)
                .ok_or(KError::NotConnected),
            None => Err(KError::NotConnected),
        }
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        self.remote_endpoint()
            .map(|it| it.0.into())
            .map(SocketAddrEx::Ip)
    }

    fn shutdown(&self, _how: Shutdown) -> KResult {
        // TODO(mivik): shutdown
        poll_interfaces();

        self.with_smol_socket(|socket| {
            debug!("UDP socket {}: shutting down", self.dispatch_irq);
            socket.close();
        });
        Ok(())
    }
}

impl Pollable for UdpSocket {
    fn poll(&self) -> IoEvents {
        poll_interfaces();
        if self.local_addr.read().is_none() {
            return IoEvents::empty();
        }

        let mut events = IoEvents::empty();
        let has_error = self.err_state.has_pending_error();
        self.with_smol_socket(|socket| {
            events.set(IoEvents::IN, socket.can_recv() || has_error);
            events.set(IoEvents::OUT, socket.can_send());
        });
        events.set(IoEvents::ERR, has_error);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.intersects(IoEvents::IN | IoEvents::OUT) {
            self.general.register_rx_waker(context.waker());
        }
        if events.intersects(IoEvents::IN | IoEvents::ERR) {
            self.err_state.register_error_waker(context.waker());
        }
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        self.shutdown(Shutdown::Both).ok();
        unregister_udp_error_state(self.dispatch_irq);
        SOCKET_SET.remove(self.dispatch_irq);
    }
}

fn get_ephemeral_port() -> KResult<u16> {
    const PORT_START: u16 = 0xc000;
    const PORT_END: u16 = 0xffff;
    static_lock! {
        static CURR: Mutex<u16> = Mutex::new(PORT_START);
    }
    let mut curr = CURR.lock();

    let port = *curr;
    if *curr == PORT_END {
        *curr = PORT_START;
    } else {
        *curr += 1;
    }
    Ok(port)
}
