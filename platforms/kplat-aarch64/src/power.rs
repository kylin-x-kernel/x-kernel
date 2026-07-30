// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power control implementation for aarch64.

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::KResult;
use kplat::sys::SysCtrl;

#[impl_dev_interface]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, _stack_top_paddr: usize) -> KResult {
        use khal::mem::{v2p, va};
        let entry_paddr = v2p(va!(
            kernel_boot::arch::_start_secondary as *const () as usize
        ));
        let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for logical CPU {}",
                logical_cpu_id.as_usize()
            )
        });
        crate::peripherals::psci::cpu_on(raw_cpu_id, entry_paddr.as_usize(), 0).map_err(Into::into)
    }

    fn shutdown() -> ! {
        crate::peripherals::psci::shutdown()
    }
}
