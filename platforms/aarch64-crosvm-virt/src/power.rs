// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power and SMP boot controls for the platform.
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kplat::sys::SysCtrl;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    /// Power on an application processor (AP) with a provided stack.
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, _stack_top_paddr: usize) {
        use khal::mem::{v2p, va};
        let entry_paddr = v2p(va!(
            kernel_boot::arch::_start_secondary as *const () as usize
        ));
        let raw_cpu_id = raw_cpu_id(logical_cpu_id);
        aarch64_peripherals::psci::cpu_on(raw_cpu_id, entry_paddr.as_usize(), 0);
    }

    /// Request a system shutdown through PSCI.
    fn shutdown() -> ! {
        aarch64_peripherals::psci::shutdown()
    }
}
