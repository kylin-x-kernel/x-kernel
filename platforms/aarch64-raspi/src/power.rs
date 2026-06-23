// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raspberry Pi system control implementation.
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::KResult;
use kplat::sys::SysCtrl;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) -> KResult {
        let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for logical CPU {}",
                logical_cpu_id.as_usize()
            )
        });
        crate::mp::start_secondary_cpu(raw_cpu_id.as_usize(), khal::mem::pa!(stack_top_paddr));
        Ok(())
    }
    fn shutdown() -> ! {
        log::info!("Shutting down...");
        loop {
            karch::stop_cpu();
        }
    }
}
