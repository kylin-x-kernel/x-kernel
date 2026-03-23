// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform boot-stage interface definitions.

pub use boot_info::BootInfo;
use kplat_macros::device_interface;

#[device_interface]
pub trait BootHandler {
    /// Early initialization on the boot CPU.
    fn early_init(boot_info: &BootInfo);

    #[cfg(feature = "smp")]
    /// Early initialization on an application processor (SMP only).
    fn early_init_ap(id: usize);

    /// Final initialization on the boot CPU.
    fn final_init(boot_info: &BootInfo);

    #[cfg(feature = "smp")]
    /// Final initialization on an application processor (SMP only).
    fn final_init_ap(id: usize);
}
