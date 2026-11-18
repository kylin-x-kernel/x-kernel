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
use kspin::{SpinNoIrq, static_lock};
use ktime_types::{MonotonicInstant, TimeSpan};
#[cfg(not(unittest))]
use lazyinit::LazyInit;
#[cfg(unittest)]
use net as driver_net;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::ETHERNET_MAX_PENDING_PACKETS,
    device::{
        IF_OPER_DOWN, IF_OPER_UP, LINK_FLAG_BROADCAST, LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST,
        LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborEvent,
        NeighborSnapshot, NeighborState, NeighborUpdate, NetDevice as NetDeviceOps, NetDeviceId,
        RxPoll,
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
static_lock! {
    static NET_RX_SOURCES: SpinNoIrq<NetRxSources> = SpinNoIrq::new(NetRxSources::new());
}
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
pub(crate) fn ensure_net_rx_softirq_available() -> bool {
    NET_RX_SOFTIRQ_INIT.call_once(|| {
        super::net_rx::ensure_net_rx_queue();
        if kirq::softirq::open_softirq(kirq::softirq::SoftirqVec::NetRx, run_net_rx_softirq) {
            NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        } else {
            warn!("knet: NetRx softirq vector already registered");
        }
    });
    NET_RX_SOFTIRQ_AVAILABLE.load(Ordering::Acquire)
}

#[cfg(unittest)]
pub(crate) fn ensure_net_rx_softirq_available() -> bool {
    super::net_rx::ensure_net_rx_queue();
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
    let has_more_packets = super::net_rx::process_pending(super::net_rx::NET_RX_BUDGET);

    let mut wake_batch: [Option<PollSet>; NET_RX_SOFTIRQ_BATCH] = core::array::from_fn(|_| None);
    let (wake_count, has_deferred_pending) = NET_RX_SOURCES
        .lock()
        .collect_scheduled_fallback_batch(&mut wake_batch);

    for waiters in wake_batch.into_iter().flatten() {
        waiters.wake();
    }

    if wake_count > 0 {
        crate::poller::network_poller().notify(crate::poller::PollReason::Rx);
    }

    if has_deferred_pending || has_more_packets {
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::NetRx);
    }
}

/// ARP table entry for one IPv4 address.
///
/// An entry without a hardware address is incomplete and owns the probe
/// schedule.  A valid entry keeps its hardware address when it ages out, as
/// Linux does: expiry downgrades it to `STALE` and the next probe or use
/// refreshes it.  Treating expiry as a fresh `INCOMPLETE` entry would discard a
/// still-usable binding and emit a state transition the kernel never makes.
struct ArpNeighbor {
    hardware_address: Option<MacAddress>,
    /// Whether the control plane installed this entry as permanent.
    permanent: bool,
    /// Whether a valid dynamic entry was learned without asserting
    /// reachability.
    stale: bool,
    /// Whether resolution exhausted its solicitation budget.  Linux keeps the
    /// entry as `NUD_FAILED` (without exposing the cached link address) and
    /// restarts resolution if traffic arrives again.
    failed: bool,
    /// Last accepted ARP update, used to retain the first binding for
    /// Linux's one-second ARP locktime.
    updated_at: MonotonicInstant,
    /// When this dynamic entry expires (TTL = 300s).
    expires_at: MonotonicInstant,
    /// Solicitation attempts made for an incomplete entry.
    probes: u8,
    /// Next solicitation deadline while resolution is in progress.
    next_probe_at: Option<MonotonicInstant>,
    /// Source address retained for unresolved-neighbor retries.
    probe_source: Option<crate::ip::Ipv4Address>,
}

/// Queued IP packet awaiting ARP resolution.
struct PendingTxPacket {
    next_hop: IpAddress,
    source_addr: crate::ip::Ipv4Address,
    packet: PacketBuf,
}

/// Inputs to data-plane ARP learning for one accepted packet.
struct ArpLearnUpdate {
    ifindex: i32,
    source_ip: IpAddress,
    source_hardware_addr: MacAddress,
    state: NeighborState,
    is_garp: bool,
    request_to_local: bool,
    now: MonotonicInstant,
}

/// Ethernet device backed by a driver-provided NIC.
pub(crate) struct EthernetDevice {
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    inner: ClassDevice<NetDeviceImpl>,
    neighbors: HashMap<IpAddress, ArpNeighbor>,
    /// Earliest time at which the whole neighbor table may be scanned for
    /// expiry or retry work again.
    next_neighbor_scan: Option<MonotonicInstant>,
    assigned_ipv4_addrs: Vec<Ipv4Cidr>,
    local_ipv4_addrs: Vec<Ipv4Cidr>,

    pending_tx: VecDeque<PendingTxPacket>,
    /// Neighbor changes caused by ARP traffic or resolution, owned by the
    /// Router until it publishes them after releasing the device lock.
    pending_neighbor_events: Vec<NeighborEvent>,
    rx_source: Option<Arc<NetRxPollSource>>,
}

impl EthernetDevice {
    const NEIGHBOR_LOCKTIME: TimeSpan = TimeSpan::from_secs(1);
    /// Linux ARP uses three multicast solicitations, one second apart.
    const NEIGHBOR_MAX_PROBES: u8 = 3;
    const NEIGHBOR_RETRANS: TimeSpan = TimeSpan::from_secs(1);
    const NEIGHBOR_TTL: TimeSpan = TimeSpan::from_secs(300);

    /// Create a new Ethernet device wrapper.
    pub(crate) fn new(name: String, inner: ClassDevice<NetDeviceImpl>) -> Self {
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
            next_neighbor_scan: None,
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
            pending_tx: VecDeque::with_capacity(ETHERNET_MAX_PENDING_PACKETS),
            pending_neighbor_events: Vec::new(),
            rx_source,
        }
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
            EtherType::Arp => self.handle_arp_packet(
                ifindex,
                frame.payload(),
                timestamp,
                dst_addr == self.mac_addr(),
            ),
            _ => {}
        }

        None
    }

    fn arp_source_address(&self) -> Option<crate::ip::Ipv4Address> {
        self.assigned_ipv4_addrs
            .first()
            .map(|cidr| cidr.address())
            .or_else(|| self.local_ipv4_addrs.first().map(|cidr| cidr.address()))
    }

    /// Emits one ARP request without changing neighbor state.
    fn emit_arp_request(
        &mut self,
        ifindex: i32,
        target_ip: IpAddress,
        source_addr: crate::ip::Ipv4Address,
    ) -> bool {
        let IpAddress::Ipv4(target_ipv4) = target_ip else {
            warn!("IPv6 address ARP is not supported: {}", target_ip);
            return false;
        };
        // Linux `arp_solicit()` falls back to `inet_select_addr()` and still
        // emits a request when no usable local address exists, using 0.0.0.0
        // as the sender protocol address.  Such a probe is valid and must
        // consume the solicitation budget instead of spinning on a timer.
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
        true
    }

    /// Starts resolution for a missing or failed entry and publishes its
    /// `INCOMPLETE` transition.  The entry stays in the table while probes are
    /// retried.
    fn start_neighbor_resolution(
        &mut self,
        ifindex: i32,
        target_ip: IpAddress,
        source_addr: crate::ip::Ipv4Address,
        now: MonotonicInstant,
    ) {
        let before = self.neighbor_state(target_ip);
        if !matches!(before, None | Some(NeighborState::Failed)) {
            return;
        }
        if !self.emit_arp_request(ifindex, target_ip, source_addr) {
            return;
        }
        if let Some(neighbor) = self.neighbors.get_mut(&target_ip) {
            // A failed entry keeps its old binding only for diagnostics.  A
            // new resolution must not send data to it before ARP confirms it.
            neighbor.hardware_address = None;
            neighbor.stale = false;
            neighbor.failed = false;
            neighbor.updated_at = now;
            neighbor.expires_at = now + Self::NEIGHBOR_TTL;
            neighbor.probes = 1;
            neighbor.next_probe_at = Some(now + Self::NEIGHBOR_RETRANS);
            neighbor.probe_source = Some(source_addr);
        } else {
            self.neighbors.insert(
                target_ip,
                ArpNeighbor {
                    hardware_address: None,
                    permanent: false,
                    stale: false,
                    failed: false,
                    updated_at: now,
                    expires_at: now + Self::NEIGHBOR_TTL,
                    probes: 1,
                    next_probe_at: Some(now + Self::NEIGHBOR_RETRANS),
                    probe_source: Some(source_addr),
                },
            );
        }
        self.record_neighbor_change(ifindex, target_ip, before, NeighborState::Incomplete);
    }

    /// Sends one probe for an unresolved entry and advances its retry
    /// deadline.  Exhausting the budget publishes `NUD_FAILED` and keeps the
    /// entry for a later retry, matching Linux's `neigh_timer_handler()`.
    fn probe_incomplete_neighbor(
        &mut self,
        ifindex: i32,
        target_ip: IpAddress,
        now: MonotonicInstant,
    ) {
        let should_probe = self.neighbors.get(&target_ip).is_some_and(|neighbor| {
            neighbor.hardware_address.is_none()
                && neighbor
                    .next_probe_at
                    .is_some_and(|deadline| deadline <= now)
        });
        if !should_probe {
            return;
        }
        let probes = self
            .neighbors
            .get(&target_ip)
            .map(|neighbor| neighbor.probes)
            .unwrap_or(Self::NEIGHBOR_MAX_PROBES);
        if probes >= Self::NEIGHBOR_MAX_PROBES {
            self.fail_neighbor(ifindex, target_ip, now);
            return;
        }
        let source_addr = self
            .neighbors
            .get(&target_ip)
            .and_then(|neighbor| neighbor.probe_source)
            .or_else(|| self.arp_source_address())
            .unwrap_or(crate::ip::Ipv4Address::UNSPECIFIED);
        // Linux increments `neigh->probes` after `solicit()` whether or not
        // the transmit path accepted the request.  Do the same so an
        // unserviceable probe still exhausts its budget and reaches FAILED.
        let _emitted = self.emit_arp_request(ifindex, target_ip, source_addr);
        if let Some(neighbor) = self.neighbors.get_mut(&target_ip) {
            neighbor.probes = neighbor.probes.saturating_add(1);
            neighbor.next_probe_at = Some(now + Self::NEIGHBOR_RETRANS);
        }
    }

    /// Sends one probe for an aged but still valid binding.  The old hardware
    /// address remains in use while the probe is outstanding.
    fn probe_valid_neighbor(
        &mut self,
        ifindex: i32,
        dst: IpAddress,
        source_addr: crate::ip::Ipv4Address,
        now: MonotonicInstant,
    ) {
        let should_probe = self.neighbors.get(&dst).is_some_and(|neighbor| {
            neighbor.hardware_address.is_some()
                && neighbor.stale
                && !neighbor.failed
                && !neighbor.permanent
                && neighbor.next_probe_at.is_none()
        });
        if !should_probe {
            return;
        }
        let _emitted = self.emit_arp_request(ifindex, dst, source_addr);
        if let Some(neighbor) = self.neighbors.get_mut(&dst) {
            neighbor.probes = 1;
            neighbor.next_probe_at = Some(now + Self::NEIGHBOR_RETRANS);
        }
    }

    /// Marks an entry `NUD_FAILED` after its solicitation budget is spent.
    ///
    /// Linux retains the entry (and publishes `RTM_NEWNEIGH` with
    /// `NUD_FAILED`) instead of deleting it.  The cached link address is no
    /// longer valid and is not exposed, but the entry keeps enough identity to
    /// restart resolution in place when traffic arrives again.  The normal
    /// expiry timer garbage-collects it after the neighbor TTL.
    fn fail_neighbor(&mut self, ifindex: i32, dst: IpAddress, now: MonotonicInstant) {
        let before = self.neighbor_state(dst);
        let Some(neighbor) = self.neighbors.get_mut(&dst) else {
            return;
        };
        if matches!(before, Some(NeighborState::Failed)) {
            return;
        }
        neighbor.failed = true;
        neighbor.stale = false;
        neighbor.expires_at = now + Self::NEIGHBOR_TTL;
        neighbor.next_probe_at = None;
        neighbor.probes = Self::NEIGHBOR_MAX_PROBES;
        neighbor.probe_source = None;
        self.pending_tx.retain(|pending| pending.next_hop != dst);
        self.record_neighbor_change(ifindex, dst, before, NeighborState::Failed);
    }

    /// Ages a valid dynamic entry in place.  The binding remains usable and
    /// only the externally visible state changes to `STALE`.
    fn expire_valid_neighbor(
        &mut self,
        ifindex: i32,
        dst: IpAddress,
        now: MonotonicInstant,
    ) -> Option<MacAddress> {
        let before = self.neighbor_state(dst);
        let neighbor = self.neighbors.get_mut(&dst)?;
        if neighbor.permanent || neighbor.hardware_address.is_none() {
            return neighbor.hardware_address;
        }
        let hardware_address = neighbor.hardware_address;
        if neighbor.expires_at > now {
            return hardware_address;
        }
        neighbor.stale = true;
        neighbor.updated_at = now;
        neighbor.expires_at = now + Self::NEIGHBOR_TTL;
        neighbor.probes = 0;
        neighbor.next_probe_at = None;
        self.record_neighbor_change(
            ifindex,
            dst,
            before,
            NeighborState::Stale {
                hardware_addr: hardware_address.expect("checked valid entry").0,
            },
        );
        hardware_address
    }

    /// Runs neighbor expiry and unresolved-retry timers.  Called from the
    /// device poll path even when no frame arrived, because probe retries and
    /// failure are time-driven.
    ///
    /// Both timer kinds have one-second granularity, so scanning at most once
    /// per retransmit interval avoids rebuilding the key snapshot for every
    /// received frame.  Remote ARP traffic influences the table size, so
    /// timer housekeeping must not multiply the cost of the receive path.
    fn process_neighbor_timers(&mut self, ifindex: i32, now: MonotonicInstant) {
        if self.neighbors.is_empty() {
            return;
        }
        if self
            .next_neighbor_scan
            .is_some_and(|deadline| now < deadline)
        {
            return;
        }
        self.next_neighbor_scan = Some(now + Self::NEIGHBOR_RETRANS);
        let destinations: Vec<IpAddress> = self.neighbors.keys().copied().collect();
        for dst in destinations {
            let Some(neighbor) = self.neighbors.get(&dst) else {
                continue;
            };
            if neighbor.failed {
                // `NUD_FAILED` has no probe timer.  Reclaim it once its TTL
                // expires; Linux's GC does the same for unreferenced failed
                // entries.
                if neighbor.expires_at <= now {
                    let before = self.neighbor_state(dst);
                    self.neighbors.remove(&dst);
                    if let Some(before) = before {
                        self.record_neighbor_removal(ifindex, dst, before);
                    }
                }
                continue;
            }
            let is_incomplete = neighbor.hardware_address.is_none();
            if is_incomplete {
                self.probe_incomplete_neighbor(ifindex, dst, now);
                continue;
            }
            if self.expire_valid_neighbor(ifindex, dst, now).is_none() {
                continue;
            }
            let probe_due = self.neighbors.get(&dst).is_some_and(|neighbor| {
                neighbor
                    .next_probe_at
                    .is_some_and(|deadline| deadline <= now)
            });
            if !probe_due {
                continue;
            }
            let probes = self
                .neighbors
                .get(&dst)
                .map(|neighbor| neighbor.probes)
                .unwrap_or(Self::NEIGHBOR_MAX_PROBES);
            if probes >= Self::NEIGHBOR_MAX_PROBES {
                self.fail_neighbor(ifindex, dst, now);
                continue;
            }
            let source_addr = self
                .arp_source_address()
                .unwrap_or(crate::ip::Ipv4Address::UNSPECIFIED);
            // As in `neigh_probe()`, the request consumes a probe even when
            // the device cannot transmit it.
            let _emitted = self.emit_arp_request(ifindex, dst, source_addr);
            if let Some(neighbor) = self.neighbors.get_mut(&dst) {
                neighbor.probes = neighbor.probes.saturating_add(1);
                neighbor.next_probe_at = Some(now + Self::NEIGHBOR_RETRANS);
            }
        }
    }

    /// Queues a data-plane neighbor transition when it changes externally
    /// visible state.  Control-plane updates are reported by the Router.
    fn record_neighbor_change(
        &mut self,
        ifindex: i32,
        dst: IpAddress,
        before: Option<NeighborState>,
        after: NeighborState,
    ) {
        if before == Some(after) {
            return;
        }
        let Some(dev) = NetDeviceId::new(ifindex) else {
            warn!("Ignoring neighbor change on invalid ifindex {ifindex}");
            return;
        };
        self.pending_neighbor_events
            .push(NeighborEvent::Added(NeighborSnapshot {
                dev,
                dst,
                state: after,
            }));
    }

    fn record_neighbor_removal(&mut self, ifindex: i32, dst: IpAddress, state: NeighborState) {
        let Some(dev) = NetDeviceId::new(ifindex) else {
            warn!("Ignoring neighbor removal on invalid ifindex {ifindex}");
            return;
        };
        self.pending_neighbor_events
            .push(NeighborEvent::Removed(NeighborSnapshot { dev, dst, state }));
    }

    /// Applies data-plane ARP learning.
    ///
    /// Linux updates an existing entry for any accepted ARP packet but creates
    /// entries only for requests addressed to a local address (or when
    /// `arp_accept` is enabled, which this stack does not expose).  A different
    /// hardware address is accepted only after the one-second locktime or when
    /// the packet is a valid gratuitous ARP / request to a local address.
    /// Identical updates do not refresh the locktime or publish an event.
    fn learn_arp_neighbor(&mut self, update: ArpLearnUpdate) {
        let ArpLearnUpdate {
            ifindex,
            source_ip,
            source_hardware_addr,
            state,
            is_garp,
            request_to_local,
            now,
        } = update;
        let before = self.neighbor_state(source_ip);
        if matches!(before, Some(NeighborState::Permanent { .. })) {
            return;
        }
        let exists = before.is_some();
        if !exists && !request_to_local {
            return;
        }

        let old_hardware_addr = before.and_then(NeighborState::hardware_addr);
        // `__neigh_update()` applies the LOCKTIME/OVERRIDE check only while
        // the old entry is valid.  An INCOMPLETE entry has no binding to
        // protect, so a reply that resolves it is accepted even inside the
        // lock window.
        let old_is_valid = matches!(
            before,
            Some(
                NeighborState::Reachable { .. }
                    | NeighborState::Stale { .. }
                    | NeighborState::Permanent { .. }
            )
        );
        let locktime_expired = self
            .neighbors
            .get(&source_ip)
            .is_some_and(|neighbor| now > neighbor.updated_at + Self::NEIGHBOR_LOCKTIME);
        // `arp_process()` computes the override bit from LOCKTIME or GARP, and
        // `neigh_event_ns()` always passes `NEIGH_UPDATE_F_OVERRIDE` for a
        // request addressed to a local address.
        let override_locktime = is_garp || request_to_local || locktime_expired;
        if old_hardware_addr == Some(source_hardware_addr.0) {
            let Some(neighbor) = self.neighbors.get_mut(&source_ip) else {
                return;
            };
            let previous_state = before.expect("existing entry");
            // `__neigh_update()` preserves an existing valid state when the
            // new state is STALE and the link-layer address is unchanged.
            let effective_state = if matches!(state, NeighborState::Stale { .. })
                && matches!(
                    previous_state,
                    NeighborState::Reachable { .. } | NeighborState::Stale { .. }
                ) {
                previous_state
            } else {
                state
            };
            // A no-op update neither moves the LOCKTIME window nor emits a
            // notification.  Linux still refreshes `confirmed` for any
            // incoming connected state, which extends the REACHABLE
            // lifetime without changing the stored binding.
            if effective_state == previous_state {
                if neighbor_state_is_connected(state) {
                    neighbor.expires_at = now + Self::NEIGHBOR_TTL;
                }
                return;
            }
            neighbor.hardware_address = Some(source_hardware_addr);
            neighbor.stale = matches!(effective_state, NeighborState::Stale { .. });
            neighbor.failed = false;
            neighbor.updated_at = now;
            neighbor.expires_at = now + Self::NEIGHBOR_TTL;
            neighbor.probes = 0;
            neighbor.next_probe_at = None;
            self.record_neighbor_change(ifindex, source_ip, Some(previous_state), effective_state);
            return;
        }
        if old_is_valid && !override_locktime {
            return;
        }

        self.neighbors.insert(
            source_ip,
            ArpNeighbor {
                hardware_address: Some(source_hardware_addr),
                permanent: false,
                stale: matches!(state, NeighborState::Stale { .. }),
                failed: false,
                updated_at: now,
                expires_at: now + Self::NEIGHBOR_TTL,
                probes: 0,
                next_probe_at: None,
                probe_source: None,
            },
        );
        self.record_neighbor_change(ifindex, source_ip, before, state);
    }

    /// Sends queued packets whose next hop has a usable binding.  Incomplete
    /// entries remain queued; their timer either resolves them or removes the
    /// entry and drops the packets explicitly.
    fn drain_pending_tx(&mut self, ifindex: i32, now: MonotonicInstant) {
        let mut kept_packets = Vec::with_capacity(ETHERNET_MAX_PENDING_PACKETS);
        for _ in 0..ETHERNET_MAX_PENDING_PACKETS {
            let Some(pending) = self.pending_tx.pop_front() else {
                break;
            };
            let hardware_address = self.expire_valid_neighbor(ifindex, pending.next_hop, now);
            match hardware_address {
                Some(hardware_address) => {
                    self.send_ipv4_packet_to(ifindex, hardware_address, &pending.packet)
                }
                None => kept_packets.push(pending),
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

    fn handle_arp_packet(
        &mut self,
        ifindex: i32,
        payload: &[u8],
        now: MonotonicInstant,
        is_packet_to_host: bool,
    ) {
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

        if let ArpOperation::Unknown(_) = operation {
            return;
        }
        if !source_hardware_addr.is_unicast() {
            return;
        }
        if target_protocol_addr.is_multicast() || target_protocol_addr.octets()[0] == 127 {
            return;
        }

        let request_to_local = matches!(operation, ArpOperation::Request)
            && self
                .local_ipv4_addrs
                .iter()
                .any(|addr| addr.address() == target_protocol_addr);

        // RFC 2131 duplicate-address-detection requests use a zero sender
        // address.  Linux replies when the target is local but never caches
        // the sender.
        if source_protocol_addr.is_unspecified() {
            if request_to_local {
                self.send_arp_reply(
                    ifindex,
                    target_protocol_addr,
                    source_hardware_addr,
                    source_protocol_addr,
                );
            }
            return;
        }
        if source_protocol_addr.is_broadcast() || source_protocol_addr.is_multicast() {
            return;
        }

        trace!("ARP: {} -> {}", source_protocol_addr, source_hardware_addr);
        let source_ip = IpAddress::Ipv4(source_protocol_addr);
        let source_is_local = self
            .local_ipv4_addrs
            .iter()
            .any(|addr| addr.address() == source_protocol_addr);
        // `arp_is_garp()` also requires the source address to classify as
        // RTN_UNICAST.  The stack has no separate address-type lookup here, so
        // reject the non-unicast cases represented by assigned prefixes.
        let source_is_directed_broadcast = self
            .local_ipv4_addrs
            .iter()
            .any(|cidr| cidr.is_directed_broadcast(source_protocol_addr));
        let is_garp = target_protocol_addr == source_protocol_addr
            && !source_is_local
            && source_protocol_addr.is_unicast()
            && !source_is_directed_broadcast
            && match operation {
                ArpOperation::Reply => target_hardware_addr == source_hardware_addr,
                ArpOperation::Request => true,
                ArpOperation::Unknown(_) => false,
            };
        // Broadcast replies and request packets do not assert reachability.
        // Only a reply delivered directly to this device's Ethernet address is
        // `PACKET_HOST`.
        let state = if matches!(operation, ArpOperation::Reply) && is_packet_to_host {
            NeighborState::Reachable {
                hardware_addr: source_hardware_addr.0,
            }
        } else {
            NeighborState::Stale {
                hardware_addr: source_hardware_addr.0,
            }
        };
        self.learn_arp_neighbor(ArpLearnUpdate {
            ifindex,
            source_ip,
            source_hardware_addr,
            state,
            is_garp,
            request_to_local,
            now,
        });

        if request_to_local {
            self.send_arp_reply(
                ifindex,
                target_protocol_addr,
                source_hardware_addr,
                source_protocol_addr,
            );
        }

        self.drain_pending_tx(ifindex, now);
    }

    fn send_arp_reply(
        &mut self,
        ifindex: i32,
        local_protocol_addr: crate::ip::Ipv4Address,
        target_hardware_addr: MacAddress,
        target_protocol_addr: crate::ip::Ipv4Address,
    ) {
        let response = ArpIpv4Packet {
            operation: ArpOperation::Reply,
            source_hardware_addr: self.mac_addr(),
            source_protocol_addr: local_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        };
        self.send_to(
            ifindex,
            target_hardware_addr,
            ArpIpv4Packet::LEN,
            |buf| {
                let emit_result = response.emit(buf);
                debug_assert!(emit_result.is_some());
            },
            EtherType::Arp,
        );
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

    #[cfg(unittest)]
    fn device_id(&self) -> Option<kdevice::DeviceId> {
        Some(self.inner.id())
    }

    fn publication_id(&self) -> Option<kdevice::PublicationId> {
        Some(self.inner.publication_id())
    }

    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> KResult<RxPoll> {
        if !self.is_link_up() {
            return Ok(RxPoll::Empty);
        }
        // Neighbor probes and expiry are time-driven.  They must advance even
        // when the NIC has no receive work, otherwise an unresolved entry can
        // stay pending forever and a valid entry never transitions to STALE.
        self.process_neighbor_timers(ifindex, timestamp);
        let rx_buf: NetBufHandle = match self.inner.recv() {
            Ok(buf) => buf,
            Err(DriverError::WouldBlock) => return Ok(RxPoll::Empty),
            Err(err) => return Err(map_dev_err(err)),
        };
        trace!("RECV {} bytes: {:02X?}", rx_buf.len(), rx_buf.data());
        let packet = self.handle_rx_frame(ifindex, rx_buf.data(), timestamp);
        self.inner.recycle_rx(rx_buf).map_err(map_dev_err)?;
        Ok(RxPoll::Consumed(
            packet.map(super::net_rx::RxPacket::Network),
        ))
    }

    fn has_rx_work(&self) -> bool {
        // `poll_rx` refuses to consume while the link is administratively
        // down. Reporting queued frames as work in that state makes the
        // poller requeue itself forever without making progress, so keep the
        // buffered frames for when the link returns instead.
        self.is_link_up() && self.inner.can_rx()
    }

    fn next_neighbor_deadline(&self) -> Option<MonotonicInstant> {
        // `poll_rx` deliberately freezes neighbor state while the link is
        // administratively down. Reporting a deadline that the poll path
        // cannot service would leave the Service timer permanently expired
        // and spin the poller until the link comes back.
        if !self.is_link_up() || self.neighbors.is_empty() {
            return None;
        }
        let state_deadline = self
            .neighbors
            .values()
            .filter(|neighbor| !neighbor.permanent)
            .flat_map(|neighbor| [neighbor.next_probe_at, Some(neighbor.expires_at)])
            .flatten()
            .min()?;
        // `process_neighbor_timers` deliberately coalesces scans.  A state
        // deadline earlier than the next permitted scan cannot make progress,
        // so report the later instant to avoid an immediate-wakeup loop.  A
        // table with no state deadline needs no scan at all.
        Some(
            self.next_neighbor_scan
                .map_or(state_deadline, |scan| scan.max(state_deadline)),
        )
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

        let neighbor = self.neighbors.get(&next_hop).map(|neighbor| {
            (
                neighbor.hardware_address,
                neighbor.expires_at,
                neighbor.permanent,
                neighbor.stale,
                neighbor.failed,
            )
        });
        match neighbor {
            Some((_hardware_address, _, _, _, true)) => {
                // `NUD_FAILED` is not a usable binding.  Linux moves it back
                // to INCOMPLETE and starts a fresh solicitation on the next
                // packet, retaining the entry instead of reporting a
                // neighbour deletion.
                self.start_neighbor_resolution(ifindex, next_hop, source_addr, timestamp);
            }
            Some((Some(hardware_address), expires_at, permanent, stale, false)) => {
                // Expiry demotes the entry to STALE but keeps the cached
                // address usable.  Traffic must revalidate that address; the
                // timer also retries an outstanding probe, so only start one
                // when no retry is already scheduled.
                let expired = !permanent && expires_at <= timestamp;
                if expired {
                    self.expire_valid_neighbor(ifindex, next_hop, timestamp);
                }
                if stale || expired {
                    self.probe_valid_neighbor(ifindex, next_hop, source_addr, timestamp);
                }
                self.send_ipv4_packet_to(ifindex, hardware_address, &packet);
                return false;
            }
            Some((None, ..)) => {
                // Resolution is already in progress; its retry timer owns the
                // next solicitation.
            }
            None => self.start_neighbor_resolution(ifindex, next_hop, source_addr, timestamp),
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

    fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        can_override: bool,
    ) -> Result<(), LinuxError> {
        if !matches!(update.dst, IpAddress::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        let ifindex = update.dev.ifindex();
        let old_state = self.neighbor_state(update.dst);
        let old_usable = old_state.is_some_and(|state| state.hardware_addr().is_some());
        if !can_override
            && let (Some(old), Some(new)) = (
                old_state.and_then(NeighborState::hardware_addr),
                update.state.hardware_addr(),
            )
            && old != new
        {
            // Linux `__neigh_update` leaves an existing valid entry untouched
            // when the request proposes a different address without
            // `NEIGH_UPDATE_F_OVERRIDE`; it succeeds without a notification.
            return Ok(());
        }

        // `__neigh_update` refreshes `confirmed` for an incoming connected
        // state even when the stored state and link-layer address do not
        // change.  That extends the entry's usable lifetime without moving
        // `updated`, probing the peer, or publishing a notification.
        if old_state == Some(update.state) {
            if neighbor_state_is_connected(update.state)
                && let Some(neighbor) = self.neighbors.get_mut(&update.dst)
            {
                neighbor.expires_at = khal::time::monotonic_time() + Self::NEIGHBOR_TTL;
            }
            return Ok(());
        }

        // A plain NUD_STALE update that carries the current address does not
        // demote a reachable entry; Linux keeps the stronger existing state.
        let new_state = if !can_override
            && !matches!(old_state, Some(NeighborState::Permanent { .. }))
            && let (Some(old), NeighborState::Stale { hardware_addr }) = (
                old_state.and_then(NeighborState::hardware_addr),
                update.state,
            )
            && old == hardware_addr
        {
            old_state.expect("matched an existing state")
        } else {
            update.state
        };
        if old_state == Some(new_state) {
            return Ok(());
        }

        let now = khal::time::monotonic_time();
        let neighbor = match new_state {
            NeighborState::Incomplete => ArpNeighbor {
                hardware_address: None,
                permanent: false,
                stale: false,
                failed: false,
                updated_at: now,
                expires_at: now + Self::NEIGHBOR_TTL,
                probes: 0,
                // An administrative INCOMPLETE entry starts probing on the
                // next poll, using the device's current IPv4 source.
                next_probe_at: Some(now),
                probe_source: None,
            },
            NeighborState::Reachable { hardware_addr } | NeighborState::Stale { hardware_addr } => {
                ArpNeighbor {
                    hardware_address: Some(MacAddress(hardware_addr)),
                    permanent: false,
                    stale: matches!(new_state, NeighborState::Stale { .. }),
                    failed: false,
                    updated_at: now,
                    expires_at: now + Self::NEIGHBOR_TTL,
                    probes: 0,
                    next_probe_at: None,
                    probe_source: None,
                }
            }
            NeighborState::Failed => return Err(LinuxError::EOPNOTSUPP),
            NeighborState::Permanent { hardware_addr } => ArpNeighbor {
                hardware_address: Some(MacAddress(hardware_addr)),
                permanent: true,
                stale: false,
                failed: false,
                updated_at: now,
                expires_at: MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(
                    u64::MAX / 2,
                )),
                probes: 0,
                next_probe_at: None,
                probe_source: None,
            },
        };
        let new_usable = neighbor.hardware_address.is_some();
        self.neighbors.insert(update.dst, neighbor);
        // A new control-plane entry can need immediate work, such as the first
        // probe of an administrative INCOMPLETE entry.  Do not let an earlier
        // scan-coalescing deadline delay it.
        self.next_neighbor_scan = None;
        // Linux `__neigh_update()` reinjects the ARP queue only when the entry
        // made the transition from invalid to valid.  A binding replacement
        // for an already-usable neighbor leaves queued packets alone.
        if !old_usable && new_usable {
            self.drain_pending_tx(ifindex, now);
        }
        Ok(())
    }

    fn has_neighbor(&self, dst: IpAddress) -> bool {
        self.neighbors.contains_key(&dst)
    }

    fn neighbor_state(&self, dst: IpAddress) -> Option<NeighborState> {
        let neighbor = self.neighbors.get(&dst)?;
        if neighbor.failed {
            return Some(NeighborState::Failed);
        }
        let Some(hardware_address) = neighbor.hardware_address else {
            return Some(NeighborState::Incomplete);
        };
        let hardware_addr = hardware_address.0;
        if neighbor.permanent {
            Some(NeighborState::Permanent { hardware_addr })
        } else if neighbor.stale {
            Some(NeighborState::Stale { hardware_addr })
        } else {
            Some(NeighborState::Reachable { hardware_addr })
        }
    }

    fn neighbor_snapshots(&self, dev: NetDeviceId) -> Vec<NeighborSnapshot> {
        self.neighbors
            .keys()
            .map(|dst| NeighborSnapshot {
                dev,
                dst: *dst,
                state: self
                    .neighbor_state(*dst)
                    .expect("snapshot keys come from the same neighbor table"),
            })
            .collect()
    }

    fn remove_neighbor(&mut self, dst: IpAddress) -> bool {
        let removed = self.neighbors.remove(&dst).is_some();
        if removed {
            self.pending_tx.retain(|pending| pending.next_hop != dst);
        }
        removed
    }

    fn take_neighbor_events(&mut self) -> Vec<NeighborEvent> {
        core::mem::take(&mut self.pending_neighbor_events)
    }
}

fn neighbor_state_is_connected(state: NeighborState) -> bool {
    matches!(
        state,
        NeighborState::Reachable { .. } | NeighborState::Stale { .. }
    )
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

unittest::path_mod_test!(rx_budget_tests, "../../ut-tests/ethernet_rx_budget.rs");

unittest::path_mod_test!(tests, "../../ut-tests/device/ethernet/tests.rs");
