// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform system control interface.

#[cfg(feature = "smp")]
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use kplat_macros::device_interface;

#[device_interface]
pub trait SysCtrl {
    #[cfg(feature = "smp")]
    /// Boots an application processor selected by logical CPU ID.
    ///
    /// Platform implementations must translate the logical CPU ID to any
    /// firmware- or hardware-specific raw CPU identifier before issuing the
    /// actual boot command.
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top: usize) -> KResult;

    /// Halts the calling CPU without cutting system power.
    ///
    /// This is a bare platform terminal: it stops execution of the calling
    /// CPU (masking local interrupts first) and must never invoke the
    /// platform power-off agent. Higher-level cleanup such as filesystem
    /// sync, process teardown, or device removal is owned by callers and
    /// must have completed before this is reached.
    fn halt() -> !;

    /// Powers off the system through the platform power-off agent.
    ///
    /// This is a bare platform terminal (PSCI `SYSTEM_OFF`, SBI SRST
    /// shutdown, ACPI PM power-off port, and so on, depending on the
    /// platform). Higher-level cleanup such as filesystem sync, process
    /// teardown, or device removal is owned by callers and must have
    /// completed before this is reached.
    fn power_off() -> !;

    /// Suspends the system to RAM through the platform sleep agent (ACPI
    /// S3 on x86-64).
    ///
    /// Unlike `halt` and `power_off` this is not a terminal: it returns an
    /// error when the platform cannot suspend, and a platform that slept
    /// and resumed inline may return successfully. Higher-level cleanup —
    /// filesystem sync, device quiesce, stopping other CPUs — is owned by
    /// callers and must have completed before this is reached.
    fn suspend_to_ram() -> KResult;
}
