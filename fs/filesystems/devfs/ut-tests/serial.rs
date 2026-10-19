// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

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
