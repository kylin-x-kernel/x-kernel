// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared AArch64 SMCCC call helpers.

/// Raw return registers captured from an SMCCC call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmcccResult {
    pub x0: usize,
    pub x1: usize,
}

/// Invoke an SMCCC HVC call and return the raw `x0`/`x1` results.
pub fn hvc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    let x0;
    let x1;
    // SAFETY: this issues the standard AArch64 HVC trap with SMCCC register
    // conventions, using only caller-provided argument values and reading back
    // the returned `x0`/`x1` results.
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inlateout("x0") func as usize => x0,
            inlateout("x1") arg0 => x1,
            in("x2") arg1,
            in("x3") arg2,
        )
    }
    SmcccResult { x0, x1 }
}

/// Invoke an SMCCC SMC call and return the raw `x0`/`x1` results.
pub fn smc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    let x0;
    let x1;
    // SAFETY: this issues the standard AArch64 SMC trap with SMCCC register
    // conventions, using only caller-provided argument values and reading back
    // the returned `x0`/`x1` results.
    unsafe {
        core::arch::asm!(
            "smc #0",
            inlateout("x0") func as usize => x0,
            inlateout("x1") arg0 => x1,
            in("x2") arg1,
            in("x3") arg2,
        )
    }
    SmcccResult { x0, x1 }
}
