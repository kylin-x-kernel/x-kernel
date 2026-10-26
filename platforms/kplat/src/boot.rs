// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform boot-stage interface definitions.

pub use boot_info::BootInfo;
#[cfg(feature = "smp")]
use kcpu_id_map::LogicalCpuId;
use kplat_macros::device_interface;

#[device_interface]
pub trait PlatformBootIf {
    /// Platform-specific boot-memory preparation before common memory assembly.
    fn prepare_boot_memory(boot_info: &BootInfo);

    /// Firmware-specific platform initialization after DT/ACPI parsing.
    fn firmware_init(boot_info: &BootInfo);

    /// Early driver initialization after runtime page tables are active.
    ///
    /// This hook is only for hardware required before generic descriptor-first
    /// probing can run, such as timer, interrupt controller, and boot console
    /// setup. Devices initialized here should be adopted into the runtime
    /// driver model only when a later subsystem needs a `DeviceObject` handle.
    fn early_driver_init();

    /// Complete boot-CPU controller preparation before scheduler setup.
    ///
    /// CPU IRQ delivery must remain masked. Clockevent activation is separate.
    /// Early console/controller discovery still belongs to `early_driver_init`.
    ///
    /// # Errors
    /// Returns an error if mandatory controller preparation cannot complete.
    fn prepare_interrupts(boot_info: &BootInfo) -> kerrno::KResult<()>;

    #[cfg(feature = "smp")]
    /// Prepare an AP's controller interface with CPU IRQ delivery masked.
    ///
    /// # Errors
    /// Returns an error if local controller preparation fails.
    fn prepare_local_interrupts(logical_cpu_id: LogicalCpuId) -> kerrno::KResult<()>;

    /// Activate local foundation sources after their handlers are installed.
    ///
    /// Runs with current installed and CPU IRQ delivery masked. Does not start
    /// tasks, register handlers, open global IRQ delivery or publish readiness.
    ///
    /// # Errors
    /// Returns an error if local source activation fails.
    fn activate_local_interrupts() -> kerrno::KResult<()>;
}
