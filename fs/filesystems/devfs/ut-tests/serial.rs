// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{Device, DeviceKind, DriverResult};
use kclass::{CharDevice, CharDeviceImpl, ClassDevice};
use kpoll::IoEvents;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::nodes::test_device::open_character_node;

#[def_test]
fn auxiliary_serial_minors_do_not_overlap_fixed_vts() {
    let tty0 = DeviceId::new(SERIAL_MAJOR, 0);
    let tty1 = DeviceId::new(SERIAL_MAJOR, 1);
    let first_serial = serial_device_id(0);

    assert_eq!(first_serial.minor(), SERIAL_MINOR_BASE);
    assert!(SERIAL_MINOR_BASE > tty0.minor());
    assert!(SERIAL_MINOR_BASE > tty1.minor());
    assert_ne!(first_serial, tty0);
    assert_ne!(first_serial, tty1);
    // The production mapping is monotonic, so every later serialN minor
    // remains above both fixed VT minors as well.
    assert!(serial_device_id(1).minor() > tty1.minor());
}

/// A UART that is always writable and never has input, enough to observe the
/// node's poll capability without any hardware.
struct TestUart;

impl Device for TestUart {
    fn name(&self) -> &str {
        "serial-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }
}

impl CharDevice for TestUart {
    fn read(&self, _buf: &mut [u8]) -> DriverResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        Ok(buf.len())
    }
}

/// Builds a `SerialDev` over a synthetic UART.
///
/// The handle reuses a live parent identity and is never published, so no boot
/// device or other test is affected.
fn test_serial_dev() -> Option<SerialDev> {
    let devices = char_devices();
    let parent = devices.first()?.parent();
    let runtime: CharDeviceImpl = Box::new(TestUart);
    let handle =
        ClassDevice::try_new_with_info(parent, runtime, "serial-test".into(), DeviceKind::Char, None)
            .ok()?;
    Some(SerialDev::new(handle))
}

/// A UART node is `tty_fops` in Linux and therefore pollable; without a handler
/// `epoll_ctl` would refuse it with `EPERM`, which no Linux UART does.
#[def_test(serial)]
fn serial_node_is_pollable_like_a_tty() {
    let Some(dev) = test_serial_dev() else {
        return unittest::TestResult::Ignored;
    };
    let file = open_character_node("serial0", Arc::new(dev));
    assert!(file.can_poll());
    let expected = IoEvents::IN | IoEvents::OUT;
    assert_eq!(
        file.poll_handler().map(|events| events.bits()),
        Some(expected.bits())
    );
}