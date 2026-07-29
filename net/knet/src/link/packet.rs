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
    RecvFlags, RecvOptions, SERVICE, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    buf::{PacketBuf, PacketType},
    general::GeneralOptions,
    netlink::{IFF_UP, LinkState, link_state_for_ifindex},
    options::{
        Configurable, GetSocketOption, OptionHandled, PacketMembership, PacketStatistics,
        SetSocketOption,
    },
    wire::{ETHERNET_HEADER_LEN, EthernetFrameRef},
};

const ETH_P_ALL: u16 = 0x0003;

const ARPHRD_ETHER: u16 = 1;

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
        self.inner
            .general
            .send_poller_with_nonblocking(self, nonblocking, || {
                if !SERVICE.is_inited() {
                    return Err(KError::NotFound);
                }
                SERVICE.lock().send_link_frame(ifindex, frame)
            })
    }

    fn send_datagram(&self, addr: PacketAddr, payload: &[u8], nonblocking: bool) -> KResult<usize> {
        let protocol = if addr.protocol == 0 {
            self.inner.protocol
        } else {
            addr.protocol
        };
        let link = validate_link_for_send(addr.ifindex, payload.len(), PacketSocketKind::Datagram)?;
        let frame = build_datagram_frame(addr, link.mac, protocol, payload)?;

        self.send_raw(addr.ifindex, &frame, nonblocking)
            .map(|_| payload.len())
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

    fn connect(&self, _remote_addr: SocketAddrEx) -> KResult {
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
        validate_ifindex(addr.ifindex)?;

        let mut data = Vec::with_capacity(src.remaining());
        src.read_to_end(&mut data)?;

        match self.inner.kind {
            PacketSocketKind::Raw => {
                validate_link_for_send(addr.ifindex, data.len(), PacketSocketKind::Raw)?;
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

fn validate_link_for_send(
    ifindex: i32,
    data_len: usize,
    kind: PacketSocketKind,
) -> KResult<LinkState> {
    let link = link_state_for_ifindex(ifindex).ok_or(KError::from(LinuxError::ENODEV))?;
    validate_link_state_for_send(&link, data_len, kind)?;
    Ok(link)
}

fn validate_link_state_for_send(
    link: &LinkState,
    data_len: usize,
    kind: PacketSocketKind,
) -> KResult {
    if link.flags & IFF_UP == 0 {
        return Err(LinuxError::ENETDOWN.into());
    }

    let mtu = link.mtu as usize;
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

fn validate_ifindex(ifindex: i32) -> KResult {
    if ifindex <= 0 {
        return Err(LinuxError::ENXIO.into());
    }
    if link_state_for_ifindex(ifindex).is_some() {
        Ok(())
    } else {
        Err(LinuxError::ENODEV.into())
    }
}

fn validate_ifindex_or_any(ifindex: i32) -> KResult {
    if ifindex == 0 || link_state_for_ifindex(ifindex).is_some() {
        Ok(())
    } else {
        Err(LinuxError::ENODEV.into())
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::String, vec};

    use kio::{Cursor, IoBufMut, Write};
    use kpoll::{IoEvents, Pollable};
    use unittest::def_test;

    use super::*;
    use crate::{RecvOptions, SendOptions, SocketAddrEx};

    const ETH_P_IP: u16 = 0x0800;
    const TEST_LOCAL_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
    const TEST_REMOTE_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 2];
    const TEST_OTHER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 3];

    fn ethernet_frame(dst: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + payload.len());
        frame.extend_from_slice(&dst);
        frame.extend_from_slice(&TEST_REMOTE_MAC);
        frame.extend_from_slice(&ethertype.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn publish_test_frame(ifindex: i32, data: &[u8]) {
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, data);
        publish_link_frame(ifindex, &frame, TEST_LOCAL_MAC);
    }

    fn test_link_state(flags: u32, mtu: u32) -> LinkState {
        LinkState {
            index: 2,
            name: String::from("eth0"),
            flags,
            mtu,
            operstate: 6,
            link_type: ARPHRD_ETHER,
            mac: TEST_LOCAL_MAC,
            broadcast: [0xff; 6],
        }
    }

    #[def_test(serial)]
    fn test_protocol_and_ifindex_filtering() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
        *socket.inner.local_addr.write() = Some(PacketAddr::new(ETH_P_IP.to_be(), 2));

        publish_test_frame(1, &[1, 2, 3, 4]);
        assert!(!socket.poll().contains(IoEvents::IN));

        publish_test_frame(2, &[5, 6, 7, 8]);
        assert!(socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_recvmsg_writes_packet_address() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 32];
        let mut from = SocketAddrEx::Packet(PacketAddr::new(0, 0));
        let received = socket
            .recv(
                Cursor::new(buf.as_mut_slice()),
                RecvOptions {
                    from: Some(&mut from),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(received, frame.len());
        assert_eq!(&buf[..frame.len()], &frame);
        let SocketAddrEx::Packet(addr) = from else {
            panic!("expected packet address");
        };
        assert_eq!(addr.ifindex, 2);
        assert_eq!(addr.protocol, ETH_P_IP.to_be());
        assert_eq!(addr.pkttype, PACKET_HOST);
        assert_eq!(&addr.addr[..6], &TEST_REMOTE_MAC);
    }

    #[def_test(serial)]
    fn test_datagram_recv_strips_ethernet_header() {
        let socket = PacketSocket::new(PacketSocketKind::Datagram, ETH_P_IP.to_be()).unwrap();
        let payload = [1, 2, 3, 4];
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &payload);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 16];
        let mut from = SocketAddrEx::Packet(PacketAddr::new(0, 0));
        let received = socket
            .recv(
                Cursor::new(buf.as_mut_slice()),
                RecvOptions {
                    from: Some(&mut from),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(received, payload.len());
        assert_eq!(&buf[..payload.len()], &payload);
        let SocketAddrEx::Packet(addr) = from else {
            panic!("expected packet address");
        };
        assert_eq!(addr.protocol, ETH_P_IP.to_be());
        assert_eq!(addr.ifindex, 2);
        assert_eq!(&addr.addr[..6], &TEST_REMOTE_MAC);
    }

    #[def_test(serial)]
    fn test_peek_preserves_frame() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut peek_buf = [0u8; 32];
        let peeked = socket
            .recv(
                Cursor::new(peek_buf.as_mut_slice()),
                RecvOptions {
                    flags: RecvFlags::PEEK,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(peeked, frame.len());
        assert!(socket.poll().contains(IoEvents::IN));

        let mut recv_buf = [0u8; 32];
        let received = socket
            .recv(Cursor::new(recv_buf.as_mut_slice()), RecvOptions::default())
            .unwrap();
        assert_eq!(received, frame.len());
        assert_eq!(&peek_buf[..frame.len()], &recv_buf[..frame.len()]);
    }

    #[def_test(serial)]
    fn test_truncate_returns_original_frame_len() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 2];
        let mut out_flags = RecvFlags::empty();
        let received = socket
            .recv(
                Cursor::new(buf.as_mut_slice()),
                RecvOptions {
                    flags: RecvFlags::TRUNCATE,
                    out_flags: Some(&mut out_flags),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(received, frame.len());
        assert_eq!(buf, [TEST_LOCAL_MAC[0], TEST_LOCAL_MAC[1]]);
        assert!(out_flags.contains(RecvFlags::TRUNCATE));
        assert!(!socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_small_buffer_without_truncate_returns_written_len() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 2];
        let mut out_flags = RecvFlags::empty();
        let received = socket
            .recv(
                Cursor::new(buf.as_mut_slice()),
                RecvOptions {
                    out_flags: Some(&mut out_flags),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(received, buf.len());
        assert_eq!(buf, [TEST_LOCAL_MAC[0], TEST_LOCAL_MAC[1]]);
        assert!(out_flags.contains(RecvFlags::TRUNCATE));
        assert!(!socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_peek_with_truncation_preserves_frame() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 2];
        let received = socket
            .recv(
                Cursor::new(buf.as_mut_slice()),
                RecvOptions {
                    flags: RecvFlags::PEEK,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(received, buf.len());
        assert!(socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_eth_p_all_receives_ipv6_and_unknown_protocols() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
        let ipv6_frame = ethernet_frame(TEST_LOCAL_MAC, 0x86DD, &[1, 2, 3, 4]);
        let unknown_frame = ethernet_frame(TEST_LOCAL_MAC, 0x88B5, &[5, 6, 7, 8]);

        publish_link_frame(2, &ipv6_frame, TEST_LOCAL_MAC);
        publish_link_frame(2, &unknown_frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 32];
        assert_eq!(
            socket
                .recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default())
                .unwrap(),
            ipv6_frame.len()
        );
        assert_eq!(
            socket
                .recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default())
                .unwrap(),
            unknown_frame.len()
        );
    }

    #[def_test(serial)]
    fn test_eth_p_ip_filters_ipv6_and_unknown_protocols() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let ipv6_frame = ethernet_frame(TEST_LOCAL_MAC, 0x86DD, &[1, 2, 3, 4]);
        let unknown_frame = ethernet_frame(TEST_LOCAL_MAC, 0x88B5, &[5, 6, 7, 8]);

        publish_link_frame(2, &ipv6_frame, TEST_LOCAL_MAC);
        publish_link_frame(2, &unknown_frame, TEST_LOCAL_MAC);

        assert!(!socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_custom_ethertype_matches() {
        const ETH_P_CUSTOM: u16 = 0x88b5;

        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_CUSTOM.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_CUSTOM, &[1, 2, 3, 4]);

        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 32];
        let received = socket
            .recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default())
            .unwrap();
        assert_eq!(received, frame.len());
        assert_eq!(&buf[..frame.len()], &frame);
    }

    #[def_test(serial)]
    fn test_datagram_empty_payload_frames_are_accounted() {
        let socket = PacketSocket::new(PacketSocketKind::Datagram, ETH_P_ALL.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[]);

        for _ in 0..=PACKET_RX_QUEUE_LIMIT_FRAMES {
            publish_link_frame(2, &frame, TEST_LOCAL_MAC);
        }

        let stats = socket.statistics();
        assert_eq!(stats.packets, (PACKET_RX_QUEUE_LIMIT_FRAMES + 1) as u32);
        assert_eq!(stats.drops, 1);
    }

    #[def_test(serial)]
    fn test_zero_protocol_does_not_receive_until_bound() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, 0).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);

        publish_link_frame(2, &frame, TEST_LOCAL_MAC);
        assert!(!socket.poll().contains(IoEvents::IN));

        socket
            .bind(SocketAddrEx::Packet(PacketAddr::new(ETH_P_IP.to_be(), 0)))
            .unwrap();
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);
        assert!(socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_outgoing_frame_is_published() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);

        publish_outgoing_frame(2, &frame, TEST_LOCAL_MAC);

        let mut buf = [0u8; 32];
        let mut from = SocketAddrEx::Packet(PacketAddr::new(0, 0));
        socket
            .recv(
                Cursor::new(buf.as_mut_slice()),
                RecvOptions {
                    from: Some(&mut from),
                    ..Default::default()
                },
            )
            .unwrap();
        let SocketAddrEx::Packet(addr) = from else {
            panic!("expected packet address");
        };
        assert_eq!(addr.pkttype, PACKET_OUTGOING);
    }

    #[def_test(serial)]
    fn test_otherhost_is_filtered_without_promisc_support() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
        let frame = ethernet_frame(TEST_OTHER_MAC, ETH_P_IP, &[1, 2, 3, 4]);

        publish_link_frame(2, &frame, TEST_LOCAL_MAC);
        assert!(!socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_packet_membership_is_unsupported() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
        let membership = PacketMembership {
            ifindex: 2,
            membership_type: PACKET_MR_PROMISC,
            addr_len: 0,
            addr: [0; 8],
        };

        assert_eq!(
            socket.record_membership(membership, true),
            Err(KError::OperationNotSupported)
        );
        assert_eq!(
            socket.record_membership(membership, false),
            Err(KError::OperationNotSupported)
        );
    }

    #[def_test(serial)]
    fn test_zero_destination_mac_is_otherhost() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
        let frame = ethernet_frame([0; 6], ETH_P_IP, &[1, 2, 3, 4]);

        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        assert!(!socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_send_ifindex_errors() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);

        assert_eq!(
            socket.send(Cursor::new(frame.as_slice()), SendOptions::default()),
            Err(KError::NotConnected)
        );

        let mut any_addr = PacketAddr::new(ETH_P_IP.to_be(), 0);
        assert_eq!(
            socket.send(
                Cursor::new(frame.as_slice()),
                SendOptions {
                    to: Some(SocketAddrEx::Packet(any_addr)),
                    ..Default::default()
                }
            ),
            Err(LinuxError::ENXIO.into())
        );

        any_addr.ifindex = 123;
        assert_eq!(
            socket.send(
                Cursor::new(frame.as_slice()),
                SendOptions {
                    to: Some(SocketAddrEx::Packet(any_addr)),
                    ..Default::default()
                }
            ),
            Err(LinuxError::ENODEV.into())
        );
    }

    #[def_test(serial)]
    fn test_send_link_state_validation() {
        let up_link = test_link_state(IFF_UP, 1500);
        let down_link = test_link_state(0, 1500);

        assert_eq!(
            validate_link_state_for_send(&down_link, ETHERNET_HEADER_LEN, PacketSocketKind::Raw),
            Err(LinuxError::ENETDOWN.into())
        );
        assert_eq!(
            validate_link_state_for_send(&up_link, ETHERNET_HEADER_LEN - 1, PacketSocketKind::Raw),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            validate_link_state_for_send(
                &up_link,
                ETHERNET_HEADER_LEN + up_link.mtu as usize + 1,
                PacketSocketKind::Raw,
            ),
            Err(LinuxError::EMSGSIZE.into())
        );
        assert_eq!(
            validate_link_state_for_send(
                &up_link,
                up_link.mtu as usize + 1,
                PacketSocketKind::Datagram,
            ),
            Err(LinuxError::EMSGSIZE.into())
        );
        assert!(
            validate_link_state_for_send(
                &up_link,
                ETHERNET_HEADER_LEN + up_link.mtu as usize,
                PacketSocketKind::Raw,
            )
            .is_ok()
        );
        assert!(
            validate_link_state_for_send(
                &up_link,
                up_link.mtu as usize,
                PacketSocketKind::Datagram
            )
            .is_ok()
        );
    }

    #[def_test(serial)]
    fn test_unsupported_connection_ops() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let addr = SocketAddrEx::Packet(PacketAddr::new(ETH_P_IP.to_be(), 2));

        assert_eq!(socket.connect(addr), Err(KError::OperationNotSupported));
        assert_eq!(
            socket.peer_addr().err(),
            Some(KError::OperationNotSupported)
        );
        assert_eq!(
            socket.shutdown(Shutdown::Both),
            Err(KError::OperationNotSupported)
        );
    }

    #[def_test(serial)]
    fn test_datagram_frame_builder() {
        let mut addr = PacketAddr::new(ETH_P_IP.to_be(), 2);
        addr.addr_len = 6;
        addr.addr[..6].copy_from_slice(&TEST_REMOTE_MAC);

        let payload = [1, 2, 3, 4];
        let frame = build_datagram_frame(addr, TEST_LOCAL_MAC, ETH_P_IP.to_be(), &payload).unwrap();

        assert_eq!(&frame[..6], &TEST_REMOTE_MAC);
        assert_eq!(&frame[6..12], &TEST_LOCAL_MAC);
        assert_eq!(&frame[12..14], &ETH_P_IP.to_be_bytes());
        assert_eq!(&frame[14..], &payload);

        addr.addr_len = 5;
        assert_eq!(
            build_datagram_frame(addr, TEST_LOCAL_MAC, ETH_P_IP.to_be(), &payload),
            Err(KError::InvalidInput)
        );
    }

    #[def_test(serial)]
    fn test_recv_write_failure_preserves_packet() {
        struct FailingDst;

        impl Write for FailingDst {
            fn write(&mut self, _buf: &[u8]) -> kio::Result<usize> {
                Err(KError::BadAddress)
            }

            fn flush(&mut self) -> kio::Result<()> {
                Ok(())
            }
        }

        impl IoBufMut for FailingDst {
            fn remaining_mut(&self) -> usize {
                32
            }
        }

        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        assert_eq!(
            socket.recv(FailingDst, RecvOptions::default()),
            Err(KError::BadAddress)
        );
        assert!(socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_recv_retries_partial_writes() {
        struct ShortDst {
            buf: [u8; 32],
            pos: usize,
        }

        impl Write for ShortDst {
            fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
                let write_len = buf.len().min(1);
                self.buf[self.pos..self.pos + write_len].copy_from_slice(&buf[..write_len]);
                self.pos += write_len;
                Ok(write_len)
            }

            fn flush(&mut self) -> kio::Result<()> {
                Ok(())
            }
        }

        impl IoBufMut for ShortDst {
            fn remaining_mut(&self) -> usize {
                self.buf.len() - self.pos
            }
        }

        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let mut dst = ShortDst {
            buf: [0; 32],
            pos: 0,
        };
        assert_eq!(
            socket.recv(&mut dst, RecvOptions::default()).unwrap(),
            frame.len()
        );
        assert_eq!(&dst.buf[..frame.len()], &frame);
        assert!(!socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_recv_partial_write_failure_preserves_packet() {
        struct PartialFailDst {
            wrote_once: bool,
        }

        impl Write for PartialFailDst {
            fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
                if self.wrote_once {
                    return Err(KError::BadAddress);
                }
                self.wrote_once = true;
                Ok(buf.len().min(1))
            }

            fn flush(&mut self) -> kio::Result<()> {
                Ok(())
            }
        }

        impl IoBufMut for PartialFailDst {
            fn remaining_mut(&self) -> usize {
                32
            }
        }

        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        assert_eq!(
            socket.recv(PartialFailDst { wrote_once: false }, RecvOptions::default()),
            Err(KError::BadAddress)
        );
        assert!(socket.poll().contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_queue_drop_statistics() {
        let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
        let frame = ethernet_frame(
            TEST_LOCAL_MAC,
            ETH_P_IP,
            &vec![0; PACKET_RX_QUEUE_LIMIT_BYTES],
        );

        publish_link_frame(2, &frame, TEST_LOCAL_MAC);

        let stats = socket.statistics();
        assert_eq!(stats.packets, 1);
        assert_eq!(stats.drops, 1);
        assert!(!socket.poll().contains(IoEvents::IN));
    }
}
