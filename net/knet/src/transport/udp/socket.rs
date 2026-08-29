// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use ::core::net::SocketAddr;
use kerrno::{KError, KResult, LinuxError, k_bail};
use kio::prelude::*;
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};

use super::{
    IPV4_HEADER_LEN, UDP_HEADER_LEN, UDP_MAX_PAYLOAD_LEN,
    output::{ipv4_pair, write_udp_header},
    pcb::{RecvMode, UdpDatagram, UdpPcb},
    registry,
    state::UdpSocketState,
};
use crate::{
    AncillaryData, ConnectOptions, KernelAncillaryData, RecvFlags, RecvOptions, SERVICE, SendFlags,
    SendOptions, Shutdown, SocketAddrEx, SocketOps,
    consts::{UDP_RX_BUF_LEN, UDP_TX_BUF_LEN},
    ip::{IpAddress, IpEndpoint, Ipv4Address},
    ipv4,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    poller::{PollReason, assist_once, network_poller},
    udp_err::QueuedUdpError,
};

const IP_PMTUDISC_DONT: u8 = 0;
const IP_PMTUDISC_WANT: u8 = 1;
const IP_PMTUDISC_DO: u8 = 2;
const IP_PMTUDISC_PROBE: u8 = 3;
const IP_PMTUDISC_INTERFACE: u8 = 4;
const IP_PMTUDISC_OMIT: u8 = 5;

struct RecvErrorMsg<'a> {
    data: &'a mut [u8],
    from: Option<&'a mut SocketAddrEx>,
    flags: RecvFlags,
    ancillary: Option<&'a mut Vec<AncillaryData>>,
    out_flags: Option<&'a mut RecvFlags>,
}

/// A UDP socket that provides POSIX-like APIs.
pub struct UdpSocket {
    pub(super) pcb: Arc<UdpPcb>,
}

impl UdpSocket {
    /// Creates a new UDP socket.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        registry::init_udp_registry();
        let state = UdpSocketState::new();
        Self {
            pcb: UdpPcb::new(state),
        }
    }

    pub(super) fn state(&self) -> &Arc<UdpSocketState> {
        &self.pcb.state
    }

    pub(crate) fn send_datagram_now(
        &self,
        remote_addr: SocketAddr,
        payload: &[u8],
    ) -> KResult<usize> {
        SocketOps::send(
            self,
            payload,
            SendOptions {
                to: Some(SocketAddrEx::Ip(remote_addr)),
                flags: SendFlags::DONT_WAIT,
                ancillary: Vec::new(),
            },
        )
    }

    pub(crate) fn recv_datagram_now(&self, buf: &mut [u8]) -> KResult<Option<(usize, SocketAddr)>> {
        let mut from = SocketAddrEx::Unspecified;
        match SocketOps::recv(
            self,
            &mut buf[..],
            RecvOptions {
                from: Some(&mut from),
                flags: RecvFlags::DONT_WAIT,
                ancillary: None,
                out_flags: None,
            },
        ) {
            Ok(len) => Ok(Some((len, from.into_ip()?))),
            Err(KError::WouldBlock) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn remote_endpoint(&self) -> KResult<(IpEndpoint, IpAddress)> {
        self.state().peer_endpoint().ok_or(KError::NotConnected)
    }

    fn bind_endpoint(&self, local_endpoint: IpEndpoint) -> KResult {
        if self.state().local_endpoint().is_some() {
            k_bail!(InvalidInput, "already bound");
        }

        let reuse_address = self.state().reuse_address();
        registry::bind_udp_pcb(self.pcb.clone(), local_endpoint, reuse_address)?;
        let endpoint = registry::listen_endpoint(local_endpoint);
        self.state()
            .set_device_mask(SERVICE.device_mask_for(&endpoint));
        info!("UDP socket: bound on {}", endpoint);
        Ok(())
    }

    fn bind_ephemeral(&self) -> KResult {
        if self.state().local_endpoint().is_some() {
            k_bail!(InvalidInput, "already bound");
        }

        let reuse_address = self.state().reuse_address();
        let endpoint = registry::bind_udp_auto_ephemeral_pcb(
            self.pcb.clone(),
            Ipv4Address::UNSPECIFIED.into(),
            reuse_address,
        )?;
        self.state()
            .set_device_mask(SERVICE.device_mask_for(&registry::listen_endpoint(endpoint)));
        info!(
            "UDP socket: bound on {}",
            registry::listen_endpoint(endpoint)
        );
        Ok(())
    }

    fn disconnect(&self) {
        self.state().set_peer_endpoint(None);
        self.state().clear_error_queue();
        if registry::is_udp_pcb_explicitly_bound(&self.pcb) {
            if let Some(local) = self.state().local_endpoint() {
                self.state()
                    .set_device_mask(SERVICE.device_mask_for(&registry::listen_endpoint(local)));
            }
            return;
        }

        registry::unregister_udp_pcb(&self.pcb);
        self.state().set_local_endpoint(None);
        self.state().set_device_mask(0);
    }

    fn send_reader(
        &self,
        src: &mut (impl Read + IoBuf),
        to: Option<SocketAddrEx>,
        flags: SendFlags,
    ) -> KResult<usize> {
        if self.state().is_write_shutdown() {
            return Err(KError::BrokenPipe);
        }

        let remote_endpoint = match to {
            Some(addr) => IpEndpoint::from(addr.into_ip()?),
            None => self
                .remote_endpoint()
                .map(|(endpoint, _)| endpoint)
                .map_err(|_| KError::from(LinuxError::EDESTADDRREQ))?,
        };
        validate_remote_endpoint(remote_endpoint)?;
        let allow_broadcast = self.state().broadcast();
        if matches!(&remote_endpoint.addr, IpAddress::Ipv4(addr) if addr.is_broadcast())
            && !allow_broadcast
        {
            return Err(KError::from(LinuxError::EACCES));
        }

        if self.state().local_endpoint().is_none() {
            self.bind_ephemeral()?;
        }

        let payload_len = src.remaining();
        if payload_len > UDP_MAX_PAYLOAD_LEN {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let packet_len = IPV4_HEADER_LEN + UDP_HEADER_LEN + payload_len;
        let mut packet = Some(vec![0u8; packet_len]);
        src.read_exact(
            packet
                .as_mut()
                .and_then(|packet| packet.get_mut(IPV4_HEADER_LEN + UDP_HEADER_LEN..))
                .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?,
        )?;
        self.state()
            .send_poller_with_nonblocking(self, flags.nonblocking(), || {
                let bound_source_addr = self
                    .state()
                    .local_endpoint()
                    .and_then(|local| (!local.addr.is_unspecified()).then_some(local.addr));
                SERVICE.prepare_and_send_ipv4_packet(
                    bound_source_addr,
                    &remote_endpoint.addr,
                    self.state().bound_dev_if(),
                    allow_broadcast,
                    &mut packet,
                    |packet, source_addr, route_mtu| {
                        let mtu_discovery = *self.pcb.mtu_discovery.read();
                        let dont_fragment =
                            should_set_dont_fragment(mtu_discovery, packet_len, route_mtu);
                        self.write_datagram(packet, remote_endpoint, source_addr, dont_fragment)
                    },
                )?;
                network_poller().notify(PollReason::Tx);
                assist_once();
                Ok(payload_len)
            })
    }

    fn source_addr_for(&self, remote_addr: IpAddress) -> KResult<IpAddress> {
        if let Some(local) = self.state().local_endpoint()
            && !local.addr.is_unspecified()
        {
            return Ok(local.addr);
        }
        SERVICE.get_source_address(&remote_addr)
    }

    fn write_datagram(
        &self,
        packet: &mut [u8],
        remote_endpoint: IpEndpoint,
        source_addr: IpAddress,
        dont_fragment: bool,
    ) -> KResult {
        let local = self
            .state()
            .local_endpoint()
            .ok_or(KError::from(LinuxError::EDESTADDRREQ))?;
        let source_port = local.port;
        let ttl = *self.pcb.ttl.read();
        let (src_addr, dst_addr) = ipv4_pair(source_addr, remote_endpoint.addr)?;
        write_udp_header(
            packet
                .get_mut(IPV4_HEADER_LEN..)
                .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?,
            src_addr,
            dst_addr,
            source_port,
            remote_endpoint.port,
        )
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
        ipv4::write_ipv4_packet_header(
            packet,
            src_addr,
            dst_addr,
            ipv4::PROTOCOL_UDP,
            ttl,
            dont_fragment,
        )
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))
    }

    fn recv_payload(
        &self,
        dst: &mut (impl Write + IoBufMut),
        from: Option<&mut SocketAddrEx>,
        flags: RecvFlags,
        out_flags: Option<&mut RecvFlags>,
    ) -> KResult<usize> {
        if let Some(error) = self.state().take_socket_error() {
            return Err(error);
        }
        if self.state().is_read_shutdown() {
            return Ok(0);
        }

        let mode = recv_mode_from_flags(flags);
        let datagram = match self.try_recv_datagram(mode) {
            Some(datagram) => datagram,
            None => {
                assist_once();
                self.try_recv_datagram(mode).ok_or(KError::WouldBlock)?
            }
        };

        if let Some(remote_addr) = from {
            *remote_addr = SocketAddrEx::Ip(datagram.remote_addr);
        }

        copy_udp_payload_to(datagram.payload.as_slice(), dst, flags, out_flags)
    }

    fn try_recv_datagram(&self, mode: RecvMode) -> Option<UdpDatagram> {
        self.pcb.recv_datagram(mode)
    }

    fn recv_error_msg(&self, msg: &mut RecvErrorMsg<'_>) -> KResult<usize> {
        assist_once();
        let error = if msg.flags.contains(RecvFlags::PEEK) {
            self.state().peek_error()
        } else {
            self.state().pop_error()
        };
        let Some(error) = error else {
            return Err(KError::WouldBlock);
        };
        let QueuedUdpError {
            payload,
            addr,
            ancillary: recv_error,
        } = error;

        if let Some(from) = msg.from.as_deref_mut() {
            *from = SocketAddrEx::Ip(addr);
        }
        if let Some(ancillary) = msg.ancillary.as_deref_mut() {
            ancillary.push(Box::new(KernelAncillaryData::IpError(recv_error)));
        }

        Ok(copy_udp_error_payload(&payload, msg))
    }
}

impl Configurable for UdpSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        if let GetSocketOption::Error(error) = opt {
            assist_once();
            **error = self.state().consume_socket_error();
            return Ok(OptionHandled::Yes);
        }
        if self.state().get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }
        match opt {
            GetSocketOption::Ttl(ttl) => {
                **ttl = *self.pcb.ttl.read();
            }
            GetSocketOption::SendBuffer(size) => {
                **size = UDP_TX_BUF_LEN;
            }
            GetSocketOption::ReceiveBuffer(size) => {
                **size = UDP_RX_BUF_LEN;
            }
            GetSocketOption::RecvErr(recv_err) => {
                **recv_err = self.state().recv_err_enabled();
            }
            GetSocketOption::MtuDiscover(mtu_discovery) => {
                **mtu_discovery = *self.pcb.mtu_discovery.read();
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        if self.state().set_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }
        match opt {
            SetSocketOption::Ttl(ttl) => {
                if *ttl == 0 {
                    return Err(KError::InvalidInput);
                }
                *self.pcb.ttl.write() = *ttl;
            }
            SetSocketOption::RecvErr(recv_err) => {
                self.state().set_recv_err(*recv_err);
            }
            SetSocketOption::MtuDiscover(mtu_discovery) => {
                if *mtu_discovery > IP_PMTUDISC_OMIT {
                    return Err(KError::InvalidInput);
                }
                *self.pcb.mtu_discovery.write() = *mtu_discovery;
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}

impl SocketOps for UdpSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let local_addr = local_addr.into_ip()?;
        if !matches!(local_addr, SocketAddr::V4(_)) {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }

        if local_addr.port() == 0 {
            if self.state().local_endpoint().is_some() {
                k_bail!(InvalidInput, "already bound");
            }

            let reuse_address = self.state().reuse_address();
            let endpoint = registry::bind_udp_explicit_ephemeral_pcb(
                self.pcb.clone(),
                local_addr.ip().into(),
                reuse_address,
            )?;
            self.state()
                .set_device_mask(SERVICE.device_mask_for(&registry::listen_endpoint(endpoint)));
            info!(
                "UDP socket: bound on {}",
                registry::listen_endpoint(endpoint)
            );
            return Ok(());
        }

        self.bind_endpoint(IpEndpoint::from(local_addr))
    }

    fn connect(&self, remote_addr: SocketAddrEx, _options: ConnectOptions) -> KResult {
        if matches!(remote_addr, SocketAddrEx::Unspecified) {
            self.disconnect();
            debug!("UDP socket: disconnected");
            return Ok(());
        }

        let remote_addr = remote_addr.into_ip()?;
        if !matches!(remote_addr, SocketAddr::V4(_)) {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
        if self.state().local_endpoint().is_none() {
            self.bind_ephemeral()?;
        }

        let remote_endpoint = IpEndpoint::from(remote_addr);
        validate_remote_endpoint(remote_endpoint)?;
        let source_addr = self.source_addr_for(remote_endpoint.addr)?;
        self.state()
            .set_peer_endpoint(Some((remote_endpoint, source_addr)));
        self.state()
            .set_device_mask(SERVICE.device_mask_for_addr(&remote_endpoint.addr));
        debug!("UDP socket: connected to {}", remote_endpoint);
        Ok(())
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        let SendOptions {
            to,
            flags,
            ancillary: _,
        } = options;
        let initial_len = src.remaining();
        self.send_reader(&mut src, to, flags)?;
        Ok(initial_len)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, options: RecvOptions) -> KResult<usize> {
        let RecvOptions {
            mut from,
            flags,
            ancillary,
            mut out_flags,
        } = options;
        if !flags.contains(RecvFlags::ERROR_QUEUE) {
            return self
                .state()
                .recv_poller_with_nonblocking(self, flags.nonblocking(), || {
                    self.recv_payload(
                        &mut dst,
                        from.as_deref_mut(),
                        flags,
                        out_flags.as_deref_mut(),
                    )
                });
        }

        let mut data = vec![0; dst.remaining_mut().min(UDP_RX_BUF_LEN)];
        let mut error_msg = RecvErrorMsg {
            data: &mut data,
            from,
            flags,
            ancillary,
            out_flags,
        };
        self.state()
            .recv_poller_with_nonblocking(self, flags.nonblocking(), || {
                let recv = self.recv_error_msg(&mut error_msg)?;
                let write_len = recv.min(error_msg.data.len());
                dst.write_all(&error_msg.data[..write_len])?;
                Ok(recv)
            })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        self.state()
            .local_endpoint()
            .map(Into::into)
            .map(SocketAddrEx::Ip)
            .ok_or(KError::NotConnected)
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        self.remote_endpoint()
            .map(|it| it.0.into())
            .map(SocketAddrEx::Ip)
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        self.state().shutdown(how);
        assist_once();
        debug!("UDP socket: shutting down");
        Ok(())
    }
}

fn validate_remote_endpoint(endpoint: IpEndpoint) -> KResult {
    if !matches!(endpoint.addr, IpAddress::Ipv4(_)) {
        return Err(KError::from(LinuxError::EAFNOSUPPORT));
    }
    if endpoint.port == 0 || endpoint.addr.is_unspecified() {
        k_bail!(InvalidInput, "invalid address");
    }
    Ok(())
}

fn should_set_dont_fragment(mtu_discovery: u8, packet_len: usize, path_mtu: Option<usize>) -> bool {
    match mtu_discovery {
        IP_PMTUDISC_DO | IP_PMTUDISC_PROBE => true,
        IP_PMTUDISC_WANT => path_mtu.is_none_or(|mtu| packet_len <= mtu),
        IP_PMTUDISC_DONT | IP_PMTUDISC_INTERFACE | IP_PMTUDISC_OMIT => false,
        _ => false,
    }
}

fn recv_mode_from_flags(flags: RecvFlags) -> RecvMode {
    if flags.contains(RecvFlags::PEEK) {
        RecvMode::Peek
    } else {
        RecvMode::Consume
    }
}

fn copy_udp_error_payload(payload: &[u8], msg: &mut RecvErrorMsg<'_>) -> usize {
    let write_len = payload.len().min(msg.data.len());
    msg.data[..write_len].copy_from_slice(&payload[..write_len]);
    if write_len < payload.len()
        && let Some(out_flags) = msg.out_flags.as_deref_mut()
    {
        *out_flags |= RecvFlags::TRUNCATE;
    }

    if msg.flags.contains(RecvFlags::TRUNCATE) {
        payload.len()
    } else {
        write_len
    }
}

fn copy_udp_payload_to(
    payload: &[u8],
    dst: &mut (impl Write + IoBufMut),
    flags: RecvFlags,
    out_flags: Option<&mut RecvFlags>,
) -> KResult<usize> {
    let write_len = payload.len().min(dst.remaining_mut());
    dst.write_all(&payload[..write_len])?;
    if write_len < payload.len()
        && let Some(out_flags) = out_flags
    {
        *out_flags |= RecvFlags::TRUNCATE;
    }

    if flags.contains(RecvFlags::TRUNCATE) {
        Ok(payload.len())
    } else {
        Ok(write_len)
    }
}

impl Pollable for UdpSocket {
    fn poll(&self) -> IoEvents {
        assist_once();
        let mut events = IoEvents::empty();
        let can_receive = self.pcb.has_recv_data()
            || self.state().has_pending_error()
            || self.state().has_socket_error();
        events.set(IoEvents::IN, can_receive);
        events.set(
            IoEvents::OUT,
            !self.state().is_write_shutdown() && SERVICE.can_send_ip_packet(),
        );
        self.state().readiness(events)
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND) {
            self.state().register_rx_waker(context)?;
        }
        if events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND) {
            self.state().register_tx_waker(context)?;
        }
        // Network progress and socket-local state changes use separate wake
        // sources, so a read-write waiter is intentionally registered in both.
        if events.intersects(
            IoEvents::IN
                | IoEvents::RDNORM
                | IoEvents::RDBAND
                | IoEvents::OUT
                | IoEvents::WRNORM
                | IoEvents::WRBAND
                | IoEvents::ERR
                | IoEvents::HUP
                | IoEvents::RDHUP,
        ) {
            self.state().register_waiter(context, events)?;
        }
        Ok(())
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        self.shutdown(Shutdown::Both).ok();
        registry::unregister_udp_pcb(&self.pcb);
    }
}

#[cfg(unittest)]
mod tests {
    use ::core::net::{Ipv4Addr, SocketAddrV4};
    use unittest::def_test;

    use super::{super::output::has_valid_udp_checksum, *};

    #[def_test]
    fn error_msg_trunc_returns_original_payload_len() {
        let mut data = [0u8; 2];
        let mut flags = RecvFlags::empty();
        let mut msg = RecvErrorMsg {
            data: &mut data,
            from: None,
            flags: RecvFlags::TRUNCATE,
            ancillary: None,
            out_flags: Some(&mut flags),
        };

        let copied = copy_udp_error_payload(&[1, 2, 3, 4], &mut msg);

        assert_eq!(copied, 4);
        assert_eq!(msg.data, &[1, 2]);
        assert!(flags.contains(RecvFlags::TRUNCATE));
    }

    #[def_test]
    fn udp_datagram_is_written_into_preallocated_ipv4_packet() {
        let socket = UdpSocket::new();
        let src_addr = Ipv4Address::new(192, 0, 2, 1);
        let dst_addr = Ipv4Address::new(192, 0, 2, 2);
        let local_endpoint: IpEndpoint =
            SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234).into();
        let remote_endpoint: IpEndpoint =
            SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 4321).into();
        socket.state().set_local_endpoint(Some(local_endpoint));
        let mut packet = vec![0u8; IPV4_HEADER_LEN + UDP_HEADER_LEN + 4];
        packet[IPV4_HEADER_LEN + UDP_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);

        socket
            .write_datagram(
                &mut packet,
                remote_endpoint,
                IpAddress::Ipv4(src_addr),
                false,
            )
            .unwrap();

        let header = ipv4::Ipv4Header::parse_input(&packet).unwrap();
        let udp_packet = &packet[header.header_len()..];
        assert_eq!(header.total_len(), packet.len());
        assert!(has_valid_udp_checksum(src_addr, dst_addr, udp_packet));
        assert_eq!(&udp_packet[UDP_HEADER_LEN..], &[1, 2, 3, 4]);
    }

    #[def_test]
    fn mtu_discovery_selects_df_like_linux_ipv4_output() {
        assert!(!should_set_dont_fragment(
            IP_PMTUDISC_DONT,
            1500,
            Some(1500)
        ));
        assert!(should_set_dont_fragment(IP_PMTUDISC_WANT, 1500, Some(1500)));
        assert!(!should_set_dont_fragment(
            IP_PMTUDISC_WANT,
            1501,
            Some(1500)
        ));
        assert!(should_set_dont_fragment(IP_PMTUDISC_DO, 1501, Some(1500)));
        assert!(should_set_dont_fragment(
            IP_PMTUDISC_PROBE,
            1501,
            Some(1500)
        ));
        assert!(!should_set_dont_fragment(
            IP_PMTUDISC_INTERFACE,
            1500,
            Some(1500)
        ));
        assert!(!should_set_dont_fragment(
            IP_PMTUDISC_OMIT,
            1500,
            Some(1500)
        ));
    }
}
