// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, sync::Arc};

use ::core::net::SocketAddr;
use kspin::SpinNoIrq;
use ksync::RwLock;

use super::{IPV4_DEFAULT_TTL, UDP_RX_QUEUE_CAPACITY, state::UdpSocketState};
use crate::buf::{PacketBuf, UdpPacketMetadata};

#[derive(Clone)]
pub(super) struct UdpPayload {
    packet: PreparedUdpPacket,
}

impl UdpPayload {
    pub(super) fn as_slice(&self) -> &[u8] {
        self.packet
            .packet()
            .network_packet()
            .and_then(|data| data.get(self.packet.metadata().payload_range()))
            .unwrap_or(&[])
    }
}

#[derive(Clone)]
pub(super) struct UdpDatagram {
    pub(super) payload: UdpPayload,
    pub(super) remote_addr: SocketAddr,
}

/// A validated IPv4 UDP view over a reference-counted [`PacketBuf`].
#[derive(Clone)]
pub(crate) struct PreparedUdpPacket {
    packet: PacketBuf,
}

impl PreparedUdpPacket {
    pub(super) fn new(
        mut packet: PacketBuf,
        payload_offset: usize,
        payload_len: usize,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> Result<Self, PacketBuf> {
        let Some(payload_end) = payload_offset.checked_add(payload_len) else {
            return Err(packet);
        };
        if packet
            .network_packet()
            .and_then(|data| data.get(payload_offset..payload_end))
            .is_none()
        {
            return Err(packet);
        }

        packet.set_udp_metadata(UdpPacketMetadata::new(
            local_addr,
            remote_addr,
            payload_offset,
            payload_end,
        ));
        Ok(Self { packet })
    }

    pub(crate) fn packet(&self) -> &PacketBuf {
        &self.packet
    }

    pub(super) fn local_addr(&self) -> SocketAddr {
        self.metadata().local_addr()
    }

    pub(super) fn remote_addr(&self) -> SocketAddr {
        self.metadata().remote_addr()
    }

    pub(crate) fn into_packet(self) -> PacketBuf {
        self.packet
    }

    /// Rebuilds a validated view from a packet stamped by [`super::input::prepare_ipv4_packet`].
    pub(crate) fn from_stamped(packet: PacketBuf) -> Result<Self, PacketBuf> {
        if packet.udp_metadata().is_none() {
            return Err(packet);
        }
        Ok(Self { packet })
    }

    fn metadata(&self) -> UdpPacketMetadata {
        self.packet
            .udp_metadata()
            .expect("PreparedUdpPacket always carries validated UDP metadata")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecvMode {
    Peek,
    Consume,
}

pub(super) struct UdpPcb {
    pub(super) state: Arc<UdpSocketState>,
    /// Accessed from task recv and from `NetRx` loopback delivery.
    /// Capacity is reserved at PCB creation so `enqueue` does not grow
    /// the `VecDeque` under this lock.
    rx_queue: SpinNoIrq<VecDeque<PreparedUdpPacket>>,
    pub(super) ttl: RwLock<u8>,
    pub(super) mtu_discovery: RwLock<u8>,
}

impl UdpPcb {
    pub(super) fn new(state: Arc<UdpSocketState>) -> Arc<Self> {
        // Reserve the occupancy bound in task context. `enqueue` runs from
        // `NetRx`, and Linux `__udp_enqueue_schedule_skb` only
        // `__skb_queue_tail`s an existing `skb` under `sk_receive_queue.lock`.
        Arc::new(Self {
            state,
            rx_queue: SpinNoIrq::new(VecDeque::with_capacity(UDP_RX_QUEUE_CAPACITY)),
            ttl: RwLock::new(IPV4_DEFAULT_TTL),
            mtu_discovery: RwLock::new(1),
        })
    }

    /// Enqueues a validated packet. Safe from `NetRx` as well as task context.
    ///
    /// The socket queue is reserved to its occupancy bound when the PCB is
    /// created, and `PacketBuf` already owns the shared packet allocation, so
    /// this path does not allocate or grow the `VecDeque`. A full queue drops
    /// the packet and returns `false`.
    pub(super) fn enqueue(&self, packet: PreparedUdpPacket) -> bool {
        let mut queue = self.rx_queue.lock();
        if queue.len() >= UDP_RX_QUEUE_CAPACITY {
            // Linux `__udp_queue_rcv_skb` also releases the rejected `skb` in
            // receive context. Drop the queue guard first so packet reclamation
            // never runs while `rx_queue` has local IRQs masked.
            drop(queue);
            drop(packet);
            return false;
        }
        debug_assert!(queue.capacity() >= UDP_RX_QUEUE_CAPACITY);
        queue.push_back(packet);
        drop(queue);
        self.state.wake_read();
        true
    }

    pub(super) fn has_recv_data(&self) -> bool {
        !self.rx_queue.lock().is_empty()
    }

    pub(super) fn recv_datagram(&self, mode: RecvMode) -> Option<UdpDatagram> {
        // Take only a packet handle (or pop) under `SpinNoIrq`. Payload copy
        // runs after the guard drops.
        let queued = {
            let mut queue = self.rx_queue.lock();
            match mode {
                RecvMode::Peek => queue.front().cloned(),
                RecvMode::Consume => queue.pop_front(),
            }
        };

        queued.map(|packet| {
            let remote_addr = packet.remote_addr();
            UdpDatagram {
                payload: UdpPayload { packet },
                remote_addr,
            }
        })
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/transport/udp/pcb/tests.rs");
