<<<<<<< HEAD
//! Platform system control interface.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)

use kplat_macros::device_interface;

#[device_interface]
pub trait SysCtrl {
    #[cfg(feature = "smp")]
    /// Boots an application processor.
    fn boot_ap(id: usize, stack_top: usize);

    /// Shuts down the system.
    fn shutdown() -> !;
}
