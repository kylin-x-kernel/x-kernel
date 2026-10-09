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

#[unittest::mod_test]
mod tests {
    use alloc::{vec, vec::Vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::buf::PacketOwner;

    #[def_test]
    fn data_tx_queue_preserves_rejected_packets_and_enforces_capacity() {
        let packet_fn =
            |marker| PacketBuf::from_ip_packet_vec(1, vec![marker], PacketOwner::Ipv4Stack);
        let mut queue = DataTxQueue::with_capacity(2);
        assert!(queue.try_push(packet_fn(0)).is_ok());
        assert!(!queue.can_push(usize::MAX));

        let first = packet_fn(1);
        let second = packet_fn(2);
        let rejected = queue
            .try_extend(vec![first.clone(), second.clone()].into_iter())
            .err()
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(queue.available(), 1);
        assert_eq!(rejected.len(), 2);
        assert!(rejected[0].shares_storage_with(&first));
        assert!(rejected[1].shares_storage_with(&second));

        let queued = queue.pop_front().unwrap();
        assert_eq!(queued.network_packet(), Some(&[0][..]));
        assert!(queue.try_extend(rejected.into_iter()).is_ok());
        assert!(queue.is_full());
        let extra = packet_fn(3);
        let rejected = queue.try_push(extra.clone()).err().unwrap();
        assert!(rejected.shares_storage_with(&extra));
        assert_eq!(queue.available(), 0);
        assert!(queue.try_extend(core::iter::empty()).is_ok());
        assert!(queue.pop_front().unwrap().shares_storage_with(&first));
        assert!(queue.pop_front().unwrap().shares_storage_with(&second));
        assert!(queue.is_empty());
    }
}
