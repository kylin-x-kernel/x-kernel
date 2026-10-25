// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AF_PACKET socket implementation for Ethernet DIX frames.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};

use ::core::{
    ops::Range,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use klazy::lazy_static;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use crate::{
    ConnectOptions, RecvFlags, RecvOptions, SERVICE, SendOptions, Shutdown, SocketAddrEx,
    SocketOps,
    buf::{PacketBuf, PacketType},
    device::{ARPHRD_ETHER, LinkSendSnapshot},
    general::GeneralOptions,
    options::{
        Configurable, GetSocketOption, OptionHandled, PacketMembership, PacketStatistics,
        SetSocketOption,
    },
    service::Service,
    wire::{ETHERNET_HEADER_LEN, EthernetFrameRef},
};

const ETH_P_ALL: u16 = 0x0003;

const PACKET_HOST: u8 = 0;
const PACKET_BROADCAST: u8 = 1;
const PACKET_MULTICAST: u8 = 2;
const PACKET_OTHERHOST: u8 = 3;
const PACKET_OUTGOING: u8 = 4;

const PACKET_MR_PROMISC: u16 = 1;

const PACKET_RX_QUEUE_LIMIT_BYTES: usize = 64 * 1024;
const PACKET_RX_QUEUE_LIMIT_FRAMES: usize = 1024;

lazy_static! {
    static ref PACKET_HANDLERS: PacketHandlerRegistry = PacketHandlerRegistry::new();
}

struct PacketHandlerRegistry {
    handlers: Mutex<Vec<Weak<PacketSocketInner>>>,
    active_count: AtomicUsize,
}

impl PacketHandlerRegistry {
    fn new() -> Self {
        Self {
            handlers: Mutex::new(Vec::new()),
            active_count: AtomicUsize::new(0),
        }
    }

    fn register(&self, socket: &Arc<PacketSocketInner>) {
        let mut handlers = self.handlers.lock();
        handlers.retain(|weak| weak.strong_count() > 0);
        handlers.push(Arc::downgrade(socket));
        self.active_count.fetch_add(1, Ordering::Relaxed);
    }

    fn has_active_sockets(&self) -> bool {
        self.active_count.load(Ordering::Relaxed) != 0
    }

    fn unregister(&self) {
        self.active_count.fetch_sub(1, Ordering::Relaxed);
    }

    fn active_sockets(&self) -> Vec<Arc<PacketSocketInner>> {
        let mut active_sockets = Vec::new();
        let mut handlers = self.handlers.lock();
        handlers.retain(|weak| {
            let Some(socket) = weak.upgrade() else {
                return false;
            };
            active_sockets.push(socket);
            true
        });
        active_sockets
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketAddr {
    pub protocol: u16,
    pub ifindex: i32,
    pub hatype: u16,
    pub pkttype: u8,
    pub addr_len: u8,
    pub addr: [u8; 8],
}

impl PacketAddr {
    pub fn new(protocol: u16, ifindex: i32) -> Self {
        Self {
            protocol,
            ifindex,
            // The current AF_PACKET implementation is Ethernet-only.
            // Generalizing this requires per-device hardware type,
            // hardware address length, and link-layer header length.
            hatype: ARPHRD_ETHER,
            pkttype: PACKET_HOST,
            addr_len: 0,
            addr: [0; 8],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketSocketKind {
    Raw,
    Datagram,
}

#[derive(Clone)]
struct PacketFrame {
    from: PacketAddr,
    data: Arc<[u8]>,
    range: Range<usize>,
}

impl PacketFrame {
    fn new(from: PacketAddr, data: Arc<[u8]>, range: Range<usize>) -> Self {
        Self { from, data, range }
    }

    fn bytes(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }

    fn queued_bytes(&self) -> usize {
        self.data.len()
    }
}

#[derive(Default)]
struct PacketRxQueue {
    frames: VecDeque<PacketFrame>,
    bytes: usize,
}

impl PacketRxQueue {
    fn push_back(&mut self, frame: PacketFrame) -> bool {
        let queued_bytes = frame.queued_bytes();
        if self.frames.len() >= PACKET_RX_QUEUE_LIMIT_FRAMES
            || queued_bytes > PACKET_RX_QUEUE_LIMIT_BYTES
            || self.bytes.saturating_add(queued_bytes) > PACKET_RX_QUEUE_LIMIT_BYTES
        {
            return false;
        }

        self.bytes += queued_bytes;
        self.frames.push_back(frame);
        true
    }

    fn pop_front(&mut self) -> Option<PacketFrame> {
        let frame = self.frames.pop_front()?;
        self.bytes = self.bytes.saturating_sub(frame.queued_bytes());
        Some(frame)
    }

    fn front(&self) -> Option<&PacketFrame> {
        self.frames.front()
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

struct PacketSocketInner {
    kind: PacketSocketKind,
    protocol: u16,
    local_addr: RwLock<Option<PacketAddr>>,
    rx_queue: Mutex<PacketRxQueue>,
    recv_lock: Mutex<()>,
    poll_rx: Arc<PollSet>,
    general: GeneralOptions,
    packets: AtomicU32,
    drops: AtomicU32,
}

impl Drop for PacketSocketInner {
    fn drop(&mut self) {
        PACKET_HANDLERS.unregister();
    }
}

pub struct PacketSocket {
    inner: Arc<PacketSocketInner>,
}

impl PacketSocket {
    /// Creates an AF_PACKET socket with the initial Ethernet protocol.
    ///
    /// A zero protocol leaves the socket protocol unset. Such a socket receives
    /// no frames until `bind` supplies a non-zero protocol.
    pub fn new(kind: PacketSocketKind, protocol: u16) -> KResult<Self> {
        validate_protocol(protocol)?;

        let socket = Self {
            inner: Arc::new(PacketSocketInner {
                kind,
                protocol,
                local_addr: RwLock::new(None),
                rx_queue: Mutex::new(PacketRxQueue::default()),
                recv_lock: Mutex::new(()),
                poll_rx: Arc::new(PollSet::new()),
                general: GeneralOptions::new(),
                packets: AtomicU32::new(0),
                drops: AtomicU32::new(0),
            }),
        };
        register_packet_socket(&socket.inner);
        Ok(socket)
    }

    fn effective_addr(&self) -> PacketAddr {
        (*self.inner.local_addr.read()).unwrap_or_else(|| PacketAddr::new(self.inner.protocol, 0))
    }

    fn send_with_link<T, F>(
        &self,
        ifindex: i32,
        data_len: usize,
        kind: PacketSocketKind,
        nonblocking: bool,
        mut operation: F,
    ) -> KResult<T>
    where
        F: FnMut(&Service, LinkSendSnapshot) -> KResult<T>,
    {
        self.inner
            .general
            .send_poller_with_nonblocking(self, nonblocking, || {
                if !SERVICE.is_inited() {
                    return Err(LinuxError::ENODEV.into());
                }
                let service: &Service = &SERVICE;
                let link = service
                    .link_send_snapshot_for_ifindex(ifindex)
                    .ok_or(KError::from(LinuxError::ENODEV))?;
                validate_link_state_for_send(&link, data_len, kind)?;
                operation(service, link)
            })
    }

    // TODO: Implement PACKET_MR_PROMISC with device-side promiscuous-mode
    // reference counts, then clear memberships automatically when the socket
    // is dropped. PACKET_MR_MULTICAST and PACKET_MR_ALLMULTI should go through
    // the same device membership layer.
    fn record_membership(&self, membership: PacketMembership, _add: bool) -> KResult {
        if membership.membership_type != PACKET_MR_PROMISC {
            return Err(KError::OperationNotSupported);
        }
        Err(KError::OperationNotSupported)
    }

    fn send_raw(&self, ifindex: i32, frame: &[u8], nonblocking: bool) -> KResult<usize> {
        self.send_with_link(
            ifindex,
            frame.len(),
            PacketSocketKind::Raw,
            nonblocking,
            |service, _link| service.send_link_frame(ifindex, frame),
        )
    }

    fn send_datagram(&self, addr: PacketAddr, payload: &[u8], nonblocking: bool) -> KResult<usize> {
        let protocol = if addr.protocol == 0 {
            self.inner.protocol
        } else {
            addr.protocol
        };
        let mut frame = build_datagram_frame(addr, [0; 6], protocol, payload)?;

        self.send_with_link(
            addr.ifindex,
            payload.len(),
            PacketSocketKind::Datagram,
            nonblocking,
            |service, link| {
                frame[6..12].copy_from_slice(&link.hardware_addr);
                service.send_link_frame(addr.ifindex, &frame)?;
                Ok(payload.len())
            },
        )
    }

    fn statistics(&self) -> PacketStatistics {
        PacketStatistics {
            packets: self.inner.packets.swap(0, Ordering::Relaxed),
            drops: self.inner.drops.swap(0, Ordering::Relaxed),
        }
    }
}

impl Configurable for PacketSocket {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.inner.general.get_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::PacketStatistics(stats) => {
                **stats = self.statistics();
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        if self.inner.general.set_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::PacketAddMembership(membership) => self.record_membership(*membership, true)?,
            O::PacketDropMembership(membership) => self.record_membership(*membership, false)?,
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}

impl SocketOps for PacketSocket {
    /// Returns the `SO_TYPE` reported for this socket.
    ///
    /// Linux keeps the type the socket was created with in `sk_type`
    /// (`sock_getsockopt` reports it back), and `sys_socket` stores the requested
    /// type in `kind`, so a datagram socket must report `SOCK_DGRAM` even though
    /// the transport below is the same frame path.
    fn socket_type(&self) -> i32 {
        match self.inner.kind {
            PacketSocketKind::Raw => linux_raw_sys::net::SOCK_RAW as i32,
            PacketSocketKind::Datagram => linux_raw_sys::net::SOCK_DGRAM as i32,
        }
    }

    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let mut addr = local_addr.into_packet()?;
        validate_ifindex_or_any(addr.ifindex)?;
        if addr.protocol == 0 {
            addr.protocol = self.inner.protocol;
        }
        validate_protocol(addr.protocol)?;

        *self.inner.local_addr.write() = Some(addr);
        self.inner.poll_rx.wake();
        Ok(())
    }

    fn connect(&self, _remote_addr: SocketAddrEx, _options: ConnectOptions) -> KResult {
        Err(KError::OperationNotSupported)
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        let (addr, has_to) = match options.to {
            Some(SocketAddrEx::Packet(addr)) => (addr, true),
            Some(_) => return Err(LinuxError::EAFNOSUPPORT.into()),
            None => (self.effective_addr(), false),
        };
        if addr.ifindex <= 0 {
            return if has_to {
                Err(LinuxError::ENXIO.into())
            } else {
                Err(KError::NotConnected)
            };
        }
        let mut data = Vec::with_capacity(src.remaining());
        src.read_to_end(&mut data)?;

        match self.inner.kind {
            PacketSocketKind::Raw => {
                self.send_raw(addr.ifindex, &data, options.flags.nonblocking())
            }
            PacketSocketKind::Datagram => {
                self.send_datagram(addr, &data, options.flags.nonblocking())
            }
        }
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions<'_>) -> KResult<usize> {
        self.inner
            .general
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let _recv_guard = self.inner.recv_lock.lock();
                let frame = {
                    let rx_queue = self.inner.rx_queue.lock();
                    rx_queue.front().cloned().ok_or(KError::WouldBlock)?
                };

                if let Some(from) = options.from.as_deref_mut() {
                    *from = SocketAddrEx::Packet(frame.from);
                }

                let frame_data = frame.bytes();
                let write_len = frame_data.len().min(dst.remaining_mut());
                dst.write_all(&frame_data[..write_len])?;
                if !options.flags.contains(RecvFlags::PEEK) {
                    let mut rx_queue = self.inner.rx_queue.lock();
                    let _ = rx_queue.pop_front();
                }
                if write_len < frame_data.len()
                    && let Some(out_flags) = options.out_flags.as_deref_mut()
                {
                    *out_flags |= RecvFlags::TRUNCATE;
                }
                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    frame_data.len()
                } else {
                    write_len
                })
            })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Packet(self.effective_addr()))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        Err(KError::OperationNotSupported)
    }

    fn shutdown(&self, _how: Shutdown) -> KResult {
        Err(KError::OperationNotSupported)
    }
}

impl Pollable for PacketSocket {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if !self.inner.rx_queue.lock().is_empty() {
            events |= IoEvents::IN;
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.inner.poll_rx)?;
            if !self.inner.rx_queue.lock().is_empty() {
                self.inner.poll_rx.wake();
            }
        }
        Ok(())
    }
}

#[cfg(unittest)]
pub(crate) fn publish_link_frame(ifindex: i32, frame: &[u8], local_addr: [u8; 6]) {
    let active_sockets = PACKET_HANDLERS.active_sockets();
    if active_sockets.is_empty() {
        return;
    }

    let Some(frame_ref) = EthernetFrameRef::new_checked(frame) else {
        return;
    };
    publish_frame(
        active_sockets,
        ifindex,
        frame,
        packet_addr_from_frame(
            ifindex,
            raw_ethertype(frame),
            frame_ref.dst_addr().bytes(),
            frame_ref.src_addr().bytes(),
            local_addr,
            None,
        ),
    );
}

pub(crate) fn publish_link_packet(packet: &PacketBuf) {
    let active_sockets = PACKET_HANDLERS.active_sockets();
    if active_sockets.is_empty() {
        return;
    }

    let Some(link_metadata) = packet.link_metadata() else {
        return;
    };

    publish_frame(
        active_sockets,
        packet.ifindex(),
        packet.data(),
        packet_addr_from_packet(packet, link_metadata.src_addr.0),
    );
}

pub(crate) fn publish_outgoing_frame(ifindex: i32, frame: &[u8], local_addr: [u8; 6]) {
    let active_sockets = PACKET_HANDLERS.active_sockets();
    if active_sockets.is_empty() {
        return;
    }

    let Some(frame_ref) = EthernetFrameRef::new_checked(frame) else {
        return;
    };
    publish_frame(
        active_sockets,
        ifindex,
        frame,
        packet_addr_from_frame(
            ifindex,
            raw_ethertype(frame),
            frame_ref.dst_addr().bytes(),
            frame_ref.src_addr().bytes(),
            local_addr,
            Some(PACKET_OUTGOING),
        ),
    );
}

pub(crate) fn has_packet_handlers() -> bool {
    PACKET_HANDLERS.has_active_sockets()
}

fn publish_frame(
    active_sockets: Vec<Arc<PacketSocketInner>>,
    ifindex: i32,
    frame: &[u8],
    from: PacketAddr,
) {
    let data: Arc<[u8]> = Arc::from(frame);
    let protocol = protocol_to_host(from.protocol);

    for socket in active_sockets {
        if socket_matches(&socket, ifindex, protocol, from.pkttype) {
            let packet = PacketFrame::new(
                from,
                data.clone(),
                packet_frame_range(socket.kind, frame.len()),
            );
            socket.packets.fetch_add(1, Ordering::Relaxed);
            if socket.rx_queue.lock().push_back(packet) {
                socket.poll_rx.wake();
            } else {
                debug!("packet socket receive queue is full, dropping frame");
                socket.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn register_packet_socket(socket: &Arc<PacketSocketInner>) {
    PACKET_HANDLERS.register(socket);
}

fn packet_frame_range(kind: PacketSocketKind, frame_len: usize) -> Range<usize> {
    match kind {
        PacketSocketKind::Raw => 0..frame_len,
        PacketSocketKind::Datagram => ETHERNET_HEADER_LEN..frame_len,
    }
}

fn socket_matches(socket: &PacketSocketInner, ifindex: i32, protocol: u16, pkttype: u8) -> bool {
    let addr = (*socket.local_addr.read()).unwrap_or_else(|| PacketAddr::new(socket.protocol, 0));
    if addr.ifindex != 0 && addr.ifindex != ifindex {
        return false;
    }
    if pkttype == PACKET_OTHERHOST {
        return false;
    }

    let socket_protocol = protocol_to_host(addr.protocol);
    if socket_protocol == 0 {
        return false;
    }
    socket_protocol == ETH_P_ALL || socket_protocol == protocol
}

fn packet_addr_from_frame(
    ifindex: i32,
    protocol: u16,
    dst_addr: [u8; 6],
    src_addr: [u8; 6],
    local_addr: [u8; 6],
    pkttype: Option<u8>,
) -> PacketAddr {
    let mut addr = [0; 8];
    addr[..src_addr.len()].copy_from_slice(&src_addr);

    PacketAddr {
        protocol: protocol.to_be(),
        ifindex,
        hatype: ARPHRD_ETHER,
        pkttype: pkttype.unwrap_or_else(|| packet_type(dst_addr, local_addr)),
        addr_len: src_addr.len() as u8,
        addr,
    }
}

fn packet_addr_from_packet(packet: &PacketBuf, src_addr: [u8; 6]) -> PacketAddr {
    let mut addr = [0; 8];
    addr[..src_addr.len()].copy_from_slice(&src_addr);
    let protocol = packet
        .link_metadata()
        .map(|metadata| u16::from(metadata.protocol))
        .unwrap_or(0);

    PacketAddr {
        protocol: protocol.to_be(),
        ifindex: packet.ifindex(),
        hatype: ARPHRD_ETHER,
        pkttype: packet_type_to_packet_addr(packet.packet_type()),
        addr_len: src_addr.len() as u8,
        addr,
    }
}

fn build_datagram_frame(
    addr: PacketAddr,
    source_addr: [u8; 6],
    protocol: u16,
    payload: &[u8],
) -> KResult<Vec<u8>> {
    // TODO: Support non-DIX Ethernet packet socket semantics, including
    // ETH_P_802_3 length fields, VLAN headers, and non-Ethernet hard headers.
    if addr.addr_len < 6 || protocol == 0 {
        return Err(KError::InvalidInput);
    }
    validate_protocol(protocol)?;

    let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + payload.len());
    frame.extend_from_slice(&addr.addr[..6]);
    frame.extend_from_slice(&source_addr);
    frame.extend_from_slice(&protocol_to_host(protocol).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn validate_link_state_for_send(
    link: &LinkSendSnapshot,
    data_len: usize,
    kind: PacketSocketKind,
) -> KResult {
    if !link.is_up {
        return Err(LinuxError::ENETDOWN.into());
    }

    let mtu = link.mtu;
    match kind {
        PacketSocketKind::Raw => {
            if data_len < ETHERNET_HEADER_LEN {
                return Err(KError::InvalidInput);
            }
            if data_len > mtu.saturating_add(ETHERNET_HEADER_LEN) {
                return Err(LinuxError::EMSGSIZE.into());
            }
        }
        PacketSocketKind::Datagram => {
            if data_len > mtu {
                return Err(LinuxError::EMSGSIZE.into());
            }
        }
    }
    Ok(())
}

fn raw_ethertype(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[12], frame[13]])
}

fn packet_type(dst_addr: [u8; 6], local_addr: [u8; 6]) -> u8 {
    if dst_addr == [0xff; 6] {
        PACKET_BROADCAST
    } else if dst_addr[0] & 1 != 0 {
        PACKET_MULTICAST
    } else if dst_addr == local_addr {
        PACKET_HOST
    } else {
        PACKET_OTHERHOST
    }
}

fn packet_type_to_packet_addr(packet_type: PacketType) -> u8 {
    match packet_type {
        PacketType::Host => PACKET_HOST,
        PacketType::Broadcast => PACKET_BROADCAST,
        PacketType::Multicast => PACKET_MULTICAST,
        PacketType::OtherHost => PACKET_OTHERHOST,
    }
}

fn validate_protocol(_protocol: u16) -> KResult {
    Ok(())
}

fn protocol_to_host(protocol: u16) -> u16 {
    u16::from_be(protocol)
}

fn validate_ifindex_or_any(ifindex: i32) -> KResult {
    if ifindex == 0 || SERVICE.is_inited() && SERVICE.has_device(ifindex) {
        Ok(())
    } else {
        Err(LinuxError::ENODEV.into())
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/link/packet/tests.rs");
