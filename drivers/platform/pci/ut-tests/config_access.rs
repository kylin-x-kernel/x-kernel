// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[repr(align(4))]
struct Window([u8; 16]);

#[repr(align(4096))]
struct BusWindow([u8; 0x800]);

fn function() -> DeviceFunction {
    DeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    }
}

#[def_test]
fn checked_access_validates_bdf_offset_width_and_window() {
    let mut window = Window([0; 16]);
    // SAFETY: the stack window stays alive for the test, is 4-byte aligned,
    // and the bound matches its size.
    let config =
        unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 16) };

    let invalid_bdf = DeviceFunction {
        device: 32,
        ..function()
    };
    assert_eq!(
        config.try_read(invalid_bdf, 0, 4),
        Err(PciConfigError::InvalidBdf)
    );
    assert_eq!(
        config.try_read(function(), 0x1000, 4),
        Err(PciConfigError::InvalidOffset)
    );
    assert_eq!(
        config.try_read(function(), 2, 4),
        Err(PciConfigError::InvalidOffset)
    );
    assert_eq!(
        config.try_read(function(), 0, 3),
        Err(PciConfigError::UnsupportedWidth)
    );
    assert_eq!(
        config.try_read(function(), 0x10, 4),
        Err(PciConfigError::OutOfWindow)
    );
}

#[def_test]
fn interrupt_pin_reads_only_the_pin_register() {
    let mut window = BusWindow([0; 0x800]);
    // SAFETY: the stack window stays alive for the test, is 4-byte aligned,
    // and the bound matches its size.
    let config =
        unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 0x800) };

    // The Interrupt Line byte must not influence the pin.
    assert_eq!(config.try_write(function(), 0x3C, 1, 0xaa), Ok(()));
    for pin in [0, 1, 4, 0xff] {
        assert_eq!(config.try_write(function(), 0x3D, 1, pin), Ok(()));
        assert_eq!(interrupt_pin(&config, function()), Ok(pin as u8));
    }
}

#[def_test]
fn interrupt_pin_reports_configuration_failures() {
    let mut window = Window([0; 16]);
    // SAFETY: the stack window stays alive for the test, is 4-byte aligned,
    // and the bound matches its size.
    let config =
        unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 16) };

    let invalid_bdf = DeviceFunction {
        device: 32,
        ..function()
    };
    assert_eq!(
        interrupt_pin(&config, invalid_bdf),
        Err(PciConfigError::InvalidBdf)
    );
    assert_eq!(
        interrupt_pin(&config, function()),
        Err(PciConfigError::OutOfWindow)
    );
}

#[def_test]
fn checked_write_does_not_read_modify_write_the_enclosing_word() {
    let mut window = Window([0; 16]);
    // SAFETY: the stack window stays alive for the test, is 4-byte aligned,
    // and the bound matches its size.
    let config =
        unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 16) };

    assert_eq!(config.try_write(function(), 0, 4, 0x1122_3344), Ok(()));
    assert_eq!(config.try_write(function(), 1, 1, 0xaa), Ok(()));
    assert_eq!(
        config.try_read(function(), 0, 4),
        Ok(u32::from_ne_bytes([0x44, 0xaa, 0x22, 0x11]))
    );
    assert_eq!(config.try_read(function(), 2, 2), Ok(0x1122));
}

#[def_test]
fn window_bound_rejects_the_first_out_of_window_access() {
    let mut window = BusWindow([0; 0x800]);
    // SAFETY: the stack window stays alive for the test, is 4-byte aligned,
    // and the bound matches its size.
    let config =
        unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 0x800) };

    assert_eq!(config.try_write(function(), 0x7fc, 4, 0x1234_5678), Ok(()));
    assert_eq!(config.try_read(function(), 0x7fc, 4), Ok(0x1234_5678));
    assert_eq!(
        config.try_read(function(), 0x800, 4),
        Err(PciConfigError::OutOfWindow)
    );
    assert_eq!(
        config.try_write(function(), 0x800, 1, 0xff),
        Err(PciConfigError::OutOfWindow)
    );
}

#[def_test]
fn checked_access_is_zero_extended_and_width_exact() {
    let mut window = Window([0xff; 16]);
    // SAFETY: the stack window stays alive for the test, is 4-byte aligned,
    // and the bound matches its size.
    let config =
        unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 16) };

    assert_eq!(config.try_read(function(), 0x1, 1), Ok(0xff));
    assert_eq!(config.try_read(function(), 0x2, 2), Ok(0xffff));
    assert_eq!(config.try_write(function(), 0x4, 2, 0x1234), Ok(()));
    assert_eq!(
        config.try_read(function(), 0x4, 2),
        Ok(0x1234)
    );
    assert_eq!(
        config.try_read(function(), 0x6, 2),
        Ok(0xffff)
    );
}
