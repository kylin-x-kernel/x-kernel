// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;
use kplat::boot::{BootHandler, BootInfo};

#[kiface::provide]
impl BootHandler {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        let _ = boot_info;
    }

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        timer_driver::riscv_sbi::init(timer_driver::riscv_sbi::TimerConfig::platform_static(
            irq_driver::riscv::S_TIMER,
            ktime_types::Frequency::from_hz(10_000_000),
        ));
        irq_driver::riscv::init_primary();
        irq_driver::riscv::init_current_cpu_context();
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: stdout configuration \
                      cannot be parsed; early_driver_init has no error return or recovery"
        )]
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        if let Some(fdt) = of::fdt() {
            kcpu::init_hwprobe_from_fdt(fdt);
        }
        console_driver::register_input_irq_handler();
        #[cfg(feature = "rtc")]
        {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the device-tree RTC \
                          cannot supply the startup wall clock; early_driver_init has no error \
                          return or recovery"
            )]
            let sample =
                rtc_driver::read_from_device_tree().expect("failed to read rtc from device tree");
            ktime::initialize_realtime(sample);
        }
    }

    fn prepare_interrupts(_boot_info: &BootInfo) -> kerrno::KResult<()> {
        // The primary PLIC context was prepared during early discovery.
        Ok(())
    }

    #[cfg(feature = "smp")]
    fn prepare_local_interrupts(_logical_cpu_id: LogicalCpuId) -> kerrno::KResult<()> {
        irq_driver::riscv::init_current_cpu_context();
        Ok(())
    }

    fn activate_local_interrupts() -> kerrno::KResult<()> {
        irq_driver::riscv::activate_local_interrupts();
        timer_driver::riscv_sbi::init_percpu();
        Ok(())
    }
}
