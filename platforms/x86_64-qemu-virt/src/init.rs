<<<<<<< HEAD
//! Platform initialization hooks for x86_64-qemu-virt.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)

use kplat::boot::BootHandler;
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(_cpu_id: usize, mbi: usize) {
        kcpu::boot::init_trap();
        crate::console::init();
        crate::time::early_init();
        crate::mem::init(mbi);
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {
        kcpu::boot::init_trap();
    }

    fn final_init(_cpu_id: usize, _arg: usize) {
        crate::apic::init_primary();
        crate::time::init_primary();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        crate::apic::init_secondary();
        crate::time::init_secondary();
    }
}
