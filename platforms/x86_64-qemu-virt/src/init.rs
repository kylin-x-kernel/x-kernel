// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for x86_64-qemu-virt.

use kplat::boot::{BootHandler, BootInfo};
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(boot_info: &BootInfo) {
        x86_peripherals::ns16550::init();
        x86_peripherals::tsc_timer::early_init();
        x86_peripherals::bootmem::init_ap_trampoline_page(boot_info);
        crate::mem::init(boot_info);
        crate::acpi::init();
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_boot_info: &BootInfo) {
        let io_apic_paddr = ::acpi::find_io_apic_from_init()
            .map(|entry| kplat::memory::pa!(entry.address as usize))
            .unwrap_or_else(|| {
                warn!("ACPI MADT IOAPIC not found, fallback to static IOAPIC base");
                kplat::memory::pa!(0xFEC0_0000)
            });
        x86_peripherals::apic::init_primary(io_apic_paddr);
        x86_peripherals::tsc_timer::init_primary();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        x86_peripherals::apic::init_secondary();
        x86_peripherals::tsc_timer::init_secondary();
    }
}
