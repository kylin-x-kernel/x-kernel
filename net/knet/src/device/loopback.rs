// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Loopback network device implementation.
use alloc::collections::VecDeque;

use ::core::task::Waker;
use kpoll::{PollContext, PollRegisterError};
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    device::NetDevice,
    ip::IpAddress,
};

/// Loopback device backed by an in-memory queue.
///
/// RX wakeups use a single stored [`Waker`], not a multi-waiter [`kpoll::PollSet`].
/// That is intentional and not a multi-task regression:
///
/// 1. [`crate::stack::service::Service::register_rx_waker`] registers each
///    waiting **task** on the shared `timeout_poll` [`kpoll::PollSet`] (true
///    multi-waiter fan-out, including edge-triggered rechecks).
/// 2. It then installs one `timeout_poll`-backed **source** waker on this
///    device. Concurrent polls therefore see `will_wake == true` and do not
///    replace the slot; a packet wake kicks `timeout_poll`, which wakes every
///    registered task.
///
/// Restoring a device-level `PollSet` would be wrong for this call pattern:
/// Service re-registers the same source waker on every wait, and the new
/// `PollSet` does not dedupe, so waiters would accumulate until the next RX.
pub struct LoopbackDevice {
    queue: VecDeque<PacketBuf>,
    /// Aggregated Service RX/timeout waker. Must not be replaced by a
    /// task-local waker from a bypass of `Service::register_rx_waker`.
    rx_waker: Mutex<Option<Waker>>,
}
impl LoopbackDevice {
    /// Create a new loopback device.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            rx_waker: Mutex::new(None),
        }
    }
}

impl NetDevice for LoopbackDevice {
    fn name(&self) -> &str {
        "lo"
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> Option<PacketBuf> {
        self.queue.pop_front()
    }

    fn has_rx_work(&self) -> bool {
        !self.queue.is_empty()
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        mut packet: PacketBuf,
        _timestamp: MonotonicInstant,
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
        let waker = self.rx_waker.lock().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }

    fn register_rx_waker(
        &self,
        source_waker: &Waker,
        _context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        let mut registered = self.rx_waker.lock();
        // Polarity matters:
        // - `will_wake == true`  → same aggregated Service `timeout_poll` waker;
        //   skip the store. Task A and task B both already registered *their*
        //   task wakers on that `PollSet`; loopback only needs one upstream kick.
        // - `will_wake == false` → a different waker (Service bypass); replace.
        //   That path is unsupported and can strand the previous waiter, hence
        //   the `debug_assert` below — it is not the multi-task poll case.
        debug_assert!(
            registered
                .as_ref()
                .is_none_or(|current| current.will_wake(source_waker)),
            "loopback RX waker replaced by a non-equivalent waker; only Service's aggregated \
             timeout_poll waker is supported"
        );
        if !registered
            .as_ref()
            .is_some_and(|current| current.will_wake(source_waker))
        {
            *registered = Some(source_waker.clone());
        }
        Ok(())
    }
}
