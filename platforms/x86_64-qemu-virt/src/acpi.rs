// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::boot::BootInfo;
pub(crate) use x86_peripherals::acpi::find_mcfg;

pub(crate) fn init(boot_info: &BootInfo) {
    kprintln!("ACPI init: rsdp_addr={:#x}", boot_info.rsdp_addr);
    let Some(mcfg) = find_mcfg(boot_info.rsdp_addr) else {
        kprintln!("ACPI MCFG not found, fallback to static PCI ECAM config");
        return;
    };

    if mcfg.pci_segment != 0 {
        kprintln!(
            "ignoring ACPI MCFG segment {} base={:#x}",
            mcfg.pci_segment,
            mcfg.base_address
        );
        return;
    }

    kprintln!(
        "ACPI MCFG: ecam={:#x}, buses={:#x}..={:#x}",
        mcfg.base_address,
        mcfg.start_bus,
        mcfg.end_bus
    );
    kdriver::set_pci_config_space(mcfg.base_address, mcfg.end_bus);
}
