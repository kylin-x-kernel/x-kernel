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

const CFG_SIZE_OFFSET: usize = 2;
const CFG_DATA_OFFSET: usize = 8;

fn config_data(offset: usize, bytes: &[u8]) -> u8 {
    offset
        .checked_sub(CFG_DATA_OFFSET)
        .and_then(|index| bytes.get(index))
        .copied()
        .unwrap_or(0)
}

fn abs_record(offset: usize, words: [u32; 5]) -> u8 {
    if offset == CFG_SIZE_OFFSET {
        return 20;
    }
    let Some(index) = offset.checked_sub(CFG_DATA_OFFSET) else {
        return 0;
    };
    let Some(word) = words.get(index / 4) else {
        return 0;
    };
    word.to_le_bytes()[index % 4]
}

/// Builds a transport answering the config selects that input probing uses.
///
/// `has_abs == false` models a device without `EV_ABS` (the query reports a
/// zero-length bitmap); `has_abs == true` with an all-zero bitmap models a
/// device that has absolute support but advertises no axes.
fn input_transport_config(
    abs_bitmap: [u8; 8],
    has_abs: bool,
    prop_bits: Option<[u8; 4]>,
) -> MockTransport {
    let mut transport = MockTransport::new_with_type(DeviceType::Input);
    transport.config_handler = Some(Arc::new(move |select, subsel, offset| match select {
        // VIRTIO_INPUT_CFG_ID_NAME
        0x01 => {
            if offset == CFG_SIZE_OFFSET {
                8
            } else {
                config_data(offset, b"test\0\0\0\0")
            }
        }
        // VIRTIO_INPUT_CFG_ID_DEVIDS
        0x03 => {
            if offset == CFG_SIZE_OFFSET {
                8
            } else {
                config_data(offset, &[1, 0, 2, 0, 3, 0, 4, 0])
            }
        }
        // VIRTIO_INPUT_CFG_PROP_BITS
        0x10 => match prop_bits {
            Some(bits) => {
                if offset == CFG_SIZE_OFFSET {
                    4
                } else {
                    config_data(offset, &bits)
                }
            }
            None => 0,
        },
        // VIRTIO_INPUT_CFG_EVBITS for EV_ABS
        0x11 if subsel == EventType::Absolute as u8 => {
            if !has_abs {
                0
            } else if offset == CFG_SIZE_OFFSET {
                8
            } else {
                config_data(offset, &abs_bitmap)
            }
        }
        0x11 => 0,
        // VIRTIO_INPUT_CFG_ABSINFO
        0x12 => match subsel {
            0 => abs_record(offset, [(-1024i32) as u32, 1024, 0, 0, 0]),
            2 => abs_record(offset, [0, 32_767, 1, 2, 3]),
            _ => 0,
        },
        _ => 0,
    }));
    transport
}

fn input_transport() -> MockTransport {
    input_transport_config([0b0000_0101, 0, 0, 0, 0, 0, 0, 0], true, Some([2, 0, 0, 0]))
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
fn test_virtio_input_abs_info_is_cached_with_signed_fields() {
    let transport = input_transport();
    let reads = transport.config_read_count.clone();
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let reads_after_probe = reads.load(Ordering::Relaxed);

    assert_eq!(
        device.abs_info(0).unwrap(),
        Some(AbsInfo::new(-1024, 1024, 0, 0, 0))
    );
    assert_eq!(
        device.abs_info(2).unwrap(),
        Some(AbsInfo::new(0, 32_767, 1, 2, 3))
    );
    assert_eq!(device.abs_info(1).unwrap(), None);
    assert_eq!(device.abs_info(u16::MAX).unwrap(), None);

    // Another ioctl must not touch config space.
    assert_eq!(reads.load(Ordering::Relaxed), reads_after_probe);
}

#[def_test]
fn test_virtio_input_abs_info_is_absent_without_ev_abs() {
    let transport = input_transport_config([0; 8], false, None);
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();

    assert_eq!(device.abs_info(0).unwrap(), None);
}

#[def_test]
fn test_virtio_input_abs_info_is_absent_for_unadvertised_axis() {
    let transport = input_transport_config([0; 8], true, None);
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();

    assert_eq!(device.abs_info(0).unwrap(), None);
}

#[def_test]
fn test_virtio_input_prop_bits_are_cached_and_truncated() {
    let transport = input_transport();
    let reads = transport.config_read_count.clone();
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let reads_after_probe = reads.load(Ordering::Relaxed);

    let mut bits = [0xAAu8; 8];
    assert!(device.get_prop_bits(&mut bits).unwrap());
    assert_eq!(bits[0], 2);
    for byte in &bits[1..PROP_BITMAP_LEN] {
        assert_eq!(*byte, 0);
    }
    for byte in &bits[PROP_BITMAP_LEN..] {
        assert_eq!(*byte, 0xAA);
    }

    let mut short = [0xAAu8; 3];
    assert!(device.get_prop_bits(&mut short).unwrap());
    assert_eq!(short, [2, 0, 0]);

    assert_eq!(reads.load(Ordering::Relaxed), reads_after_probe);
}

#[def_test]
fn test_virtio_input_prop_bits_absent_without_prop_config() {
    let transport = input_transport_config([0; 8], true, None);
    let device = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11).unwrap();
    let mut bits = [0xAAu8; 8];

    assert!(!device.get_prop_bits(&mut bits).unwrap());
    for byte in &bits[..PROP_BITMAP_LEN] {
        assert_eq!(*byte, 0);
    }
    for byte in &bits[PROP_BITMAP_LEN..] {
        assert_eq!(*byte, 0xAA);
    }
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
