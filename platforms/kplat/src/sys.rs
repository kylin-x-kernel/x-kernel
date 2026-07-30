// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform system control interface.

#[cfg(feature = "smp")]
use kcpu_id_map::LogicalCpuId;
#[cfg(feature = "smp")]
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

    /// Shuts down the system.
    fn shutdown() -> !;
}
