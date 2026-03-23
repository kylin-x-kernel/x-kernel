// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::boot::{BootHandler, BootInfo};
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(boot_info: &BootInfo) {
        x86_peripherals::ns16550::init();
        x86_peripherals::tsc_timer::early_init();
        crate::mem::init(boot_info);
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_boot_info: &BootInfo) {
        crate::psci::init();
        x86_peripherals::apic::init_primary(kplat::memory::pa!(0xFEC0_0000));
        x86_peripherals::tsc_timer::init_primary();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        x86_peripherals::apic::init_secondary();
        x86_peripherals::tsc_timer::init_secondary();
    }
}
