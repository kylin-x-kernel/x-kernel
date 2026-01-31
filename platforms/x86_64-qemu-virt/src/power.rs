// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power control implementation for x86_64-qemu-virt.

use kplat::sys::SysCtrl;
use x86_64::instructions::port::PortWriteOnly;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(cpu_id: usize, stack_top_paddr: usize) {
        use kplat::memory::pa;
        crate::mp::start_secondary_cpu(cpu_id, pa!(stack_top_paddr))
    }

    fn shutdown() -> ! {
        info!("Shutting down...");
        if cfg!(feature = "reboot-on-system-off") {
            kplat::kprintln!("System will reboot, press any key to continue ...");
            while super::console::getchar().is_none() {}
            kplat::kprintln!("Rebooting ...");
            unsafe { PortWriteOnly::new(0x64).write(0xfeu8) };
        } else {
            unsafe { PortWriteOnly::new(0x604).write(0x2000u16) };
        }
        kcpu::instrs::stop_cpu();
        warn!("It should shutdown!");
        loop {
            kcpu::instrs::stop_cpu();
        }
    }
}
