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
    device::{LinkConfigUpdate, LinkSendSnapshot, LinkSnapshot},
    ip::{IpAddress, IpListenEndpoint},
    netlink::RtnetlinkState,
    poller::{PollBudget, PollProgress},
    router::{DeviceRemoval, Router},
    stack::ingress::IngressProcessor,
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
    /// this path acquires sleepable network and socket-set mutexes.
    pub fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let round_started_at = monotonic_time();
        let device_timestamp = round_started_at;
        let mut has_reached_time_limit = false;
        let mut rx_packets = 0;
        let mut rx_remaining = budget.rx_packets;
        let mut rx_packets_since_time_check = 0;
        let mut sockets = SOCKET_SET.inner.lock();
        let mut control_packets = self.control_batch.lock();
        let mut rx_batch = self.rx_batch.lock();
        let mut accepted_packets = self.accepted_batch.lock();
        control_packets.clear();

        while rx_remaining > 0 {
            if rx_packets_since_time_check >= POLL_TIME_CHECK_INTERVAL_WORK_ITEMS {
                rx_packets_since_time_check = 0;
                if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
                    has_reached_time_limit = true;
                    break;
                }
            }

            let mut router = self.router.lock();
            let batch_budget = rx_remaining
                .min(RX_INGRESS_BATCH_PACKETS)
                .min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
                .min(router.ingress_capacity());
            if batch_budget == 0 {
                break;
            }
            let drain =
                router.drain_rx_budgeted_into(device_timestamp, batch_budget, &mut rx_batch);
            drop(router);
            rx_packets += drain.work_done;
            rx_remaining = rx_remaining.saturating_sub(drain.work_done);
            rx_packets_since_time_check =
                rx_packets_since_time_check.saturating_add(drain.work_done);

            self.ingress.lock().handle_rx_packets(
                device_timestamp,
                &mut rx_batch,
                &mut accepted_packets,
                &mut control_packets,
                &mut sockets,
            );
            let mut router = self.router.lock();
            router.enqueue_ingress_packets(&mut accepted_packets);
            for packet in control_packets.drain(..) {
                if let Err(err) = router.queue_control_ipv4_packet(packet) {
                    warn!("Dropping network control packet: {}", err);
                }
            }
            if !drain.has_more || drain.work_done == 0 {
                break;
            }
        }

        if rx_packets > 0 && has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
            has_reached_time_limit = true;
        }

        let mut router = self.router.lock();
        let current = now();
        let mut iface = self.iface.lock();
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
        LISTEN_TABLE.wake_touched_acceptors(&mut sockets);
        drop(sockets);

        let ingress_has_more = router.has_pending_ingress();
        let rx_has_more = router.has_immediate_rx_work();
        let tx_capacity_before = router.available_tx_packet_slots();
        let (_, mut tx_has_more) = router.dispatch_budgeted(device_timestamp, 0);
        let mut tx_packets = 0;
        let mut tx_remaining = if has_reached_time_limit {
            0
        } else {
            budget.tx_packets
        };
        while tx_remaining > 0 {
            let chunk = tx_remaining.min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS);
            let (work_done, has_more) = router.dispatch_budgeted(device_timestamp, chunk);
            tx_packets += work_done;
            tx_remaining = tx_remaining.saturating_sub(work_done);
            tx_has_more = has_more;
            if work_done < chunk {
                break;
            }
            if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
                break;
            }
        }
        PollProgress {
            rx_packets,
            tx_packets,
            timer_events: usize::from(budget.timer_events > 0 && has_socket_state_change),
            tx_capacity_changed: router.available_tx_packet_slots() > tx_capacity_before,
            has_more: rx_has_more || ingress_has_more || tx_has_more || timer_has_more,
        }
    }

    pub fn get_smoltcp_source_address(
        &self,
        dst_addr: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        let dst_addr = super::from_smoltcp_ip_address(*dst_addr);
        self.router
            .lock()
            .output_route_source(&dst_addr)
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
        self.router.lock().output_route_source(dst_addr)
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
        packet: &mut Option<Vec<u8>>,
        prepare: impl FnOnce(&mut [u8], IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        let mut router = self.router.lock();
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        if !router.can_enqueue_tx_packet() {
            return Err(KError::WouldBlock);
        }

        let route_source = router.output_route_source(dst_addr)?;
        let source_addr = bound_src.unwrap_or(route_source);
        let packet_count = output_ipv4_packet_count(&router, dst_addr, packet_len)?;
        if !router.can_enqueue_tx_packets(packet_count) {
            return Err(KError::WouldBlock);
        }

        let route_mtu = router.route_mtu(dst_addr);
        prepare(
            packet.as_deref_mut().ok_or(KError::InvalidInput)?,
            source_addr,
            route_mtu,
        )?;
        router.queue_ipv4_packet(packet.take().ok_or(KError::InvalidInput)?)
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

    pub fn sync_netlink(&self, state: &RtnetlinkState) {
        // Keep the Router locked until all derived views are refreshed so a
        // new RX batch cannot observe a partially synchronized configuration.
        let mut router = self.router.lock();
        router.sync_netlink(state);
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
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
    /// Returns the removed interface and orphaned IPv4 sources when found.
    pub(crate) fn remove_device_by_model_id(&self, id: kdevice::DeviceId) -> Option<DeviceRemoval> {
        let mut router = self.router.lock();
        let effective_mtu = router.effective_mtu();
        let removal = router.remove_device_by_model_id(id)?;
        if router.effective_mtu() != effective_mtu {
            let mut iface = self.iface.lock();
            self.rebuild_interface(&mut router, &mut iface);
        }
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Some(removal)
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
    use alloc::{vec, vec::Vec};

    use smoltcp::iface::SocketSet;
    use unittest::{assert, def_test};

    use super::*;
    use crate::buf::PacketOwner;

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
}
