// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform boot-stage interface definitions.

pub use boot_info::BootInfo;
use kcpu_id_map::LogicalCpuId;
use kplat_macros::device_interface;

#[device_interface]
pub trait BootHandler {
    /// Platform-specific boot-memory preparation before common memory assembly.
    fn prepare_boot_memory(boot_info: &BootInfo);

    /// Firmware-specific platform initialization after DT/ACPI parsing.
    fn firmware_init(boot_info: &BootInfo);

    /// Early driver initialization after runtime page tables are active.
    fn early_driver_init();

    /// Final initialization on the boot CPU.
    fn final_init(boot_info: &BootInfo);

    #[cfg(feature = "smp")]
    /// Final initialization on an application processor (SMP only).
    fn final_init_ap(logical_cpu_id: LogicalCpuId);
}
