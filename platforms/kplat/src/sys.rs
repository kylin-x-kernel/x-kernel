// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform system control interface.

use kplat_macros::device_interface;

#[device_interface]
pub trait SysCtrl {
    #[cfg(feature = "smp")]
    /// Boots an application processor.
    fn boot_ap(id: usize, stack_top: usize);

    /// Shuts down the system.
    fn shutdown() -> !;
}
