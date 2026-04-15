// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::mem::PhysAddr;
use kplat::sys::SysCtrl;

const GED_PADDR: usize = 0x100E_001C;

struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(cpu_id: usize, stack_top_paddr: usize) {
        crate::mp::start_secondary_cpu(cpu_id, pa!(stack_top_paddr));
    }

    fn shutdown() -> ! {
        let halt_addr = memspace::iomap_device(PhysAddr::from_usize(GED_PADDR), 0x1000, "ged")
            .unwrap_or_else(|err| panic!("failed to iomap ged: {err:?}"))
            .as_mut_ptr();
        info!("Shutting down...");
        unsafe { halt_addr.write_volatile(0x34) };
        karch::stop_cpu();
        warn!("It should shutdown!");
        loop {
            karch::stop_cpu();
        }
    }
}
