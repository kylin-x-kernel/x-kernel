// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform performance monitoring interface.

use kplat_macros::device_interface;

#[device_interface]
pub trait PmuInterruptIf {
    /// Register the PMU overflow-dispatch handler on the platform's PMU
    /// interrupt line.
    ///
    /// The platform owns the architecture-specific wiring: the PMU IRQ
    /// number, the interrupt descriptor, and the delivery mode (a normal IRQ
    /// handler, or an NMI handler when the PMU is the compiled NMI source).
    /// Called once at boot, before IRQs are enabled.
    fn register_overflow_irq() -> bool;
    /// Enable the PMU interrupt line on the current CPU.
    fn enable_irq();
}
