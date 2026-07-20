// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Loopback network device implementation.
use alloc::collections::VecDeque;

use ::core::task::Waker;
use khal::time::TimeValue;
use kpoll::PollSet;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    device::NetDevice,
    ip::IpAddress,
};

/// Loopback device backed by an in-memory queue.
pub struct LoopbackDevice {
    queue: VecDeque<PacketBuf>,
    wakers: PollSet,
}
impl LoopbackDevice {
    /// Create a new loopback device.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            wakers: PollSet::new(),
        }
    }
}

impl NetDevice for LoopbackDevice {
    fn name(&self) -> &str {
        "lo"
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: TimeValue) -> Option<PacketBuf> {
        self.queue.pop_front()
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        mut packet: PacketBuf,
        _timestamp: TimeValue,
    ) -> bool {
        if self.queue.len() >= SOCKET_BUFFER_SIZE {
            warn!(
                "Loopback device buffer is full, dropping packet to {}",
                next_hop
            );
            return false;
        }

        packet.set_ifindex(ifindex);
        packet.set_owner(PacketOwner::Loopback);
        self.queue.push_back(packet);
        self.wakers.wake();
        true
    }

    fn register_rx_waker(&self, waker: &Waker) {
        self.wakers.register(waker);
    }
}
