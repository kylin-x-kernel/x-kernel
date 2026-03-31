// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::boot::{BootHandler, BootInfo};
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(boot_info: &BootInfo) {
        crate::mem::early_init(boot_info.dtb_addr, boot_info.kernel_load_paddr);
        crate::console::early_init();
        crate::time::early_init();
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_boot_info: &BootInfo) {
        crate::irq::init_percpu();
        crate::time::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        crate::irq::init_percpu();
        crate::time::init_percpu();
    }
}
