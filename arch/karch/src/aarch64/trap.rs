// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exception/trap vector operations for AArch64.

use aarch64_cpu::registers::Writeable;

/// Writes the exception vector base address register (`VBAR_EL1` or `VBAR_EL2`).
///
/// When the `arm-el2` feature is enabled, writes `VBAR_EL2`; otherwise
/// writes `VBAR_EL1`.
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(addr: usize) {
    #[cfg(not(feature = "arm-el2"))]
    {
        use aarch64_cpu::registers::VBAR_EL1;
        VBAR_EL1.set(addr as _);
    }
    #[cfg(feature = "arm-el2")]
    {
        use aarch64_cpu::registers::VBAR_EL2;
        VBAR_EL2.set(addr as _);
    }
}
