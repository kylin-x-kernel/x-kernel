// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use pci::{ConfigurationAccess, DeviceFunction, PciInitError, PciRoot, configure_device};

use super::*;

struct InvalidBarConfig;

impl ConfigurationAccess for InvalidBarConfig {
    fn read_word(&self, _bdf: DeviceFunction, offset: u8) -> u32 {
        if offset == 0x10 { 0b110 } else { 0 }
    }

    fn write_word(&mut self, _bdf: DeviceFunction, _offset: u8, _value: u32) {}

    unsafe fn unsafe_clone(&self) -> Self {
        Self
    }
}

#[unittest::def_test]
fn invalid_bar_encoding_returns_error() {
    let mut root = PciRoot::new(InvalidBarConfig);
    let bdf = DeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    };
    unittest::assert_eq!(
        configure_device(&mut root, bdf, &mut None),
        Err(PciInitError::InvalidBar)
    );
    unittest::assert_eq!(
        configure_pci_device_if_needed(&mut root, bdf),
        Err(DriverError::InvalidInput)
    );
}
