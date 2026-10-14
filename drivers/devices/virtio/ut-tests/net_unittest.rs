// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

struct MockIrqHandle {
    id: usize,
    irq_num: usize,
    handled: bool,
    scheduler: Option<Arc<dyn NetRxScheduler>>,
}

struct MockRxScheduler {
    schedules: AtomicUsize,
}

impl NetRxScheduler for MockRxScheduler {
    fn schedule_rx(&self) {
        self.schedules.fetch_add(1, Ordering::Relaxed);
    }
}

impl VirtIoNetIrqAck for MockIrqHandle {
    fn ack_interrupt(&self) -> bool {
        self.handled
    }

    fn handle_id(&self) -> usize {
        self.id
    }

    fn irq(&self) -> usize {
        self.irq_num
    }

    fn schedule_rx(&self) {
        if let Some(scheduler) = &self.scheduler {
            scheduler.schedule_rx();
        }
    }
}

fn reset_irq_state() {
    NET_IRQ_HANDLES.lock().clear();
    REGISTERED_NET_IRQS.lock().clear();
}

fn mock_handle(id: usize, irq_num: usize) -> Arc<MockIrqHandle> {
    Arc::new(MockIrqHandle {
        id,
        irq_num,
        handled: false,
        scheduler: None,
    })
}

fn handled_mock_handle(
    id: usize,
    irq_num: usize,
    scheduler: Option<Arc<dyn NetRxScheduler>>,
) -> Arc<MockIrqHandle> {
    Arc::new(MockIrqHandle {
        id,
        irq_num,
        handled: true,
        scheduler,
    })
}

#[def_test(serial)]
fn test_unregister_removes_correct_handle() {
    reset_irq_state();

    NET_IRQ_HANDLES.lock().push(mock_handle(10, 32));
    NET_IRQ_HANDLES.lock().push(mock_handle(11, 33));
    NET_IRQ_HANDLES.lock().push(mock_handle(12, 34));
    REGISTERED_NET_IRQS.lock().insert(32);
    REGISTERED_NET_IRQS.lock().insert(33);
    REGISTERED_NET_IRQS.lock().insert(34);

    unregister_virtio_net_irq(33, 11);

    {
        let handles = NET_IRQ_HANDLES.lock();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].handle_id(), 10);
        assert_eq!(handles[1].handle_id(), 12);
    }
    assert!(!REGISTERED_NET_IRQS.lock().contains(&33));
    assert!(REGISTERED_NET_IRQS.lock().contains(&32));
    assert!(REGISTERED_NET_IRQS.lock().contains(&34));

    reset_irq_state();
}

#[def_test(serial)]
fn test_shared_irq_preserved_when_handles_remain() {
    reset_irq_state();

    NET_IRQ_HANDLES.lock().push(mock_handle(20, 40));
    NET_IRQ_HANDLES.lock().push(mock_handle(21, 40));
    REGISTERED_NET_IRQS.lock().insert(40);

    unregister_virtio_net_irq(40, 20);

    {
        let handles = NET_IRQ_HANDLES.lock();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].handle_id(), 21);
    }
    assert!(REGISTERED_NET_IRQS.lock().contains(&40));

    reset_irq_state();
}

#[def_test(serial)]
fn test_shared_irq_unregistered_when_all_handles_removed() {
    reset_irq_state();

    NET_IRQ_HANDLES.lock().push(mock_handle(30, 50));
    NET_IRQ_HANDLES.lock().push(mock_handle(31, 50));
    REGISTERED_NET_IRQS.lock().insert(50);

    unregister_virtio_net_irq(50, 30);
    assert!(REGISTERED_NET_IRQS.lock().contains(&50));

    unregister_virtio_net_irq(50, 31);
    assert!(!REGISTERED_NET_IRQS.lock().contains(&50));
    assert!(NET_IRQ_HANDLES.lock().is_empty());

    reset_irq_state();
}

#[def_test(serial)]
fn test_unregister_nonexistent_handle_is_noop() {
    reset_irq_state();

    NET_IRQ_HANDLES.lock().push(mock_handle(40, 60));
    REGISTERED_NET_IRQS.lock().insert(60);

    unregister_virtio_net_irq(60, 9999);

    assert_eq!(NET_IRQ_HANDLES.lock().len(), 1);
    assert!(REGISTERED_NET_IRQS.lock().contains(&60));

    reset_irq_state();
}

#[def_test(serial)]
fn test_failed_try_new_does_not_leak_irq_handle() {
    reset_irq_state();

    let mut transport = MockTransport::new();
    transport.device_type = virtio_drivers::transport::DeviceType::Network;
    let _dev = VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None);

    assert!(NET_IRQ_HANDLES.lock().is_empty());

    reset_irq_state();
}

#[def_test(serial)]
fn test_rx_scheduler_requires_registered_irq_but_allows_detach() {
    reset_irq_state();

    let mut transport = MockTransport::new();
    transport.device_type = virtio_drivers::transport::DeviceType::Network;
    let dev =
        VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();
    let scheduler = Arc::new(MockRxScheduler {
        schedules: AtomicUsize::new(0),
    });

    assert_eq!(
        dev.set_rx_scheduler(Some(scheduler)),
        Err(DriverError::Unsupported)
    );
    assert_eq!(dev.set_rx_scheduler(None), Ok(()));

    reset_irq_state();
}

#[def_test]
fn test_recv_acknowledges_interrupt_in_poll_mode() {
    let mut transport = MockTransport::new();
    transport.device_type = virtio_drivers::transport::DeviceType::Network;
    let interrupt_ack_count = transport.interrupt_ack_count.clone();
    let dev =
        VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();

    assert!(matches!(dev.recv(), Err(DriverError::WouldBlock)));
    assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_recv_leaves_interrupt_ack_to_registered_handler() {
    reset_irq_state();

    let mut transport = MockTransport::new();
    transport.device_type = virtio_drivers::transport::DeviceType::Network;
    let interrupt_ack_count = transport.interrupt_ack_count.clone();
    let mut dev =
        VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None, None).unwrap();
    let irq = 73;
    let handle_id = 53;
    NET_IRQ_HANDLES
        .lock()
        .push(Arc::new(VirtIoNetIrqHandle::<MockHal, MockTransport, 32> {
            inner: dev.inner.clone(),
            rx_scheduler: dev.rx_scheduler.clone(),
            id: handle_id,
            irq,
        }));
    REGISTERED_NET_IRQS.lock().insert(irq);
    dev.irq = Some(irq);
    dev.irq_handle_id = Some(handle_id);

    assert!(matches!(dev.recv(), Err(DriverError::WouldBlock)));
    assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 0);

    let _ = handle_virtio_net_irq(irq);
    assert_eq!(interrupt_ack_count.load(Ordering::Relaxed), 1);

    drop(dev);
    reset_irq_state();
}

#[def_test(serial)]
fn test_irq_handler_filters_irq_and_schedules_rx() {
    reset_irq_state();

    let scheduler = Arc::new(MockRxScheduler {
        schedules: AtomicUsize::new(0),
    });
    NET_IRQ_HANDLES
        .lock()
        .push(handled_mock_handle(50, 70, None));
    NET_IRQ_HANDLES
        .lock()
        .push(handled_mock_handle(51, 71, Some(scheduler.clone())));

    let event = handle_virtio_net_irq(71);

    assert!(event.has_source(RX_SRC));
    assert!(event.has_source(TX_SRC));
    assert_eq!(scheduler.schedules.load(Ordering::Relaxed), 1);

    reset_irq_state();
}

#[def_test(serial)]
fn test_irq_handler_treats_matched_empty_ack_as_miss() {
    reset_irq_state();

    let scheduler = Arc::new(MockRxScheduler {
        schedules: AtomicUsize::new(0),
    });
    NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
        id: 52,
        irq_num: 72,
        handled: false,
        scheduler: Some(scheduler.clone()),
    }));

    let event = handle_virtio_net_irq(72);

    assert!(!event.handled());
    assert!(!event.has_source(RX_SRC));
    assert_eq!(scheduler.schedules.load(Ordering::Relaxed), 0);

    reset_irq_state();
}

#[def_test(serial)]
fn test_remove_irq_handle_uses_id_not_stack_order() {
    reset_irq_state();

    NET_IRQ_HANDLES.lock().push(mock_handle(60, 80));
    NET_IRQ_HANDLES.lock().push(mock_handle(61, 81));
    NET_IRQ_HANDLES.lock().push(mock_handle(62, 82));

    remove_virtio_net_irq_handle(61);

    let handles = NET_IRQ_HANDLES.lock();
    assert_eq!(handles.len(), 2);
    assert_eq!(handles[0].handle_id(), 60);
    assert_eq!(handles[1].handle_id(), 62);
    drop(handles);

    reset_irq_state();
}

#[def_test]
fn test_virtio_net_concurrency_traits() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VirtIoNetDev<MockHal, MockTransport, 32>>();
}
