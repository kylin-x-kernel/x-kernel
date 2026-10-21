// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Weak, task::Wake, vec, vec::Vec};
use core::{sync::atomic::AtomicBool, task::Context};

use kpoll::PollRegistrations;
use smoltcp::iface::SocketSet;
use unittest::{assert, def_test};

use super::*;
use crate::{
    buf::PacketOwner,
    device::LoopbackDevice,
    ip::{Ipv4Address, Ipv4Cidr},
    ipv4,
    network::{interface_address::Ipv4AddrEntry, router::ROUTE_SCOPE_HOST},
};

const TEST_POLL_BUDGET: PollBudget = PollBudget {
    rx_packets: 1,
    tx_packets: 1,
    timer_events: 1,
};

struct TxCapacityWakeProbe {
    service: Weak<Service>,
    wake_count: AtomicUsize,
    was_router_unlocked: AtomicBool,
}

impl Wake for TxCapacityWakeProbe {
    fn wake(self: Arc<Self>) {
        let was_unlocked = self
            .service
            .upgrade()
            .is_some_and(|service| service.router.try_lock().is_some());
        self.was_router_unlocked
            .store(was_unlocked, Ordering::Relaxed);
        self.wake_count.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn tx_capacity_wakes_after_router_unlock_before_deferred_return() {
    let mut router = Router::new();
    let mut device = router.stack_device();
    let token =
        smoltcp::phy::Device::transmit(&mut device, SmoltcpInstant::from_millis(0)).unwrap();
    smoltcp::phy::TxToken::consume(token, 20, |bytes| bytes.fill(0));
    let service = Arc::new(Service::new(router));
    let probe = Arc::new(TxCapacityWakeProbe {
        service: Arc::downgrade(&service),
        wake_count: AtomicUsize::new(0),
        was_router_unlocked: AtomicBool::new(false),
    });
    let waker = Waker::from(probe.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    service
        .register_tx_waker(&mut registrations.context(&context))
        .unwrap();

    // Force the round to return before protocol processing, after TX.
    let _iface = service.iface.lock();
    let progress = service.poll_budgeted(TEST_POLL_BUDGET);

    assert_eq!(progress.tx_packets, 1);
    assert!(progress.has_more);
    assert_eq!(probe.wake_count.load(Ordering::Relaxed), 1);
    assert!(probe.was_router_unlocked.load(Ordering::Relaxed));
}

#[def_test]
fn protocol_deadline_update_preserves_ipv4_reassembly_timer_source() {
    let service = Service::new(Router::new());
    let reassembly_deadline = monotonic_time() + TimeSpan::from_secs(30);
    service.update_ipv4_reassembly_timeout(Some(reassembly_deadline));
    let reassembly_state = service
        .ipv4_reassembly_deadline_micros
        .load(Ordering::Acquire);

    service.update_poll_timeout(now(), None);

    assert_eq!(
        service
            .ipv4_reassembly_deadline_micros
            .load(Ordering::Acquire),
        reassembly_state
    );
}

#[def_test]
fn unrelated_ingress_lock_contention_is_not_ipv4_timer_work() {
    let service = Service::new(Router::new());
    let _ingress = service.ingress.lock();
    let mut control_packets = Vec::new();

    let progress = service.try_expire_ipv4_fragments(
        monotonic_time(),
        TEST_POLL_BUDGET.timer_events,
        &mut control_packets,
    );

    assert_eq!(progress.timer_events, 0);
    assert!(!progress.has_more);
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
    let mut iface = Interface::new(config, &mut router.stack_device(), current);
    let mut packets = Vec::from([
        PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx),
        PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx),
    ]);
    router.enqueue_ingress_packets(&mut packets);
    let mut sockets = SocketSet::new(vec![]);

    let progress = poll_smoltcp_ingress_budgeted(
        &mut iface,
        current,
        &mut router.stack_device(),
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
fn poll_round_dispatches_tx_when_due_ipv4_expiration_lock_is_contended() {
    let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
    let mut router = Router::new();
    let loopback = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: loopback,
            addr: Ipv4Cidr::new(loopback_addr, 8).unwrap(),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        })
        .unwrap();
    let packet =
        ipv4::build_ipv4_packet(loopback_addr, loopback_addr, ipv4::PROTOCOL_ICMP, 64, &[])
            .unwrap();
    router
        .queue_ipv4_packet(packet, loopback.ifindex())
        .unwrap();
    let service = Service::new(router);
    service
        .ipv4_reassembly_deadline_micros
        .store(IPV4_REASSEMBLY_TIMER_PENDING, Ordering::Release);
    let _ingress = service.ingress.lock();

    let progress = service.poll_budgeted(TEST_POLL_BUDGET);

    assert_eq!(progress.tx_packets, 1);
    assert!(progress.has_more);
}

#[def_test(serial)]
fn tx_dispatch_exposes_loopback_rx_in_the_same_round() {
    let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
    let mut router = Router::new();
    let loopback = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: loopback,
            addr: Ipv4Cidr::new(loopback_addr, 8).unwrap(),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        })
        .unwrap();
    let packet =
        ipv4::build_ipv4_packet(loopback_addr, loopback_addr, ipv4::PROTOCOL_ICMP, 64, &[])
            .unwrap();
    router
        .queue_ipv4_packet(packet, loopback.ifindex())
        .unwrap();
    let round_started_at = monotonic_time();

    let tx_progress = dispatch_tx_budgeted(
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

    assert_eq!(tx_progress.packet_count, 1);
    assert!(tx_progress.capacity_poll.is_some());
    assert_eq!(rx_progress.work_done, 1);
}
