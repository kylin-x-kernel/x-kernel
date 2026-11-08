// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_os_api::{PortWidth, ResError};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn port_width_reports_bytes_and_value_mask() {
    assert_eq!(PortWidth::U8.bytes(), 1);
    assert_eq!(PortWidth::U16.bytes(), 2);
    assert_eq!(PortWidth::U32.bytes(), 4);
    assert_eq!(PortWidth::U8.value_mask(), 0xff);
    assert_eq!(PortWidth::U16.value_mask(), 0xffff);
    assert_eq!(PortWidth::U32.value_mask(), 0xffff_ffff);
}

#[def_test]
fn port_width_rejects_values_that_do_not_fit() {
    assert!(PortWidth::U8.accepts(0xff));
    assert!(!PortWidth::U8.accepts(0x100));
    assert!(PortWidth::U16.accepts(0xffff));
    assert!(!PortWidth::U16.accepts(0x1_0000));
    assert!(PortWidth::U32.accepts(u32::MAX));
}

#[def_test]
fn port_access_must_stay_inside_16_bit_port_space() {
    assert_eq!(checked_port(0xffff, PortWidth::U8), Ok(()));
    assert_eq!(checked_port(0xfffe, PortWidth::U16), Ok(()));
    assert_eq!(
        checked_port(0xffff, PortWidth::U16),
        Err(ResError::InvalidResource)
    );
    assert_eq!(
        checked_port(0xfffc, PortWidth::U32),
        Ok(())
    );
    assert_eq!(
        checked_port(0xfffd, PortWidth::U32),
        Err(ResError::InvalidResource)
    );
}

#[def_test]
fn port_write_rejects_values_wider_than_the_transfer() {
    assert_eq!(checked_write(0x80, PortWidth::U8, 0xff), Ok(()));
    assert_eq!(
        checked_write(0x80, PortWidth::U8, 0x100),
        Err(ResError::InvalidResource)
    );
    assert_eq!(checked_write(0x80, PortWidth::U16, 0xffff), Ok(()));
    assert_eq!(
        checked_write(0x80, PortWidth::U16, 0x1_0000),
        Err(ResError::InvalidResource)
    );
    assert_eq!(checked_write(0x80, PortWidth::U32, u32::MAX), Ok(()));
}

#[cfg(not(target_arch = "x86_64"))]
#[def_test]
fn port_capability_is_unsupported_without_port_space() {
    let provider = XKernelResourceProvider;
    assert_eq!(
        provider.read_port(0, PortWidth::U8),
        Err(ResError::Unsupported)
    );
    assert_eq!(
        provider.write_port(0, PortWidth::U8, 0),
        Err(ResError::Unsupported)
    );
}
