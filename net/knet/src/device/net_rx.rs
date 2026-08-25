// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared `NetRx` packet queue.
//!
//! Analog of Linux `softnet_data.input_pkt_queue`: producers enqueue a
//! [`PacketBuf`] and raise `NetRx`; the softirq (or a task-context fallback)
//! drains stamped UDP. X-Kernel currently has one netns and one loopback, so one
//! global queue state is enough. `NetRx` does not look up `LoopbackDevice`.
//!
//! Complete IPv4 UDP is stamped in task context before BH is disabled
//! ([`prepare_for_enqueue`]). The action then moves those handles into the PCB
//! queue. That keeps `Arc::make_mut` / parser work off the `SoftirqAction` hot
//! path, while PCB enqueue itself only splices an existing handle into a
//! reserved `VecDeque`.
//!
//! Packets without UDP metadata enter a separate deferred handle queue for the
//! task poller. Unmatched UDP has its metadata cleared before joining that
//! queue, so a later `process_pending` does not pick it up again.
//!
//! Occupancy is `pending_udp.len() + deferred.len() + in_flight`. `in_flight`
//! covers packets taken out for UDP delivery so producers cannot refill those
//! slots before unmatched packets are returned. Overflow returns the packet to
//! the caller to drop after `NET_RX_QUEUE` and the BH guard, matching Linux
//! `enqueue_to_backlog` unlocking before `kfree_skb`. `discard_ifindex`
//! likewise drops after unlock.

use alloc::{collections::VecDeque, vec::Vec};

use kspin::SpinNoIrq;
use lazyinit::LazyInit;

use crate::{
    buf::PacketBuf,
    consts::SOCKET_BUFFER_SIZE,
    ipv4::{self, Ipv4Header},
    udp,
};

/// Packets processed by one `NetRx` softirq invocation.
pub(crate) const NET_RX_BUDGET: usize = 16;

struct NetRxQueues {
    pending_udp: VecDeque<PacketBuf>,
    deferred: VecDeque<PacketBuf>,
    /// UDP packets currently outside the lock for PCB delivery.
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
/// `in_flight` does not retain per-device counts, so an in-flight UDP packet
/// conservatively reports work for every queried interface. The runtime
/// currently has one loopback device, which keeps this approximation bounded.
pub(crate) fn has_work_for(ifindex: i32) -> bool {
    ensure_net_rx_queue();
    let queues = NET_RX_QUEUE.lock();
    queues.in_flight > 0
        || queues
            .pending_udp
            .iter()
            .chain(queues.deferred.iter())
            .any(|packet| packet.ifindex() == ifindex)
}

/// Stamps complete IPv4 UDP metadata onto `packet` in task context.
///
/// Returns `None` when the packet is IPv4 UDP but fails the checked parser;
/// the caller should drop it without occupying the queue. Other packets are
/// returned unchanged for the task poller.
pub(crate) fn prepare_for_enqueue(packet: PacketBuf) -> Option<PacketBuf> {
    debug_assert!(
        !kirq::context::is_in_interrupt_context(),
        "prepare_for_enqueue requires task context"
    );
    let udp_header = packet
        .network_packet()
        .and_then(|ip_packet| Ipv4Header::parse_input(ip_packet).ok())
        .filter(|header| header.protocol() == ipv4::PROTOCOL_UDP && !header.is_fragmented());
    match udp_header {
        Some(header) => match udp::prepare_ipv4_packet(header, packet) {
            Ok(prepared) => Some(prepared.into_packet()),
            Err(_) => None,
        },
        None => Some(packet),
    }
}

/// Enqueue `packet` onto the matching shared handle queue.
///
/// Returns `Err(packet)` when occupancy is already [`SOCKET_BUFFER_SIZE`].
pub(crate) fn enqueue(packet: PacketBuf) -> Result<(), PacketBuf> {
    ensure_net_rx_queue();
    let mut queues = NET_RX_QUEUE.lock();
    if queues.occupancy() >= SOCKET_BUFFER_SIZE {
        return Err(packet);
    }
    if packet.udp_metadata().is_some() {
        debug_assert!(queues.pending_udp.len() < queues.pending_udp.capacity());
        queues.pending_udp.push_back(packet);
    } else {
        debug_assert!(queues.deferred.len() < queues.deferred.capacity());
        queues.deferred.push_back(packet);
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
    let mut batch: [Option<PacketBuf>; NET_RX_BUDGET] = core::array::from_fn(|_| None);
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
        let Some(mut packet) = slot.take() else {
            continue;
        };
        packet.clear_udp_metadata();
        debug_assert!(queues.occupancy() < SOCKET_BUFFER_SIZE);
        if queues.occupancy() < SOCKET_BUFFER_SIZE {
            debug_assert!(queues.deferred.len() < queues.deferred.capacity());
            queues.deferred.push_back(packet);
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

pub(crate) fn pop_deferred(ifindex: i32) -> Option<PacketBuf> {
    ensure_net_rx_queue();
    let mut queues = NET_RX_QUEUE.lock();
    let position = queues
        .deferred
        .iter()
        .position(|packet| packet.ifindex() == ifindex)?;
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
    let mut dropped = Vec::with_capacity(SOCKET_BUFFER_SIZE);
    {
        let mut queues = NET_RX_QUEUE.lock();
        take_ifindex(&mut queues.pending_udp, ifindex, &mut dropped);
        take_ifindex(&mut queues.deferred, ifindex, &mut dropped);
    }
}

fn take_ifindex(queue: &mut VecDeque<PacketBuf>, ifindex: i32, dropped: &mut Vec<PacketBuf>) {
    for _ in 0..queue.len() {
        let Some(packet) = queue.pop_front() else {
            break;
        };
        if packet.ifindex() == ifindex {
            dropped.push(packet);
        } else {
            queue.push_back(packet);
        }
    }
}

fn deliver_stamped_udp(packet: PacketBuf) -> Option<PacketBuf> {
    let prepared = match udp::PreparedUdpPacket::from_stamped(packet) {
        Ok(packet) => packet,
        Err(packet) => return Some(packet),
    };
    match udp::deliver_ipv4_packet(prepared) {
        (udp::InputDisposition::Accepted, _) => None,
        (udp::InputDisposition::NoSocket, returned) => {
            returned.map(udp::PreparedUdpPacket::into_packet)
        }
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;
    use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use unittest::def_test;

    use super::*;
    use crate::buf::{PacketOwner, UdpPacketMetadata};

    fn test_packet(ifindex: i32) -> PacketBuf {
        PacketBuf::from_ip_packet_vec(ifindex, vec![0; 20], PacketOwner::Loopback)
    }

    // Serial: fills and discards entries on the shared NetRx queue.
    #[def_test(serial)]
    fn overflow_returns_the_packet_and_discard_drops_it_after_unlock() {
        ensure_net_rx_queue();
        const IFINDEX: i32 = 1_000_001;
        let overflowed = loop {
            match enqueue(prepare_for_enqueue(test_packet(IFINDEX)).unwrap()) {
                Ok(()) => {}
                Err(packet) => break packet,
            }
        };
        assert_eq!(overflowed.ifindex(), IFINDEX);
        drop(overflowed);
        discard_ifindex(IFINDEX);
        assert!(!has_work_for(IFINDEX));
    }

    #[def_test(serial)]
    fn unstamped_packets_are_visible_to_the_task_poller() {
        ensure_net_rx_queue();
        const IFINDEX: i32 = 1_000_002;
        enqueue(prepare_for_enqueue(test_packet(IFINDEX)).unwrap()).unwrap();

        assert!(!process_pending(NET_RX_BUDGET));
        let packet = pop_deferred(IFINDEX).expect("non-UDP should remain queued");
        assert_eq!(packet.ifindex(), IFINDEX);
        assert!(packet.udp_metadata().is_none());
        assert!(!has_work_for(IFINDEX));
    }

    #[def_test(serial)]
    fn stamped_udp_is_not_visible_to_the_task_poller() {
        ensure_net_rx_queue();
        const IFINDEX: i32 = 1_000_003;
        let mut packet = test_packet(IFINDEX);
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12_345));
        packet.set_udp_metadata(UdpPacketMetadata::new(addr, addr, 0, 0));
        enqueue(packet).unwrap();

        assert!(pop_deferred(IFINDEX).is_none());
        discard_ifindex(IFINDEX);
        assert!(!has_work_for(IFINDEX));
    }
}
