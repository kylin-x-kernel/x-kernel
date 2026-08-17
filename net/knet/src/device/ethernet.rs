// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ethernet device adapter.
use alloc::{
    collections::VecDeque,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hashbrown::HashMap;
use kclass::{ClassDevice, prelude::*};
use kerrno::{KError, KResult, LinuxError};
use kpoll::PollSet;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};
#[cfg(not(unittest))]
use lazyinit::LazyInit;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::ETHERNET_MAX_PENDING_PACKETS,
    device::{
        IF_OPER_DOWN, IF_OPER_UP, LINK_FLAG_BROADCAST, LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST,
        LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind, LinkSendSnapshot, LinkSnapshot,
        NetDevice as NetDeviceOps,
    },
    ip::{IpAddress, Ipv4Cidr},
    packet,
    wire::{
        ArpIpv4Packet, ArpOperation, ETHERNET_HEADER_LEN, EtherType, EthernetFrameRef, MacAddress,
        emit_ethernet_header,
    },
};

const NET_RX_SOFTIRQ_BATCH: usize = 8;

fn is_ipv4_source_usable_for_egress(
    next_hop: IpAddress,
    source_addr: crate::ip::Ipv4Address,
    assigned_ipv4_addrs: &[Ipv4Cidr],
) -> bool {
    next_hop.is_broadcast() || (!source_addr.is_unspecified() && !assigned_ipv4_addrs.is_empty())
}

static NEXT_NET_RX_SOURCE_ID: AtomicUsize = AtomicUsize::new(1);
static NET_RX_SOURCES: SpinNoIrq<NetRxSources> = SpinNoIrq::new(NetRxSources::new());
#[cfg(not(unittest))]
static NET_RX_SOFTIRQ_INIT: LazyInit<()> = LazyInit::new();
static NET_RX_SOFTIRQ_AVAILABLE: AtomicBool = AtomicBool::new(false);

struct NetRxSources {
    entries: Vec<Arc<NetRxPollSource>>,
    scan_cursor: usize,
}

impl NetRxSources {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            scan_cursor: 0,
        }
    }

    fn push(&mut self, source: Arc<NetRxPollSource>) {
        self.entries.push(source);
    }

    fn remove(&mut self, id: usize) -> Option<Arc<NetRxPollSource>> {
        let removed = self
            .entries
            .iter()
            .position(|source| source.id == id)
            .map(|index| self.entries.swap_remove(index));
        self.normalize_cursor();
        removed
    }

    fn collect_scheduled_fallback_batch(
        &mut self,
        wake_batch: &mut [Option<PollSet>; NET_RX_SOFTIRQ_BATCH],
    ) -> (usize, bool) {
        let source_count = self.entries.len();
        if source_count == 0 {
            self.scan_cursor = 0;
            return (0, false);
        }

        self.scan_cursor %= source_count;
        let start = self.scan_cursor;
        let mut next_cursor = start;
        let mut wake_count = 0;
        let mut has_deferred_pending = false;

        for offset in 0..source_count {
            if wake_count == NET_RX_SOFTIRQ_BATCH {
                has_deferred_pending = self.has_pending_from(start, offset);
                break;
            }

            let index = (start + offset) % source_count;
            let source = &self.entries[index];
            if source.take_pending() {
                wake_batch[wake_count] = Some(source.waiters_clone());
                wake_count += 1;
                next_cursor = (index + 1) % source_count;
            }
        }

        self.scan_cursor = next_cursor;
        (wake_count, has_deferred_pending)
    }

    fn has_pending_from(&self, start: usize, offset_start: usize) -> bool {
        (offset_start..self.entries.len()).any(|offset| {
            let index = (start + offset) % self.entries.len();
            self.entries[index].has_pending()
        })
    }

    fn normalize_cursor(&mut self) {
        if self.entries.is_empty() {
            self.scan_cursor = 0;
        } else if self.scan_cursor >= self.entries.len() {
            self.scan_cursor %= self.entries.len();
        }
    }
}

struct NetRxPollSource {
    id: usize,
    pending: AtomicBool,
    waiters: PollSet,
}

impl NetRxPollSource {
    fn new(id: usize) -> Self {
        Self {
            id,
            pending: AtomicBool::new(false),
            waiters: PollSet::new(),
        }
    }

    fn waiters(&self) -> &PollSet {
        &self.waiters
    }

    fn waiters_clone(&self) -> PollSet {
        self.waiters.clone()
    }

    fn schedule(&self) {
        self.pending.store(true, Ordering::Release);
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::NetRx);
    }

    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

struct KnetRxScheduler {
    source: Weak<NetRxPollSource>,
}

impl NetRxScheduler for KnetRxScheduler {
    fn schedule_rx(&self) {
        if let Some(source) = self.source.upgrade() {
            source.schedule();
        }
    }
}

#[cfg(not(unittest))]
fn ensure_net_rx_softirq_available() -> bool {
    NET_RX_SOFTIRQ_INIT.call_once(|| {
        if kirq::softirq::open_softirq(kirq::softirq::SoftirqVec::NetRx, run_net_rx_softirq) {
            NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        } else {
            warn!("knet: NetRx softirq vector already registered");
        }
    });
    NET_RX_SOFTIRQ_AVAILABLE.load(Ordering::Acquire)
}

#[cfg(unittest)]
fn ensure_net_rx_softirq_available() -> bool {
    let vec = kirq::softirq::SoftirqVec::NetRx;
    if kirq::softirq::softirq_action_matches_for_tests(vec, run_net_rx_softirq) {
        NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        return true;
    }
    if kirq::softirq::is_softirq_open(vec) {
        NET_RX_SOFTIRQ_AVAILABLE.store(false, Ordering::Release);
        return false;
    }

    if kirq::softirq::open_softirq(vec, run_net_rx_softirq) {
        NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        true
    } else {
        let available = kirq::softirq::softirq_action_matches_for_tests(vec, run_net_rx_softirq);
        NET_RX_SOFTIRQ_AVAILABLE.store(available, Ordering::Release);
        available
    }
}

fn register_net_rx_source() -> Option<Arc<NetRxPollSource>> {
    if !ensure_net_rx_softirq_available() {
        return None;
    }
    let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
    let source = Arc::new(NetRxPollSource::new(id));
    NET_RX_SOURCES.lock().push(source.clone());
    Some(source)
}

fn unregister_net_rx_source(id: usize) {
    let removed = {
        let mut sources = NET_RX_SOURCES.lock();
        sources.remove(id)
    };
    if let Some(source) = removed {
        source.waiters.wake();
    }
}

fn run_net_rx_softirq() {
    let mut wake_batch: [Option<PollSet>; NET_RX_SOFTIRQ_BATCH] = core::array::from_fn(|_| None);
    let (_, has_deferred_pending) = NET_RX_SOURCES
        .lock()
        .collect_scheduled_fallback_batch(&mut wake_batch);

    for waiters in wake_batch.into_iter().flatten() {
        waiters.wake();
    }

    if has_deferred_pending {
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::NetRx);
    }
}

/// ARP table entry mapping an IP address to a MAC address.
struct ArpNeighbor {
    hardware_address: MacAddress,
    /// When this entry expires (TTL = 300s).
    expires_at: MonotonicInstant,
}

/// Queued IP packet awaiting ARP resolution.
struct PendingTxPacket {
    next_hop: IpAddress,
    source_addr: crate::ip::Ipv4Address,
    packet: PacketBuf,
}

/// Ethernet device backed by a driver-provided NIC.
pub struct EthernetDevice {
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    inner: ClassDevice<NetDeviceImpl>,
    neighbors: HashMap<IpAddress, Option<ArpNeighbor>>,
    assigned_ipv4_addrs: Vec<Ipv4Cidr>,
    local_ipv4_addrs: Vec<Ipv4Cidr>,

    pending_tx: VecDeque<PendingTxPacket>,
    rx_source: Option<Arc<NetRxPollSource>>,
}

impl EthernetDevice {
    const NEIGHBOR_TTL: TimeSpan = TimeSpan::from_secs(300);

    /// Create a new Ethernet device wrapper.
    pub fn new(name: String, inner: ClassDevice<NetDeviceImpl>) -> Self {
        let rx_source = if let Some(rx_source) = register_net_rx_source() {
            let scheduler: Arc<dyn NetRxScheduler> = Arc::new(KnetRxScheduler {
                source: Arc::downgrade(&rx_source),
            });
            match inner.set_rx_scheduler(Some(scheduler)) {
                Ok(()) => Some(rx_source),
                Err(DriverError::Unsupported) => {
                    warn!("Ethernet driver does not support interrupt-driven RX scheduling");
                    unregister_net_rx_source(rx_source.id);
                    None
                }
                Err(err) => {
                    warn!("failed to attach Ethernet RX scheduler: {:?}", err);
                    unregister_net_rx_source(rx_source.id);
                    None
                }
            }
        } else {
            warn!("knet: NetRx softirq unavailable; Ethernet RX uses timeout polling fallback");
            None
        };
        Self {
            name,
            flags: LINK_FLAG_UP
                | LINK_FLAG_RUNNING
                | LINK_FLAG_BROADCAST
                | LINK_FLAG_MULTICAST
                | LINK_FLAG_LOWER_UP,
            mtu: crate::consts::STANDARD_MTU,
            operstate: IF_OPER_UP,
            inner,
            neighbors: HashMap::new(),
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
            pending_tx: VecDeque::with_capacity(ETHERNET_MAX_PENDING_PACKETS),
            rx_source,
        }
    }

    pub(crate) fn rx_poll_set(&self) -> Option<PollSet> {
        self.rx_source.as_ref().map(|source| source.waiters_clone())
    }

    /// Spawn the RX poll task for an initialized network stack.
    pub(crate) fn spawn_rx_task(rx_poll: PollSet) {
        let _ = ktask::spawn_with_name(
            move || {
                use core::{future::poll_fn, task::Poll};

                use kpoll::PollRegistrations;

                let mut registrations = PollRegistrations::new();
                ktask::future::block_on(poll_fn(move |cx| {
                    loop {
                        let mut context = registrations.context(cx);
                        if context.register(&rx_poll).is_err() {
                            drop(context);
                            // Sleeping without an RX registration would stall
                            // RX forever; yield and retry under memory pressure.
                            ktask::yield_now();
                            continue;
                        }
                        drop(context);
                        // Register before publishing the RX recheck so an IRQ
                        // arriving across this boundary either wakes this task
                        // or is covered by the bounded data-plane batch.
                        crate::poller::network_poller().publish_and_poll_rx();
                        return Poll::<()>::Pending;
                    }
                }));
            },
            "knet-rx".into(),
        );
    }

    #[inline]
    fn mac_addr(&self) -> MacAddress {
        MacAddress(self.inner.mac().0)
    }

    fn send_frame<F>(
        inner: &dyn NetDevice,
        dst: MacAddress,
        size: usize,
        f: F,
        proto: EtherType,
    ) -> Option<([u8; 6], Vec<u8>)>
    where
        F: FnOnce(&mut [u8]),
    {
        if let Err(err) = inner.recycle_tx() {
            warn!("recycle_tx failed: {:?}", err);
            return None;
        }

        let src_addr = MacAddress(inner.mac().0);
        let mut tx_buf: NetBufHandle = match inner.alloc_tx_buf(ETHERNET_HEADER_LEN + size) {
            Ok(buf) => buf,
            Err(err) => {
                warn!("alloc_tx_buf failed: {:?}", err);
                return None;
            }
        };
        let Some(payload) = emit_ethernet_header(tx_buf.data_mut(), dst, src_addr, proto) else {
            warn!("alloc_tx_buf returned a short Ethernet buffer");
            return None;
        };
        f(payload);
        let frame_data = packet::has_packet_handlers().then(|| tx_buf.data().to_vec());
        trace!("SEND {} bytes: {:02X?}", tx_buf.len(), tx_buf.data());
        if let Err(err) = inner.send(tx_buf) {
            warn!("send failed: {:?}", err);
            None
        } else {
            frame_data.map(|frame_data| (src_addr.bytes(), frame_data))
        }
    }

    fn send_to<F>(&mut self, ifindex: i32, dst: MacAddress, size: usize, f: F, proto: EtherType)
    where
        F: FnOnce(&mut [u8]),
    {
        let sent = Self::send_frame(&self.inner, dst, size, f, proto);
        if let Some((src_addr, frame_data)) = sent {
            packet::publish_outgoing_frame(ifindex, &frame_data, src_addr);
        }
    }

    fn send_ipv4_packet_to(&mut self, ifindex: i32, dst: MacAddress, packet: &PacketBuf) {
        if !self.is_link_up() {
            return;
        }
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        if ip_packet.len() > self.mtu {
            warn!(
                "Dropping IPv4 packet of {} bytes exceeding device MTU {}",
                ip_packet.len(),
                self.mtu
            );
            return;
        }
        self.send_to(
            ifindex,
            dst,
            ip_packet.len(),
            |buf| buf.copy_from_slice(ip_packet),
            EtherType::Ipv4,
        );
    }

    fn handle_rx_frame(
        &mut self,
        ifindex: i32,
        frame_data: &[u8],
        timestamp: MonotonicInstant,
    ) -> Option<PacketBuf> {
        let Some(frame) = EthernetFrameRef::new_checked(frame_data) else {
            warn!("Dropping malformed Ethernet frame");
            return None;
        };
        let dst_addr = frame.dst_addr();
        let src_addr = frame.src_addr();
        let ethertype = frame.ethertype();

        let packet_buf = PacketBuf::from_ethernet_frame(
            ifindex,
            frame_data,
            dst_addr,
            src_addr,
            ethertype,
            self.mac_addr(),
            PacketOwner::DeviceRx,
        );
        packet::publish_link_packet(&packet_buf);

        if !dst_addr.is_broadcast() && dst_addr != MacAddress::ZERO && dst_addr != self.mac_addr() {
            return None;
        }

        match ethertype {
            EtherType::Ipv4 => {
                return Some(packet_buf);
            }
            EtherType::Arp => self.handle_arp_packet(ifindex, frame.payload(), timestamp),
            _ => {}
        }

        None
    }

    fn send_arp_request(
        &mut self,
        ifindex: i32,
        target_ip: IpAddress,
        source_addr: crate::ip::Ipv4Address,
    ) {
        let IpAddress::Ipv4(target_ipv4) = target_ip else {
            warn!("IPv6 address ARP is not supported: {}", target_ip);
            return;
        };
        if source_addr.is_unspecified() {
            warn!("Cannot resolve ARP from an unspecified IPv4 source");
            return;
        }
        debug!("Requesting ARP for {}", target_ipv4);

        let arp_packet = ArpIpv4Packet {
            operation: ArpOperation::Request,
            source_hardware_addr: self.mac_addr(),
            source_protocol_addr: source_addr,
            target_hardware_addr: MacAddress::BROADCAST,
            target_protocol_addr: target_ipv4,
        };

        self.send_to(
            ifindex,
            MacAddress::BROADCAST,
            ArpIpv4Packet::LEN,
            |buf| {
                let emit_result = arp_packet.emit(buf);
                debug_assert!(emit_result.is_some());
            },
            EtherType::Arp,
        );

        self.neighbors.insert(target_ip, None);
    }

    fn handle_arp_packet(&mut self, ifindex: i32, payload: &[u8], now: MonotonicInstant) {
        let Some(packet) = ArpIpv4Packet::parse(payload) else {
            debug!("Dropping malformed ARP packet");
            return;
        };

        let ArpIpv4Packet {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        } = packet;
        let is_unicast_mac =
            target_hardware_addr != MacAddress::ZERO && !target_hardware_addr.is_broadcast();
        if is_unicast_mac && self.mac_addr() != target_hardware_addr {
            return;
        }

        if let ArpOperation::Unknown(_) = operation {
            return;
        }

        if !source_hardware_addr.is_unicast()
            || source_protocol_addr.is_broadcast()
            || source_protocol_addr.is_multicast()
            || source_protocol_addr.is_unspecified()
        {
            return;
        }
        if !self
            .local_ipv4_addrs
            .iter()
            .any(|addr| addr.address() == target_protocol_addr)
        {
            return;
        }

        debug!("ARP: {} -> {}", source_protocol_addr, source_hardware_addr);
        self.neighbors.insert(
            IpAddress::Ipv4(source_protocol_addr),
            Some(ArpNeighbor {
                hardware_address: source_hardware_addr,
                expires_at: now + Self::NEIGHBOR_TTL,
            }),
        );

        if let ArpOperation::Request = operation {
            let response = ArpIpv4Packet {
                operation: ArpOperation::Reply,
                source_hardware_addr: self.mac_addr(),
                source_protocol_addr: target_protocol_addr,
                target_hardware_addr: source_hardware_addr,
                target_protocol_addr: source_protocol_addr,
            };

            self.send_to(
                ifindex,
                source_hardware_addr,
                ArpIpv4Packet::LEN,
                |buf| {
                    let emit_result = response.emit(buf);
                    debug_assert!(emit_result.is_some());
                },
                EtherType::Arp,
            );
        }

        enum PendingAction {
            Send(MacAddress),
            Keep,
            Refresh,
        }

        let mut kept_packets = Vec::with_capacity(ETHERNET_MAX_PENDING_PACKETS);
        for _ in 0..ETHERNET_MAX_PENDING_PACKETS {
            let Some(pending) = self.pending_tx.pop_front() else {
                break;
            };

            let action = match self.neighbors.get(&pending.next_hop) {
                Some(Some(neighbor)) if neighbor.expires_at > now => {
                    PendingAction::Send(neighbor.hardware_address)
                }
                Some(Some(_)) => PendingAction::Refresh,
                _ => PendingAction::Keep,
            };

            match action {
                PendingAction::Send(hardware_address) => {
                    self.send_ipv4_packet_to(ifindex, hardware_address, &pending.packet)
                }
                PendingAction::Keep => kept_packets.push(pending),
                PendingAction::Refresh => {
                    self.neighbors.remove(&pending.next_hop);
                    self.send_arp_request(ifindex, pending.next_hop, pending.source_addr);
                    kept_packets.push(pending);
                }
            }
        }

        for pending in kept_packets {
            if self.pending_tx.len() >= ETHERNET_MAX_PENDING_PACKETS {
                warn!("Pending packets buffer is full, dropping packet");
            } else {
                self.pending_tx.push_back(pending);
            }
        }
    }
}

impl NetDeviceOps for EthernetDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Ethernet
    }

    fn mtu(&self) -> usize {
        self.mtu
    }

    fn is_link_up(&self) -> bool {
        self.flags & LINK_FLAG_UP != 0
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            name: self.name.clone(),
            flags: self.flags,
            mtu: self.mtu,
            operstate: self.operstate,
            kind: LinkKind::Ethernet,
            hardware_addr: self.mac_addr().0,
            broadcast_addr: [0xff; 6],
        }
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        LinkSendSnapshot {
            is_up: self.is_link_up(),
            mtu: self.mtu,
            hardware_addr: self.mac_addr().0,
        }
    }

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        Some(self.inner.id())
    }

    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> Option<PacketBuf> {
        if !self.is_link_up() {
            return None;
        }
        loop {
            let rx_buf: NetBufHandle = match self.inner.recv() {
                Ok(buf) => buf,
                Err(err) => {
                    if !matches!(err, DriverError::WouldBlock) {
                        warn!("recv failed: {:?}", err);
                    }
                    return None;
                }
            };
            trace!("RECV {} bytes: {:02X?}", rx_buf.len(), rx_buf.data());

            let packet = self.handle_rx_frame(ifindex, rx_buf.data(), timestamp);
            self.inner.recycle_rx(rx_buf).unwrap();
            if packet.is_some() {
                return packet;
            }
        }
    }

    fn has_rx_work(&self) -> bool {
        self.inner.can_rx()
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        source_addr: IpAddress,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        if !self.is_link_up() {
            return false;
        }
        if !matches!(next_hop, IpAddress::Ipv4(_)) {
            warn!("Dropping IPv6 packet on IPv4-only Ethernet device");
            return false;
        }
        let IpAddress::Ipv4(source_addr) = source_addr else {
            warn!("Dropping packet with a non-IPv4 source on IPv4-only Ethernet device");
            return false;
        };

        packet.set_ifindex(ifindex);
        let Some(ip_packet) = packet.network_packet() else {
            warn!("Dropping malformed IPv4 output packet");
            return false;
        };
        let is_limited_broadcast = next_hop.is_broadcast();
        if !is_ipv4_source_usable_for_egress(next_hop, source_addr, &self.assigned_ipv4_addrs) {
            warn!("Dropping IPv4 packet without a local IPv4 source");
            return false;
        }
        if ip_packet.len() > self.mtu {
            warn!("Dropping packet exceeding Ethernet MTU {}", self.mtu);
            return false;
        }
        let is_directed_broadcast = matches!(next_hop, IpAddress::Ipv4(next_hop) if self
            .assigned_ipv4_addrs
            .iter()
            .any(|addr| addr.is_directed_broadcast(next_hop)));
        if is_limited_broadcast || is_directed_broadcast {
            self.send_ipv4_packet_to(ifindex, MacAddress::BROADCAST, &packet);
            return false;
        }

        let need_request = match self.neighbors.get(&next_hop) {
            Some(Some(neighbor)) if neighbor.expires_at > timestamp => {
                self.send_ipv4_packet_to(ifindex, neighbor.hardware_address, &packet);
                return false;
            }
            Some(Some(_)) => true,
            // Request already sent
            Some(None) => false,
            None => true,
        };
        // Only send ARP request if we haven't already requested it
        if need_request {
            self.send_arp_request(ifindex, next_hop, source_addr);
        }
        if self.pending_tx.len() >= ETHERNET_MAX_PENDING_PACKETS {
            warn!("Pending packets buffer is full, dropping packet");
            return false;
        }
        self.pending_tx.push_back(PendingTxPacket {
            next_hop,
            source_addr,
            packet,
        });
        false
    }

    fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        if !self.is_link_up() {
            return Err(LinuxError::ENETDOWN.into());
        }
        let local_addr = self.mac_addr().0;
        self.inner.recycle_tx().map_err(map_dev_err)?;
        let mut tx_buf = self.inner.alloc_tx_buf(frame.len()).map_err(map_dev_err)?;
        tx_buf.data_mut().copy_from_slice(frame);
        self.inner.send(tx_buf).map_err(map_dev_err)?;
        packet::publish_outgoing_frame(ifindex, frame, local_addr);
        Ok(frame.len())
    }

    fn register_rx_waker(
        &self,
        _source_waker: &core::task::Waker,
        context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        if let Some(source) = &self.rx_source {
            context.register(source.waiters())?;
        }
        Ok(())
    }

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        LinkKind::Ethernet.validate_mtu(mtu)?;
        self.mtu = mtu;
        Ok(())
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.operstate = IF_OPER_UP;
        } else {
            self.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.operstate = IF_OPER_DOWN;
        }
    }

    fn set_ipv4_addrs(&mut self, assigned_addrs: &[Ipv4Cidr], local_addrs: &[Ipv4Cidr]) {
        self.assigned_ipv4_addrs.clear();
        self.assigned_ipv4_addrs.extend_from_slice(assigned_addrs);
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(local_addrs);
    }

    fn remove_pending_ipv4_source(&mut self, addr: crate::ip::Ipv4Address) {
        Self::remove_pending_packets_with_source(&mut self.pending_tx, addr);
    }

    fn sync_neighbors(&mut self, neighbors: &[(IpAddress, [u8; 6])]) {
        self.neighbors.clear();
        for (dst_addr, hardware_addr) in neighbors {
            self.neighbors.insert(
                *dst_addr,
                Some(ArpNeighbor {
                    hardware_address: MacAddress(*hardware_addr),
                    expires_at: MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(
                        u64::MAX / 2,
                    )),
                }),
            );
        }
    }
}

impl EthernetDevice {
    fn remove_pending_packets_with_source(
        pending_tx: &mut VecDeque<PendingTxPacket>,
        addr: crate::ip::Ipv4Address,
    ) {
        pending_tx.retain(|pending| pending.source_addr != addr);
    }
}

impl Drop for EthernetDevice {
    fn drop(&mut self) {
        if let Some(source) = &self.rx_source {
            if let Err(err) = self.inner.set_rx_scheduler(None) {
                warn!("failed to detach Ethernet RX scheduler: {:?}", err);
            }
            unregister_net_rx_source(source.id);
        }
    }
}

fn map_dev_err(err: DriverError) -> KError {
    match err {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::NoMemory => LinuxError::ENOBUFS.into(),
        DriverError::Io => KError::Io,
        _ => KError::BadState,
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable, Waker},
    };

    use unittest::{assert_eq, def_test};

    use super::*;

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn waker_wake(data: *const ()) {
        // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_drop(_data: *const ()) {}

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

    fn new_counter() -> &'static AtomicUsize {
        Box::leak(Box::new(AtomicUsize::new(0)))
    }

    fn make_waker(counter: &'static AtomicUsize) -> Waker {
        let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
        // SAFETY: the raw waker data pointer is the leaked `AtomicUsize` above
        // and the vtable only performs atomic increments on that allocation.
        unsafe { Waker::from_raw(raw) }
    }

    fn pending_packet(source: crate::ip::Ipv4Address) -> PacketBuf {
        let mut bytes = vec![0; 20];
        bytes[0] = 0x45;
        let packet_len = bytes.len() as u16;
        bytes[2..4].copy_from_slice(&packet_len.to_be_bytes());
        bytes[12..16].copy_from_slice(&source.octets());
        bytes[16..20].copy_from_slice(&[198, 51, 100, 1]);
        PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx)
    }

    #[def_test]
    fn remove_pending_packets_with_deleted_source() {
        let deleted = crate::ip::Ipv4Address::new(192, 0, 2, 7);
        let retained = crate::ip::Ipv4Address::new(192, 0, 2, 8);
        let mut pending = VecDeque::from([
            PendingTxPacket {
                next_hop: IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 1)),
                source_addr: deleted,
                packet: pending_packet(deleted),
            },
            PendingTxPacket {
                next_hop: IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 2)),
                source_addr: retained,
                packet: pending_packet(retained),
            },
        ]);

        EthernetDevice::remove_pending_packets_with_source(&mut pending, deleted);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending.front().unwrap().source_addr, retained);
    }

    #[def_test]
    fn limited_broadcast_accepts_unspecified_ipv4_source() {
        assert!(is_ipv4_source_usable_for_egress(
            IpAddress::Ipv4(crate::ip::Ipv4Address::BROADCAST),
            crate::ip::Ipv4Address::UNSPECIFIED,
            &[],
        ));
        assert!(!is_ipv4_source_usable_for_egress(
            IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 0, 2, 1)),
            crate::ip::Ipv4Address::UNSPECIFIED,
            &[],
        ));
    }

    fn test_source(sources: &mut NetRxSources, id: usize) -> Arc<NetRxPollSource> {
        let source = Arc::new(NetRxPollSource::new(id));
        sources.push(source.clone());
        source
    }

    fn collect_and_wake_test_sources(sources: &mut NetRxSources) -> (usize, bool) {
        let mut wake_batch: [Option<PollSet>; NET_RX_SOFTIRQ_BATCH] =
            core::array::from_fn(|_| None);
        let result = sources.collect_scheduled_fallback_batch(&mut wake_batch);
        for waiters in wake_batch.into_iter().flatten() {
            waiters.wake();
        }
        result
    }

    #[def_test(serial)]
    fn test_net_rx_softirq_wakes_only_pending_sources() {
        let mut sources = NetRxSources::new();
        let pending_id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let idle_id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let pending_source = test_source(&mut sources, pending_id);
        let idle_source = test_source(&mut sources, idle_id);
        let pending_counter = new_counter();
        let idle_counter = new_counter();
        let pending_registration = pending_source
            .waiters()
            .register(&make_waker(pending_counter))
            .unwrap();
        let idle_registration = idle_source
            .waiters()
            .register(&make_waker(idle_counter))
            .unwrap();

        pending_source.pending.store(true, Ordering::Release);
        assert_eq!(collect_and_wake_test_sources(&mut sources), (1, false));

        assert_eq!(pending_counter.load(Ordering::SeqCst), 1);
        assert_eq!(idle_counter.load(Ordering::SeqCst), 0);

        drop(pending_registration);
        drop(idle_registration);
    }

    #[def_test(serial)]
    fn test_net_rx_softirq_round_robin_cursor_reaches_sources_after_batch() {
        let mut local_sources = NetRxSources::new();
        let mut sources = Vec::new();
        let mut counters = Vec::new();
        let mut registrations = Vec::new();
        for _ in 0..(NET_RX_SOFTIRQ_BATCH + 2) {
            let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
            let source = test_source(&mut local_sources, id);
            let counter = new_counter();
            registrations.push(source.waiters().register(&make_waker(counter)).unwrap());
            counters.push(counter);
            sources.push(source);
        }

        for source in &sources {
            source.pending.store(true, Ordering::Release);
        }

        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (NET_RX_SOFTIRQ_BATCH, true)
        );
        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (2, false)
        );

        for counter in &counters {
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }

        drop(registrations);
    }

    #[def_test(serial)]
    fn test_net_rx_softirq_full_batch_without_remaining_pending_does_not_defer() {
        let mut local_sources = NetRxSources::new();
        let mut sources = Vec::new();
        let mut counters = Vec::new();
        let mut registrations = Vec::new();
        for _ in 0..(NET_RX_SOFTIRQ_BATCH + 2) {
            let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
            let source = test_source(&mut local_sources, id);
            let counter = new_counter();
            registrations.push(source.waiters().register(&make_waker(counter)).unwrap());
            counters.push(counter);
            sources.push(source);
        }

        for source in sources.iter().take(NET_RX_SOFTIRQ_BATCH) {
            source.pending.store(true, Ordering::Release);
        }

        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (NET_RX_SOFTIRQ_BATCH, false)
        );
        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (0, false)
        );

        for counter in counters.iter().take(NET_RX_SOFTIRQ_BATCH) {
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        for counter in counters.iter().skip(NET_RX_SOFTIRQ_BATCH) {
            assert_eq!(counter.load(Ordering::SeqCst), 0);
        }

        drop(registrations);
    }
}
