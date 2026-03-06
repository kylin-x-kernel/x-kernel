// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exception/trap vector operations for LoongArch64.

use loongArch64::register::{ecfg, eentry};

/// Writes the Exception Entry Base Address register (`EENTRY`).
///
/// It also sets the Exception Configuration register (`ECFG`) to `VS=0`.
///
/// - ECFG: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#exception-configuration>
/// - EENTRY: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#exception-entry-base-address>
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(addr: usize) {
    ecfg::set_vs(0);
    eentry::set_eentry(addr);
}
