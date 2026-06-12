// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power control implementation for x86_64-qemu-virt.

#[cfg(feature = "smp")]
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kplat::sys::SysCtrl;
use x86_64::instructions::port::PortWriteOnly;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) {
        use khal::mem::pa;

        let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for logical CPU {}",
                logical_cpu_id.as_usize()
            )
        });
        crate::mp::start_secondary_cpu(raw_cpu_id, pa!(stack_top_paddr))
    }

    fn shutdown() -> ! {
        info!("Shutting down...");
        if cfg!(feature = "reboot-on-system-off") {
            khal::kprintln!("System will reboot, press any key to continue ...");
            while console_driver::getchar().is_none() {}
            khal::kprintln!("Rebooting ...");
            unsafe { PortWriteOnly::new(0x64).write(0xfeu8) };
        } else {
            unsafe { PortWriteOnly::new(0x604).write(0x2000u16) };
        }
        karch::stop_cpu();
        warn!("It should shutdown!");
        loop {
            karch::stop_cpu();
        }
    }
}
