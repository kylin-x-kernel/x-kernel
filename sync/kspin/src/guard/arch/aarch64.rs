// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Save IRQ state and disable local interrupts.
#[inline]
pub fn save_disable() -> usize {
    crate_interface::call_interface!(crate::guard::KernelGuardIf::save_disable)
}

/// Restore local interrupt state from saved flags.
#[inline]
pub fn restore(flags: usize) {
    crate_interface::call_interface!(crate::guard::KernelGuardIf::restore(flags))
}
