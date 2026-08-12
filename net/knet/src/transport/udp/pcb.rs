// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, sync::Arc};

use ::core::{net::SocketAddr, ops::Range};
use ksync::{Mutex, RwLock};

use super::{
    IPV4_DEFAULT_TTL, UDP_RX_QUEUE_CAPACITY, UDP_RX_QUEUE_RETAINED_CAPACITY, state::UdpSocketState,
};
use crate::buf::PacketBuf;

#[derive(Clone)]
pub(super) struct UdpPayload {
    packet: PacketBuf,
    range: Range<usize>,
}

impl UdpPayload {
    pub(super) fn new(packet: PacketBuf, offset: usize, len: usize) -> Result<Self, PacketBuf> {
        let Some(end) = offset.checked_add(len) else {
            return Err(packet);
        };

        if packet
            .network_packet()
            .and_then(|data| data.get(offset..end))
            .is_none()
        {
            return Err(packet);
        }

        Ok(Self {
            packet,
            range: offset..end,
        })
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        self.packet
            .network_packet()
            .and_then(|data| data.get(self.range.clone()))
            .unwrap_or(&[])
    }
}

#[derive(Clone)]
pub(super) struct UdpDatagram {
    pub(super) payload: UdpPayload,
    pub(super) remote_addr: SocketAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecvMode {
    Peek,
    Consume,
}

pub(super) struct UdpPcb {
    pub(super) state: Arc<UdpSocketState>,
    rx_queue: Mutex<VecDeque<UdpDatagram>>,
    pub(super) ttl: RwLock<u8>,
    pub(super) mtu_discovery: RwLock<u8>,
}

impl UdpPcb {
    pub(super) fn new(state: Arc<UdpSocketState>) -> Arc<Self> {
        Arc::new(Self {
            state,
            rx_queue: Mutex::new(VecDeque::new()),
            ttl: RwLock::new(IPV4_DEFAULT_TTL),
            mtu_discovery: RwLock::new(1),
        })
    }

    pub(super) fn enqueue(&self, datagram: UdpDatagram) -> bool {
        let mut queue = self.rx_queue.lock();
        if queue.len() >= UDP_RX_QUEUE_CAPACITY {
            return false;
        }
        if queue.len() == queue.capacity() && queue.try_reserve(1).is_err() {
            return false;
        }
        queue.push_back(datagram);
        drop(queue);
        self.state.wake_read();
        true
    }

    pub(super) fn has_recv_data(&self) -> bool {
        !self.rx_queue.lock().is_empty()
    }

    pub(super) fn recv_datagram(&self, mode: RecvMode) -> Option<UdpDatagram> {
        let mut queue = self.rx_queue.lock();
        let datagram = match mode {
            RecvMode::Peek => queue.front().cloned(),
            RecvMode::Consume => queue.pop_front(),
        };
        if mode == RecvMode::Consume
            && queue.is_empty()
            && queue.capacity() > UDP_RX_QUEUE_RETAINED_CAPACITY
        {
            queue.shrink_to(UDP_RX_QUEUE_RETAINED_CAPACITY);
        }
        datagram
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;
    use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use unittest::def_test;

    use super::*;
    use crate::{buf::PacketOwner, udp::UdpSocketState};

    fn datagram(remote_addr: SocketAddrV4, payload: u8) -> UdpDatagram {
        let packet = PacketBuf::from_ip_packet_vec(1, vec![payload], PacketOwner::DeviceRx);
        UdpDatagram {
            payload: UdpPayload::new(packet, 0, 1).unwrap(),
            remote_addr: SocketAddr::V4(remote_addr),
        }
    }

    #[def_test]
    fn receive_preserves_fifo_order() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let old_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        let connected_remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 2000);
        assert!(pcb.enqueue(datagram(old_remote, 1)));
        assert!(pcb.enqueue(datagram(connected_remote, 2)));

        let first = pcb.recv_datagram(RecvMode::Consume).unwrap();
        assert_eq!(first.remote_addr, SocketAddr::V4(old_remote));
        assert_eq!(first.payload.as_slice(), &[1]);
        let second = pcb.recv_datagram(RecvMode::Consume).unwrap();
        assert_eq!(second.remote_addr, SocketAddr::V4(connected_remote));
        assert_eq!(second.payload.as_slice(), &[2]);
    }

    #[def_test]
    fn receive_queue_allocates_capacity_on_demand() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        assert_eq!(pcb.rx_queue.lock().capacity(), 0);

        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        assert!(pcb.enqueue(datagram(remote, 1)));

        let capacity = pcb.rx_queue.lock().capacity();
        assert!(capacity > 0);
        assert!(capacity < UDP_RX_QUEUE_CAPACITY);
    }

    #[def_test]
    fn drained_receive_queue_releases_burst_capacity() {
        let pcb = UdpPcb::new(UdpSocketState::new());
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1000);
        for payload in 0..128 {
            assert!(pcb.enqueue(datagram(remote, payload)));
        }
        let burst_capacity = pcb.rx_queue.lock().capacity();

        for _ in 0..128 {
            assert!(pcb.recv_datagram(RecvMode::Consume).is_some());
        }

        let retained_capacity = pcb.rx_queue.lock().capacity();
        assert!(retained_capacity <= UDP_RX_QUEUE_RETAINED_CAPACITY);
        assert!(retained_capacity < burst_capacity);
    }
}
