// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use driver_net::{NetBuf, NetBufPool};
use kdevice::{DeviceId, DeviceObject};
use ksync::Mutex;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::router::Router;

const LOCAL_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];

struct RxState {
    frames: Mutex<VecDeque<Vec<u8>>>,
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
        false
    }

    fn can_rx(&self) -> bool {
        !self.state.frames.lock().is_empty()
    }

    fn rx_queue_len(&self) -> usize {
        self.state.frames.lock().len()
    }

    fn tx_queue_len(&self) -> usize {
        0
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

    fn alloc_tx_buf(&self, _size: usize) -> DriverResult<NetBufHandle> {
        Err(DriverError::Unsupported)
    }

    fn send(&self, _buffer: NetBufHandle) -> DriverResult {
        panic!("the RX fixture does not allocate TX buffers")
    }
}

fn device(frames: impl IntoIterator<Item = Vec<u8>>) -> (EthernetDevice, Arc<RxState>) {
    let state = Arc::new(RxState {
        frames: Mutex::new(frames.into_iter().collect()),
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
        pool: NetBufPool::new(1, 2048).unwrap(),
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
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
            pending_tx: VecDeque::new(),
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
    router.add_device(Box::new(ethernet)).unwrap();
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
    router.add_device(Box::new(first)).unwrap();
    router.add_device(Box::new(second)).unwrap();
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
    router.add_device(Box::new(ethernet)).unwrap();
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
    router.add_device(Box::new(ethernet)).unwrap();
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
