// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service with budgeted data-plane progression.

use alloc::{sync::Arc, vec, vec::Vec};
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
    iface::{Interface, PollIngressSingleResult, PollResult},
    time::Instant as SmoltcpInstant,
    wire::{HardwareAddress, IpAddress as SmoltcpIpAddress, IpCidr as SmoltcpIpCidr},
};

use crate::{
    LISTEN_TABLE, SOCKET_SET,
    buf::PacketBuf,
    device::{
        LINK_FLAG_UP, LinkConfigUpdate, LinkSendSnapshot, LinkSnapshot, NeighborEvent,
        NeighborSnapshot, NeighborState, NeighborUpdate, NetDeviceId, RxDeviceSelection,
        net_rx::RxPacket,
    },
    ip::{self, IpAddress, Ipv4Address},
    network::{
        event::{NetworkEvent, RouteProvenance},
        interface_address::Ipv4AddrSnapshot,
        router::Rule,
    },
    poller::{PollBudget, PollProgress},
    router::{Ipv4OutputRoute, NeighborUpdatePolicy, Router},
    stack::{
        device::StackDevice,
        ingress::{IngressProcessor, prepare_smoltcp_ingress},
        socket_set::SocketSetState,
        time::{earliest_poll_deadline, has_immediate_timer_work, now, to_smoltcp_instant},
    },
};

const IPV4_HEADER_LEN: usize = 20;
const RX_INGRESS_BATCH_PACKETS: usize = 64;
const POLL_ROUND_TIME_LIMIT: TimeSpan = TimeSpan::from_millis(1);
const POLL_TIME_CHECK_INTERVAL_WORK_ITEMS: usize = 32;
const IPV4_REASSEMBLY_TIMER_PENDING: u64 = 1;
const IPV4_REASSEMBLY_DEADLINE_OFFSET: u64 = 2;

#[derive(Default)]
struct SmoltcpIngressProgress {
    packets: usize,
    has_socket_state_change: bool,
    has_reached_time_limit: bool,
}

#[derive(Default)]
struct DeviceRxProgress {
    /// Consumed frames, including handled or dropped frames without an IP packet.
    packets: usize,
    has_reached_time_limit: bool,
    has_lock_contention: bool,
}

#[derive(Default)]
struct Ipv4ExpirationProgress {
    timer_events: usize,
    has_more: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExistingIpv4AddrAction {
    Keep,
    Reject,
}

fn ipv4_reassembly_timer_state(next: Option<MonotonicInstant>) -> u64 {
    let Some(next) = next else {
        return 0;
    };
    if next <= monotonic_time() {
        return IPV4_REASSEMBLY_TIMER_PENDING;
    }
    u64::try_from(next.span_since_origin().as_micros())
        .unwrap_or(u64::MAX)
        .saturating_add(IPV4_REASSEMBLY_DEADLINE_OFFSET)
}

struct NetworkTimer;

#[cfg(unittest)]
static SAMPLE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(unittest)]
static NEIGHBOR_TIMER_SCHEDULES: AtomicUsize = AtomicUsize::new(0);

unittest::path_mod_test!(
    timer_irq_tests,
    "../../ut-tests/runtime/service/timer_irq_tests.rs"
);

impl ktask::TimerHandler for NetworkTimer {
    fn on_expiration(&self, timer: ktask::TimerControl<'_>, _: ktask::Expiration) {
        crate::SERVICE.handle_timer_tick();
        // Preserve completion-relative sampling, without catch-up callbacks.
        if let Err(error) = timer.rearm(monotonic_time() + crate::consts::TIMER_SAMPLE_PERIOD) {
            log::error!("[knet] timer sampling stopped: {error:?}");
        }
        #[cfg(unittest)]
        SAMPLE_COUNT.fetch_add(1, Ordering::Release);
    }
}

pub(crate) struct Service {
    timer: ktask::Timer<NetworkTimer>,
    pub(crate) iface: Mutex<Interface>,
    router: Mutex<Router>,
    ingress: Mutex<IngressProcessor>,
    rx_batch: Mutex<Vec<RxPacket>>,
    accepted_batch: Mutex<Vec<PacketBuf>>,
    control_batch: Mutex<Vec<Vec<u8>>>,
    timeout_deadline_micros: AtomicU64,
    ipv4_reassembly_deadline_micros: AtomicU64,
    timeout_poll: Arc<PollSet>,
    #[cfg(unittest)]
    rebuild_count: AtomicUsize,
}

impl Service {
    pub(crate) fn new(mut router: Router) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(
            config,
            &mut router.stack_device(),
            to_smoltcp_instant(monotonic_time()),
        );

        let service = Self {
            timer: ktask::Timer::new(NetworkTimer),
            iface: Mutex::new(iface),
            router: Mutex::new(router),
            ingress: Mutex::new(IngressProcessor::new()),
            rx_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            accepted_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            control_batch: Mutex::new(Vec::new()),
            timeout_deadline_micros: AtomicU64::new(0),
            ipv4_reassembly_deadline_micros: AtomicU64::new(0),
            timeout_poll: Arc::new(PollSet::new()),
            #[cfg(unittest)]
            rebuild_count: AtomicUsize::new(0),
        };
        service.sync_local_ipv4_views();
        service
    }

    /// Starts sampling after the global service and poller have been published.
    ///
    /// # Panics
    /// Panics if the first deadline exceeds the timer core's nanosecond range.
    pub(crate) fn start_timer(&'static self) {
        // SAFETY: network initialization follows local scheduler/IRQ setup.
        // SERVICE and the poller are published and remain live. The handler
        // performs IRQ-safe notification without sleeping or changing IRQ state.
        unsafe {
            self.timer
                .start(monotonic_time() + crate::consts::TIMER_SAMPLE_PERIOD)
        }
        .expect("network sampling deadline must fit the timer core");
    }

    /// Advances the network data plane within one bounded poll round.
    ///
    /// The caller must run in task context without holding spinlocks because
    /// transport and device handlers may acquire sleepable mutexes. Shared
    /// progression locks are acquired with `try_lock`; contention leaves the
    /// pending batches intact and reports immediate work for a later round.
    /// A due IPv4 reassembly timer remains pending across ingress-lock
    /// contention without aborting TX or protocol progress in the current round.
    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: TODO(cg): legacy budgeted poll round; split ingress, \
                  timers and transmit phases"
    )]
    pub(crate) fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let round_started_at = monotonic_time();
        let device_timestamp = round_started_at;
        let mut control_packets = self.control_batch.lock();
        let mut rx_batch = self.rx_batch.lock();
        let mut accepted_packets = self.accepted_batch.lock();

        let ipv4_expiration = self.try_expire_ipv4_fragments(
            device_timestamp,
            budget.timer_events,
            &mut control_packets,
        );
        let mut timer_events = ipv4_expiration.timer_events;
        if !self.try_flush_ingress_batches(&mut accepted_packets, &mut control_packets) {
            return deferred_poll_progress(0, 0, timer_events);
        }

        // Neighbor transitions are committed in Router but published after
        // its lock is dropped.  Serialize the take/publish pair with the
        // control plane so a data-plane `NeighborAdded` cannot be observed
        // after the control plane has removed the same entry.
        let Some(network_config_guard) = crate::control::try_network_config_lock() else {
            return deferred_poll_progress(0, 0, timer_events);
        };
        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(0, 0, timer_events);
        };
        let TxDispatchProgress {
            packet_count: mut tx_packets,
            mut has_reached_time_limit,
            capacity_poll,
        } = dispatch_tx_budgeted(
            &mut router,
            device_timestamp,
            budget.tx_packets,
            round_started_at,
        );
        let neighbor_events = router.take_neighbor_events();
        drop(router);
        wake_tx_capacity_waiters(capacity_poll);
        self.publish_neighbor_events(neighbor_events);
        drop(network_config_guard);
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
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
        }

        let Some(mut sockets) = SOCKET_SET.inner.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
        };
        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
        };
        let current = now();
        let Some(mut iface) = self.iface.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
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
                &mut router.stack_device(),
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
            let result = iface.poll_egress(current, &mut router.stack_device(), &mut sockets);
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
        let protocol_deadline = iface.poll_at(current, &sockets);
        drop(iface);
        LISTEN_TABLE.refresh_acceptors(&mut sockets);
        drop(sockets);
        timer_events += usize::from(timer_events < budget.timer_events && has_socket_state_change);
        // The ingress phase above holds Router for smoltcp.  Release it before
        // taking the configuration lock so the data plane always follows the
        // control plane's `configuration -> Router` lock order.
        drop(router);

        if has_reached_time_limit {
            tx_remaining = 0;
        }
        let Some(network_config_guard) = crate::control::try_network_config_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
        };
        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
        };
        let TxDispatchProgress {
            packet_count: dispatched,
            has_reached_time_limit: tx_time_limit,
            capacity_poll,
        } = dispatch_tx_budgeted(
            &mut router,
            device_timestamp,
            tx_remaining,
            round_started_at,
        );
        tx_packets += dispatched;
        has_reached_time_limit |= tx_time_limit;
        let neighbor_events = router.take_neighbor_events();
        drop(router);
        wake_tx_capacity_waiters(capacity_poll);
        self.publish_neighbor_events(neighbor_events);
        drop(network_config_guard);

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
                return deferred_poll_progress(rx_packets, tx_packets, timer_events);
            }
        }

        let Some(mut router) = self.router.try_lock() else {
            return deferred_poll_progress(rx_packets, tx_packets, timer_events);
        };
        let ingress_has_more = router.has_pending_ingress();
        let rx_has_more = router.has_immediate_rx_work();
        let (_, tx_has_more) = router.dispatch_budgeted(device_timestamp, 0);
        let next_poll = earliest_poll_deadline(
            protocol_deadline,
            deferred_close_deadline,
            router.next_neighbor_deadline().map(to_smoltcp_instant),
        );
        let timer_current = now();
        let timer_has_more = has_immediate_timer_work(timer_current, next_poll);
        self.update_poll_timeout(timer_current, next_poll);
        PollProgress {
            rx_packets,
            tx_packets,
            timer_events,
            has_more: rx_has_more
                || ingress_has_more
                || tx_has_more
                || timer_has_more
                || ipv4_expiration.has_more
                || self.has_pending_ipv4_reassembly_expiration(),
        }
    }

    /// Publishes asynchronous neighbor transitions after the Router and
    /// device locks have been released.
    ///
    /// Only poller work reaches this method. The `knet-poller` normal queue
    /// runs callbacks in worker task context, never inline or in BH context,
    /// so taking netlink's sleepable `PORTS` lock while publishing is valid.
    fn publish_neighbor_events(&self, events: Vec<NeighborEvent>) {
        if events.is_empty() {
            return;
        }
        let events = events
            .into_iter()
            .map(|event| match event {
                NeighborEvent::Added(neighbor) => NetworkEvent::NeighborAdded {
                    neighbor: NeighborUpdate {
                        dev: neighbor.dev,
                        dst: neighbor.dst,
                        state: neighbor.state,
                    },
                },
                NeighborEvent::Removed(neighbor) => NetworkEvent::NeighborRemoved { neighbor },
            })
            .collect();
        crate::netlink::publish_network_events(events);
    }

    fn poll_device_rx_budgeted(
        &self,
        device_timestamp: MonotonicInstant,
        round_started_at: MonotonicInstant,
        budget: usize,
        rx_batch: &mut Vec<RxPacket>,
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

            // Keep the configuration guard through ingress dispatch so a
            // data-plane neighbor transition cannot be published after the
            // control plane has removed the same entry.  Processing holds
            // config -> ingress and releases ingress before the flush takes
            // SOCKET_SET -> Router; no socket dispatch callback acquires the
            // configuration lock, so that order has no inverse path.
            let Some(_network_config_guard) = crate::control::try_network_config_lock() else {
                progress.has_lock_contention = true;
                break;
            };
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
                let neighbor_events = router.take_neighbor_events();
                drop(router);
                self.publish_neighbor_events(neighbor_events);
                break;
            }
            let drain = router.drain_rx_budgeted_into(device_timestamp, batch_budget, rx_batch);
            let neighbor_events = router.take_neighbor_events();
            drop(router);
            self.publish_neighbor_events(neighbor_events);
            if let Some((ifindex, error)) = &drain.error {
                warn!("network RX poll failed on ifindex {ifindex}: {error:?}");
            }
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
            // Keep the partial batch above, but end this round rather than
            // spinning on a failed device; later poll rounds may retry it.
            if drain.error.is_some() {
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

    fn try_expire_ipv4_fragments(
        &self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> Ipv4ExpirationProgress {
        if !self.has_pending_ipv4_reassembly_expiration() {
            return Ipv4ExpirationProgress::default();
        }
        let Some(mut ingress) = self.ingress.try_lock() else {
            return Ipv4ExpirationProgress {
                timer_events: 0,
                has_more: true,
            };
        };
        let timer_events = ingress.expire_ipv4_fragments(timestamp, timer_budget, control_packets);
        let next_deadline = ingress.ipv4_reassembly_deadline();
        let has_more = next_deadline.is_some_and(|deadline| deadline <= timestamp);
        self.complete_ipv4_reassembly_expiration(next_deadline, has_more);
        drop(ingress);
        Ipv4ExpirationProgress {
            timer_events,
            has_more: self.has_pending_ipv4_reassembly_expiration(),
        }
    }

    fn try_process_rx_batch(
        &self,
        device_timestamp: MonotonicInstant,
        rx_batch: &mut Vec<RxPacket>,
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
            crate::netfilter::global(),
            rx_batch,
            accepted_packets,
            control_packets,
        );
        let next_deadline = ingress.ipv4_reassembly_deadline();
        self.update_ipv4_reassembly_timeout(next_deadline);
        drop(ingress);
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

    pub(crate) fn get_smoltcp_source_address(
        &self,
        dst_addr: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        let dst_addr = ip::from_smoltcp_ip_address(*dst_addr);
        self.router
            .lock()
            .output_route_source(&dst_addr, 0)
            .map(ip::to_smoltcp_ip_address)
    }

    #[cfg(unittest)]
    pub(crate) fn get_source_address(&self, dst_addr: &IpAddress) -> KResult<IpAddress> {
        self.router.lock().output_route_source(dst_addr, 0)
    }

    /// Resolves the source and effective destination before transport state or
    /// packet checksums are committed. Non-IPv4 traffic retains its source lookup.
    pub(crate) fn resolve_ip_output(
        &self,
        dst: IpAddress,
        src: Option<IpAddress>,
        oif: i32,
    ) -> KResult<(IpAddress, IpAddress)> {
        let router = self.router.lock();
        match dst {
            IpAddress::Ipv4(dst) => {
                let src = match src {
                    Some(IpAddress::Ipv4(src)) => Some(src),
                    Some(_) => return Err(LinuxError::EAFNOSUPPORT.into()),
                    None => None,
                };
                let route = router.lookup_ipv4_output(
                    dst,
                    src,
                    NetDeviceId::from_optional_ifindex(oif)?,
                )?;
                Ok((
                    IpAddress::Ipv4(route.source.ok_or(LinuxError::ENETUNREACH)?),
                    IpAddress::Ipv4(route.destination),
                ))
            }
            _ => {
                let source = match src {
                    Some(source) => source,
                    None => router.output_route_source(&dst, oif)?,
                };
                Ok((source, dst))
            }
        }
    }

    pub(crate) fn lookup_ipv4_output(
        &self,
        dst: Ipv4Address,
        src: Option<Ipv4Address>,
        oif: Option<NetDeviceId>,
    ) -> Result<Ipv4OutputRoute, LinuxError> {
        Ok(self.router.lock().lookup_ipv4_output(dst, src, oif)?)
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_route_mtu(&self, dst_addr: &IpAddress) -> Option<usize> {
        self.router.lock().route_mtu(dst_addr)
    }

    pub(crate) fn can_send_ip_packet(&self) -> bool {
        self.router.lock().can_enqueue_tx_packet()
    }

    pub(crate) fn prepare_and_send_ipv4_packet(
        &self,
        bound_src: Option<IpAddress>,
        dst_addr: &IpAddress,
        oif: i32,
        allow_broadcast: bool,
        packet: &mut Option<Vec<u8>>,
        prepare: impl FnOnce(&mut [u8], IpAddress, IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        let mut router = self.router.lock();
        let IpAddress::Ipv4(destination) = *dst_addr else {
            return Err(LinuxError::EAFNOSUPPORT.into());
        };
        let source = match bound_src {
            Some(IpAddress::Ipv4(source)) => Some(source),
            Some(_) => return Err(LinuxError::EAFNOSUPPORT.into()),
            None => None,
        };
        let route = router.lookup_ipv4_output(
            destination,
            source,
            NetDeviceId::from_optional_ifindex(oif)?,
        )?;
        let dst_addr = &IpAddress::Ipv4(route.destination);
        if router.ipv4_dest_requires_broadcast(dst_addr) && !allow_broadcast {
            return Err(KError::from(LinuxError::EACCES));
        }
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        let source_addr = IpAddress::Ipv4(route.source.ok_or(LinuxError::ENETUNREACH)?);
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
            *dst_addr,
            route_mtu,
        )?;
        let packet = packet.take().ok_or(KError::InvalidInput)?;
        if is_loopback {
            router.transmit_ipv4_now(packet, monotonic_time())
        } else {
            router.queue_ipv4_packet(packet, oif)
        }
    }

    pub(crate) fn register_rx_waker(
        &self,
        selection: RxDeviceSelection,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        context.register(self.timeout_poll.as_ref())?;
        let source_waker = Waker::from(self.timeout_poll.clone());

        let current = now();
        let sockets = SOCKET_SET.inner.lock();
        let protocol_deadline = self.iface.lock().poll_at(current, &sockets);
        let deferred_close_deadline = sockets.deferred_tcp_close_deadline();
        drop(sockets);
        let next = {
            let router = self.router.lock();
            let next = earliest_poll_deadline(
                protocol_deadline,
                deferred_close_deadline,
                router.next_neighbor_deadline().map(to_smoltcp_instant),
            );

            for (slot, device) in router.devices.iter().enumerate() {
                let id = router.devices.id_at(slot);
                if selection.includes(id) {
                    // Ethernet devices that support interrupt-driven RX register
                    // on their NetRx softirq-backed poll source. Loopback and
                    // poll-only devices keep the timeout_poll-backed source waker.
                    device.register_rx_waker(&source_waker, context)?;
                }
            }
            next
        };
        // Publish the timer wake after releasing Router: waking the poller
        // takes the poller state lock, which must not nest inside the data
        // plane's routing lock.
        self.update_poll_timeout(current, next);
        Ok(source_waker)
    }

    /// Registers protocol progress and data-queue capacity for a send recheck.
    ///
    /// The caller must recheck send readiness after registration.
    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        let source_waker = self.register_rx_waker(RxDeviceSelection::All, context)?;
        self.router.lock().register_tx_waker(context)?;
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

    fn update_ipv4_reassembly_timeout(&self, next: Option<MonotonicInstant>) {
        let next_state = ipv4_reassembly_timer_state(next);
        let mut current = self.ipv4_reassembly_deadline_micros.load(Ordering::Acquire);
        loop {
            // A timer tick owns the pending state until the poller consumes
            // the due queues. A concurrent RX refresh must not disarm it.
            if current == IPV4_REASSEMBLY_TIMER_PENDING {
                return;
            }
            match self.ipv4_reassembly_deadline_micros.compare_exchange_weak(
                current,
                next_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn complete_ipv4_reassembly_expiration(&self, next: Option<MonotonicInstant>, has_more: bool) {
        let next_state = if has_more {
            IPV4_REASSEMBLY_TIMER_PENDING
        } else {
            ipv4_reassembly_timer_state(next)
        };
        self.ipv4_reassembly_deadline_micros
            .store(next_state, Ordering::Release);
    }

    fn has_pending_ipv4_reassembly_expiration(&self) -> bool {
        self.ipv4_reassembly_deadline_micros.load(Ordering::Acquire)
            == IPV4_REASSEMBLY_TIMER_PENDING
    }

    pub(crate) fn handle_timer_tick(&self) {
        let current_micros = u64::try_from(now().total_micros()).unwrap_or(0);
        let protocol_deadline = self.timeout_deadline_micros.load(Ordering::Acquire);
        let has_protocol_expired = protocol_deadline != 0
            && current_micros.saturating_add(1) >= protocol_deadline
            && self
                .timeout_deadline_micros
                .compare_exchange(protocol_deadline, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();

        let reassembly_deadline = self.ipv4_reassembly_deadline_micros.load(Ordering::Acquire);
        let has_reassembly_expired = reassembly_deadline > IPV4_REASSEMBLY_TIMER_PENDING
            && current_micros.saturating_add(IPV4_REASSEMBLY_DEADLINE_OFFSET)
                >= reassembly_deadline
            && self
                .ipv4_reassembly_deadline_micros
                .compare_exchange(
                    reassembly_deadline,
                    IPV4_REASSEMBLY_TIMER_PENDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();

        if has_protocol_expired || has_reassembly_expired {
            self.publish_timer_expiration();
        }
    }

    fn publish_timer_expiration(&self) {
        self.timeout_poll.as_ref().wake();
        crate::poller::network_poller().notify(crate::poller::PollReason::Timer);
    }

    fn sync_local_ipv4_views(&self) {
        let router = self.router.lock();
        self.sync_ingress_device_names(&router);
        let local_ipv4_addrs = router.local_ipv4_addrs();
        drop(router);
        self.replace_local_ipv4_views(&local_ipv4_addrs);
    }

    fn sync_ingress_device_names(&self, router: &Router) {
        self.ingress.lock().update_device_names(
            router
                .devices
                .iter_with_ids()
                .map(|(id, device)| (id.ifindex(), device.name())),
        );
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
                    .expect("DeviceRegistry enforces the shared smoltcp address capacity");
            }
        });
    }

    fn rebuild_interface(&self, router: &mut Router, iface: &mut Interface) {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let mut new_iface = Interface::new(
            config,
            &mut router.stack_device(),
            to_smoltcp_instant(monotonic_time()),
        );
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

    pub(crate) fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.router.lock().devices.link_snapshots()
    }

    pub(crate) fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.router
            .lock()
            .devices
            .link_snapshot_for_ifindex(ifindex)
    }

    pub(crate) fn has_device(&self, ifindex: i32) -> bool {
        self.router.lock().devices.has_device(ifindex)
    }

    pub(crate) fn device_id_by_name(&self, name: &str) -> Option<NetDeviceId> {
        self.router.lock().devices.device_id_by_name(name)
    }

    pub(crate) fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        self.router
            .lock()
            .devices
            .link_send_snapshot_for_ifindex(ifindex)
    }

    pub(crate) fn ipv4_addr_snapshots(
        &self,
    ) -> Vec<crate::network::interface_address::Ipv4AddrSnapshot> {
        self.router.lock().ipv4_addr_snapshots()
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addr_entries(
        &self,
    ) -> Vec<(
        NetDeviceId,
        crate::network::interface_address::Ipv4AddrEntry,
    )> {
        self.router
            .lock()
            .ipv4_addr_snapshots()
            .into_iter()
            .map(|snapshot| (snapshot.dev, snapshot.entry))
            .collect()
    }

    pub(crate) fn interface_ipv4_addr(
        &self,
        name: &str,
    ) -> Result<crate::ip::Ipv4Cidr, LinuxError> {
        self.router.lock().interface_ipv4_addr(name)
    }

    pub(crate) fn add_ipv4_addr(
        &self,
        dev: NetDeviceId,
        entry: crate::network::interface_address::Ipv4AddrEntry,
        existing_action: ExistingIpv4AddrAction,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        if let Some(addr) = router.ipv4_addr_snapshot(dev, entry.addr) {
            return match existing_action {
                // A matching address with NLM_F_REPLACE keeps its attributes,
                // but Linux still sends RTM_NEWADDR for the existing entry.
                ExistingIpv4AddrAction::Keep => Ok(vec![NetworkEvent::AddrAdded { addr }]),
                ExistingIpv4AddrAction::Reject => Err(LinuxError::EEXIST),
            };
        }
        let before = ipv4_snapshots_for_device(&router, dev);
        let routes_before = router.table.snapshot();
        router.add_ipv4_addr(dev, entry)?;
        let after = ipv4_snapshots_for_device(&router, dev);
        let routes_after = router.table.snapshot();
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(ipv4_transition_events(
            before,
            after,
            &[],
            routes_before,
            &routes_after,
        ))
    }

    pub(crate) fn remove_ipv4_addr(
        &self,
        dev: NetDeviceId,
        addr: crate::ip::Ipv4Cidr,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        let before = ipv4_snapshots_for_device(&router, dev);
        let routes_before = router.table.snapshot();
        if !router
            .ipv4_addr_snapshots()
            .iter()
            .any(|snapshot| snapshot.dev == dev && snapshot.entry.addr == addr)
        {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        let removed_sources = router.remove_ipv4_addr(dev, addr);
        if removed_sources.is_empty() {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        let after = ipv4_snapshots_for_device(&router, dev);
        let routes_after = router.table.snapshot();
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(ipv4_transition_events(
            before,
            after,
            &removed_sources,
            routes_before,
            &routes_after,
        ))
    }

    pub(crate) fn set_primary_ipv4_addr(
        &self,
        dev: NetDeviceId,
        addr: crate::ip::Ipv4Cidr,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        let before = ipv4_snapshots_for_device(&router, dev);
        let routes_before = router.table.snapshot();
        router.set_primary_ipv4_addr(dev, addr)?;
        let after = ipv4_snapshots_for_device(&router, dev);
        let routes_after = router.table.snapshot();
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(ipv4_transition_events(
            before,
            after,
            &[],
            routes_before,
            &routes_after,
        ))
    }

    pub(crate) fn remove_primary_ipv4_addr(
        &self,
        dev: NetDeviceId,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        let before = ipv4_snapshots_for_device(&router, dev);
        let routes_before = router.table.snapshot();
        let removed_sources = router.remove_primary_ipv4_addr(dev)?;
        let after = ipv4_snapshots_for_device(&router, dev);
        let routes_after = router.table.snapshot();
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(ipv4_transition_events(
            before,
            after,
            &removed_sources,
            routes_before,
            &routes_after,
        ))
    }

    pub(crate) fn route_snapshot(&self) -> Vec<Rule> {
        self.router.lock().table.snapshot()
    }

    pub(crate) fn add_route_rule(&self, rule: Rule) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        router.add_route_rule(rule)?;
        Ok(vec![NetworkEvent::RouteAdded {
            route: rule,
            provenance: RouteProvenance::Request,
        }])
    }

    pub(crate) fn replace_route_rule(
        &self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        router.replace_route_rule(existing, replacement)?;
        // Linux `fib_table_insert` replaces an existing route in place and
        // emits only RTM_NEWROUTE; there is no removal notification.
        Ok(vec![NetworkEvent::RouteAdded {
            route: replacement,
            provenance: RouteProvenance::Request,
        }])
    }

    pub(crate) fn remove_route_rule(&self, route: Rule) -> Vec<NetworkEvent> {
        self.router
            .lock()
            .table
            .take_exact_rule(route)
            .map(|route| NetworkEvent::RouteRemoved {
                route,
                provenance: RouteProvenance::Request,
            })
            .into_iter()
            .collect()
    }

    pub(crate) fn set_ipv4_broadcast(
        &self,
        dev: NetDeviceId,
        broadcast: crate::ip::Ipv4Address,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        let before = ipv4_snapshots_for_device(&router, dev);
        let routes_before = router.table.snapshot();
        router.set_ipv4_broadcast(dev, broadcast)?;
        let after = ipv4_snapshots_for_device(&router, dev);
        let routes_after = router.table.snapshot();
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(ipv4_transition_events(
            before,
            after,
            &[],
            routes_before,
            &routes_after,
        ))
    }

    pub(crate) fn set_ipv4_netmask(
        &self,
        dev: NetDeviceId,
        prefix_len: u8,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        let before = ipv4_snapshots_for_device(&router, dev);
        let routes_before = router.table.snapshot();
        router.set_ipv4_netmask(dev, prefix_len)?;
        let after = ipv4_snapshots_for_device(&router, dev);
        let routes_after = router.table.snapshot();
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Ok(ipv4_transition_events(
            before,
            after,
            &[],
            routes_before,
            &routes_after,
        ))
    }

    pub(crate) fn update_device_link(
        &self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<Vec<NetworkEvent>, LinuxError> {
        let mut router = self.router.lock();
        let before = router.devices.link_snapshot_for_ifindex(ifindex);
        let routes_before = router.table.snapshot();
        let is_effective_mtu_changed = router.devices.update_device_link(ifindex, update)?;
        self.sync_ingress_device_names(&router);
        if is_effective_mtu_changed {
            let mut iface = self.iface.lock();
            self.rebuild_interface(&mut router, &mut iface);
        }
        router.sync_ipv4_address_routes();
        let routes_after = router.table.snapshot();
        let Some(before) = before else {
            return Ok(Vec::new());
        };
        let Some(after) = router.devices.link_snapshot_for_ifindex(ifindex) else {
            return Ok(Vec::new());
        };
        let link_became_up = before.flags & LINK_FLAG_UP == 0 && after.flags & LINK_FLAG_UP != 0;
        if before == after && routes_before == routes_after {
            return Ok(Vec::new());
        }
        // Linux publishes the link transition from `__dev_notify_flags()`
        // before running the `NETDEV_UP`/`NETDEV_DOWN` notifier chain that
        // resynchronizes addresses and derived routes.  Route additions from
        // `fib_add_ifaddr()` are real RTM_NEWROUTE notifications.  On the way
        // down, `fib_table_flush()` only calls the internal FIB notifier for
        // dead routes; it calls `rtmsg_fib(RTM_DELROUTE, ...)` solely when the
        // preferred source was removed, which a link transition does not do.
        // The routes disappear from the table without a userspace deletion
        // notification.
        let mut route_events =
            ipv4_transition_events(Vec::new(), Vec::new(), &[], routes_before, &routes_after);
        route_events.retain(|event| !matches!(event, NetworkEvent::RouteRemoved { .. }));
        // A derived-route change with an unchanged link is not a link event.
        // `__dev_notify_flags()` only calls `rtmsg_ifinfo()` when the link
        // flags or the device state actually changed; emitting a LinkChanged
        // with `changed_flags == 0` here would be a synthetic notification.
        let mut events = Vec::new();
        if before != after {
            events.push(NetworkEvent::LinkChanged {
                changed_flags: before.flags ^ after.flags,
                link: after,
            });
        }
        events.extend(route_events);
        drop(router);
        // Neighbor deadlines are frozen, not retired, while the link is down
        // (`next_neighbor_deadline` reports no work so a down device cannot
        // spin the timer). Bringing the link back up makes those deadlines
        // serviceable again, so the poller must recompute and arm the Service
        // timer even when the link transition changes no routes.
        if link_became_up {
            self.schedule_neighbor_timer();
        }
        Ok(events)
    }

    #[cfg(unittest)]
    pub(crate) fn replace_router_for_tests(&self, mut router: Router) {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(
            config,
            &mut router.stack_device(),
            to_smoltcp_instant(monotonic_time()),
        );
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

    #[cfg(unittest)]
    pub(crate) fn neighbor_timer_schedule_count(&self) -> usize {
        NEIGHBOR_TIMER_SCHEDULES.load(Ordering::Relaxed)
    }

    /// Removes a device and refreshes every address-derived data-plane view.
    ///
    /// Callers must hold [`crate::control::network_config_lock`] so device
    /// removal cannot interleave with other control-plane mutations.
    pub(crate) fn remove_device_by_model_id(
        &self,
        id: kdevice::DeviceId,
    ) -> Option<Vec<NetworkEvent>> {
        let mut router = self.router.lock();
        let effective_mtu = router.devices.effective_mtu();
        let removed = router.remove_device_by_model_id(id)?;
        self.sync_ingress_device_names(&router);
        if router.devices.effective_mtu() != effective_mtu {
            let mut iface = self.iface.lock();
            self.rebuild_interface(&mut router, &mut iface);
        }
        let local_ipv4_addrs = router.local_ipv4_addrs();
        self.replace_local_ipv4_views(&local_ipv4_addrs);
        Some(removed)
    }

    #[cfg(unittest)]
    pub(crate) fn apply_neighbor_update(
        &self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<Option<NetworkEvent>, LinuxError> {
        let event = self.router.lock().apply_neighbor_update(update, policy)?;
        self.schedule_neighbor_timer();
        Ok(event)
    }

    pub(crate) fn apply_neighbor_admin_update<F>(
        &self,
        dev: NetDeviceId,
        dst: IpAddress,
        policy: NeighborUpdatePolicy,
        build: F,
    ) -> Result<Option<NetworkEvent>, LinuxError>
    where
        F: FnOnce(Option<NeighborState>) -> Result<NeighborUpdate, LinuxError>,
    {
        let event = self
            .router
            .lock()
            .apply_neighbor_admin_update(dev, dst, policy, build)?;
        self.schedule_neighbor_timer();
        Ok(event)
    }

    /// Wakes timer waiters and schedules a poll round after a neighbor change.
    ///
    /// A neighbor mutation can install an immediately due probe or a future
    /// TTL. The last poll round may have recorded no aggregate deadline on an
    /// idle link, and `timeout_poll` only wakes poll waiters; the sampling
    /// timer therefore needs an explicit poller notification to recompute and
    /// arm the neighbor deadline.
    fn schedule_neighbor_timer(&self) {
        self.timeout_poll.wake();
        crate::poller::network_poller().notify(crate::poller::PollReason::Timer);
        #[cfg(unittest)]
        NEIGHBOR_TIMER_SCHEDULES.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn remove_neighbor(
        &self,
        dev: NetDeviceId,
        dst: IpAddress,
    ) -> Result<NeighborSnapshot, LinuxError> {
        let snapshot = self.router.lock().remove_neighbor(dev, dst)?;
        self.timeout_poll.wake();
        Ok(snapshot)
    }

    pub(crate) fn neighbor_snapshot(
        &self,
        dev: NetDeviceId,
        dst: IpAddress,
    ) -> Option<NeighborSnapshot> {
        self.router
            .lock()
            .devices
            .get_by_id(dev)?
            .neighbor_state(dst)
            .map(|state| NeighborSnapshot { dev, dst, state })
    }

    pub(crate) fn neighbor_snapshots(&self) -> Vec<NeighborSnapshot> {
        self.router.lock().neighbor_snapshots()
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: NetDeviceId, dst: IpAddress) -> bool {
        self.router.lock().has_neighbor(dev, dst)
    }

    pub(crate) fn send_link_frame(&self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.router.lock().send_link_frame(ifindex, frame)
    }
}

fn poll_smoltcp_ingress_budgeted(
    iface: &mut Interface,
    current: SmoltcpInstant,
    device: &mut StackDevice<'_>,
    sockets: &mut SocketSetState<'_>,
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

        match sockets.poll_ingress_single(iface, current, device) {
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

/// Reports deferred work as immediately runnable.
///
/// `try_*` contention preserves all pending state.  Returning `has_more`
/// makes the poller run another bounded round and then requeue itself, so a
/// temporarily held configuration lock delays rather than loses the work.
fn deferred_poll_progress(
    rx_packets: usize,
    tx_packets: usize,
    timer_events: usize,
) -> PollProgress {
    PollProgress {
        rx_packets,
        tx_packets,
        timer_events,
        has_more: true,
    }
}

struct TxDispatchProgress {
    packet_count: usize,
    has_reached_time_limit: bool,
    /// Captured under the queue lock and consumed after releasing that lock.
    capacity_poll: Option<PollSet>,
}

fn dispatch_tx_budgeted(
    router: &mut Router,
    timestamp: MonotonicInstant,
    budget: usize,
    round_started_at: MonotonicInstant,
) -> TxDispatchProgress {
    let capacity_before = router.available_tx_packet_slots();
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

    let capacity_poll =
        (router.available_tx_packet_slots() > capacity_before).then(|| router.tx_capacity_poll());
    TxDispatchProgress {
        packet_count: work_done,
        has_reached_time_limit,
        capacity_poll,
    }
}

fn wake_tx_capacity_waiters(capacity_poll: Option<PollSet>) {
    if let Some(capacity_poll) = capacity_poll {
        capacity_poll.wake();
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

fn ipv4_snapshots_for_device(router: &Router, dev: NetDeviceId) -> Vec<Ipv4AddrSnapshot> {
    router
        .ipv4_addr_snapshots()
        .into_iter()
        .filter(|snapshot| snapshot.dev == dev)
        .collect()
}

/// Interleaves an address transition with the routes derived from it.
///
/// Linux sends the address notification first, then the FIB notifier removes
/// prefix routes for a deleted address and adds them for a new address.  A
/// replacement therefore removes the old address and its routes before adding
/// the new address and its routes.
fn ipv4_transition_events(
    before_addrs: Vec<Ipv4AddrSnapshot>,
    after_addrs: Vec<Ipv4AddrSnapshot>,
    removed_sources: &[Ipv4Address],
    routes_before: Vec<Rule>,
    routes_after: &[Rule],
) -> Vec<NetworkEvent> {
    let mut added_routes = routes_after.to_vec();
    let mut removed_routes = Vec::new();
    for route in routes_before {
        if let Some(index) = added_routes
            .iter()
            .position(|candidate| *candidate == route)
        {
            added_routes.remove(index);
        } else {
            removed_routes.push(route);
        }
    }

    // Preserve the cascade order reported by the address set: explicitly
    // removed sources first, in their removal order, followed by any other
    // address that disappeared.
    let mut removed = Vec::new();
    for source in removed_sources {
        if let Some(snapshot) = before_addrs.iter().find(|snapshot| {
            snapshot.entry.addr.address() == *source && !after_addrs.contains(snapshot)
        }) && !removed
            .iter()
            .any(|existing: &Ipv4AddrSnapshot| existing.entry.addr == snapshot.entry.addr)
        {
            removed.push(snapshot.clone());
        }
    }
    for snapshot in &before_addrs {
        if !after_addrs.contains(snapshot)
            && !removed
                .iter()
                .any(|removed: &Ipv4AddrSnapshot| removed.entry.addr == snapshot.entry.addr)
        {
            removed.push(snapshot.clone());
        }
    }

    let take_routes = |routes: &mut Vec<Rule>, snapshot: &Ipv4AddrSnapshot, deleting: bool| {
        let mut owned = Vec::new();
        let mut index = 0;
        while index < routes.len() {
            if routes[index].is_derived_from_address(snapshot.dev, snapshot.entry) {
                owned.push(routes.remove(index));
            } else {
                index += 1;
            }
        }
        // The snapshot's storage order is a lookup concern, not an
        // announcement order.  Linux installs and removes the same set of
        // derived routes in different sequences; apply the matching one.
        owned.sort_by_key(|route| {
            route
                .address_route_transition_rank(snapshot.entry, deleting)
                .unwrap_or(u8::MAX)
        });
        owned
    };

    // Linux sends the address notification first, then the FIB notifier
    // removes or adds the routes derived from that exact address before the
    // next address is processed.  Do not batch all address events ahead of
    // the route events: listeners observe a per-address transition.
    let mut events = Vec::new();
    for snapshot in removed {
        events.push(NetworkEvent::AddrRemoved {
            addr: snapshot.clone(),
        });
        events.extend(
            take_routes(&mut removed_routes, &snapshot, true)
                .into_iter()
                .map(|route| NetworkEvent::RouteRemoved {
                    route,
                    provenance: RouteProvenance::Kernel,
                }),
        );
    }
    for snapshot in after_addrs
        .into_iter()
        .filter(|snapshot| !before_addrs.contains(snapshot))
    {
        events.push(NetworkEvent::AddrAdded {
            addr: snapshot.clone(),
        });
        events.extend(
            take_routes(&mut added_routes, &snapshot, false)
                .into_iter()
                .map(|route| NetworkEvent::RouteAdded {
                    route,
                    provenance: RouteProvenance::Kernel,
                }),
        );
    }
    // Defensive fallback for route-only changes that cannot be attributed to
    // an address transition (for example a link-state-driven FIB update).
    events.extend(
        removed_routes
            .into_iter()
            .map(|route| NetworkEvent::RouteRemoved {
                route,
                provenance: RouteProvenance::Kernel,
            }),
    );
    events.extend(
        added_routes
            .into_iter()
            .map(|route| NetworkEvent::RouteAdded {
                route,
                provenance: RouteProvenance::Kernel,
            }),
    );
    events
}

unittest::path_mod_test!(tests, "../../ut-tests/runtime/service/tests.rs");
