// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded data transmission and its capacity wait source.

use alloc::collections::VecDeque;

use kpoll::{PollContext, PollRegisterError, PollSet};

use crate::buf::PacketBuf;

/// Owns queued packets and capacity registrations under the enclosing I/O lock.
pub(crate) struct DataTxQueue {
    packets: VecDeque<PacketBuf>,
    capacity: usize,
    capacity_poll: PollSet,
}

impl DataTxQueue {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            packets: VecDeque::with_capacity(capacity),
            capacity,
            capacity_poll: PollSet::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.packets.len() >= self.capacity
    }

    pub(crate) fn can_push(&self, packet_count: usize) -> bool {
        packet_count <= self.available()
    }

    pub(crate) fn available(&self) -> usize {
        self.capacity.saturating_sub(self.packets.len())
    }

    pub(crate) fn try_push(&mut self, packet: PacketBuf) -> Result<(), PacketBuf> {
        if self.is_full() {
            return Err(packet);
        }

        self.packets.push_back(packet);
        Ok(())
    }

    /// Returns the unadvanced iterator when the complete batch does not fit.
    pub(crate) fn try_extend<I>(&mut self, packets: I) -> Result<(), I>
    where
        I: ExactSizeIterator<Item = PacketBuf>,
    {
        if !self.can_push(packets.len()) {
            return Err(packets);
        }

        self.packets.extend(packets);
        Ok(())
    }

    /// Removes a packet; the caller must notify capacity waiters after unlocking.
    pub(crate) fn pop_front(&mut self) -> Option<PacketBuf> {
        self.packets.pop_front()
    }

    pub(crate) fn register_capacity_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.capacity_poll)
    }

    /// Captures the queue's wait source for waking after the enclosing lock is released.
    ///
    /// Call only after removing data packets, including packets retained in a
    /// separate dispatch batch. Device transmission completion is not required.
    pub(crate) fn capacity_poll(&self) -> PollSet {
        self.capacity_poll.clone()
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/runtime/tx_queue/tests.rs");
