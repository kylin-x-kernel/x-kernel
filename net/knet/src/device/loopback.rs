// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Loopback network device implementation.
use alloc::string::String;

use ::core::task::Waker;
use kerrno::{KResult, LinuxError};
use kpoll::{PollContext, PollRegisterError};
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    device::{
        IF_OPER_DOWN, IF_OPER_UNKNOWN, LINK_FLAG_LOOPBACK, LINK_FLAG_LOWER_UP, LINK_FLAG_RUNNING,
        LINK_FLAG_UP, LOOPBACK_MAX_MTU, LinkKind, LinkSendSnapshot, LinkSnapshot, NetDevice,
        RxPoll, net_rx,
    },
    ip::IpAddress,
};

/// Loopback device backed by the shared NetRx packet queue.
///
/// RX wakeups use a single stored [`Waker`], not a multi-waiter [`kpoll::PollSet`].
/// That is intentional and not a multi-task regression:
///
/// 1. [`crate::service::Service::register_rx_waker`] registers each
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
///
/// Complete IPv4 UDP datagrams are delivered from `NetRx` (Linux
/// `loopback_xmit` → `__netif_rx` → `NET_RX_SOFTIRQ`). TCP, ICMP, IPv6,
/// fragments, and unmatched UDP stay on the deferred queue for the task poller.
pub(crate) struct LoopbackDevice {
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    /// Interface index used by this device's last successful xmit or `poll_rx`.
    /// `has_rx_work` and `Drop` use it to find this device's packets in the
    /// shared `NetRx` queue without registering the device with `NetRx`.
    rx_ifindex: Option<i32>,
    /// Aggregated Service RX/timeout waker. Must not be replaced by a
    /// task-local waker from a bypass of `Service::register_rx_waker`.
    rx_waker: Mutex<Option<Waker>>,
}

impl LoopbackDevice {
    /// Create a new loopback device.
    pub(crate) fn new() -> Self {
        let _ = super::ethernet::ensure_net_rx_softirq_available();
        Self {
            name: String::from("lo"),
            flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
            mtu: LOOPBACK_MAX_MTU,
            operstate: IF_OPER_UNKNOWN,
            rx_ifindex: None,
            rx_waker: Mutex::new(None),
        }
    }
}

impl Drop for LoopbackDevice {
    fn drop(&mut self) {
        if let Some(ifindex) = self.rx_ifindex {
            net_rx::discard_ifindex(ifindex);
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

    fn poll_rx(&mut self, ifindex: i32, _timestamp: MonotonicInstant) -> KResult<RxPoll> {
        self.rx_ifindex = Some(ifindex);
        if !self.is_link_up() {
            return Ok(RxPoll::Empty);
        }
        // Task-context fallback when NetRx did not run (unit tests, or the
        // vector is owned by another action). Stamped UDP is consumed first.
        // This fallback has its own entry-time round bound and is not charged
        // to Router's device RX budget. Empty below can still follow successful
        // UDP delivery. Do not retry until empty: concurrent producers could
        // otherwise make this poll unbounded.
        net_rx::drain_pending();
        Ok(match net_rx::pop_deferred(ifindex) {
            Some(packet) => RxPoll::Consumed(Some(packet)),
            None => RxPoll::Empty,
        })
    }

    fn has_rx_work(&self) -> bool {
        self.rx_ifindex.is_some_and(net_rx::has_work_for)
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        _source_addr: IpAddress,
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
        packet.set_ifindex(ifindex);
        packet.set_owner(PacketOwner::Loopback);
        let Some(packet) = net_rx::prepare_for_enqueue(packet) else {
            // Malformed IPv4 UDP is dropped in task context. Linux
            // `loopback_xmit` still returns `NETDEV_TX_OK` after `NET_RX_DROP`.
            return true;
        };

        // Linux `netif_rx`: if the caller is not already in BH/IRQ context,
        // disable BH around enqueue + raise so `local_bh_enable` runs
        // `NET_RX_SOFTIRQ` before this function returns.
        let enqueue_result = {
            let _bh = kirq::context::local_bh_disable();
            let result = net_rx::enqueue(packet);
            if result.is_ok() {
                kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::NetRx);
            }
            result
        };
        if let Err(dropped) = enqueue_result {
            warn!(
                "Loopback device buffer is full, dropping packet to {}",
                next_hop
            );
            drop(dropped);
            return false;
        }
        self.rx_ifindex = Some(ifindex);
        // Softirq not registered (or budget exhausted): finish stamped UDP
        // in task context so sendto still completes before recvfrom waits.
        net_rx::drain_pending();

        // A PCB hit wakes its socket waiter during UDP enqueue. The device
        // source only needs a kick when work remains for the task poller.
        if net_rx::has_work_for(ifindex) {
            let waker = self.rx_waker.lock().clone();
            if let Some(waker) = waker {
                waker.wake();
            }
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

unittest::path_mod_test!(tests, "../../ut-tests/device/loopback/tests.rs");
