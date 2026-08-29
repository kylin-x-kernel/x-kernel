// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service with budgeted data-plane progression.

use alloc::{sync::Arc, vec::Vec};
#[cfg(unittest)]
use core::sync::atomic::AtomicUsize;
use core::{
    sync::atomic::{AtomicU64, Ordering},
    task::Waker,
};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use ktime_types::{MonotonicInstant, TimeSpan};
use smoltcp::{
    iface::{Interface, PollIngressSingleResult, PollResult, SocketSet},
    time::Instant as SmoltcpInstant,
    wire::{
        HardwareAddress, IpAddress as SmoltcpIpAddress, IpCidr as SmoltcpIpCidr,
        IpListenEndpoint as SmoltcpIpListenEndpoint,
    },
};

use crate::{
    LISTEN_TABLE, SOCKET_SET,
    buf::PacketBuf,
    device::{LinkConfigUpdate, LinkSendSnapshot, LinkSnapshot, NeighborUpdate},
    ip::{IpAddress, IpListenEndpoint},
    poller::{PollBudget, PollProgress},
    router::{NeighborUpdatePolicy, Router, Rule},
    stack::ingress::{IngressProcessor, prepare_smoltcp_ingress},
};

const IPV4_HEADER_LEN: usize = 20;
const RX_INGRESS_BATCH_PACKETS: usize = 64;
const POLL_ROUND_TIME_LIMIT: TimeSpan = TimeSpan::from_millis(1);
const POLL_TIME_CHECK_INTERVAL_WORK_ITEMS: usize = 32;

#[derive(Default)]
struct SmoltcpIngressProgress {
    packets: usize,
    has_socket_state_change: bool,
    has_reached_time_limit: bool,
}

#[derive(Default)]
struct DeviceRxProgress {
    packets: usize,
    has_reached_time_limit: bool,
    has_lock_contention: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExistingIpv4AddrAction {
    Keep,
    Reject,
}

fn to_smoltcp_instant(instant: MonotonicInstant) -> SmoltcpInstant {
    let micros = instant.span_since_origin().as_micros();
    let micros = i64::try_from(micros).unwrap_or(i64::MAX);
    SmoltcpInstant::from_micros_const(micros)
}

#[cfg(unittest)]
fn from_smoltcp_instant(instant: SmoltcpInstant) -> MonotonicInstant {
    let micros = u64::try_from(instant.total_micros()).unwrap_or(0);
    MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(micros))
}

pub(crate) fn now() -> SmoltcpInstant {
    to_smoltcp_instant(monotonic_time())
}

pub struct Service {
    pub(crate) iface: Mutex<Interface>,
    router: Mutex<Router>,
    ingress: Mutex<IngressProcessor>,
    rx_batch: Mutex<Vec<PacketBuf>>,
    accepted_batch: Mutex<Vec<PacketBuf>>,
    control_batch: Mutex<Vec<Vec<u8>>>,
    timeout_deadline_micros: AtomicU64,
    timeout_poll: Arc<PollSet>,
    #[cfg(unittest)]
    rebuild_count: AtomicUsize,
}

impl Service {
    pub fn new(mut router: Router) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, to_smoltcp_instant(monotonic_time()));

        let service = Self {
            iface: Mutex::new(iface),
            router: Mutex::new(router),
            ingress: Mutex::new(IngressProcessor::new()),
            rx_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            accepted_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            control_batch: Mutex::new(Vec::new()),
            timeout_deadline_micros: AtomicU64::new(0),
            timeout_poll: Arc::new(PollSet::new()),
            #[cfg(unittest)]
            rebuild_count: AtomicUsize::new(0),
        };
        service.sync_local_ipv4_views();
        service
    }

    /// Advances the network data plane within one bounded poll round.
    ///
    /// The caller must run in task context without holding spinlocks because
    /// transport and device handlers may acquire sleepable mutexes. Shared
    /// progression locks are acquired with `try_lock`; contention leaves the
    /// pending batches intact and reports immediate work for a later round.
    pub fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let round_started_at = monotonic_time();
        let device_timestamp = round_started_at;
        let mut control_packets = self.control_batch.lock();
        let mut rx_batch = self.rx_batch.lock();
        let mut accepted_packets = self.accepted_batch.lock();

        if !self.try_flush_ingress_batches(&mut accepted_packets, &mut control_packets) {
            return deferred_poll_progress(0, 0, 0, false);
        }

        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(0, 0, 0, false);
        };
        let tx_capacity_before = router.available_tx_packet_slots();
        let (mut tx_packets, mut has_reached_time_limit) = dispatch_tx_budgeted(
            &mut router,
            device_timestamp,
            budget.tx_packets,
            round_started_at,
        );
        let mut tx_capacity_changed = router.available_tx_packet_slots() > tx_capacity_before;
        drop(router);
        let mut tx_remaining = budget.tx_packets.saturating_sub(tx_packets);

        let rx_progress = self.poll_device_rx_budgeted(
            device_timestamp,
            round_started_at,
            if has_reached_time_limit {
                0
            } else {
                budget.rx_packets
            },
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        );
        let mut rx_packets = rx_progress.packets;
        has_reached_time_limit |= rx_progress.has_reached_time_limit;
        if rx_progress.has_lock_contention {
            return deferred_poll_progress(rx_packets, tx_packets, 0, tx_capacity_changed);
        }

        let Some(mut sockets) = SOCKET_SET.inner.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, 0, tx_capacity_changed);
        };
        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, 0, tx_capacity_changed);
        };
        let current = now();
        let Some(mut iface) = self.iface.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, 0, tx_capacity_changed);
        };
        // Maintenance and one egress pass remain bounded and preserve protocol
        // progress after bulk RX or TX reaches the round time limit.
        iface.poll_maintenance(current);

        let ingress_progress = if has_reached_time_limit {
            SmoltcpIngressProgress::default()
        } else {
            poll_smoltcp_ingress_budgeted(
                &mut iface,
                current,
                &mut router,
                &mut sockets,
                budget.rx_packets,
                round_started_at,
            )
        };
        has_reached_time_limit |= ingress_progress.has_reached_time_limit;
        let mut has_socket_state_change = ingress_progress.has_socket_state_change;

        let mut stack_tx_passes = 0;
        let stack_tx_pass_limit = budget.tx_packets.max(1);
        loop {
            let result = iface.poll_egress(current, &mut *router, &mut sockets);
            if result == PollResult::None {
                break;
            }
            has_socket_state_change = true;
            stack_tx_passes += 1;
            if stack_tx_passes >= stack_tx_pass_limit || has_reached_time_limit {
                break;
            }
            if stack_tx_passes.is_multiple_of(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
                && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
            {
                has_reached_time_limit = true;
                break;
            }
        }
        if !has_reached_time_limit
            && stack_tx_passes > 0
            && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
        {
            has_reached_time_limit = true;
        }

        let deferred_close_deadline = sockets.reap_deferred_tcp_closes(current);
        let next_poll =
            earliest_poll_deadline(iface.poll_at(current, &sockets), deferred_close_deadline);
        drop(iface);
        let timer_has_more = has_immediate_timer_work(current, next_poll);
        self.update_poll_timeout(current, next_poll);
        LISTEN_TABLE.refresh_acceptors(&mut sockets);
        drop(sockets);

        if has_reached_time_limit {
            tx_remaining = 0;
        }
        let tx_capacity_before = router.available_tx_packet_slots();
        let (dispatched, tx_time_limit) = dispatch_tx_budgeted(
            &mut router,
            device_timestamp,
            tx_remaining,
            round_started_at,
        );
        tx_packets += dispatched;
        tx_capacity_changed |= router.available_tx_packet_slots() > tx_capacity_before;
        has_reached_time_limit |= tx_time_limit;
        drop(router);

        let rx_remaining = budget.rx_packets.saturating_sub(rx_packets);
        if !has_reached_time_limit && rx_remaining > 0 {
            let tail_progress = self.poll_device_rx_budgeted(
                device_timestamp,
                round_started_at,
                rx_remaining,
                &mut rx_batch,
                &mut accepted_packets,
                &mut control_packets,
            );
            rx_packets += tail_progress.packets;
            if tail_progress.has_lock_contention {
                return deferred_poll_progress(
                    rx_packets,
                    tx_packets,
                    usize::from(budget.timer_events > 0 && has_socket_state_change),
                    tx_capacity_changed,
                );
            }
        }

        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                usize::from(budget.timer_events > 0 && has_socket_state_change),
                tx_capacity_changed,
            );
        };
        let ingress_has_more = router.has_pending_ingress();
        let rx_has_more = router.has_immediate_rx_work();
        let (_, tx_has_more) = router.dispatch_budgeted(device_timestamp, 0);
        PollProgress {
            rx_packets,
            tx_packets,
            timer_events: usize::from(budget.timer_events > 0 && has_socket_state_change),
            tx_capacity_changed,
            has_more: rx_has_more || ingress_has_more || tx_has_more || timer_has_more,
        }
    }

    fn poll_device_rx_budgeted(
        &self,
        device_timestamp: MonotonicInstant,
        round_started_at: MonotonicInstant,
        budget: usize,
        rx_batch: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> DeviceRxProgress {
        let mut progress = DeviceRxProgress::default();
        let mut packets_since_time_check = 0;

        if !self.try_process_rx_batch(
            device_timestamp,
            rx_batch,
            accepted_packets,
            control_packets,
        ) || !self.try_flush_ingress_batches(accepted_packets, control_packets)
        {
            progress.has_lock_contention = true;
            return progress;
        }

        while progress.packets < budget {
            if packets_since_time_check >= POLL_TIME_CHECK_INTERVAL_WORK_ITEMS {
                packets_since_time_check = 0;
                if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
                    progress.has_reached_time_limit = true;
                    break;
                }
            }

            let Some(mut router) = self.router.try_lock() else {
                progress.has_lock_contention = true;
                break;
            };
            let batch_budget = budget
                .saturating_sub(progress.packets)
                .min(RX_INGRESS_BATCH_PACKETS)
                .min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
                .min(router.ingress_capacity());
            if batch_budget == 0 {
                break;
            }
            let drain = router.drain_rx_budgeted_into(device_timestamp, batch_budget, rx_batch);
            drop(router);
            progress.packets += drain.work_done;
            packets_since_time_check = packets_since_time_check.saturating_add(drain.work_done);

            if !self.try_process_rx_batch(
                device_timestamp,
                rx_batch,
                accepted_packets,
                control_packets,
            ) || !self.try_flush_ingress_batches(accepted_packets, control_packets)
            {
                progress.has_lock_contention = true;
                break;
            }
            if !drain.has_more || drain.work_done == 0 {
                break;
            }
        }

        if progress.packets > 0
            && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
        {
            progress.has_reached_time_limit = true;
        }
        progress
    }

    fn try_process_rx_batch(
        &self,
        device_timestamp: MonotonicInstant,
        rx_batch: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> bool {
        if rx_batch.is_empty() {
            return true;
        }

        let Some(mut ingress) = self.ingress.try_lock() else {
            return false;
        };
        ingress.handle_rx_packets(
            device_timestamp,
            rx_batch,
            accepted_packets,
            control_packets,
        );
        true
    }

    fn try_flush_ingress_batches(
        &self,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> bool {
        if accepted_packets.is_empty() && control_packets.is_empty() {
            return true;
        }

        let mut sockets = if accepted_packets.is_empty() {
            None
        } else {
            let Some(sockets) = SOCKET_SET.inner.try_lock() else {
                return false;
            };
            Some(sockets)
        };
        let Some(mut router) = self.router.try_lock() else {
            return false;
        };
        if let Some(sockets) = &mut sockets {
            // Acquire both destinations before listener preparation mutates the
            // socket set. A failed nonblocking acquisition leaves the complete
            // batch available for an identical retry in a later poll round.
            prepare_smoltcp_ingress(accepted_packets, sockets);
            router.enqueue_ingress_packets(accepted_packets);
        }
        for packet in control_packets.drain(..) {
            if let Err(err) = router.queue_control_ipv4_packet(packet) {
                warn!("Dropping network control packet: {}", err);
            }
        }
        true
    }

    pub fn get_smoltcp_source_address(
        &self,
        dst_addr: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        let dst_addr = super::from_smoltcp_ip_address(*dst_addr);
        self.router
            .lock()
            .output_route_source(&dst_addr, 0)
            .map(super::to_smoltcp_ip_address)
    }

    pub fn smoltcp_device_mask_for(&self, endpoint: &SmoltcpIpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.smoltcp_device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn smoltcp_device_mask_for_addr(&self, addr: &SmoltcpIpAddress) -> u32 {
        let addr = super::from_smoltcp_ip_address(*addr);
        self.router
            .lock()
            .table
            .lookup(&addr)
            .map_or(u32::MAX, |rule| 1u32 << rule.dev)
    }

    pub fn get_source_address(&self, dst_addr: &IpAddress) -> KResult<IpAddress> {
        self.router.lock().output_route_source(dst_addr, 0)
    }

    #[cfg(unittest)]
    pub fn ipv4_route_mtu(&self, dst_addr: &IpAddress) -> Option<usize> {
        self.router.lock().route_mtu(dst_addr)
    }

    pub fn can_send_ip_packet(&self) -> bool {
        self.router.lock().can_enqueue_tx_packet()
    }

    pub fn prepare_and_send_ipv4_packet(
        &self,
        bound_src: Option<IpAddress>,
        dst_addr: &IpAddress,
        oif: i32,
        allow_broadcast: bool,
        packet: &mut Option<Vec<u8>>,
        prepare: impl FnOnce(&mut [u8], IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        let mut router = self.router.lock();
        if router.ipv4_dest_requires_broadcast(dst_addr) && !allow_broadcast {
            return Err(KError::from(LinuxError::EACCES));
        }
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        let route_source = router.output_route_source(dst_addr, oif)?;
        let source_addr = bound_src.unwrap_or(route_source);
        let is_loopback = router.is_loopback_destination(dst_addr);

        if !is_loopback {
            if !router.can_enqueue_tx_packet() {
                return Err(KError::WouldBlock);
            }

            let packet_count = output_ipv4_packet_count(&router, dst_addr, packet_len)?;
            if !router.can_enqueue_tx_packets(packet_count) {
                return Err(KError::WouldBlock);
            }
        }

        let route_mtu = router.route_mtu(dst_addr);
        prepare(
            packet.as_deref_mut().ok_or(KError::InvalidInput)?,
            source_addr,
            route_mtu,
        )?;
        let packet = packet.take().ok_or(KError::InvalidInput)?;
        if is_loopback {
            router.transmit_ipv4_now(packet, monotonic_time())
        } else {
            router.queue_ipv4_packet(packet, oif)
        }
    }

    pub fn device_mask_for(&self, endpoint: &IpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn device_mask_for_addr(&self, addr: &IpAddress) -> u32 {
        self.router
            .lock()
            .table
            .lookup(addr)
            .map_or(u32::MAX, |rule| 1u32 << rule.dev)
    }

    pub fn register_rx_waker(
        &self,
        mask: u32,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        context.register(self.timeout_poll.as_ref())?;
        let source_waker = Waker::from(self.timeout_poll.clone());

        let current = now();
        let sockets = SOCKET_SET.inner.lock();
        let next = earliest_poll_deadline(
            self.iface.lock().poll_at(current, &sockets),
            sockets.deferred_tcp_close_deadline(),
        );
        drop(sockets);
        self.update_poll_timeout(current, next);

        for (index, device) in self.router.lock().devices.iter().enumerate() {
            if mask & (1 << index) != 0 {
                // Ethernet devices that support interrupt-driven RX register
                // on their NetRx softirq-backed poll source. Loopback and
                // poll-only devices keep the timeout_poll-backed source waker.
                device.register_rx_waker(&source_waker, context)?;
            }
        }
        Ok(source_waker)
    }

    fn update_poll_timeout(&self, current: SmoltcpInstant, next: Option<SmoltcpInstant>) {
        let Some(next) = next else {
            self.timeout_deadline_micros.store(0, Ordering::Release);
            return;
        };

        if next <= current {
            self.timeout_deadline_micros.store(0, Ordering::Release);
            self.publish_timer_expiration();
            return;
        }

        let deadline = u64::try_from(next.total_micros())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.timeout_deadline_micros
            .store(deadline, Ordering::Release);
    }

    pub(crate) fn handle_timer_tick(&self) {
        let deadline = self.timeout_deadline_micros.load(Ordering::Acquire);
        if deadline == 0
            || u64::try_from(now().total_micros())
                .unwrap_or(0)
                .saturating_add(1)
                < deadline
        {
            return;
        }
        if self
            .timeout_deadline_micros
            .compare_exchange(deadline, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.publish_timer_expiration();
        }
    }

    fn publish_timer_expiration(&self) {
        self.timeout_poll.as_ref().wake();
        crate::poller::network_poller().notify(crate::poller::PollReason::Timer);
    }

    fn sync_local_ipv4_views(&self) {
        let local_ipv4_addrs = self.router.lock().local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
    }

    fn replace_local_ipv4_views(&self, addrs: &[crate::ip::Ipv4Cidr]) {
        self.ingress.lock().update_local_ipv4_addrs(addrs);
        self.iface.lock().update_ip_addrs(|ip_addrs| {
            ip_addrs.clear();
            for addr in addrs {
                ip_addrs
                    .push(SmoltcpIpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                        addr.address().into(),
                        addr.prefix_len(),
                    )))
                    .expect("Router validates the smoltcp address capacity");
            }
        });
    }

    fn rebuild_interface(&self, router: &mut Router, iface: &mut Interface) {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let mut new_iface = Interface::new(config, router, to_smoltcp_instant(monotonic_time()));
        let current_addrs = iface.ip_addrs().to_vec();
        new_iface.update_ip_addrs(|ip_addrs| {
            for addr in current_addrs {
                ip_addrs
                    .push(addr)
                    .expect("rebuilt interface must preserve the original address capacity");
            }
        });
        *iface = new_iface;
        self.timeout_deadline_micros.store(0, Ordering::Release);
        self.timeout_poll.wake();
        #[cfg(unittest)]
        self.rebuild_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.router.lock().link_snapshots()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.router.lock().link_snapshot_for_ifindex(ifindex)
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        self.router.lock().has_device(ifindex)
    }

    pub fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.router.lock().device_index_by_name(name)
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        self.router.lock().link_send_snapshot_for_ifindex(ifindex)
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<crate::router::Ipv4AddrSnapshot> {
        self.router.lock().ipv4_addr_snapshots()
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addr_entries(&self) -> Vec<crate::router::Ipv4AddrEntry> {
        self.router.lock().ipv4_addr_entries().to_vec()
    }

    pub(crate) fn add_ipv4_addr(
        &self,
        entry: crate::router::Ipv4AddrEntry,
        existing_action: ExistingIpv4AddrAction,
    ) -> Result<(), LinuxError> {
        let mut router = self.router.lock();
        let is_existing = router
            .ipv4_addr_entries()
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr);
        if is_existing {
            return match existing_action {
                ExistingIpv4AddrAction::Keep => Ok(()),
                ExistingIpv4AddrAction::Reject => Err(LinuxError::EEXIST),
            };
        }
        router.add_ipv4_addr(entry)?;
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(())
    }

    pub(crate) fn remove_ipv4_addr(
        &self,
        entry: crate::router::Ipv4AddrEntry,
    ) -> Result<bool, LinuxError> {
        let mut router = self.router.lock();
        if !router.remove_ipv4_addr(entry) {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        let is_last_owner = !router.has_ipv4_address(entry.addr.address());
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(is_last_owner)
    }

    pub(crate) fn set_primary_ipv4_addr(
        &self,
        dev: usize,
        addr: crate::ip::Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        let mut router = self.router.lock();
        router.set_primary_ipv4_addr(dev, addr)?;
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(())
    }

    pub(crate) fn remove_primary_ipv4_addr(&self, dev: usize) -> Result<(), LinuxError> {
        let mut router = self.router.lock();
        router.remove_primary_ipv4_addr(dev)?;
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(())
    }

    pub(crate) fn route_snapshot(&self) -> Vec<Rule> {
        self.router.lock().route_snapshot()
    }

    pub(crate) fn add_route_rule(&self, rule: Rule) -> Result<(), LinuxError> {
        self.router.lock().add_route_rule(rule)
    }

    pub(crate) fn replace_route_rule(
        &self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.router.lock().replace_route_rule(existing, replacement)
    }

    pub(crate) fn remove_route_rule(&self, route: Rule) {
        self.router.lock().remove_exact_route_rule(route);
    }

    pub(crate) fn set_ipv4_broadcast(
        &self,
        dev: usize,
        broadcast: crate::ip::Ipv4Address,
    ) -> Result<(), LinuxError> {
        self.router.lock().set_ipv4_broadcast(dev, broadcast)
    }

    pub(crate) fn set_ipv4_netmask(&self, dev: usize, prefix_len: u8) -> Result<(), LinuxError> {
        let mut router = self.router.lock();
        router.set_ipv4_netmask(dev, prefix_len)?;
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(())
    }

    pub fn update_device_link(
        &self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<(), LinuxError> {
        let mut router = self.router.lock();
        let is_effective_mtu_changed = router.update_device_link(ifindex, update)?;
        if is_effective_mtu_changed {
            let mut iface = self.iface.lock();
            self.rebuild_interface(&mut router, &mut iface);
        }
        Ok(())
    }

    #[cfg(unittest)]
    pub(crate) fn replace_router_for_tests(&self, mut router: Router) {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, to_smoltcp_instant(monotonic_time()));
        *self.router.lock() = router;
        *self.iface.lock() = iface;
        self.sync_local_ipv4_views();
        self.timeout_deadline_micros.store(0, Ordering::Release);
        self.timeout_poll.wake();
    }

    #[cfg(unittest)]
    pub(crate) fn rebuild_count_for_tests(&self) -> usize {
        self.rebuild_count.load(Ordering::Relaxed)
    }

    /// Removes a device and refreshes every address-derived data-plane view.
    ///
    /// Callers must hold [`crate::control::network_config_lock`] so device
    /// removal cannot interleave with other control-plane mutations.
    pub(crate) fn remove_device_by_model_id(&self, id: kdevice::DeviceId) -> Option<()> {
        let mut router = self.router.lock();
        let effective_mtu = router.effective_mtu();
        router.remove_device_by_model_id(id)?;
        if router.effective_mtu() != effective_mtu {
            let mut iface = self.iface.lock();
            self.rebuild_interface(&mut router, &mut iface);
        }
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Some(())
    }

    pub(crate) fn apply_neighbor_update(
        &self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        self.router.lock().apply_neighbor_update(update, policy)?;
        self.timeout_poll.wake();
        Ok(())
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.router.lock().has_neighbor(dev, dst)
    }

    pub fn send_link_frame(&self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.router.lock().send_link_frame(ifindex, frame)
    }
}

fn poll_smoltcp_ingress_budgeted(
    iface: &mut Interface,
    current: SmoltcpInstant,
    router: &mut Router,
    sockets: &mut SocketSet<'_>,
    budget_packets: usize,
    round_started_at: MonotonicInstant,
) -> SmoltcpIngressProgress {
    let mut progress = SmoltcpIngressProgress::default();
    while progress.packets < budget_packets {
        if progress
            .packets
            .is_multiple_of(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
            && progress.packets > 0
            && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
        {
            progress.has_reached_time_limit = true;
            break;
        }

        match iface.poll_ingress_single(current, router, sockets) {
            PollIngressSingleResult::None => break,
            PollIngressSingleResult::PacketProcessed => {}
            PollIngressSingleResult::SocketStateChanged => {
                progress.has_socket_state_change = true;
            }
        }
        progress.packets += 1;
    }

    if progress.packets > 0 && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
    {
        progress.has_reached_time_limit = true;
    }
    progress
}

fn has_reached_poll_round_time_limit(
    round_started_at: MonotonicInstant,
    current: MonotonicInstant,
) -> bool {
    current.saturating_duration_since(round_started_at) >= POLL_ROUND_TIME_LIMIT
}

fn deferred_poll_progress(
    rx_packets: usize,
    tx_packets: usize,
    timer_events: usize,
    tx_capacity_changed: bool,
) -> PollProgress {
    PollProgress {
        rx_packets,
        tx_packets,
        timer_events,
        tx_capacity_changed,
        has_more: true,
    }
}

fn dispatch_tx_budgeted(
    router: &mut Router,
    timestamp: MonotonicInstant,
    budget: usize,
    round_started_at: MonotonicInstant,
) -> (usize, bool) {
    let mut work_done = 0;
    let mut remaining = budget;
    let mut has_reached_time_limit = false;

    while remaining > 0 {
        let chunk = remaining.min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS);
        let (dispatched, _) = router.dispatch_budgeted(timestamp, chunk);
        work_done += dispatched;
        remaining = remaining.saturating_sub(dispatched);
        if dispatched < chunk {
            break;
        }
        if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
            has_reached_time_limit = true;
            break;
        }
    }

    (work_done, has_reached_time_limit)
}

fn has_immediate_timer_work(current: SmoltcpInstant, next_poll: Option<SmoltcpInstant>) -> bool {
    next_poll.is_some_and(|deadline| deadline <= current)
}

fn earliest_poll_deadline(
    protocol: Option<SmoltcpInstant>,
    deferred_close: Option<SmoltcpInstant>,
) -> Option<SmoltcpInstant> {
    match (protocol, deferred_close) {
        (Some(protocol), Some(close)) => Some(protocol.min(close)),
        (protocol, close) => protocol.or(close),
    }
}

fn output_ipv4_packet_count(
    router: &Router,
    dst_addr: &IpAddress,
    packet_len: usize,
) -> KResult<usize> {
    if packet_len > u16::MAX as usize {
        return Err(LinuxError::EMSGSIZE.into());
    }

    let Some(mtu) = router.route_mtu(dst_addr) else {
        return Ok(1);
    };
    if packet_len <= mtu {
        return Ok(1);
    }

    let payload_len = packet_len
        .checked_sub(IPV4_HEADER_LEN)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    let max_fragment_payload_len = mtu
        .checked_sub(IPV4_HEADER_LEN)
        .map(|len| len / 8 * 8)
        .filter(|len| *len > 0)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    Ok(payload_len
        .checked_add(max_fragment_payload_len - 1)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?
        / max_fragment_payload_len)
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use smoltcp::iface::SocketSet;
    use unittest::{assert, def_test};

    use super::*;
    use crate::{
        buf::PacketOwner,
        device::LoopbackDevice,
        ip::{Ipv4Address, Ipv4Cidr},
        router::{Ipv4AddrEntry, ROUTE_SCOPE_HOST},
        stack::ipv4,
    };

    const TEST_POLL_BUDGET: PollBudget = PollBudget {
        rx_packets: 1,
        tx_packets: 1,
        timer_events: 1,
    };

    #[def_test]
    fn test_smoltcp_deadline_clamps_expired_delay() {
        let current = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_000));
        let deadline = from_smoltcp_instant(SmoltcpInstant::from_micros_const(9_999));

        assert_eq!(deadline.saturating_duration_since(current), TimeSpan::ZERO);
    }

    #[def_test]
    fn test_smoltcp_deadline_preserves_future_delay() {
        let current = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_000));
        let deadline = from_smoltcp_instant(SmoltcpInstant::from_micros_const(12_500));

        assert_eq!(
            deadline.checked_duration_since(current),
            Some(TimeSpan::from_micros(2_500))
        );
    }

    #[def_test]
    fn future_timer_is_not_immediate_work() {
        let current = SmoltcpInstant::from_millis(10);
        let future = SmoltcpInstant::from_millis(11);

        assert!(!has_immediate_timer_work(current, Some(future)));
    }

    #[def_test]
    fn due_timer_is_immediate_work() {
        let current = SmoltcpInstant::from_millis(10);

        assert!(has_immediate_timer_work(current, Some(current)));
    }

    #[def_test]
    fn deferred_close_deadline_can_precede_protocol_deadline() {
        let close = SmoltcpInstant::from_millis(10);
        let protocol = SmoltcpInstant::from_millis(20);

        assert!(earliest_poll_deadline(Some(protocol), Some(close)) == Some(close));
    }

    #[def_test]
    fn poll_round_time_limit_starts_at_one_millisecond() {
        let start = MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(10));
        let before_limit = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_999));
        let at_limit = MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(11));

        assert!(!has_reached_poll_round_time_limit(start, before_limit));
        assert!(has_reached_poll_round_time_limit(start, at_limit));
    }

    #[def_test]
    fn smoltcp_ingress_keeps_packets_after_reaching_budget() {
        let current = SmoltcpInstant::from_millis(10);
        let mut router = Router::new();
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(config, &mut router, current);
        let mut packets = Vec::from([
            PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx),
            PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx),
        ]);
        router.enqueue_ingress_packets(&mut packets);
        let mut sockets = SocketSet::new(vec![]);

        let progress = poll_smoltcp_ingress_budgeted(
            &mut iface,
            current,
            &mut router,
            &mut sockets,
            1,
            monotonic_time(),
        );

        assert_eq!(progress.packets, 1);
        assert!(router.has_pending_ingress());
    }

    /// Regression test for <https://gitee.com/openkylin/x-kernel/issues/IK97G7>.
    #[def_test]
    fn poll_round_defers_router_lock_contention() {
        let service = Service::new(Router::new());
        let _router = service.router.lock();

        let progress = service.poll_budgeted(TEST_POLL_BUDGET);

        assert_eq!(progress.rx_packets, 0);
        assert_eq!(progress.tx_packets, 0);
        assert!(progress.has_more);
    }

    #[def_test(serial)]
    fn tx_dispatch_exposes_loopback_rx_in_the_same_round() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let mut router = Router::new();
        let loopback = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: loopback,
                addr: Ipv4Cidr::new(loopback_addr, 8),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();
        let packet =
            ipv4::build_ipv4_packet(loopback_addr, loopback_addr, ipv4::PROTOCOL_ICMP, 64, &[])
                .unwrap();
        router
            .queue_ipv4_packet(packet, loopback as i32 + 1)
            .unwrap();
        let round_started_at = monotonic_time();

        let (tx_packets, _) = dispatch_tx_budgeted(
            &mut router,
            round_started_at,
            TEST_POLL_BUDGET.tx_packets,
            round_started_at,
        );
        let mut packets = Vec::new();
        let rx_progress = router.drain_rx_budgeted_into(
            round_started_at,
            TEST_POLL_BUDGET.rx_packets,
            &mut packets,
        );

        assert_eq!(tx_packets, 1);
        assert_eq!(rx_progress.work_done, 1);
    }
}
