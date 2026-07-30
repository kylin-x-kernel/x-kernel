// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "smp")]
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
#[cfg(feature = "smp")]
use kerrno::KResult;
use khal::mem::PhysAddr;
#[cfg(feature = "smp")]
use khal::mem::pa;
use kplat::sys::SysCtrl;

const GED_PADDR: usize = 0x100E_001C;

#[impl_dev_interface]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) -> KResult {
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
        let halt_addr = memspace::iomap_device(PhysAddr::from_usize(GED_PADDR), 0x1000, "ged")
            .unwrap_or_else(|err| panic!("failed to iomap ged: {err:?}"))
            .as_mut_ptr();
        info!("Shutting down...");
        // SAFETY: `halt_addr` comes from `iomap_device()` for the GED MMIO
        // page, and writing `0x34` to this documented shutdown register is the
        // required QEMU power-off sequence for this platform.
        unsafe { halt_addr.write_volatile(0x34) };
        karch::stop_cpu();
        warn!("It should shutdown!");
        loop {
            karch::stop_cpu();
        }
    }
}
