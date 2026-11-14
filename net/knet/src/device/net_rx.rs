// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared `NetRx` packet queue.
//!
//! Analog of Linux `softnet_data.input_pkt_queue`: producers enqueue a
//! [`RxPacket`] and raise `NetRx`; the softirq (or a task-context fallback)
//! drains stamped UDP. X-Kernel currently has one netns and one loopback, so one
//! global queue state is enough. `NetRx` does not look up `LoopbackDevice`.
//!
//! Complete IPv4 UDP is stamped in task context before BH is disabled
//! ([`prepare_for_enqueue`]). The action then moves those handles into the PCB
//! queue. That keeps `Arc::make_mut` / parser work off the `SoftirqAction` hot
//! path, while PCB enqueue itself only splices an existing handle into a
//! reserved `VecDeque`.
//!
//! Deferred work records its receive stage. Unmatched UDP retains its prepared
//! view and resumes local delivery in task context without running INPUT again.
//! Other packets enter ordinary network input.
//!
//! Occupancy is `pending_udp.len() + deferred.len() + in_flight`. `in_flight`
//! covers input preparation and UDP delivery outside the lock, so producers
//! cannot take those slots before packets are queued or returned. Overflow
//! returns the packet to the caller to drop after unlock. `discard_ifindex`
//! likewise drops after unlock.

use alloc::{collections::VecDeque, vec::Vec};

use kspin::SpinNoIrq;
use lazyinit::LazyInit;

use crate::{buf::PacketBuf, consts::SOCKET_BUFFER_SIZE, ipv4, netfilter::FilterConfig, udp};

/// Packets processed by one `NetRx` softirq invocation.
pub(crate) const NET_RX_BUDGET: usize = 16;

/// Receive work waiting for network input or continuing after INPUT acceptance.
#[derive(Debug)]
pub(crate) enum RxPacket {
    Network(PacketBuf),
    LocalUdp(udp::PreparedUdpPacket),
}

impl RxPacket {
    fn packet(&self) -> &PacketBuf {
        match self {
            Self::Network(packet) => packet,
            Self::LocalUdp(packet) => packet.packet(),
        }
    }
}

impl From<PacketBuf> for RxPacket {
    fn from(packet: PacketBuf) -> Self {
        Self::Network(packet)
    }
}

struct NetRxQueues {
    pending_udp: VecDeque<udp::PreparedUdpPacket>,
    deferred: VecDeque<RxPacket>,
    /// Slots held outside the lock for input preparation or UDP delivery.
    in_flight: usize,
}

impl NetRxQueues {
    fn occupancy(&self) -> usize {
        self.pending_udp.len() + self.deferred.len() + self.in_flight
    }
}

static NET_RX_QUEUE: LazyInit<SpinNoIrq<NetRxQueues>> = LazyInit::new();

/// Initialize the shared queue with pre-allocated capacity.
///
/// Called from [`super::ethernet::ensure_net_rx_softirq_available`] so the
/// queue is ready before the softirq vector can ever fire, and before any
/// loopback xmit enqueues.
pub(crate) fn ensure_net_rx_queue() {
    NET_RX_QUEUE.call_once(|| {
        SpinNoIrq::new(NetRxQueues {
            pending_udp: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            deferred: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            in_flight: 0,
        })
    });
}

#[cfg(unittest)]
pub(crate) fn queued_len() -> usize {
    ensure_net_rx_queue();
    NET_RX_QUEUE.lock().occupancy()
}

/// Returns whether the shared queue may contain work for `ifindex`.
///
/// `in_flight` does not retain per-device counts, so a reserved slot
/// conservatively reports work for every queried interface. The runtime
/// currently has one loopback device, which keeps this approximation bounded.
pub(crate) fn has_work_for(ifindex: i32) -> bool {
    ensure_net_rx_queue();
    let queues = NET_RX_QUEUE.lock();
    queues.in_flight > 0
        || queues
            .pending_udp
            .iter()
            .map(udp::PreparedUdpPacket::packet)
            .chain(queues.deferred.iter().map(RxPacket::packet))
            .any(|packet| packet.ifindex() == ifindex)
}

/// Prepares receive work in task context, applying INPUT to complete IPv4 UDP.
///
/// Applies INPUT before UDP parsing. A filter or parser drop returns `None`.
fn prepare_for_enqueue(
    mut packet: PacketBuf,
    indev: &str,
    filter: &FilterConfig,
) -> Option<RxPacket> {
    debug_assert!(
        !kirq::context::is_in_interrupt_context(),
        "prepare_for_enqueue requires task context"
    );
    let is_udp = packet
        .network_packet()
        .and_then(|ip_packet| ipv4::Ipv4Header::parse_header_only(ip_packet).ok())
        .is_some_and(|header| header.protocol() == ipv4::PROTOCOL_UDP && !header.is_fragmented());
    if !is_udp {
        return Some(RxPacket::Network(packet));
    }
    ipv4::validate_input_packet(&mut packet).ok()?;
    if !filter.accept_input(&packet, indev) {
        return None;
    }
    udp::prepare_ipv4_packet(packet)
        .ok()
        .map(RxPacket::LocalUdp)
}

/// Reserves queue capacity before preparing input in task context.
/// Returns `Err(packet)` when full, without evaluating INPUT or changing its counters.
pub(crate) fn enqueue(
    packet: PacketBuf,
    indev: &str,
    filter: &FilterConfig,
) -> Result<(), PacketBuf> {
    ensure_net_rx_queue();
    {
        let mut queues = NET_RX_QUEUE.lock();
        if queues.occupancy() >= SOCKET_BUFFER_SIZE {
            return Err(packet);
        }
        queues.in_flight += 1;
    }
    let packet = prepare_for_enqueue(packet, indev, filter);
    let mut queues = NET_RX_QUEUE.lock();
    queues.in_flight -= 1;
    let Some(packet) = packet else {
        return Ok(());
    };
    match packet {
        RxPacket::LocalUdp(packet) => {
            debug_assert!(queues.pending_udp.len() < queues.pending_udp.capacity());
            queues.pending_udp.push_back(packet);
        }
        RxPacket::Network(packet) => {
            debug_assert!(queues.deferred.len() < queues.deferred.capacity());
            queues.deferred.push_back(RxPacket::Network(packet));
        }
    }
    Ok(())
}

/// Deliver up to `budget` stamped UDP packets without holding `NET_RX_QUEUE`
/// across PCB enqueue or waiter wake.
///
/// Returns whether stamped UDP remains after this batch, so the softirq can
/// re-raise and [`drain_pending`] can continue.
pub(crate) fn process_pending(budget: usize) -> bool {
    ensure_net_rx_queue();
    let batch_budget = budget.min(NET_RX_BUDGET);
    let mut batch: [Option<udp::PreparedUdpPacket>; NET_RX_BUDGET] = core::array::from_fn(|_| None);
    let mut batch_len = 0;
    {
        let mut queues = NET_RX_QUEUE.lock();
        while batch_len < batch_budget {
            let Some(packet) = queues.pending_udp.pop_front() else {
                break;
            };
            queues.in_flight += 1;
            batch[batch_len] = Some(packet);
            batch_len += 1;
        }
        if batch_len == 0 {
            return !queues.pending_udp.is_empty();
        }
    }

    for slot in &mut batch[..batch_len] {
        let Some(packet) = slot.take() else {
            continue;
        };
        *slot = deliver_stamped_udp(packet);
    }

    let mut queues = NET_RX_QUEUE.lock();
    debug_assert!(queues.in_flight >= batch_len);
    queues.in_flight -= batch_len;
    for slot in &mut batch[..batch_len] {
        let Some(packet) = slot.take() else {
            continue;
        };
        debug_assert!(queues.occupancy() < SOCKET_BUFFER_SIZE);
        if queues.occupancy() < SOCKET_BUFFER_SIZE {
            debug_assert!(queues.deferred.len() < queues.deferred.capacity());
            queues.deferred.push_back(RxPacket::LocalUdp(packet));
        } else {
            *slot = Some(packet);
        }
    }
    !queues.pending_udp.is_empty()
}

/// Drain the stamped UDP present at entry with a bounded number of rounds.
pub(crate) fn drain_pending() {
    ensure_net_rx_queue();
    let pending_at_entry = NET_RX_QUEUE.lock().pending_udp.len();
    let rounds = pending_at_entry.div_ceil(NET_RX_BUDGET);
    for _ in 0..rounds {
        if !process_pending(NET_RX_BUDGET) {
            break;
        }
    }
}

pub(crate) fn pop_deferred(ifindex: i32) -> Option<RxPacket> {
    ensure_net_rx_queue();
    let mut queues = NET_RX_QUEUE.lock();
    let position = queues
        .deferred
        .iter()
        .position(|packet| packet.packet().ifindex() == ifindex)?;
    queues.deferred.remove(position)
}

/// Remove queued packets for `ifindex`.
///
/// Called from `LoopbackDevice::drop` in task context. Matching packets are
/// moved into a local buffer under the lock and dropped after the guard is
/// released. Packets already extracted into `in_flight` are best-effort, as
/// with Linux `skb->dev` teardown during `netif_receive_skb`.
pub(crate) fn discard_ifindex(ifindex: i32) {
    ensure_net_rx_queue();
    let mut dropped_udp = Vec::with_capacity(SOCKET_BUFFER_SIZE);
    let mut dropped_deferred = Vec::with_capacity(SOCKET_BUFFER_SIZE);
    {
        let mut queues = NET_RX_QUEUE.lock();
        take_ifindex(
            &mut queues.pending_udp,
            ifindex,
            &mut dropped_udp,
            |packet| packet.packet().ifindex(),
        );
        take_ifindex(
            &mut queues.deferred,
            ifindex,
            &mut dropped_deferred,
            |packet| packet.packet().ifindex(),
        );
    }
}

fn take_ifindex<T>(
    queue: &mut VecDeque<T>,
    ifindex: i32,
    dropped: &mut Vec<T>,
    ifindex_fn: impl Fn(&T) -> i32,
) {
    for _ in 0..queue.len() {
        let Some(packet) = queue.pop_front() else {
            break;
        };
        if ifindex_fn(&packet) == ifindex {
            dropped.push(packet);
        } else {
            queue.push_back(packet);
        }
    }
}

fn deliver_stamped_udp(packet: udp::PreparedUdpPacket) -> Option<udp::PreparedUdpPacket> {
    match udp::deliver_ipv4_packet(packet) {
        (udp::InputDisposition::Accepted, _) => None,
        (udp::InputDisposition::NoSocket, returned) => returned,
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/device/net_rx/tests.rs");
