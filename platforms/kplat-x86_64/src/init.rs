// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for kplat-x86_64.

use kbuild_config::TIMER_FREQUENCY_HZ;
use khal::mem::pa;
use kplat::boot::{BootInfo, PlatformBootIf};

fn io_apic_paddr_from_firmware_or_fallback() -> khal::mem::PhysAddr {
    khal::firmware::devices::io_apic_paddr()
        .map(|addr| pa!(addr))
        .unwrap_or_else(|| {
            warn!("ACPI MADT IOAPIC not found, fallback to static IOAPIC base");
            pa!(0xFEC0_0000)
        })
}

#[kiface::provide]
impl PlatformBootIf {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        crate::peripherals::bootmem::init_ap_trampoline_page(boot_info);
    }

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        timer_driver::x86_lapic_tsc::early_init(
            timer_driver::x86_lapic_tsc::TimerConfig::platform_static(
                ktime_types::Frequency::from_hz(TIMER_FREQUENCY_HZ as u64),
            ),
        );
        kernel_boot::bootln!("timer init");
        console_driver::init_stdout_ioport(
            console_driver::boot_console_io_port(),
            Some(
                kirq::IrqDesc::new(4, kirq::IrqTrigger::EdgeRising)
                    .with_source(kirq::IrqSource::PlatformStatic)
                    .with_controller(kirq::IrqController::IoApic)
                    .with_domain(kirq::IO_APIC_DOMAIN),
            ),
        );
        kernel_boot::bootln!("console driver init");
        #[cfg(feature = "rtc")]
        {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: stdout configuration \
                          cannot initialize; early_driver_init has no error return or recovery"
            )]
            let sample = rtc_driver::read(rtc_driver::RtcConfig::platform(
                rtc_driver::RtcKind::Cmos,
                rtc_driver::RtcSource::PlatformStatic,
            ))
            .expect("failed to read platform rtc");
            ktime::initialize_realtime(sample);
        }
        kernel_boot::bootln!("rtc init");
    }

    fn prepare_interrupts(_boot_info: &BootInfo) -> kerrno::KResult<()> {
        x86_apic::init_primary(io_apic_paddr_from_firmware_or_fallback());
        Ok(())
    }

    #[cfg(feature = "smp")]
    fn prepare_local_interrupts(_logical_cpu_id: kcpu_id_map::LogicalCpuId) -> kerrno::KResult<()> {
        x86_apic::init_secondary();
        Ok(())
    }

    fn activate_local_interrupts() -> kerrno::KResult<()> {
        timer_driver::x86_lapic_tsc::init_percpu();
        Ok(())
    }
}
