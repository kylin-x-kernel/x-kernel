// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power control implementation for x86_64-qemu-virt.

#[cfg(feature = "smp")]
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::KResult;
use kplat::sys::SysCtrl;
use x86_64::instructions::port::PortWriteOnly;
#[impl_dev_interface]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) -> KResult {
        use khal::mem::pa;

        let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for logical CPU {}",
                logical_cpu_id.as_usize()
            )
        });
        crate::mp::start_secondary_cpu(raw_cpu_id, pa!(stack_top_paddr));
        Ok(())
    }

    fn shutdown() -> ! {
        info!("Shutting down...");
        if cfg!(feature = "reboot-on-system-off") {
            khal::kprintln!("System will reboot, press any key to continue ...");
            while console_driver::getchar().is_none() {}
            khal::kprintln!("Rebooting ...");
            // SAFETY: port `0x64` is the standard x86 keyboard-controller command
            // port, and writing `0xfe` requests a CPU reset on this platform.
            unsafe { PortWriteOnly::new(0x64).write(0xfeu8) };
        } else {
            // SAFETY: port `0x604` is the QEMU/ACPI power-management shutdown port
            // for this platform, and writing `0x2000` requests power-off.
            unsafe { PortWriteOnly::new(0x604).write(0x2000u16) };
        }
        karch::stop_cpu();
        warn!("It should shutdown!");
        loop {
            karch::stop_cpu();
        }
    }
}
