// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Floating-point/SIMD operations for AArch64.

use aarch64_cpu::{
    asm::barrier,
    registers::{CPACR_EL1, Writeable},
};

/// Enable FP/SIMD instructions by setting the `FPEN` field in `CPACR_EL1`.
#[inline]
pub fn enable_fp() {
    CPACR_EL1.write(CPACR_EL1::FPEN::TrapNothing);
    barrier::isb(barrier::SY);
}
