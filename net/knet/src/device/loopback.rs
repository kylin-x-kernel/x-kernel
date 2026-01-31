// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Loopback network device implementation.
use alloc::vec;
use core::task::Waker;

use kpoll::PollSet;
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata},
    time::Instant,
    wire::IpAddress,
};

use crate::{
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::NetDevice,
};

/// Loopback device backed by an in-memory queue.
pub struct LoopbackDevice {
    queue: PacketBuffer<'static, ()>,
    wakers: PollSet,
}
impl LoopbackDevice {
    /// Create a new loopback device.
    pub fn new() -> Self {
        let queue = PacketBuffer::new(
            vec![PacketMetadata::EMPTY; SOCKET_BUFFER_SIZE],
            vec![0u8; STANDARD_MTU * SOCKET_BUFFER_SIZE],
        );
        Self {
            queue,
            wakers: PollSet::new(),
        }
    }
}

impl NetDevice for LoopbackDevice {
    fn name(&self) -> &str {
        "lo"
    }

    fn poll_rx(&mut self, buffer: &mut PacketBuffer<()>, _timestamp: Instant) -> bool {
        self.queue.dequeue().ok().is_some_and(|(_, rx_buf)| {
            buffer
                .enqueue(rx_buf.len(), ())
                .unwrap()
                .copy_from_slice(rx_buf);
            true
        })
    }

    fn send_ip_packet(
        &mut self,
        next_hop: IpAddress,
        ip_packet: &[u8],
        _timestamp: Instant,
    ) -> bool {
        match self.queue.enqueue(ip_packet.len(), ()) {
            Ok(tx_buf) => {
                tx_buf.copy_from_slice(ip_packet);
                self.wakers.wake();
                true
            }
            Err(_) => {
                warn!(
                    "Loopback device buffer is full, dropping packet to {}",
                    next_hop
                );
                false
            }
        }
    }

    fn register_rx_waker(&self, waker: &Waker) {
        self.wakers.register(waker);
    }
}
