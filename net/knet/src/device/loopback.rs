// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Loopback network device implementation.
use alloc::{collections::VecDeque, string::String};

use ::core::task::Waker;
use kerrno::LinuxError;
use kpoll::{PollContext, PollRegisterError};
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    device::{
        IF_OPER_DOWN, IF_OPER_UNKNOWN, LINK_FLAG_LOOPBACK, LINK_FLAG_LOWER_UP, LINK_FLAG_RUNNING,
        LINK_FLAG_UP, LOOPBACK_MAX_MTU, LinkKind, LinkSendSnapshot, LinkSnapshot, NetDevice,
    },
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
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    queue: VecDeque<PacketBuf>,
    /// Aggregated Service RX/timeout waker. Must not be replaced by a
    /// task-local waker from a bypass of `Service::register_rx_waker`.
    rx_waker: Mutex<Option<Waker>>,
}
impl LoopbackDevice {
    /// Create a new loopback device.
    pub fn new() -> Self {
        Self {
            name: String::from("lo"),
            flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
            mtu: LOOPBACK_MAX_MTU,
            operstate: IF_OPER_UNKNOWN,
            queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            rx_waker: Mutex::new(None),
        }
    }
}

impl NetDevice for LoopbackDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Loopback
    }

    fn mtu(&self) -> usize {
        self.mtu
    }

    fn is_link_up(&self) -> bool {
        self.flags & LINK_FLAG_UP != 0
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            name: self.name.clone(),
            flags: self.flags,
            mtu: self.mtu,
            operstate: self.operstate,
            kind: LinkKind::Loopback,
            hardware_addr: [0; 6],
            broadcast_addr: [0; 6],
        }
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        LinkSendSnapshot {
            is_up: self.is_link_up(),
            mtu: self.mtu,
            hardware_addr: [0; 6],
        }
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> Option<PacketBuf> {
        if !self.is_link_up() {
            return None;
        }
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
        if !self.is_link_up() {
            return false;
        }
        if packet
            .network_packet()
            .is_none_or(|network_packet| network_packet.len() > self.mtu)
        {
            warn!("Dropping packet exceeding loopback MTU {}", self.mtu);
            return false;
        }
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

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        LinkKind::Loopback.validate_mtu(mtu)?;
        self.mtu = mtu;
        Ok(())
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.operstate = IF_OPER_UNKNOWN;
        } else {
            self.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.operstate = IF_OPER_DOWN;
        }
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::def_test;

    use super::*;
    use crate::device::IPV4_MIN_MTU;

    #[def_test]
    fn loopback_rechecks_current_mtu_before_enqueue() {
        let mut device = LoopbackDevice::new();
        device.set_mtu(IPV4_MIN_MTU).unwrap();
        let packet =
            PacketBuf::from_ip_packet_vec(0, vec![0; IPV4_MIN_MTU + 1], PacketOwner::Ipv4Stack);

        assert!(!device.send_ip_packet(
            1,
            IpAddress::Ipv4(crate::ip::Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        ));
        assert!(device.queue.is_empty());
    }

    #[def_test]
    fn loopback_link_state_updates_are_idempotent() {
        let mut device = LoopbackDevice::new();
        assert_eq!(device.mtu(), LOOPBACK_MAX_MTU);

        device.set_link_up(false);
        device.set_link_up(false);
        let down = device.link_snapshot(1);
        assert_eq!(down.flags & LINK_FLAG_UP, 0);
        assert_eq!(down.operstate, IF_OPER_DOWN);

        device.set_link_up(true);
        device.set_link_up(true);
        let up = device.link_snapshot(1);
        assert_ne!(up.flags & LINK_FLAG_UP, 0);
        assert_eq!(up.operstate, IF_OPER_UNKNOWN);
    }
}
