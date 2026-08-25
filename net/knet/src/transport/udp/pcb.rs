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

#[cfg(unittest)]
mod tests {
    use alloc::vec;
    use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use unittest::def_test;

    use super::*;
    use crate::{
        buf::PacketOwner,
        ip::Ipv4Address,
        ipv4,
        udp::{UDP_HEADER_LEN, UdpSocketState, output::write_udp_header},
    };

    fn queued_packet(remote_addr: SocketAddrV4, payload: u8) -> PreparedUdpPacket {
        let source_addr = Ipv4Address::from_octets(remote_addr.ip().octets());
        let destination_addr = Ipv4Address::new(192, 0, 2, 100);
        let mut udp_packet = vec![0; UDP_HEADER_LEN + 1];
        udp_packet[UDP_HEADER_LEN] = payload;
        write_udp_header(
            &mut udp_packet,
            source_addr,
            destination_addr,
            remote_addr.port(),
            3000,
        )
        .unwrap();
        let packet = ipv4::build_ipv4_packet(
            source_addr,
            destination_addr,
            ipv4::PROTOCOL_UDP,
            64,
            &udp_packet,
        )
        .unwrap();
        let packet = PacketBuf::from_ip_packet_vec(1, packet, PacketOwner::DeviceRx);
        let header = ipv4::Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
        crate::udp::prepare_ipv4_packet(header, packet).unwrap()
    }

    #[def_test]
    fn receive_preserves_fifo_order() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let old_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        let connected_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 2000);
        assert!(pcb.enqueue(queued_packet(old_remote, 1)));
        assert!(pcb.enqueue(queued_packet(connected_remote, 2)));

        let first = pcb.recv_datagram(RecvMode::Consume).unwrap();
        assert_eq!(first.remote_addr, SocketAddr::V4(old_remote));
        assert_eq!(first.payload.as_slice(), &[1]);
        let second = pcb.recv_datagram(RecvMode::Consume).unwrap();
        assert_eq!(second.remote_addr, SocketAddr::V4(connected_remote));
        assert_eq!(second.payload.as_slice(), &[2]);
    }

    #[def_test]
    fn peek_returns_a_copy_without_consuming() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let first_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        let second_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 2000);
        assert!(pcb.enqueue(queued_packet(first_remote, 1)));
        assert!(pcb.enqueue(queued_packet(second_remote, 2)));

        let peeked = pcb.recv_datagram(RecvMode::Peek).unwrap();
        assert_eq!(peeked.remote_addr, SocketAddr::V4(first_remote));
        assert_eq!(peeked.payload.as_slice(), &[1]);

        let first = pcb.recv_datagram(RecvMode::Consume).unwrap();
        assert_eq!(first.remote_addr, SocketAddr::V4(first_remote));
        assert_eq!(first.payload.as_slice(), &[1]);
        let second = pcb.recv_datagram(RecvMode::Consume).unwrap();
        assert_eq!(second.remote_addr, SocketAddr::V4(second_remote));
        assert_eq!(second.payload.as_slice(), &[2]);
    }

    #[def_test]
    fn enqueue_and_peek_reuse_the_prepared_packet_allocation() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        let packet = queued_packet(remote, 1);
        let packet_observer = packet.clone();

        assert!(pcb.enqueue(packet));
        let peeked = pcb.recv_datagram(RecvMode::Peek).unwrap();

        assert!(
            peeked
                .payload
                .packet
                .packet()
                .shares_storage_with(packet_observer.packet())
        );
        assert_eq!(pcb.rx_queue.lock().len(), 1);
    }

    #[def_test]
    fn prepared_packet_returns_the_same_shared_packet_handle() {
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        let prepared = queued_packet(remote, 1);
        let observer = prepared.clone();

        let packet = prepared.into_packet();

        assert!(packet.shares_storage_with(observer.packet()));
    }

    #[def_test]
    fn receive_queue_is_reserved_at_pcb_creation() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let capacity_at_create = pcb.rx_queue.lock().capacity();
        assert_eq!(
            core::mem::size_of::<PreparedUdpPacket>(),
            core::mem::size_of::<usize>()
        );
        assert!(capacity_at_create >= UDP_RX_QUEUE_CAPACITY);

        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        assert!(pcb.enqueue(queued_packet(remote, 1)));
        assert_eq!(pcb.rx_queue.lock().capacity(), capacity_at_create);
    }

    #[def_test]
    fn full_receive_queue_drops_without_growing() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        let capacity_at_create = pcb.rx_queue.lock().capacity();

        for payload in 0..UDP_RX_QUEUE_CAPACITY {
            assert!(pcb.enqueue(queued_packet(remote, payload as u8)));
        }
        assert!(!pcb.enqueue(queued_packet(remote, 0xff)));
        assert_eq!(pcb.rx_queue.lock().len(), UDP_RX_QUEUE_CAPACITY);
        assert_eq!(pcb.rx_queue.lock().capacity(), capacity_at_create);
    }
}
