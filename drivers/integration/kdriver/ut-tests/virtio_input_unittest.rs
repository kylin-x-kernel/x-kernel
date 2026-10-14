// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use pci::{Cam, DeviceFunction, PciConfigAccess};
use unittest::{assert_eq, def_test};

use super::set_pci_intx_disabled;

fn config_for_command(command: u32) -> ([u32; 64], DeviceFunction) {
    let mut config_space = [0u32; 64];
    config_space[1] = command;
    let bdf = DeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    };
    (config_space, bdf)
}

#[def_test]
fn input_cleanup_masks_intx_without_writing_pci_status() {
    let (mut config_space, bdf) = config_for_command(0xabcd_8807);
    // SAFETY: the aligned test array covers function 0's command register
    // and remains alive for every access through `config`.
    let mut config =
        unsafe { PciConfigAccess::new(config_space.as_mut_ptr().cast::<u8>(), Cam::MmioCam) };

    set_pci_intx_disabled(&mut config, bdf, true);

    assert_eq!(config_space[1], 0xabcd_8c07);
}

#[def_test]
fn input_activation_unmasks_intx_without_writing_pci_status() {
    let (mut config_space, bdf) = config_for_command(0xabcd_8c07);
    // SAFETY: the aligned test array covers function 0's command register
    // and remains alive for every access through `config`.
    let mut config =
        unsafe { PciConfigAccess::new(config_space.as_mut_ptr().cast::<u8>(), Cam::MmioCam) };

    set_pci_intx_disabled(&mut config, bdf, false);

    assert_eq!(config_space[1], 0xabcd_8807);
}
