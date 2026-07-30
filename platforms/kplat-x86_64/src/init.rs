// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for kplat-x86_64.

use kbuild_config::TIMER_FREQUENCY_HZ;
use khal::mem::pa;
use kplat::boot::{BootHandler, BootInfo};

fn io_apic_paddr_from_firmware_or_fallback() -> khal::mem::PhysAddr {
    khal::firmware::devices::io_apic_paddr()
        .map(|addr| pa!(addr))
        .unwrap_or_else(|| {
            warn!("ACPI MADT IOAPIC not found, fallback to static IOAPIC base");
            pa!(0xFEC0_0000)
        })
}

#[impl_dev_interface]
impl BootHandler {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        crate::peripherals::bootmem::init_ap_trampoline_page(boot_info);
    }

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        timer_driver::x86_lapic_tsc::early_init(
            timer_driver::x86_lapic_tsc::TimerConfig::platform_static(TIMER_FREQUENCY_HZ as u64),
        );
        kernel_boot::bootln!("timer init");
        console_driver::init_stdout_ioport(
            console_driver::boot_console_io_port(),
            Some(
                khal::irq::IrqDesc::new(4, khal::irq::IrqTrigger::EdgeRising)
                    .with_source(khal::irq::IrqSource::PlatformStatic)
                    .with_controller(khal::irq::IrqController::IoApic)
                    .with_domain(khal::irq::IO_APIC_DOMAIN),
            ),
        );
        kernel_boot::bootln!("console driver init");
        #[cfg(feature = "rtc")]
        rtc_driver::init(
            rtc_driver::RtcConfig::platform(
                rtc_driver::RtcKind::Cmos,
                rtc_driver::RtcSource::PlatformStatic,
            ),
            timer_driver::x86_lapic_tsc::t2ns(timer_driver::x86_lapic_tsc::now_ticks()),
        );
        kernel_boot::bootln!("rtc init");
    }

    fn final_init(_boot_info: &BootInfo) {
        x86_apic::init_primary(io_apic_paddr_from_firmware_or_fallback());
        console_driver::register_input_irq_handler();
        timer_driver::x86_lapic_tsc::init_primary();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_logical_cpu_id: kcpu_id_map::LogicalCpuId) {
        x86_apic::init_secondary();
        timer_driver::x86_lapic_tsc::init_secondary();
    }
}
