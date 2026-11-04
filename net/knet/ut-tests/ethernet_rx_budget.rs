// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use driver_net::{NetBuf, NetBufPool};
use kdevice::{DeviceId, DeviceObject};
use ksync::Mutex;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::router::{NeighborUpdatePolicy, Router};

const LOCAL_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];

struct RxState {
    frames: Mutex<VecDeque<Vec<u8>>>,
    tx_frames: Mutex<Vec<Vec<u8>>>,
    received: AtomicUsize,
    recycled: AtomicUsize,
    has_recycle_error: AtomicBool,
}

struct TestNic {
    state: Arc<RxState>,
    pool: Arc<NetBufPool>,
}

impl Device for TestNic {
    fn name(&self) -> &str {
        "rx-budget-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }
}

impl NetDevice for TestNic {
    fn mac(&self) -> driver_net::MacAddress {
        driver_net::MacAddress(LOCAL_MAC)
    }

    fn can_tx(&self) -> bool {
        true
    }

    fn can_rx(&self) -> bool {
        !self.state.frames.lock().is_empty()
    }

    fn rx_queue_len(&self) -> usize {
        self.state.frames.lock().len()
    }

    fn tx_queue_len(&self) -> usize {
        64
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        let frame = self
            .state
            .frames
            .lock()
            .pop_front()
            .ok_or(DriverError::WouldBlock)?;
        let mut buffer = self.pool.alloc_boxed().unwrap();
        buffer.set_payload_len(frame.len()).unwrap();
        buffer.payload_mut().copy_from_slice(&frame);
        self.state.received.fetch_add(1, Ordering::Relaxed);
        Ok(buffer.into_handle())
    }

    fn recycle_rx(&self, buffer: NetBufHandle) -> DriverResult {
        // SAFETY: recv creates every RX handle with NetBuf::into_handle and
        // transfers it only to the Ethernet poll, which returns it exactly once.
        drop(unsafe { NetBuf::from_handle(buffer) });
        self.state.recycled.fetch_add(1, Ordering::Relaxed);
        if self.state.has_recycle_error.load(Ordering::Relaxed) {
            Err(DriverError::Io)
        } else {
            Ok(())
        }
    }

    fn recycle_tx(&self) -> DriverResult {
        Ok(())
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
        let mut buffer = self.pool.alloc_boxed().expect("test TX buffer");
        buffer
            .set_payload_len(size)
            .expect("test TX payload length");
        Ok(buffer.into_handle())
    }

    fn send(&self, buffer: NetBufHandle) -> DriverResult {
        // SAFETY: alloc_tx_buf creates every TX handle with
        // NetBuf::into_handle and transfers ownership here exactly once.
        let buffer = unsafe { NetBuf::from_handle(buffer) };
        let frame = buffer.payload().to_vec();
        drop(buffer);
        self.state.tx_frames.lock().push(frame);
        Ok(())
    }
}

fn device(frames: impl IntoIterator<Item = Vec<u8>>) -> (EthernetDevice, Arc<RxState>) {
    let state = Arc::new(RxState {
        frames: Mutex::new(frames.into_iter().collect()),
        tx_frames: Mutex::new(Vec::new()),
        received: AtomicUsize::new(0),
        recycled: AtomicUsize::new(0),
        has_recycle_error: AtomicBool::new(false),
    });
    // Keep the synthetic device and RX source local: no registry, softirq
    // action, or scheduler is installed by this fixture.
    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(1),
        DeviceKind::Net,
    ));
    let runtime: NetDeviceImpl = Box::new(TestNic {
        state: state.clone(),
        pool: NetBufPool::new(8, 2048).unwrap(),
    });
    let inner = ClassDevice::try_new_with_info(
        parent,
        runtime,
        "rx-budget-test".into(),
        DeviceKind::Net,
        None,
    )
    .unwrap();
    (
        EthernetDevice {
            name: "rx-budget-test".into(),
            flags: LINK_FLAG_UP,
            mtu: crate::consts::STANDARD_MTU,
            operstate: IF_OPER_UP,
            inner,
            neighbors: HashMap::new(),
            next_neighbor_scan: None,
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
            pending_tx: VecDeque::new(),
            pending_neighbor_events: Vec::new(),
            rx_source: None,
        },
        state,
    )
}

fn frame(dst: [u8; 6], protocol: EtherType) -> Vec<u8> {
    let mut frame = alloc::vec![0; ETHERNET_HEADER_LEN + 28];
    emit_ethernet_header(
        &mut frame,
        MacAddress(dst),
        MacAddress([2, 0, 0, 0, 0, 2]),
        protocol,
    )
    .unwrap();
    frame
}

fn non_ip_frames() -> Vec<Vec<u8>> {
    let mut arp = frame([0xff; 6], EtherType::Arp);
    ArpIpv4Packet {
        operation: ArpOperation::Request,
        source_hardware_addr: MacAddress([2, 0, 0, 0, 0, 2]),
        source_protocol_addr: crate::ip::Ipv4Address::new(192, 0, 2, 2),
        target_hardware_addr: MacAddress::ZERO,
        target_protocol_addr: crate::ip::Ipv4Address::new(192, 0, 2, 1),
    }
    .emit(&mut arp[ETHERNET_HEADER_LEN..])
    .unwrap();
    alloc::vec![
        alloc::vec![0; ETHERNET_HEADER_LEN - 1],
        arp,
        frame([2, 0, 0, 0, 0, 3], EtherType::Ipv4),
        frame(LOCAL_MAC, EtherType::Ipv6),
    ]
}

#[def_test(serial)]
fn non_ip_frames_consume_rx_budget_and_recycle_before_returning() {
    let mut frames = non_ip_frames();
    let non_ip_count = frames.len();
    frames.push(frame(LOCAL_MAC, EtherType::Ipv4));
    let (ethernet, state) = device(frames);
    let mut router = Router::new();
    router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    let mut packets = Vec::new();

    for consumed in 1..=non_ip_count {
        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
        assert_eq!(drain.work_done, 1);
        assert!(packets.is_empty());
        assert!(drain.has_more);
        assert_eq!(drain.error, None);
        assert_eq!(state.received.load(Ordering::Relaxed), consumed);
        assert_eq!(state.recycled.load(Ordering::Relaxed), consumed);
    }

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert!(!drain.has_more);
    assert_eq!(drain.error, None);
    assert_eq!(state.recycled.load(Ordering::Relaxed), non_ip_count + 1);

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(drain.work_done, 0);
    assert!(packets.is_empty());
    assert!(!drain.has_more);
}

#[def_test(serial)]
fn dropped_frames_exhaust_budget_and_rotate_to_another_device() {
    let mut router = Router::new();
    let (first, first_state) = device(non_ip_frames());
    let (second, second_state) = device([frame(LOCAL_MAC, EtherType::Ipv4)]);
    router.add_device(kalloc_types::KBox::new(first).unwrap()).unwrap();
    router.add_device(kalloc_types::KBox::new(second).unwrap()).unwrap();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 2, &mut packets);
    assert_eq!(drain.work_done, 2);
    assert!(packets.is_empty());
    assert!(drain.has_more);
    assert_eq!(first_state.received.load(Ordering::Relaxed), 2);
    assert_eq!(second_state.received.load(Ordering::Relaxed), 0);

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert!(drain.has_more);
    assert_eq!(packets[0].ifindex(), 3);
    assert_eq!(first_state.received.load(Ordering::Relaxed), 2);
    assert_eq!(second_state.recycled.load(Ordering::Relaxed), 1);

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 2, &mut packets);
    assert_eq!(drain.work_done, 2);
    assert!(packets.is_empty());
    assert!(!drain.has_more);
    assert_eq!(first_state.recycled.load(Ordering::Relaxed), 4);
}

#[def_test(serial)]
fn zero_rx_budget_does_not_consume_an_ethernet_frame() {
    let (ethernet, state) = device(non_ip_frames());
    let mut router = Router::new();
    router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 0, &mut packets);
    assert_eq!(drain.work_done, 0);
    assert!(drain.has_more);
    assert!(packets.is_empty());
    assert_eq!(state.received.load(Ordering::Relaxed), 0);
    assert_eq!(state.recycled.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn recycling_failure_ends_rx_without_reading_another_frame() {
    let (ethernet, state) = device([
        frame(LOCAL_MAC, EtherType::Ipv4),
        frame(LOCAL_MAC, EtherType::Ipv4),
    ]);
    state.has_recycle_error.store(true, Ordering::Relaxed);
    let mut router = Router::new();
    router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
    assert_eq!(drain.work_done, 0);
    assert!(packets.is_empty());
    assert_eq!(drain.error, Some((2, KError::Io)));
    assert!(drain.has_more);
    assert_eq!(state.received.load(Ordering::Relaxed), 1);
    assert_eq!(state.recycled.load(Ordering::Relaxed), 1);

    state.has_recycle_error.store(false, Ordering::Relaxed);
    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert_eq!(drain.error, None);
    assert!(!drain.has_more);
    assert_eq!(state.recycled.load(Ordering::Relaxed), 2);
}

fn arp_reply_to(
    ethernet_dst: [u8; 6],
    source_mac: [u8; 6],
    source_ip: crate::ip::Ipv4Address,
) -> Vec<u8> {
    let mut arp = frame(ethernet_dst, EtherType::Arp);
    ArpIpv4Packet {
        operation: ArpOperation::Reply,
        source_hardware_addr: MacAddress(source_mac),
        source_protocol_addr: source_ip,
        target_hardware_addr: MacAddress(LOCAL_MAC),
        target_protocol_addr: crate::ip::Ipv4Address::new(192, 0, 2, 1),
    }
    .emit(&mut arp[ETHERNET_HEADER_LEN..])
    .unwrap();
    arp
}

fn arp_reply(source_mac: [u8; 6], source_ip: crate::ip::Ipv4Address) -> Vec<u8> {
    arp_reply_to(LOCAL_MAC, source_mac, source_ip)
}

fn arp_request(source_mac: [u8; 6], source_ip: crate::ip::Ipv4Address) -> Vec<u8> {
    let mut arp = frame(LOCAL_MAC, EtherType::Arp);
    ArpIpv4Packet {
        operation: ArpOperation::Request,
        source_hardware_addr: MacAddress(source_mac),
        source_protocol_addr: source_ip,
        target_hardware_addr: MacAddress::ZERO,
        target_protocol_addr: crate::ip::Ipv4Address::new(192, 0, 2, 1),
    }
    .emit(&mut arp[ETHERNET_HEADER_LEN..])
    .unwrap();
    arp
}

fn gratuitous_arp(source_mac: [u8; 6], address: crate::ip::Ipv4Address) -> Vec<u8> {
    let mut arp = frame([0xff; 6], EtherType::Arp);
    ArpIpv4Packet {
        operation: ArpOperation::Reply,
        source_hardware_addr: MacAddress(source_mac),
        source_protocol_addr: address,
        target_hardware_addr: MacAddress(source_mac),
        target_protocol_addr: address,
    }
    .emit(&mut arp[ETHERNET_HEADER_LEN..])
    .unwrap();
    arp
}

fn tx_arp_requests(state: &RxState) -> usize {
    state
        .tx_frames
        .lock()
        .iter()
        .filter(|frame| {
            frame
                .get(ETHERNET_HEADER_LEN..)
                .and_then(ArpIpv4Packet::parse)
                .is_some_and(|arp| arp.operation == ArpOperation::Request)
        })
        .count()
}

fn send_test_ipv4(
    router: &mut Router,
    dev: NetDeviceId,
    destination: crate::ip::Ipv4Address,
    source: crate::ip::Ipv4Address,
    now: MonotonicInstant,
) {
    let packet = PacketBuf::from_ip_packet_vec(
        dev.ifindex(),
        alloc::vec![0x45, 0, 0, 20],
        PacketOwner::Ipv4Stack,
    );
    let _rx_ready = router.devices.get_mut_by_id(dev).unwrap().send_ip_packet(
        dev.ifindex(),
        IpAddress::Ipv4(destination),
        IpAddress::Ipv4(source),
        packet,
        now,
    );
}

#[def_test(serial)]
fn unresolved_neighbor_sends_three_solicits_then_fails() {
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    send_test_ipv4(&mut router, dev, remote, local, MonotonicInstant::ORIGIN);
    assert_eq!(tx_arp_requests(&state), 1);
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(MonotonicInstant::ORIGIN + TimeSpan::from_secs(1))
    );
    let mut packets = Vec::new();
    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Incomplete,
        })]
    );

    // Polling before the retransmit deadline must not rescan the table or
    // send another solicitation.
    for elapsed_ms in [100, 500, 900] {
        router.drain_rx_budgeted_into(
            MonotonicInstant::ORIGIN + TimeSpan::from_millis(elapsed_ms),
            1,
            &mut packets,
        );
        assert_eq!(tx_arp_requests(&state), 1);
        assert_eq!(
            router.next_neighbor_deadline(),
            Some(MonotonicInstant::ORIGIN + TimeSpan::from_secs(1))
        );
        assert!(router.take_neighbor_events().is_empty());
    }

    for elapsed in 1..=2 {
        router.drain_rx_budgeted_into(
            MonotonicInstant::ORIGIN + TimeSpan::from_secs(elapsed),
            1,
            &mut packets,
        );
        assert_eq!(tx_arp_requests(&state), elapsed as usize + 1);
        assert_eq!(
            router.next_neighbor_deadline(),
            Some(MonotonicInstant::ORIGIN + TimeSpan::from_secs(elapsed + 1))
        );
        assert!(router.take_neighbor_events().is_empty());
    }

    // The third solicitation is the last one.  Linux then enters
    // NUD_FAILED, keeps the entry, and publishes RTM_NEWNEIGH rather than
    // deleting it.  Packets queued behind the unresolved next hop are dropped.
    let failed_at = MonotonicInstant::ORIGIN + TimeSpan::from_secs(3);
    router.drain_rx_budgeted_into(failed_at, 1, &mut packets);
    assert_eq!(tx_arp_requests(&state), 3);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        })]
    );
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        }]
    );
    // A failed entry has no probe timer; it is reclaimed after the neighbor
    // TTL, which also keeps the poll deadline bounded.
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(failed_at + EthernetDevice::NEIGHBOR_TTL)
    );

    // The failed entry is not deleted immediately.  Its TTL expiry publishes
    // RTM_DELNEIGH and only then removes it from the table.
    router.drain_rx_budgeted_into(failed_at + EthernetDevice::NEIGHBOR_TTL, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Removed(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        })]
    );
    assert!(router.neighbor_snapshots().is_empty());
    assert_eq!(router.next_neighbor_deadline(), None);
}

#[def_test(serial)]
fn failed_neighbor_restarts_resolution_on_traffic() {
    let hardware = [2, 0, 0, 0, 0, 9];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    send_test_ipv4(&mut router, dev, remote, local, MonotonicInstant::ORIGIN);
    let mut packets = Vec::new();
    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    let _ = router.take_neighbor_events();

    for elapsed in 1..=2 {
        router.drain_rx_budgeted_into(
            MonotonicInstant::ORIGIN + TimeSpan::from_secs(elapsed),
            1,
            &mut packets,
        );
        assert!(router.take_neighbor_events().is_empty());
    }
    let failed_at = MonotonicInstant::ORIGIN + TimeSpan::from_secs(3);
    router.drain_rx_budgeted_into(failed_at, 1, &mut packets);
    assert_eq!(tx_arp_requests(&state), 3);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        })]
    );

    // Linux keeps the failed entry and starts a fresh INCOMPLETE resolution
    // when traffic arrives, rather than creating a second table entry or
    // waiting for the TTL to elapse.
    let restarted_at = failed_at + TimeSpan::from_secs(1);
    send_test_ipv4(&mut router, dev, remote, local, restarted_at);
    assert_eq!(tx_arp_requests(&state), 4);
    router.drain_rx_budgeted_into(restarted_at, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Incomplete,
        })]
    );
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(restarted_at + EthernetDevice::NEIGHBOR_RETRANS)
    );

    // The packet queued during the failed state is sent only after ARP
    // confirms the address again.
    state.frames.lock().push_back(arp_reply(hardware, remote));
    router.drain_rx_budgeted_into(restarted_at + TimeSpan::from_millis(100), 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Reachable {
                hardware_addr: hardware,
            },
        })]
    );
    let frames = state.tx_frames.lock();
    assert_eq!(frames.len(), 5);
    assert_eq!(&frames[4][..6], &hardware);
}

#[def_test(serial)]
fn arp_reply_resolves_incomplete_neighbor_inside_locktime() {
    let remote_mac = [2, 0, 0, 0, 0, 9];
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();

    send_test_ipv4(&mut router, dev, remote, local, MonotonicInstant::ORIGIN);
    let mut packets = Vec::new();
    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Incomplete,
        })]
    );

    // The LOCKTIME window protects a valid binding from conflicting updates;
    // it must not prevent the first reply from resolving an INCOMPLETE entry.
    state.frames.lock().push_back(arp_reply(remote_mac, remote));
    let drain = router.drain_rx_budgeted_into(
        MonotonicInstant::ORIGIN + TimeSpan::from_millis(100),
        1,
        &mut packets,
    );
    assert_eq!(drain.work_done, 1);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Reachable {
                hardware_addr: remote_mac,
            },
        })]
    );
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Reachable {
                hardware_addr: remote_mac,
            },
        }]
    );
    assert_eq!(tx_arp_requests(&state), 1);
    let frames = state.tx_frames.lock();
    assert_eq!(frames.len(), 2);
    assert_eq!(&frames[1][..6], &remote_mac);
}

#[def_test(serial)]
fn link_down_freezes_neighbor_probes_and_arp_learning() {
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    send_test_ipv4(&mut router, dev, remote, local, MonotonicInstant::ORIGIN);
    let mut packets = Vec::new();
    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(tx_arp_requests(&state), 1);
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(MonotonicInstant::ORIGIN + TimeSpan::from_secs(1))
    );
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Incomplete,
        })]
    );

    // A down link must not process queued ARP frames or advance solicitation
    // timers.  The entry stays incomplete until the link comes back.
    router
        .devices
        .get_mut_by_id(dev)
        .unwrap()
        .set_link_up(false);
    // Reporting a deadline here would make the Service timer expire
    // immediately on every poll while the device cannot service it.
    assert_eq!(router.next_neighbor_deadline(), None);
    state
        .frames
        .lock()
        .push_back(arp_reply([2, 0, 0, 0, 0, 9], remote));
    // A queued frame on a down link is not pollable work. Reporting it would
    // make the Service report `has_more` and the poller requeue itself without
    // consuming the frame.
    assert!(!router.has_immediate_rx_work());

    let drain = router.drain_rx_budgeted_into(
        MonotonicInstant::ORIGIN + TimeSpan::from_secs(10),
        4,
        &mut packets,
    );
    assert_eq!(drain.work_done, 0);
    assert!(drain.error.is_none());
    assert!(packets.is_empty());
    assert_eq!(tx_arp_requests(&state), 1);
    assert_eq!(router.next_neighbor_deadline(), None);
    assert!(router.take_neighbor_events().is_empty());
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Incomplete,
        }]
    );

    // The frozen deadline becomes serviceable again when the link returns.
    router.devices.get_mut_by_id(dev).unwrap().set_link_up(true);
    assert!(router.has_immediate_rx_work());
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(MonotonicInstant::ORIGIN + TimeSpan::from_secs(1))
    );
}

#[def_test(serial)]
fn valid_neighbor_expiry_retains_hardware_and_goes_stale() {
    let hardware = [2, 0, 0, 0, 0, 2];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(remote),
                state: NeighborState::Reachable {
                    hardware_addr: hardware,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let _ = router.take_neighbor_events();
    // `apply_neighbor_update` seeds timestamps from the monotonic clock, so
    // derive the expiry instant from the same clock rather than ORIGIN.
    let expired_at =
        khal::time::monotonic_time() + EthernetDevice::NEIGHBOR_TTL + TimeSpan::from_secs(1);

    send_test_ipv4(&mut router, dev, remote, local, expired_at);
    let mut packets = Vec::new();
    router.drain_rx_budgeted_into(expired_at, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Stale {
                hardware_addr: hardware,
            },
        })]
    );
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Stale {
                hardware_addr: hardware,
            },
        }]
    );
    // A probe is sent while the still-valid hardware address continues to be
    // used for the queued data packet.  The MAC is never discarded and the
    // entry never becomes INCOMPLETE.
    let frames = state.tx_frames.lock();
    assert_eq!(frames.len(), 2);
    assert!(ArpIpv4Packet::parse(&frames[0][ETHERNET_HEADER_LEN..]).is_some());
    assert_eq!(&frames[1][..6], &hardware);
    drop(frames);

    // The probe is retried at the retransmit deadline.  Two silent retries
    // follow the initial solicitation; the fourth deadline exhausts the
    // budget and moves the retained entry to NUD_FAILED.
    let mut packets = Vec::new();
    for (elapsed, expected_requests) in [(1, 2usize), (2, 3)] {
        router.drain_rx_budgeted_into(expired_at + TimeSpan::from_secs(elapsed), 1, &mut packets);
        assert_eq!(tx_arp_requests(&state), expected_requests);
        assert!(router.take_neighbor_events().is_empty());
    }
    let failed_at = expired_at + TimeSpan::from_secs(3);
    router.drain_rx_budgeted_into(failed_at, 1, &mut packets);
    assert_eq!(tx_arp_requests(&state), 3);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        })]
    );
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(failed_at + EthernetDevice::NEIGHBOR_TTL)
    );
}

#[def_test(serial)]
fn administrative_reachable_refreshes_connected_lifetime() {
    let hardware = [2, 0, 0, 0, 0, 9];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, _) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    let update = NeighborUpdate {
        dev,
        dst: IpAddress::Ipv4(remote),
        state: NeighborState::Reachable {
            hardware_addr: hardware,
        },
    };
    let policy = NeighborUpdatePolicy {
        can_create: true,
        exclusive: false,
        can_override: true,
    };
    router.apply_neighbor_update(update, policy).unwrap();
    let _ = router.take_neighbor_events();
    let first = router
        .next_neighbor_deadline()
        .expect("reachable state must have a lifetime deadline");

    // Linux refreshes `confirmed` for every update that leaves the entry in
    // NUD_CONNECTED, even when the administrative state is unchanged.
    ktask::sleep(ktime_types::TimeSpan::from_millis(2));
    router.apply_neighbor_update(update, policy).unwrap();
    assert!(router.take_neighbor_events().is_empty());
    let second = router
        .next_neighbor_deadline()
        .expect("refresh must keep the deadline");
    assert!(
        second > first,
        "identical REACHABLE update must extend the lifetime: {first:?} -> {second:?}"
    );
}

#[def_test(serial)]
fn administrative_reachable_drains_pending_tx() {
    let hardware = [2, 0, 0, 0, 0, 9];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    send_test_ipv4(&mut router, dev, remote, local, MonotonicInstant::ORIGIN);
    assert_eq!(tx_arp_requests(&state), 1);
    assert_eq!(state.tx_frames.lock().len(), 1);
    let _ = router.take_neighbor_events();

    // A control-plane install resolves the next hop just like an ARP reply;
    // the frame queued behind the incomplete entry must be transmitted.
    let event = router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(remote),
                state: NeighborState::Reachable {
                    hardware_addr: hardware,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    assert!(matches!(
        event,
        Some(crate::network::event::NetworkEvent::NeighborAdded { neighbor })
            if matches!(neighbor.state, NeighborState::Reachable { .. })
    ));
    let frames = state.tx_frames.lock();
    assert_eq!(frames.len(), 2);
    assert_eq!(&frames[1][..6], &hardware);
}

#[def_test(serial)]
fn arp_local_requests_and_garp_override_locktime() {
    let first_mac = [2, 0, 0, 0, 0, 2];
    let second_mac = [2, 0, 0, 0, 0, 3];
    let third_mac = [2, 0, 0, 0, 0, 4];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 2);
    let (ethernet, _) = device([
        arp_request(second_mac, remote),
        gratuitous_arp(third_mac, remote),
    ]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(remote),
                state: NeighborState::Reachable {
                    hardware_addr: first_mac,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let _ = router.take_neighbor_events();
    let mut packets = Vec::new();

    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Stale {
                hardware_addr: second_mac,
            },
        })]
    );
    router.drain_rx_budgeted_into(
        MonotonicInstant::ORIGIN + TimeSpan::from_millis(200),
        1,
        &mut packets,
    );
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Stale {
                hardware_addr: third_mac,
            },
        })]
    );
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Stale {
                hardware_addr: third_mac,
            },
        }]
    );
}

#[def_test(serial)]
fn gratuitous_arp_from_directed_broadcast_does_not_override_locktime() {
    let first_mac = [2, 0, 0, 0, 0, 2];
    let second_mac = [2, 0, 0, 0, 0, 3];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let directed_broadcast = crate::ip::Ipv4Address::new(192, 0, 2, 255);
    let (ethernet, _) = device([gratuitous_arp(second_mac, directed_broadcast)]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(directed_broadcast),
                state: NeighborState::Reachable {
                    hardware_addr: first_mac,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let _ = router.take_neighbor_events();
    let mut packets = Vec::new();

    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

    // `arp_is_garp()` requires the sender address to classify as
    // RTN_UNICAST.  A subnet-directed broadcast is RTN_BROADCAST, so it must
    // not bypass the valid-entry locktime even though sip == tip.
    assert!(router.take_neighbor_events().is_empty());
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(directed_broadcast),
            state: NeighborState::Reachable {
                hardware_addr: first_mac,
            },
        }]
    );
}

#[def_test(serial)]
fn remote_gratuitous_arp_does_not_create_a_neighbor() {
    let remote_mac = [2, 0, 0, 0, 0, 2];
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, _) = device([gratuitous_arp(remote_mac, remote)]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    let mut packets = Vec::new();

    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    // Linux has arp_accept=0 by default: arbitrary gratuitous ARP can refresh
    // an existing binding, but it must not populate an empty neighbor table.
    assert!(router.take_neighbor_events().is_empty());
    assert!(router.neighbor_snapshots().is_empty());
}

#[def_test(serial)]
fn arp_request_learning_is_stale() {
    let source_mac = [2, 0, 0, 0, 0, 2];
    let source_ip = crate::ip::Ipv4Address::new(192, 0, 2, 2);
    let (ethernet, _) = device([arp_request(source_mac, source_ip)]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(
            dev,
            Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        )
        .unwrap();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Stale {
                hardware_addr: source_mac,
            },
        })]
    );
}

#[def_test(serial)]
fn arp_learning_retains_first_binding_for_locktime() {
    let first_mac = [2, 0, 0, 0, 0, 2];
    let second_mac = [2, 0, 0, 0, 0, 3];
    let source_ip = crate::ip::Ipv4Address::new(192, 0, 2, 2);
    let (ethernet, _) = device([
        arp_reply(first_mac, source_ip),
        arp_reply(second_mac, source_ip),
    ]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(
            dev,
            Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        )
        .unwrap();
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(source_ip),
                state: NeighborState::Reachable {
                    hardware_addr: first_mac,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap()
        .expect("seeding a new neighbor publishes one event");
    let mut packets = Vec::new();

    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert!(router.take_neighbor_events().is_empty());
    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert!(router.take_neighbor_events().is_empty());
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Reachable {
                hardware_addr: first_mac,
            },
        }]
    );
}

#[def_test(serial)]
fn arp_learning_publishes_one_reachable_transition() {
    let source_mac = [2, 0, 0, 0, 0, 2];
    let source_ip = crate::ip::Ipv4Address::new(192, 0, 2, 2);
    let reply = arp_reply(source_mac, source_ip);
    let (ethernet, _) = device([reply.clone(), reply]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(
            dev,
            Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        )
        .unwrap();
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(source_ip),
                state: NeighborState::Stale {
                    hardware_addr: source_mac,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Reachable {
                hardware_addr: source_mac,
            },
        })]
    );

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert!(router.take_neighbor_events().is_empty());
}

#[def_test(serial)]
fn repeated_reachable_arp_refreshes_lifetime_without_an_event() {
    let source_mac = [2, 0, 0, 0, 0, 2];
    let source_ip = crate::ip::Ipv4Address::new(192, 0, 2, 2);
    let (ethernet, _) = device([
        arp_reply(source_mac, source_ip),
        arp_reply(source_mac, source_ip),
    ]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(
            dev,
            Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        )
        .unwrap();
    // Seed a valid binding so the replies are updates instead of unsolicited
    // ARP learning, which `arp_accept=0` rejects for unknown neighbors.
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(source_ip),
                state: NeighborState::Stale {
                    hardware_addr: source_mac,
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let mut packets = Vec::new();
    let expected = alloc::vec![NeighborSnapshot {
        dev,
        dst: IpAddress::Ipv4(source_ip),
        state: NeighborState::Reachable {
            hardware_addr: source_mac,
        },
    }];

    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(router.take_neighbor_events().len(), 1);

    // A repeated REACHABLE observation extends `confirmed`/the reachability
    // lifetime but must not move `updated_at`, the LOCKTIME window, or emit a
    // notification.
    let refreshed_at = MonotonicInstant::ORIGIN + TimeSpan::from_secs(200);
    let drain = router.drain_rx_budgeted_into(refreshed_at, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert!(router.take_neighbor_events().is_empty());

    // Without the refresh the original entry would have expired at 300s.
    let still_reachable_at = MonotonicInstant::ORIGIN + TimeSpan::from_secs(400);
    let drain = router.drain_rx_budgeted_into(still_reachable_at, 1, &mut packets);
    assert_eq!(drain.work_done, 0);
    assert_eq!(router.neighbor_snapshots(), expected);
    assert!(router.take_neighbor_events().is_empty());

    router.drain_rx_budgeted_into(
        MonotonicInstant::ORIGIN + TimeSpan::from_secs(501),
        1,
        &mut packets,
    );
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Stale {
                hardware_addr: source_mac,
            },
        })]
    );
}

#[def_test(serial)]
fn arp_broadcast_replies_do_not_assert_reachability() {
    let source_mac = [2, 0, 0, 0, 0, 2];
    let replacement_mac = [2, 0, 0, 0, 0, 3];
    let source_ip = crate::ip::Ipv4Address::new(192, 0, 2, 2);
    let broadcast = [0xff; 6];
    let reachable_at = MonotonicInstant::ORIGIN + TimeSpan::from_secs(2);
    let broadcast_at = reachable_at + TimeSpan::from_millis(500);
    let replacement_at = reachable_at + TimeSpan::from_millis(1_500);
    let (ethernet, _) = device([
        arp_request(source_mac, source_ip),
        arp_reply_to(broadcast, source_mac, source_ip),
        arp_reply_to(LOCAL_MAC, source_mac, source_ip),
        arp_reply_to(broadcast, source_mac, source_ip),
        arp_reply_to(LOCAL_MAC, replacement_mac, source_ip),
    ]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(
            dev,
            Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        )
        .unwrap();
    let mut packets = Vec::new();

    router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Stale {
                hardware_addr: source_mac,
            },
        })]
    );

    // A broadcast reply that repeats the cached address cannot promote a
    // STALE entry to REACHABLE; only a PACKET_HOST reply can.
    router.drain_rx_budgeted_into(reachable_at, 1, &mut packets);
    assert!(router.take_neighbor_events().is_empty());
    assert_eq!(
        router.neighbor_snapshots(),
        alloc::vec![NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Stale {
                hardware_addr: source_mac,
            },
        }]
    );

    router.drain_rx_budgeted_into(reachable_at, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Reachable {
                hardware_addr: source_mac,
            },
        })]
    );

    // Same-address updates never move `updated_at`, so a later broadcast
    // reply cannot extend the locktime and suppress a newer binding.
    router.drain_rx_budgeted_into(broadcast_at, 1, &mut packets);
    assert!(router.take_neighbor_events().is_empty());
    router.drain_rx_budgeted_into(replacement_at, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(source_ip),
            state: NeighborState::Reachable {
                hardware_addr: replacement_mac,
            },
        })]
    );
}

#[def_test(serial)]
fn arp_learning_preserves_permanent_entries() {
    let dev = NetDeviceId::new(2).unwrap();
    let source_ip = IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 0, 2, 2));
    let permanent_mac = [2, 0, 0, 0, 0, 9];
    let (mut ethernet, _) = device([arp_reply(
        [2, 0, 0, 0, 0, 2],
        crate::ip::Ipv4Address::new(192, 0, 2, 2),
    )]);
    ethernet
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: source_ip,
                state: NeighborState::Permanent {
                    hardware_addr: permanent_mac,
                },
            },
            true,
        )
        .unwrap();
    // Administrative updates stamp the real monotonic clock, not the virtual
    // timestamp of the following drain.  Anchor the test to that stamp.
    let permanent_at = khal::time::monotonic_time();
    let mut router = Router::new();
    let registered = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    assert_eq!(registered, dev);
    router
        .set_primary_ipv4_addr(
            dev,
            Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        )
        .unwrap();
    let mut packets = Vec::new();

    // Process the conflicting reply after the one-second LOCKTIME window.
    // Before that point the locktime alone rejects the replacement, so the
    // permanent guard would not be exercised.
    let first_update_at = permanent_at + EthernetDevice::NEIGHBOR_LOCKTIME + TimeSpan::from_secs(1);
    let drain = router.drain_rx_budgeted_into(first_update_at, 1, &mut packets);
    assert_eq!(drain.work_done, 1);
    assert_eq!(router.next_neighbor_deadline(), None);
    assert!(router.take_neighbor_events().is_empty());
    assert_eq!(
        router
            .devices
            .get_by_id(dev)
            .unwrap()
            .neighbor_state(source_ip),
        Some(NeighborState::Permanent {
            hardware_addr: permanent_mac,
        })
    );

    // Permanent entries have no lifetime.  A later scan past the normal TTL
    // must neither remove them nor turn them stale.
    let after_ttl = first_update_at + EthernetDevice::NEIGHBOR_TTL + TimeSpan::from_secs(1);
    router.drain_rx_budgeted_into(after_ttl, 1, &mut packets);
    assert_eq!(router.next_neighbor_deadline(), None);
    assert!(router.take_neighbor_events().is_empty());
    assert_eq!(
        router
            .devices
            .get_by_id(dev)
            .unwrap()
            .neighbor_state(source_ip),
        Some(NeighborState::Permanent {
            hardware_addr: permanent_mac,
        })
    );
}

#[def_test(serial)]
fn administrative_incomplete_neighbor_bypasses_prior_scan_gate() {
    let local = crate::ip::Ipv4Address::new(192, 0, 2, 1);
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    router
        .set_primary_ipv4_addr(dev, Ipv4Cidr::new(local, 24).unwrap())
        .unwrap();
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(remote),
                state: NeighborState::Permanent {
                    hardware_addr: [2, 0, 0, 0, 0, 9],
                },
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let _ = router.take_neighbor_events();

    // Seed the scan gate far in the future by scanning a permanent-only
    // table.  That leaves no state deadline, so the Service sleeps until a
    // control-plane mutation creates one.
    let scan_seed = khal::time::monotonic_time() + TimeSpan::from_secs(3600);
    let mut packets = Vec::new();
    router.drain_rx_budgeted_into(scan_seed, 1, &mut packets);
    assert_eq!(router.next_neighbor_deadline(), None);

    // An administrative INCOMPLETE entry starts probing immediately.  The
    // insertion must reset the scan gate; otherwise the fresh state deadline
    // would be hidden behind the seeded one-hour gate.
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(remote),
                state: NeighborState::Incomplete,
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let after_insert = khal::time::monotonic_time();
    assert!(
        router
            .next_neighbor_deadline()
            .is_some_and(|deadline| deadline <= after_insert)
    );

    router.drain_rx_budgeted_into(after_insert, 1, &mut packets);
    assert_eq!(tx_arp_requests(&state), 1);
}

#[def_test(serial)]
fn source_less_incomplete_neighbor_exhausts_probe_budget() {
    let remote = crate::ip::Ipv4Address::new(192, 0, 2, 9);
    let (ethernet, state) = device([]);
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(ethernet).unwrap()).unwrap();
    // The device has no assigned or local IPv4 address, so resolution has no
    // natural ARP source.  Linux still sends source-0 probes and must consume
    // the solicitation budget instead of rescheduling forever.
    router
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: IpAddress::Ipv4(remote),
                state: NeighborState::Incomplete,
            },
            NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let inserted_at = khal::time::monotonic_time();
    let _ = router.take_neighbor_events();
    assert!(
        router
            .next_neighbor_deadline()
            .is_some_and(|deadline| deadline <= inserted_at)
    );

    let mut packets = Vec::new();
    for elapsed in 0..=2 {
        let now = inserted_at + TimeSpan::from_secs(elapsed);
        router.drain_rx_budgeted_into(now, 1, &mut packets);
        assert_eq!(tx_arp_requests(&state), elapsed as usize + 1);
        assert!(router.take_neighbor_events().is_empty());
        assert_eq!(
            router.next_neighbor_deadline(),
            Some(inserted_at + TimeSpan::from_secs(elapsed + 1))
        );
    }

    let failed_at = inserted_at + TimeSpan::from_secs(3);
    router.drain_rx_budgeted_into(failed_at, 1, &mut packets);
    assert_eq!(tx_arp_requests(&state), 3);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Added(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        })]
    );
    assert_eq!(
        router.next_neighbor_deadline(),
        Some(failed_at + EthernetDevice::NEIGHBOR_TTL)
    );

    router.drain_rx_budgeted_into(failed_at + EthernetDevice::NEIGHBOR_TTL, 1, &mut packets);
    assert_eq!(
        router.take_neighbor_events(),
        alloc::vec![NeighborEvent::Removed(NeighborSnapshot {
            dev,
            dst: IpAddress::Ipv4(remote),
            state: NeighborState::Failed,
        })]
    );
    assert_eq!(router.next_neighbor_deadline(), None);
}
