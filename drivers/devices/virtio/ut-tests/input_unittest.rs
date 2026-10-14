// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};
use virtio_drivers::transport::{DeviceType, InterruptStatus};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

struct CountingNotifier(AtomicUsize);

impl InputEventNotifier for CountingNotifier {
    fn notify_events(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn test_virtio_input_init_failure() {
    let mut transport = MockTransport::new();
    transport.device_type = DeviceType::Input;
    let dev = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11);
    assert!(dev.is_err());
}

#[def_test]
fn test_virtio_input_traits() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VirtIoInputDev<MockHal, MockTransport>>();
}

fn input_transport() -> MockTransport {
    let transport = MockTransport::new_with_type(DeviceType::Input);
    let mut config = transport.config_space.borrow_mut();
    config[2] = 8;
    config[8..16].copy_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
    drop(config);
    transport
}

#[def_test]
fn test_virtio_input_irq_reports_line_and_ignores_empty_isr() {
    let transport = input_transport();
    let ack_count = transport.interrupt_ack_count.clone();
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let handler = device.irq_handler();

    assert_eq!(device.irq(), Some(11));
    assert!(!handler.handle(11).handled());
    assert_eq!(ack_count.load(Ordering::Relaxed), 1);
}

#[def_test]
fn test_virtio_input_irq_handles_queue_and_config_isr() {
    let transport = input_transport();
    let ack_count = transport.interrupt_ack_count.clone();
    let interrupt_status = transport.interrupt_status.clone();
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let handler = device.irq_handler();

    for status in [
        InterruptStatus::QUEUE_INTERRUPT,
        InterruptStatus::DEVICE_CONFIGURATION_INTERRUPT,
    ] {
        interrupt_status.store(status.bits() as usize, Ordering::Release);
        assert!(handler.handle(11).handled());
    }
    assert_eq!(ack_count.load(Ordering::Relaxed), 2);
}

#[def_test]
fn test_virtio_input_notifies_only_for_event_queue_interrupts() {
    let transport = input_transport();
    let interrupt_status = transport.interrupt_status.clone();
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let notifier = Arc::new(CountingNotifier(AtomicUsize::new(0)));
    device.set_event_notifier(Some(notifier.clone())).unwrap();
    let handler = device.irq_handler();

    interrupt_status.store(
        InterruptStatus::DEVICE_CONFIGURATION_INTERRUPT.bits() as usize,
        Ordering::Release,
    );
    assert!(handler.handle(11).handled());
    assert_eq!(notifier.0.load(Ordering::Relaxed), 0);

    interrupt_status.store(
        InterruptStatus::QUEUE_INTERRUPT.bits() as usize,
        Ordering::Release,
    );
    assert!(handler.handle(11).handled());
    assert_eq!(notifier.0.load(Ordering::Relaxed), 1);
}

#[def_test]
fn test_virtio_input_irq_rejects_other_line_without_ack() {
    let transport = input_transport();
    let ack_count = transport.interrupt_ack_count.clone();
    let interrupt_status = transport.interrupt_status.clone();
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let handler = device.irq_handler();
    interrupt_status.store(
        InterruptStatus::QUEUE_INTERRUPT.bits() as usize,
        Ordering::Release,
    );

    assert!(!handler.handle(12).handled());
    assert_eq!(ack_count.load(Ordering::Relaxed), 0);
}
